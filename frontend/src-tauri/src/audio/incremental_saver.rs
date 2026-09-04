use super::encode::encode_single_audio;
use super::recording_state::AudioChunk;
use anyhow::{anyhow, Result};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

use super::ffmpeg::find_ffmpeg_path;

/// Audio data without device type (we only store mixed audio)
#[derive(Clone)]
struct AudioData {
    data: Vec<f32>,
    // sample_rate: u32,
}

/// Incremental audio saver that writes checkpoints every 30 seconds
/// to minimize memory usage and enable crash recovery
pub struct IncrementalAudioSaver {
    checkpoint_buffer: Vec<AudioData>,
    checkpoint_interval_samples: usize, // 30s at 48kHz = 1,440,000 samples
    checkpoint_count: u32,
    checkpoints_dir: PathBuf,
    meeting_folder: PathBuf,
    final_audio_path: PathBuf,
    cleanup_checkpoints_on_finalize: bool,
    sample_rate: u32,
}

impl IncrementalAudioSaver {
    /// Create a new incremental saver
    ///
    /// # Arguments
    /// * `meeting_folder` - Path to the meeting folder (contains .checkpoints/)
    /// * `sample_rate` - Sample rate of audio (typically 48000)
    pub fn new(meeting_folder: PathBuf, sample_rate: u32) -> Result<Self> {
        Self::build(
            meeting_folder,
            sample_rate,
            PathBuf::from(".checkpoints"),
            PathBuf::from("audio.mp4"),
            true,
            false,
        )
    }

    /// Create a saver with an isolated checkpoint directory and output file.
    ///
    /// Both paths must be relative to `meeting_folder`. Unlike [`Self::new`],
    /// this constructor creates the checkpoint directory and deliberately
    /// leaves cleanup to the owner. That lets a multi-track owner finalize all
    /// tracks before removing the shared `.checkpoints` root atomically from
    /// the product's point of view.
    pub fn new_with_layout(
        meeting_folder: PathBuf,
        sample_rate: u32,
        checkpoint_subdirectory: impl AsRef<Path>,
        final_audio_file: impl AsRef<Path>,
    ) -> Result<Self> {
        Self::build(
            meeting_folder,
            sample_rate,
            checkpoint_subdirectory.as_ref().to_path_buf(),
            final_audio_file.as_ref().to_path_buf(),
            false,
            true,
        )
    }

    fn build(
        meeting_folder: PathBuf,
        sample_rate: u32,
        checkpoint_subdirectory: PathBuf,
        final_audio_file: PathBuf,
        cleanup_checkpoints_on_finalize: bool,
        create_checkpoints_dir: bool,
    ) -> Result<Self> {
        if sample_rate == 0 {
            return Err(anyhow!("Sample rate must be greater than zero"));
        }
        validate_relative_layout_path(&checkpoint_subdirectory, "checkpoint directory")?;
        validate_relative_layout_path(&final_audio_file, "final audio file")?;

        let checkpoints_dir = meeting_folder.join(&checkpoint_subdirectory);
        let final_audio_path = meeting_folder.join(&final_audio_file);

        if create_checkpoints_dir {
            std::fs::create_dir_all(&checkpoints_dir)?;
        }

        // Verify checkpoints directory exists
        if !checkpoints_dir.is_dir() {
            return Err(anyhow!(
                "Checkpoints directory does not exist: {}",
                checkpoints_dir.display()
            ));
        }

        Ok(Self {
            checkpoint_buffer: Vec::new(),
            checkpoint_interval_samples: sample_rate as usize * 30, // 30 seconds
            checkpoint_count: 0,
            checkpoints_dir,
            meeting_folder,
            final_audio_path,
            cleanup_checkpoints_on_finalize,
            sample_rate,
        })
    }

    /// Add an audio chunk to the buffer
    /// Automatically saves a checkpoint when buffer reaches 30 seconds
    pub fn add_chunk(&mut self, chunk: AudioChunk) -> Result<()> {
        let audio_data = AudioData {
            data: chunk.data,
            // sample_rate: chunk.sample_rate,
        };

        self.checkpoint_buffer.push(audio_data);

        // Calculate total samples in buffer
        let total_samples: usize = self.checkpoint_buffer.iter().map(|c| c.data.len()).sum();

        // Save checkpoint when buffer reaches threshold (30 seconds)
        if total_samples >= self.checkpoint_interval_samples {
            self.save_checkpoint()?;
            self.checkpoint_buffer.clear();
        }

        Ok(())
    }

    /// Save current buffer as a checkpoint file
    fn save_checkpoint(&mut self) -> Result<()> {
        // Concatenate all chunks in buffer
        let audio_data: Vec<f32> = self
            .checkpoint_buffer
            .iter()
            .flat_map(|c| &c.data)
            .cloned()
            .collect();

        if audio_data.is_empty() {
            warn!("Attempted to save empty checkpoint, skipping");
            return Ok(());
        }

        // Generate checkpoint filename
        let checkpoint_path = self
            .checkpoints_dir
            .join(format!("audio_chunk_{:03}.mp4", self.checkpoint_count));

        // Encode and save checkpoint
        encode_single_audio(
            bytemuck::cast_slice(&audio_data),
            self.sample_rate,
            1, // mono
            &checkpoint_path,
        )?;

        let duration_seconds = audio_data.len() as f32 / self.sample_rate as f32;
        self.checkpoint_count += 1;

        info!(
            "Saved checkpoint {}: {:.2}s of audio ({} samples)",
            self.checkpoint_count,
            duration_seconds,
            audio_data.len()
        );

        Ok(())
    }

    /// Finalize the recording: save final checkpoint, merge all checkpoints, cleanup
    ///
    /// Returns the path to the final merged audio.mp4 file
    pub async fn finalize(&mut self) -> Result<PathBuf> {
        info!("Finalizing incremental recording...");

        // Save final buffer if not empty
        if !self.checkpoint_buffer.is_empty() {
            info!(
                "Saving final checkpoint with remaining {} chunks",
                self.checkpoint_buffer.len()
            );
            self.save_checkpoint()?;
            self.checkpoint_buffer.clear();
        }

        if self.checkpoint_count == 0 {
            return Err(anyhow!(
                "No audio checkpoints to merge - recording may have failed"
            ));
        }

        // Merge all checkpoints using FFmpeg concat
        self.merge_checkpoints(&self.final_audio_path).await?;

        // The legacy single-track constructor owns `.checkpoints` and keeps
        // its historical cleanup behaviour. Configured multi-track savers are
        // cleaned together by RecordingSaver after every track has finalized.
        if self.cleanup_checkpoints_on_finalize {
            info!("Cleaning up {} checkpoint files", self.checkpoint_count);
            if let Err(e) = std::fs::remove_dir_all(&self.checkpoints_dir) {
                warn!("Failed to clean up checkpoints directory: {}", e);
                // Non-fatal - user can manually delete
            }
        }

        info!("Finalized recording: {}", self.final_audio_path.display());

        Ok(self.final_audio_path.clone())
    }

    /// Merge all checkpoint files into final audio.mp4 using FFmpeg concat
    /// Uses concat demuxer for fast merging without re-encoding
    async fn merge_checkpoints(&self, output: &PathBuf) -> Result<()> {
        info!(
            "Merging {} checkpoints into final audio file...",
            self.checkpoint_count
        );

        // Create concat list file for FFmpeg
        let list_file = self.checkpoints_dir.join("concat_list.txt");
        let mut list_content = String::new();

        for i in 0..self.checkpoint_count {
            let checkpoint_path = self
                .checkpoints_dir
                .join(format!("audio_chunk_{:03}.mp4", i));

            // Verify checkpoint exists
            if !checkpoint_path.exists() {
                return Err(anyhow!(
                    "Checkpoint file missing: {}",
                    checkpoint_path.display()
                ));
            }

            // Use absolute path for FFmpeg (required for safe mode)
            let abs_path = checkpoint_path.canonicalize()?;
            list_content.push_str(&format!("file '{}'\n", abs_path.display()));
        }

        std::fs::write(&list_file, list_content)?;

        let ffmpeg_path = find_ffmpeg_path().ok_or_else(|| {
            anyhow!("FFmpeg not found. Please install FFmpeg to finalize recordings.")
        })?;
        info!("Using FFmpeg at: {:?}", ffmpeg_path);

        // Run FFmpeg concat command
        // Using concat demuxer with copy codec for fast merging (no re-encoding)

        let mut command = std::process::Command::new(ffmpeg_path);

        command.args(&[
            "-f",
            "concat", // Use concat demuxer
            "-safe",
            "0", // Allow absolute paths
            "-i",
            list_file.to_str().unwrap(),
            "-c",
            "copy", // Copy codec - no re-encoding!
            "-y",   // Overwrite output file
            output.to_str().unwrap(),
        ]);

        // Hide console window on Windows to prevent CMD popup during finalization
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let ffmpeg_output = command.output()?;

        if !ffmpeg_output.status.success() {
            let stderr = String::from_utf8_lossy(&ffmpeg_output.stderr);
            error!("FFmpeg merge failed: {}", stderr);
            return Err(anyhow!("FFmpeg concat failed: {}", stderr));
        }

        // Existence alone is not enough to authorize checkpoint cleanup. A
        // failed encoder can leave an empty file behind.
        let output_metadata = std::fs::metadata(output).map_err(|error| {
            anyhow!(
                "Merged audio file was not created or cannot be inspected ({}): {}",
                output.display(),
                error
            )
        })?;
        if !output_metadata.is_file() || output_metadata.len() == 0 {
            return Err(anyhow!(
                "Merged audio file is empty or invalid: {}",
                output.display()
            ));
        }

        info!(
            "Successfully merged {} checkpoints → {}",
            self.checkpoint_count,
            output.display()
        );

        Ok(())
    }

    /// Get the meeting folder path
    pub fn get_meeting_folder(&self) -> &PathBuf {
        &self.meeting_folder
    }

    /// Get current checkpoint count
    pub fn get_checkpoint_count(&self) -> u32 {
        self.checkpoint_count
    }
}

fn validate_relative_layout_path(path: &Path, label: &str) -> Result<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(anyhow!("{label} must be a non-empty relative path"));
    }

    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(anyhow!("{label} must stay inside the meeting folder"));
    }

    Ok(())
}

/// Audio recovery status for transcript recovery feature
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioRecoveryStatus {
    pub status: String, // "success" | "partial" | "failed" | "none"
    pub chunk_count: u32,
    pub estimated_duration_seconds: f64,
    pub audio_file_path: Option<String>,
    pub message: String,
    /// Per-track recovery and verification results. Older frontends can ignore
    /// this additive field; new frontends require it before destructive cleanup.
    #[serde(default)]
    pub tracks: Vec<AudioTrackRecoveryStatus>,
    /// True only when every recognized checkpoint track has a verified output
    /// and no unclassified checkpoint data remains.
    #[serde(default)]
    pub cleanup_ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioTrackRecoveryStatus {
    pub track: String,
    pub status: String,
    pub chunk_count: u32,
    pub audio_file_path: Option<String>,
    pub verified: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointCleanupResult {
    pub cleaned_tracks: Vec<String>,
    pub checkpoint_root_removed: bool,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryTrack {
    Mixed,
    Microphone,
    SystemAudio,
}

impl RecoveryTrack {
    const V2: [Self; 3] = [Self::Mixed, Self::Microphone, Self::SystemAudio];

    const fn name(self) -> &'static str {
        match self {
            Self::Mixed => "mixed",
            Self::Microphone => "microphone",
            Self::SystemAudio => "system_audio",
        }
    }

    const fn checkpoint_directory(self) -> &'static str {
        match self {
            Self::Mixed => "mixed",
            Self::Microphone => "microphone",
            Self::SystemAudio => "system",
        }
    }

    const fn output_file(self) -> &'static str {
        match self {
            Self::Mixed => "audio.mp4",
            Self::Microphone => "microphone.mp4",
            Self::SystemAudio => "system-audio.mp4",
        }
    }
}

#[derive(Debug)]
struct InspectedCheckpointTrack {
    track: RecoveryTrack,
    directory: PathBuf,
    checkpoint_files: Vec<PathBuf>,
    issue: Option<String>,
}

#[derive(Debug)]
struct InspectedCheckpointLayout {
    root: PathBuf,
    tracks: Vec<InspectedCheckpointTrack>,
    legacy: bool,
    unresolved: Vec<String>,
}

/// Recover audio from checkpoint files
/// This is called by the transcript recovery system to merge audio chunks after a crash
#[tauri::command]
pub async fn recover_audio_from_checkpoints(
    meeting_folder: String,
    _sample_rate: u32,
) -> Result<AudioRecoveryStatus, String> {
    let recordings_root = super::recording_preferences::get_default_recordings_folder();
    recover_audio_from_checkpoints_from_root(&recordings_root, Path::new(&meeting_folder)).await
}

/// Clean up checkpoint files after a successful, verified recovery.
///
/// The frontend cannot authorize deletion merely by claiming recovery worked.
/// This command independently re-resolves the configured recordings root,
/// validates every path, and requires a non-empty output for every checkpoint
/// track before deleting any checkpoint directory.
#[tauri::command]
pub async fn cleanup_checkpoints(
    meeting_folder: String,
) -> Result<CheckpointCleanupResult, String> {
    let recordings_root = super::recording_preferences::get_default_recordings_folder();
    cleanup_checkpoints_from_root(&recordings_root, Path::new(&meeting_folder))
}

/// Check if a meeting folder has audio checkpoint files
/// Returns true if .checkpoints/ directory exists and contains .mp4 files
#[tauri::command]
pub async fn has_audio_checkpoints(meeting_folder: String) -> Result<bool, String> {
    let recordings_root = super::recording_preferences::get_default_recordings_folder();
    has_audio_checkpoints_from_root(&recordings_root, Path::new(&meeting_folder))
}

async fn recover_audio_from_checkpoints_from_root(
    recordings_root: &Path,
    meeting_folder: &Path,
) -> Result<AudioRecoveryStatus, String> {
    let canonical_folder = validate_recording_folder(recordings_root, meeting_folder)?;
    let Some(layout) = inspect_checkpoint_layout(&canonical_folder)? else {
        return Ok(AudioRecoveryStatus {
            status: "none".to_string(),
            chunk_count: 0,
            estimated_duration_seconds: 0.0,
            audio_file_path: None,
            message: "No audio checkpoints found".to_string(),
            tracks: Vec::new(),
            cleanup_ready: false,
        });
    };

    let mut tracks = Vec::with_capacity(layout.tracks.len());
    for track in &layout.tracks {
        tracks.push(recover_checkpoint_track(&canonical_folder, track).await);
    }

    let chunk_count = tracks.iter().map(|track| track.chunk_count).sum();
    let verified_count = tracks.iter().filter(|track| track.verified).count();
    let cleanup_ready =
        !tracks.is_empty() && verified_count == tracks.len() && layout.unresolved.is_empty();
    let status = if cleanup_ready {
        "success"
    } else if verified_count > 0 {
        "partial"
    } else if tracks.is_empty() && layout.unresolved.is_empty() {
        "none"
    } else {
        "failed"
    };
    let audio_file_path = tracks
        .iter()
        .find(|track| track.track == RecoveryTrack::Mixed.name() && track.verified)
        .and_then(|track| track.audio_file_path.clone());
    let message = if cleanup_ready {
        format!("Recovered and verified {} audio track(s)", tracks.len())
    } else if layout.unresolved.is_empty() {
        "Audio recovery is incomplete; checkpoints were retained".to_string()
    } else {
        format!(
            "Audio recovery is incomplete; checkpoints were retained: {}",
            layout.unresolved.join("; ")
        )
    };

    Ok(AudioRecoveryStatus {
        status: status.to_string(),
        chunk_count,
        estimated_duration_seconds: chunk_count as f64 * 30.0,
        audio_file_path,
        message,
        tracks,
        cleanup_ready,
    })
}

async fn recover_checkpoint_track(
    meeting_folder: &Path,
    inspected: &InspectedCheckpointTrack,
) -> AudioTrackRecoveryStatus {
    let chunk_count = inspected.checkpoint_files.len() as u32;
    if let Some(issue) = &inspected.issue {
        return failed_track_status(inspected.track, chunk_count, issue.clone());
    }
    if inspected.checkpoint_files.is_empty() {
        return failed_track_status(
            inspected.track,
            0,
            "No validated checkpoint chunks were found".to_string(),
        );
    }

    let output_path = meeting_folder.join(inspected.track.output_file());
    match verify_regular_nonempty_file(meeting_folder, &output_path, "recovered audio") {
        Ok(path) => {
            return successful_track_status(
                inspected.track,
                chunk_count,
                path,
                "Existing audio output was verified; checkpoints were retained pending cleanup",
            )
        }
        Err(error) if output_path.exists() => {
            return failed_track_status(
                inspected.track,
                chunk_count,
                format!("Existing output is not safe to replace: {error}"),
            )
        }
        Err(_) => {}
    }

    let ffmpeg_path = match find_ffmpeg_path() {
        Some(path) => path,
        None => {
            return failed_track_status(
                inspected.track,
                chunk_count,
                "FFmpeg was not found; checkpoints were retained".to_string(),
            )
        }
    };

    let concat_temp = match tempfile::Builder::new()
        .prefix(".meetily-recovery-list-")
        .suffix(".txt")
        .tempfile_in(&inspected.directory)
    {
        Ok(file) => file.into_temp_path(),
        Err(error) => {
            return failed_track_status(
                inspected.track,
                chunk_count,
                format!("Failed to create recovery list: {error}"),
            )
        }
    };
    let concat_content = inspected
        .checkpoint_files
        .iter()
        .map(|path| format!("file '{}'\n", escape_ffmpeg_concat_path(path)))
        .collect::<String>();
    if let Err(error) = std::fs::write(&concat_temp, concat_content) {
        return failed_track_status(
            inspected.track,
            chunk_count,
            format!("Failed to write recovery list: {error}"),
        );
    }

    let output_temp = match tempfile::Builder::new()
        .prefix(&format!(".meetily-recovery-{}-", inspected.track.name()))
        .suffix(".mp4")
        .tempfile_in(meeting_folder)
    {
        Ok(file) => file.into_temp_path(),
        Err(error) => {
            return failed_track_status(
                inspected.track,
                chunk_count,
                format!("Failed to create temporary recovery output: {error}"),
            )
        }
    };

    let mut command = std::process::Command::new(ffmpeg_path);
    command.args([
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
        &concat_temp.to_string_lossy(),
        "-c",
        "copy",
        "-y",
        &output_temp.to_string_lossy(),
    ]);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    match command.output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!(
                "FFmpeg recovery failed for {}: {}",
                inspected.track.name(),
                stderr
            );
            return failed_track_status(
                inspected.track,
                chunk_count,
                "FFmpeg failed; checkpoints were retained".to_string(),
            );
        }
        Err(error) => {
            return failed_track_status(
                inspected.track,
                chunk_count,
                format!("Failed to run FFmpeg: {error}"),
            )
        }
    }

    if let Err(error) =
        verify_regular_nonempty_file(meeting_folder, &output_temp, "temporary audio")
    {
        return failed_track_status(
            inspected.track,
            chunk_count,
            format!("FFmpeg output verification failed: {error}"),
        );
    }
    if output_path.exists() {
        return failed_track_status(
            inspected.track,
            chunk_count,
            "Audio output appeared during recovery; checkpoints were retained".to_string(),
        );
    }
    if let Err(error) = output_temp.persist_noclobber(&output_path) {
        return failed_track_status(
            inspected.track,
            chunk_count,
            format!("Failed to publish recovered audio: {}", error.error),
        );
    }

    match verify_regular_nonempty_file(meeting_folder, &output_path, "recovered audio") {
        Ok(path) => successful_track_status(
            inspected.track,
            chunk_count,
            path,
            "Recovered audio was published and verified; checkpoints await explicit cleanup",
        ),
        Err(error) => failed_track_status(
            inspected.track,
            chunk_count,
            format!("Published output verification failed: {error}"),
        ),
    }
}

fn failed_track_status(
    track: RecoveryTrack,
    chunk_count: u32,
    message: String,
) -> AudioTrackRecoveryStatus {
    AudioTrackRecoveryStatus {
        track: track.name().to_string(),
        status: "failed".to_string(),
        chunk_count,
        audio_file_path: None,
        verified: false,
        message,
    }
}

fn successful_track_status(
    track: RecoveryTrack,
    chunk_count: u32,
    path: PathBuf,
    message: &str,
) -> AudioTrackRecoveryStatus {
    AudioTrackRecoveryStatus {
        track: track.name().to_string(),
        status: "success".to_string(),
        chunk_count,
        audio_file_path: Some(path.to_string_lossy().to_string()),
        verified: true,
        message: message.to_string(),
    }
}

fn cleanup_checkpoints_from_root(
    recordings_root: &Path,
    meeting_folder: &Path,
) -> Result<CheckpointCleanupResult, String> {
    let canonical_folder = validate_recording_folder(recordings_root, meeting_folder)?;
    let Some(layout) = inspect_checkpoint_layout(&canonical_folder)? else {
        return Ok(CheckpointCleanupResult {
            cleaned_tracks: Vec::new(),
            checkpoint_root_removed: false,
            message: "No checkpoint directory exists".to_string(),
        });
    };

    if layout.tracks.is_empty() {
        return Err("Checkpoint state is empty or unknown; nothing was deleted".to_string());
    }
    if !layout.unresolved.is_empty() {
        return Err(format!(
            "Checkpoint state contains unresolved data; nothing was deleted: {}",
            layout.unresolved.join("; ")
        ));
    }
    for track in &layout.tracks {
        if let Some(issue) = &track.issue {
            return Err(format!(
                "{} checkpoint validation failed; nothing was deleted: {}",
                track.track.name(),
                issue
            ));
        }
        let output = canonical_folder.join(track.track.output_file());
        verify_regular_nonempty_file(&canonical_folder, &output, "recovered audio").map_err(
            |error| {
                format!(
                    "{} output is not verified; nothing was deleted: {}",
                    track.track.name(),
                    error
                )
            },
        )?;
    }

    let mut cleaned_tracks = Vec::with_capacity(layout.tracks.len());
    if layout.legacy {
        // The resolved root is exact, canonical, inside the recording folder,
        // and contains only the validated legacy mixed track at this point.
        std::fs::remove_dir_all(&layout.root)
            .map_err(|error| format!("Failed to remove legacy mixed checkpoints: {error}"))?;
        cleaned_tracks.push(RecoveryTrack::Mixed.name().to_string());
    } else {
        for track in &layout.tracks {
            std::fs::remove_dir_all(&track.directory).map_err(|error| {
                format!(
                    "Failed to remove {} checkpoints; remaining tracks were retained: {}",
                    track.track.name(),
                    error
                )
            })?;
            cleaned_tracks.push(track.track.name().to_string());
        }
    }

    let checkpoint_root_removed = if layout.root.exists() {
        match std::fs::remove_dir(&layout.root) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => false,
            Err(error) => return Err(format!("Failed to remove empty checkpoint root: {error}")),
        }
    } else {
        true
    };

    Ok(CheckpointCleanupResult {
        cleaned_tracks,
        checkpoint_root_removed,
        message: "Verified checkpoint tracks were cleaned successfully".to_string(),
    })
}

fn has_audio_checkpoints_from_root(
    recordings_root: &Path,
    meeting_folder: &Path,
) -> Result<bool, String> {
    let canonical_folder = validate_recording_folder(recordings_root, meeting_folder)?;
    Ok(inspect_checkpoint_layout(&canonical_folder)?
        .map(|layout| !layout.tracks.is_empty() || !layout.unresolved.is_empty())
        .unwrap_or(false))
}

fn validate_recording_folder(
    recordings_root: &Path,
    meeting_folder: &Path,
) -> Result<PathBuf, String> {
    let canonical_root = canonical_directory(recordings_root, "recordings root")?;
    let canonical_folder = canonical_directory(meeting_folder, "recording folder")?;
    if canonical_folder == canonical_root || !canonical_folder.starts_with(&canonical_root) {
        return Err("Recording folder is outside the configured recordings root".to_string());
    }
    Ok(canonical_folder)
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("Failed to resolve {label}: {error}"))?;
    if !canonical.is_dir() {
        return Err(format!("{label} is not a directory"));
    }
    Ok(canonical)
}

fn inspect_checkpoint_layout(
    meeting_folder: &Path,
) -> Result<Option<InspectedCheckpointLayout>, String> {
    let candidate_root = meeting_folder.join(".checkpoints");
    match std::fs::symlink_metadata(&candidate_root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Failed to inspect checkpoint root: {error}")),
        Ok(metadata) if !metadata.is_dir() => {
            return Err("Checkpoint root is not a directory".to_string())
        }
        Ok(_) => {}
    }
    let checkpoint_root = std::fs::canonicalize(&candidate_root)
        .map_err(|error| format!("Failed to resolve checkpoint root: {error}"))?;
    if checkpoint_root != candidate_root || !checkpoint_root.starts_with(meeting_folder) {
        return Err("Checkpoint root resolves outside its recording folder".to_string());
    }

    let mut tracks = Vec::new();
    let mut unresolved = Vec::new();
    for track in RecoveryTrack::V2 {
        let candidate = checkpoint_root.join(track.checkpoint_directory());
        match std::fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "Failed to inspect {} checkpoint directory: {}",
                    track.name(),
                    error
                ))
            }
            Ok(metadata) if !metadata.is_dir() => {
                unresolved.push(format!(
                    "{} checkpoint path is not a directory",
                    track.name()
                ));
                continue;
            }
            Ok(_) => {}
        }
        let directory = std::fs::canonicalize(&candidate).map_err(|error| {
            format!(
                "Failed to resolve {} checkpoint directory: {}",
                track.name(),
                error
            )
        })?;
        if directory != candidate || !directory.starts_with(&checkpoint_root) {
            return Err(format!(
                "{} checkpoint directory resolves outside the checkpoint root",
                track.name()
            ));
        }
        let (files, issue) = inspect_checkpoint_files(&directory, false)?;
        if !files.is_empty() || issue.is_some() {
            tracks.push(InspectedCheckpointTrack {
                track,
                directory,
                checkpoint_files: files,
                issue,
            });
        }
    }

    let (legacy_files, legacy_issue) = inspect_checkpoint_files(&checkpoint_root, true)?;
    if tracks.is_empty() && (!legacy_files.is_empty() || legacy_issue.is_some()) {
        tracks.push(InspectedCheckpointTrack {
            track: RecoveryTrack::Mixed,
            directory: checkpoint_root.clone(),
            checkpoint_files: legacy_files,
            issue: legacy_issue,
        });
        return Ok(Some(InspectedCheckpointLayout {
            root: checkpoint_root,
            tracks,
            legacy: true,
            unresolved,
        }));
    }
    if !legacy_files.is_empty() {
        unresolved.push(
            "legacy mixed chunks coexist with version 2 tracks and were retained".to_string(),
        );
    }
    if let Some(issue) = legacy_issue {
        unresolved.push(issue);
    }

    Ok(Some(InspectedCheckpointLayout {
        root: checkpoint_root,
        tracks,
        legacy: false,
        unresolved,
    }))
}

fn inspect_checkpoint_files(
    directory: &Path,
    allow_track_directories: bool,
) -> Result<(Vec<PathBuf>, Option<String>), String> {
    let mut indexed_files = Vec::new();
    let mut issues = Vec::new();
    for entry_result in std::fs::read_dir(directory)
        .map_err(|error| format!("Failed to read checkpoint directory: {error}"))?
    {
        let entry =
            entry_result.map_err(|error| format!("Failed to inspect checkpoint entry: {error}"))?;
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().to_string();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("Failed to inspect checkpoint entry type: {error}"))?;

        if allow_track_directories
            && file_type.is_dir()
            && RecoveryTrack::V2
                .iter()
                .any(|track| file_name == track.checkpoint_directory())
        {
            continue;
        }
        if file_type.is_file() && file_name == "concat_list.txt" {
            continue;
        }
        if !file_type.is_file() {
            issues.push(format!("unrecognized checkpoint entry: {file_name}"));
            continue;
        }

        let Some(index) = checkpoint_chunk_index(&file_name) else {
            issues.push(format!("unrecognized checkpoint file: {file_name}"));
            continue;
        };
        let canonical = std::fs::canonicalize(&path)
            .map_err(|error| format!("Failed to resolve checkpoint file: {error}"))?;
        if canonical != path || !canonical.starts_with(directory) {
            return Err(format!(
                "Checkpoint file resolves outside its track directory: {file_name}"
            ));
        }
        let metadata = std::fs::metadata(&canonical)
            .map_err(|error| format!("Failed to inspect checkpoint file: {error}"))?;
        if !metadata.is_file() || metadata.len() == 0 {
            issues.push(format!("checkpoint file is empty or invalid: {file_name}"));
            continue;
        }
        indexed_files.push((index, canonical));
    }

    indexed_files.sort_by_key(|(index, _)| *index);
    for (expected, (actual, _)) in indexed_files.iter().enumerate() {
        if *actual != expected as u32 {
            issues.push(format!(
                "checkpoint sequence is incomplete: expected {expected:03}, found {actual:03}"
            ));
            break;
        }
    }
    Ok((
        indexed_files.into_iter().map(|(_, path)| path).collect(),
        (!issues.is_empty()).then(|| issues.join("; ")),
    ))
}

fn checkpoint_chunk_index(file_name: &str) -> Option<u32> {
    let digits = file_name
        .strip_prefix("audio_chunk_")?
        .strip_suffix(".mp4")?;
    if digits.len() < 3 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn verify_regular_nonempty_file(
    meeting_folder: &Path,
    candidate: &Path,
    label: &str,
) -> Result<PathBuf, String> {
    let canonical = std::fs::canonicalize(candidate)
        .map_err(|error| format!("Failed to resolve {label}: {error}"))?;
    if canonical != candidate || !canonical.starts_with(meeting_folder) {
        return Err(format!("{label} resolves outside the recording folder"));
    }
    let metadata = std::fs::metadata(&canonical)
        .map_err(|error| format!("Failed to inspect {label}: {error}"))?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(format!("{label} is empty or is not a regular file"));
    }
    Ok(canonical)
}

fn escape_ffmpeg_concat_path(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "'\\''")
}

#[cfg(test)]
mod tests {
    use super::super::recording_state::DeviceType;
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn configured_layout_isolated_and_does_not_require_ffmpeg() {
        let temp_dir = tempdir().unwrap();
        let meeting_folder = temp_dir.path().join("Test_Meeting");
        std::fs::create_dir_all(&meeting_folder).unwrap();

        let mut saver = IncrementalAudioSaver::new_with_layout(
            meeting_folder.clone(),
            48_000,
            ".checkpoints/microphone",
            "microphone.mp4",
        )
        .unwrap();

        saver
            .add_chunk(AudioChunk {
                data: vec![0.5; 2_400],
                sample_rate: 48_000,
                timestamp: 0.0,
                chunk_id: 0,
                device_type: DeviceType::Microphone,
                start_frame: Some(0),
                end_frame: Some(2_400),
            })
            .unwrap();

        assert_eq!(saver.checkpoint_count, 0);
        assert_eq!(
            saver.checkpoints_dir,
            meeting_folder.join(".checkpoints/microphone")
        );
        assert_eq!(
            saver.final_audio_path,
            meeting_folder.join("microphone.mp4")
        );
        assert!(!saver.cleanup_checkpoints_on_finalize);
        assert!(saver.checkpoints_dir.is_dir());
    }

    #[tokio::test]
    async fn test_empty_recording() {
        let temp_dir = tempdir().unwrap();
        let meeting_folder = temp_dir.path().join("Empty_Test");
        std::fs::create_dir_all(&meeting_folder).unwrap();
        std::fs::create_dir_all(meeting_folder.join(".checkpoints")).unwrap();

        let mut saver = IncrementalAudioSaver::new(meeting_folder.clone(), 48000).unwrap();

        // Try to finalize without adding any chunks
        let result = saver.finalize().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No audio checkpoints"));
    }

    fn recovery_sandbox() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let sandbox = tempdir().unwrap();
        let recordings_root = sandbox.path().join("recordings");
        let meeting_folder = recordings_root.join("meeting-a");
        std::fs::create_dir_all(&meeting_folder).unwrap();
        (sandbox, recordings_root, meeting_folder)
    }

    fn write_checkpoint(meeting_folder: &Path, track_directory: &str, index: u32) {
        let directory = meeting_folder.join(".checkpoints").join(track_directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(format!("audio_chunk_{index:03}.mp4")),
            b"synthetic-checkpoint",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn recovery_verifies_every_existing_v2_output_without_auto_cleanup() {
        let (_sandbox, recordings_root, meeting_folder) = recovery_sandbox();
        write_checkpoint(&meeting_folder, "mixed", 0);
        write_checkpoint(&meeting_folder, "microphone", 0);
        std::fs::write(meeting_folder.join("audio.mp4"), b"verified-mixed").unwrap();
        std::fs::write(
            meeting_folder.join("microphone.mp4"),
            b"verified-microphone",
        )
        .unwrap();

        let status = recover_audio_from_checkpoints_from_root(&recordings_root, &meeting_folder)
            .await
            .unwrap();

        assert_eq!(status.status, "success");
        assert!(status.cleanup_ready);
        assert_eq!(status.tracks.len(), 2);
        assert!(status.tracks.iter().all(|track| track.verified));
        assert!(meeting_folder.join(".checkpoints/mixed").exists());
        assert!(meeting_folder.join(".checkpoints/microphone").exists());
    }

    #[test]
    fn cleanup_retains_every_track_when_one_output_is_unverified() {
        let (_sandbox, recordings_root, meeting_folder) = recovery_sandbox();
        write_checkpoint(&meeting_folder, "mixed", 0);
        write_checkpoint(&meeting_folder, "microphone", 0);
        std::fs::write(meeting_folder.join("audio.mp4"), b"verified-mixed").unwrap();

        let error = cleanup_checkpoints_from_root(&recordings_root, &meeting_folder).unwrap_err();

        assert!(error.contains("microphone output is not verified"));
        assert!(meeting_folder.join(".checkpoints/mixed").exists());
        assert!(meeting_folder.join(".checkpoints/microphone").exists());
    }

    #[test]
    fn cleanup_removes_only_known_tracks_after_all_outputs_are_verified() {
        let (_sandbox, recordings_root, meeting_folder) = recovery_sandbox();
        write_checkpoint(&meeting_folder, "mixed", 0);
        write_checkpoint(&meeting_folder, "microphone", 0);
        std::fs::write(meeting_folder.join("audio.mp4"), b"verified-mixed").unwrap();
        std::fs::write(
            meeting_folder.join("microphone.mp4"),
            b"verified-microphone",
        )
        .unwrap();

        let result = cleanup_checkpoints_from_root(&recordings_root, &meeting_folder).unwrap();

        assert_eq!(result.cleaned_tracks, vec!["mixed", "microphone"]);
        assert!(result.checkpoint_root_removed);
        assert!(!meeting_folder.join(".checkpoints").exists());
    }

    #[test]
    fn cleanup_rejects_unknown_checkpoint_data_without_deleting_tracks() {
        let (_sandbox, recordings_root, meeting_folder) = recovery_sandbox();
        write_checkpoint(&meeting_folder, "mixed", 0);
        std::fs::write(meeting_folder.join("audio.mp4"), b"verified-mixed").unwrap();
        std::fs::write(
            meeting_folder.join(".checkpoints/do-not-delete.bin"),
            b"unknown-data",
        )
        .unwrap();

        let error = cleanup_checkpoints_from_root(&recordings_root, &meeting_folder).unwrap_err();

        assert!(error.contains("unresolved data"));
        assert!(meeting_folder.join(".checkpoints/mixed").exists());
        assert!(meeting_folder
            .join(".checkpoints/do-not-delete.bin")
            .exists());
    }

    #[test]
    fn recovery_commands_reject_folders_outside_recordings_root() {
        let (sandbox, recordings_root, _meeting_folder) = recovery_sandbox();
        let outside = sandbox.path().join("outside");
        std::fs::create_dir_all(outside.join(".checkpoints/mixed")).unwrap();
        std::fs::write(
            outside.join(".checkpoints/mixed/audio_chunk_000.mp4"),
            b"outside",
        )
        .unwrap();

        let has_error = has_audio_checkpoints_from_root(&recordings_root, &outside).unwrap_err();
        let cleanup_error = cleanup_checkpoints_from_root(&recordings_root, &outside).unwrap_err();

        assert!(has_error.contains("outside the configured recordings root"));
        assert!(cleanup_error.contains("outside the configured recordings root"));
        assert!(outside.join(".checkpoints/mixed").exists());
    }

    #[test]
    fn incomplete_checkpoint_sequence_is_never_cleanup_ready() {
        let (_sandbox, recordings_root, meeting_folder) = recovery_sandbox();
        write_checkpoint(&meeting_folder, "mixed", 1);
        std::fs::write(meeting_folder.join("audio.mp4"), b"existing-output").unwrap();

        let layout = inspect_checkpoint_layout(
            &validate_recording_folder(&recordings_root, &meeting_folder).unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(layout.tracks[0]
            .issue
            .as_deref()
            .unwrap()
            .contains("sequence is incomplete"));
        assert!(cleanup_checkpoints_from_root(&recordings_root, &meeting_folder).is_err());
        assert!(meeting_folder.join(".checkpoints/mixed").exists());
    }

    #[test]
    fn configured_layout_rejects_parent_traversal() {
        let temp_dir = tempdir().unwrap();
        let meeting_folder = temp_dir.path().join("Meeting");
        std::fs::create_dir_all(&meeting_folder).unwrap();

        let result = IncrementalAudioSaver::new_with_layout(
            meeting_folder,
            48_000,
            "../outside",
            "audio.mp4",
        );
        assert!(result.is_err());
    }
}
