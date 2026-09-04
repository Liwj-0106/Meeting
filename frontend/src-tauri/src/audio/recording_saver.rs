use anyhow::Result;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Runtime};
use tokio::sync::mpsc;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

use super::audio_processing::create_meeting_folder;
use super::incremental_saver::IncrementalAudioSaver;
use super::recording_state::{AudioChunk, DeviceType};
use super::synchronizer::CaptureTopology;
use super::transcription::{
    AsrMetadata, AudioSource, DiarizationMetadata, DiarizationStatus, SpeakerMetadata,
    SpeakerStatus, TranscriptChunkInput, TranscriptEventKind, TranscriptUpdate,
};

/// Structured transcript segment for JSON export
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub id: String,
    pub text: String,
    pub audio_start_time: f64, // Seconds from recording start
    pub audio_end_time: f64,   // Seconds from recording start
    pub duration: f64,         // Segment duration in seconds
    pub display_time: String,  // Formatted time for display like "[02:15]"
    pub confidence: f32,
    pub sequence_id: u64,

    // Legacy wire fields retained for reload compatibility.
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub chunk_start_time: f64,
    #[serde(default)]
    pub is_partial: bool,

    // Canonical revision event fields. These mirror TranscriptUpdate so a page
    // reload does not discard identity, correction, speaker, or provider data.
    #[serde(default)]
    pub schema_version: u16,
    #[serde(default)]
    pub event_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meeting_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub utterance_id: String,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub event_kind: TranscriptEventKind,
    #[serde(default)]
    pub is_stable: bool,
    #[serde(default)]
    pub start_ms: u64,
    #[serde(default)]
    pub end_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default)]
    pub audio_source: AudioSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<SpeakerMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_local_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_confidence: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_status: Option<SpeakerStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr: Option<AsrMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_confidence: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization: Option<DiarizationMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_model_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_window_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_window_start_frame: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_window_end_frame: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_status: Option<DiarizationStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_event_id: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

impl From<TranscriptUpdate> for TranscriptSegment {
    fn from(update: TranscriptUpdate) -> Self {
        let display_time = update.timestamp.clone();
        let source = update.source.clone();
        let chunk_start_time = update.chunk_start_time;
        let is_partial = update.is_partial;
        let legacy_confidence = update.confidence;
        let legacy_duration = update.duration;
        let sequence_id = update.sequence_id;
        let flattened_speaker_id = update.speaker_id.clone();
        let flattened_speaker_local_label = update.speaker_local_label.clone();
        let flattened_speaker_display_name = update.speaker_display_name.clone();
        let flattened_speaker_confidence = update.speaker_confidence;
        let flattened_speaker_status = update.speaker_status.clone();
        let flattened_asr_provider = update.asr_provider.clone();
        let flattened_asr_model = update.asr_model.clone();
        let flattened_asr_confidence = update.asr_confidence;
        let flattened_asr_latency_ms = update.asr_latency_ms;
        let normalized = update.normalize();

        let speaker_id = flattened_speaker_id.or_else(|| {
            normalized
                .speaker
                .as_ref()
                .map(|speaker| speaker.speaker_id.clone())
        });
        let speaker_local_label = flattened_speaker_local_label.or_else(|| {
            normalized
                .speaker
                .as_ref()
                .and_then(|speaker| speaker.local_label.clone())
        });
        let speaker_display_name = flattened_speaker_display_name.or_else(|| {
            normalized
                .speaker
                .as_ref()
                .and_then(|speaker| speaker.display_name.clone())
        });
        let speaker_confidence = flattened_speaker_confidence.or_else(|| {
            normalized
                .speaker
                .as_ref()
                .and_then(|speaker| speaker.confidence)
        });
        let speaker_status = flattened_speaker_status.or_else(|| {
            normalized
                .speaker
                .as_ref()
                .map(|speaker| speaker.status.clone())
        });
        let asr_provider = flattened_asr_provider
            .or_else(|| normalized.asr.as_ref().map(|asr| asr.provider.clone()));
        let asr_model = flattened_asr_model
            .or_else(|| normalized.asr.as_ref().and_then(|asr| asr.model.clone()));
        let asr_confidence = flattened_asr_confidence
            .or_else(|| normalized.asr.as_ref().and_then(|asr| asr.confidence));
        let asr_latency_ms = flattened_asr_latency_ms
            .or_else(|| normalized.asr.as_ref().and_then(|asr| asr.latency_ms));
        // The nested value is canonical. Derive every flattened mirror from
        // the normalized value so conflicting wire representations cannot be
        // persisted as two different diarization answers.
        let diarization_provider = normalized
            .diarization
            .as_ref()
            .map(|metadata| metadata.provider.clone());
        let diarization_model = normalized
            .diarization
            .as_ref()
            .and_then(|metadata| metadata.model.clone());
        let diarization_model_revision = normalized
            .diarization
            .as_ref()
            .and_then(|metadata| metadata.model_revision.clone());
        let diarization_revision = normalized
            .diarization
            .as_ref()
            .map(|metadata| metadata.revision);
        let diarization_window_id = normalized
            .diarization
            .as_ref()
            .and_then(|metadata| metadata.window_id.clone());
        let diarization_window_start_frame = normalized
            .diarization
            .as_ref()
            .and_then(|metadata| metadata.window_start_frame);
        let diarization_window_end_frame = normalized
            .diarization
            .as_ref()
            .and_then(|metadata| metadata.window_end_frame);
        let diarization_status = normalized
            .diarization
            .as_ref()
            .map(|metadata| metadata.status.clone());
        let diarization_latency_ms = normalized
            .diarization
            .as_ref()
            .and_then(|metadata| metadata.latency_ms);
        let audio_start_time = normalized.start_ms as f64 / 1_000.0;
        let audio_end_time = normalized.end_ms as f64 / 1_000.0;
        let duration = if legacy_duration.is_finite() && legacy_duration > 0.0 {
            legacy_duration
        } else {
            (audio_end_time - audio_start_time).max(0.0)
        };

        Self {
            id: format!("seg_{}", sequence_id),
            text: normalized.text.clone(),
            audio_start_time,
            audio_end_time,
            duration,
            display_time,
            confidence: asr_confidence.unwrap_or(legacy_confidence),
            sequence_id,
            source,
            chunk_start_time,
            is_partial,
            schema_version: normalized.schema_version,
            event_id: normalized.event_id,
            meeting_id: normalized.meeting_id,
            session_id: normalized.session_id,
            utterance_id: normalized.utterance_id,
            revision: normalized.revision,
            event_kind: normalized.event_kind,
            is_stable: normalized.is_stable,
            start_ms: normalized.start_ms,
            end_ms: normalized.end_ms,
            language: normalized.language,
            audio_source: normalized.audio_source,
            speaker: normalized.speaker,
            speaker_id,
            speaker_local_label,
            speaker_display_name,
            speaker_confidence,
            speaker_status,
            asr: normalized.asr,
            asr_provider,
            asr_model,
            asr_confidence,
            asr_latency_ms,
            diarization: normalized.diarization,
            diarization_provider,
            diarization_model,
            diarization_model_revision,
            diarization_revision,
            diarization_window_id,
            diarization_window_start_frame,
            diarization_window_end_frame,
            diarization_status,
            diarization_latency_ms,
            replaces_event_id: normalized.replaces_event_id,
            provider_event_id: normalized.provider_event_id,
            created_at: normalized.created_at,
            trace_id: normalized.trace_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptApplyResult {
    Added,
    Updated,
    Duplicate,
    Stale,
    ConflictIgnored,
}

#[derive(Debug, Default)]
struct TranscriptState {
    materialized: Vec<TranscriptSegment>,
    event_history: Vec<TranscriptSegment>,
}

/// Cloneable transcript-only persistence handle used by event listeners.
///
/// Keeping this handle independent from `RecordingManager` prevents transcript
/// events from being dropped while the manager is temporarily moved out of the
/// global slot during asynchronous shutdown.
#[derive(Clone)]
pub struct TranscriptPersistenceSink {
    transcript_state: Arc<Mutex<TranscriptState>>,
    meeting_folder: Option<PathBuf>,
    persistence_lock: Arc<Mutex<()>>,
}

const MAX_TRANSCRIPT_RECOVERY_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TRANSCRIPT_RECOVERY_LINE_BYTES: usize = 8 * 1024 * 1024;

/// File selected as the source for a recovered transcript event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptRecoverySource {
    TranscriptEventsNdjson,
    TranscriptsJson,
}

/// A non-fatal issue encountered while reading or replaying recovery data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptRecoveryWarning {
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    pub message: String,
}

/// Replay-safe transcript history recovered from one recording folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptRecoveryResult {
    pub events: Vec<TranscriptSegment>,
    pub source: TranscriptRecoverySource,
    pub warnings: Vec<TranscriptRecoveryWarning>,
}

impl TranscriptPersistenceSink {
    pub fn add_transcript_segment(&self, segment: TranscriptSegment) {
        let _persistence_guard = match self.persistence_lock.lock() {
            Ok(guard) => guard,
            Err(error) => {
                error!("Failed to lock transcript persistence: {}", error);
                return;
            }
        };

        let event_id = segment.event_id.clone();
        let utterance_id = segment.materialization_key();
        let revision = segment.revision;
        let apply_result = if let Ok(mut state) = self.transcript_state.lock() {
            state.apply(segment.clone())
        } else {
            error!("Failed to lock transcript state for event {}", event_id);
            return;
        };

        info!(
            "Transcript event {} applied to {} revision {}: {:?}",
            event_id, utterance_id, revision, apply_result
        );

        // Rust-only observer. This runs after the canonical sink accepted the
        // delivery and never trusts the public `transcript-update` event.
        // The summary registry performs its own bounded async work, so the
        // audio/persistence hot path only enqueues a clone.
        if apply_result != TranscriptApplyResult::Duplicate {
            crate::summary::live::observe_trusted_transcript(
                segment.clone(),
                self.meeting_folder.clone(),
            );
        }

        if let Some(folder) = &self.meeting_folder {
            if apply_result != TranscriptApplyResult::Duplicate {
                // The audit stream is append-only on the hot path. A partially
                // written final line is tolerated by the recovery reader, and
                // stop-time compaction atomically rewrites a clean file. This
                // avoids O(n²) full-history rewrites for streaming partials.
                if let Err(error) = append_transcript_event_ndjson(folder, &segment) {
                    warn!("Failed to append transcript revision: {}", error);
                    if let Err(fallback_error) =
                        write_transcript_events_ndjson_from_state(&self.transcript_state, folder)
                    {
                        warn!(
                            "Failed to rewrite transcript history after append error: {}",
                            fallback_error
                        );
                    }
                }
            }

            // Materialized JSON is a compatibility snapshot, not the audit
            // source. Rewriting it only for stable visible-state changes keeps
            // high-frequency partial hypotheses off the synchronous I/O path.
            if matches!(
                apply_result,
                TranscriptApplyResult::Added | TranscriptApplyResult::Updated
            ) && segment.is_stable
            {
                if let Err(error) =
                    write_transcripts_json_from_state(&self.transcript_state, folder)
                {
                    warn!("Failed to write incremental transcript snapshot: {}", error);
                }
            }
        }
    }
}

impl TranscriptState {
    fn apply(&mut self, incoming: TranscriptSegment) -> TranscriptApplyResult {
        if self.is_duplicate(&incoming) {
            return TranscriptApplyResult::Duplicate;
        }

        // The append-only audit stream includes stale/conflicting revisions;
        // replay determines which one is currently materialized.
        self.event_history.push(incoming.clone());
        let incoming_key = incoming.materialization_key();
        let Some(index) = self
            .materialized
            .iter()
            .position(|current| current.materialization_key() == incoming_key)
        else {
            self.materialized.push(incoming);
            self.sort_materialized();
            return TranscriptApplyResult::Added;
        };

        let current = &self.materialized[index];
        if incoming.revision < current.revision {
            return TranscriptApplyResult::Stale;
        }
        if incoming.revision == current.revision && !same_revision_wins(&incoming, current) {
            return TranscriptApplyResult::ConflictIgnored;
        }

        self.materialized[index] = incoming;
        self.sort_materialized();
        TranscriptApplyResult::Updated
    }

    fn is_duplicate(&self, incoming: &TranscriptSegment) -> bool {
        if incoming.event_id.is_empty() || incoming.event_id.starts_with("legacy:") {
            self.event_history.iter().any(|event| event == incoming)
        } else {
            self.event_history
                .iter()
                .any(|event| event.event_id == incoming.event_id)
        }
    }

    fn sort_materialized(&mut self) {
        self.materialized.sort_by(|left, right| {
            (
                left.start_ms,
                left.end_ms,
                left.sequence_id,
                left.utterance_id.as_str(),
            )
                .cmp(&(
                    right.start_ms,
                    right.end_ms,
                    right.sequence_id,
                    right.utterance_id.as_str(),
                ))
        });
    }

    fn visible_materialized(&self) -> Vec<TranscriptSegment> {
        self.materialized
            .iter()
            .filter(|segment| segment.event_kind != TranscriptEventKind::Retraction)
            .cloned()
            .collect()
    }
}

impl TranscriptSegment {
    fn materialization_key(&self) -> String {
        if self.utterance_id.is_empty() {
            format!("legacy:{}", self.sequence_id)
        } else {
            self.utterance_id.clone()
        }
    }
}

fn same_revision_wins(incoming: &TranscriptSegment, current: &TranscriptSegment) -> bool {
    (
        incoming.is_stable as u8,
        event_kind_rank(&incoming.event_kind),
        incoming.created_at.as_str(),
        incoming.event_id.as_str(),
    ) > (
        current.is_stable as u8,
        event_kind_rank(&current.event_kind),
        current.created_at.as_str(),
        current.event_id.as_str(),
    )
}

fn event_kind_rank(kind: &TranscriptEventKind) -> u8 {
    match kind {
        TranscriptEventKind::Unknown(_) => 0,
        TranscriptEventKind::Partial => 1,
        TranscriptEventKind::Final => 2,
        TranscriptEventKind::Correction => 3,
        TranscriptEventKind::SpeakerUpdate | TranscriptEventKind::LanguageUpdate => 4,
        TranscriptEventKind::Retraction => 5,
    }
}

/// Meeting metadata structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingMetadata {
    pub version: String,
    pub meeting_id: Option<String>,
    pub meeting_name: Option<String>,
    pub created_at: String,
    pub completed_at: Option<String>,
    pub duration_seconds: Option<f64>,
    pub devices: DeviceInfo,
    pub audio_file: String,
    pub transcript_file: String,
    pub sample_rate: u32,
    pub status: String, // "recording", "completed", "error"
    /// Version 2 per-track persistence status. Missing in version 1 metadata.
    #[serde(default)]
    pub audio_tracks: Vec<AudioTrackMetadata>,
    /// Shared frame-clock information for aligning audio, transcripts, and
    /// later diarization results. Missing in version 1 metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline: Option<AudioTimelineMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub microphone: Option<String>,
    pub system_audio: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AudioTrackMetadata {
    #[serde(default)]
    pub track: String,
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AudioTimelineMetadata {
    #[serde(default)]
    pub duration_frames: u64,
    #[serde(default)]
    pub sample_rate: u32,
    #[serde(default)]
    pub timebase: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordingTrack {
    Mixed,
    Microphone,
    SystemAudio,
}

impl RecordingTrack {
    const ALL: [Self; 3] = [Self::Mixed, Self::Microphone, Self::SystemAudio];

    const fn name(self) -> &'static str {
        match self {
            Self::Mixed => "mixed",
            Self::Microphone => "microphone",
            Self::SystemAudio => "system_audio",
        }
    }

    const fn file_name(self) -> &'static str {
        match self {
            Self::Mixed => "audio.mp4",
            Self::Microphone => "microphone.mp4",
            Self::SystemAudio => "system-audio.mp4",
        }
    }

    const fn checkpoint_subdirectory(self) -> &'static str {
        match self {
            Self::Mixed => ".checkpoints/mixed",
            Self::Microphone => ".checkpoints/microphone",
            Self::SystemAudio => ".checkpoints/system",
        }
    }

    const fn enabled(self, topology: CaptureTopology) -> bool {
        match self {
            Self::Mixed => topology.microphone || topology.system_audio,
            Self::Microphone => topology.microphone,
            Self::SystemAudio => topology.system_audio,
        }
    }
}

#[derive(Debug, Default)]
struct TrackAccumulationReport {
    chunks_received: u64,
    error: Option<String>,
}

#[derive(Debug, Default)]
struct AccumulationReport {
    chunks_received: u64,
    max_end_frame: u64,
    sample_rate: u32,
    mixed: TrackAccumulationReport,
    microphone: TrackAccumulationReport,
    system_audio: TrackAccumulationReport,
}

impl AccumulationReport {
    fn track_mut(&mut self, track: RecordingTrack) -> &mut TrackAccumulationReport {
        match track {
            RecordingTrack::Mixed => &mut self.mixed,
            RecordingTrack::Microphone => &mut self.microphone,
            RecordingTrack::SystemAudio => &mut self.system_audio,
        }
    }

    fn track(&self, track: RecordingTrack) -> &TrackAccumulationReport {
        match track {
            RecordingTrack::Mixed => &self.mixed,
            RecordingTrack::Microphone => &self.microphone,
            RecordingTrack::SystemAudio => &self.system_audio,
        }
    }
}

#[derive(Clone, Default)]
struct TrackSavers {
    mixed: Option<Arc<AsyncMutex<IncrementalAudioSaver>>>,
    microphone: Option<Arc<AsyncMutex<IncrementalAudioSaver>>>,
    system_audio: Option<Arc<AsyncMutex<IncrementalAudioSaver>>>,
}

impl TrackSavers {
    fn for_track(&self, track: RecordingTrack) -> Option<&Arc<AsyncMutex<IncrementalAudioSaver>>> {
        match track {
            RecordingTrack::Mixed => self.mixed.as_ref(),
            RecordingTrack::Microphone => self.microphone.as_ref(),
            RecordingTrack::SystemAudio => self.system_audio.as_ref(),
        }
    }
}

fn recording_track_for_device(device_type: &DeviceType) -> RecordingTrack {
    match device_type {
        DeviceType::Microphone => RecordingTrack::Microphone,
        DeviceType::System => RecordingTrack::SystemAudio,
        DeviceType::Mixed => RecordingTrack::Mixed,
    }
}

async fn drain_audio_chunks(
    mut receiver: mpsc::UnboundedReceiver<AudioChunk>,
    save_audio: bool,
    savers: TrackSavers,
) -> AccumulationReport {
    let mut report = AccumulationReport::default();

    while let Some(chunk) = receiver.recv().await {
        report.chunks_received = report.chunks_received.saturating_add(1);
        if report.sample_rate == 0 && chunk.sample_rate > 0 {
            report.sample_rate = chunk.sample_rate;
        }
        report.max_end_frame = report.max_end_frame.max(canonical_chunk_end_frame(&chunk));

        let track = recording_track_for_device(&chunk.device_type);
        let track_report = report.track_mut(track);
        track_report.chunks_received = track_report.chunks_received.saturating_add(1);

        if !save_audio {
            continue;
        }

        let Some(saver) = savers.for_track(track) else {
            append_track_error(
                track_report,
                format!("received {} chunk without a configured saver", track.name()),
            );
            continue;
        };

        let mut saver = saver.lock().await;
        if let Err(error) = saver.add_chunk(chunk) {
            error!("Failed to add {} chunk: {}", track.name(), error);
            append_track_error(track_report, error.to_string());
        }
    }

    info!(
        "Recording saver drained {} queued chunks to EOF",
        report.chunks_received
    );
    report
}

fn canonical_chunk_end_frame(chunk: &AudioChunk) -> u64 {
    if let Some(end_frame) = chunk.end_frame {
        return end_frame;
    }
    if let Some(start_frame) = chunk.start_frame {
        return start_frame.saturating_add(chunk.data.len() as u64);
    }

    let timestamp_frame = if chunk.timestamp.is_finite() && chunk.timestamp > 0.0 {
        (chunk.timestamp * chunk.sample_rate as f64).round() as u64
    } else {
        0
    };
    timestamp_frame.saturating_add(chunk.data.len() as u64)
}

fn append_track_error(report: &mut TrackAccumulationReport, message: String) {
    match &mut report.error {
        Some(existing) if !existing.contains(&message) => {
            existing.push_str("; ");
            existing.push_str(&message);
        }
        None => report.error = Some(message),
        _ => {}
    }
}

#[derive(Debug)]
struct TrackFinalizeOutcome {
    track: RecordingTrack,
    path: Option<PathBuf>,
    status: String,
    error: Option<String>,
    succeeded: bool,
}

async fn finalize_track_saver(
    track: RecordingTrack,
    saver: Option<Arc<AsyncMutex<IncrementalAudioSaver>>>,
    accumulation_error: Option<String>,
) -> Option<TrackFinalizeOutcome> {
    let saver = saver?;
    let result = saver.lock().await.finalize().await;
    Some(match result {
        Ok(path) => TrackFinalizeOutcome {
            track,
            path: Some(path),
            status: if accumulation_error.is_some() {
                "completed_with_warnings"
            } else {
                "completed"
            }
            .to_string(),
            error: accumulation_error,
            succeeded: true,
        },
        Err(error) => TrackFinalizeOutcome {
            track,
            path: None,
            status: "failed".to_string(),
            error: Some(join_error_messages(
                accumulation_error,
                Some(error.to_string()),
            )),
            succeeded: false,
        },
    })
}

fn join_error_messages(first: Option<String>, second: Option<String>) -> String {
    match (first, second) {
        (Some(first), Some(second)) if first != second => format!("{first}; {second}"),
        (Some(first), _) => first,
        (_, Some(second)) => second,
        (None, None) => String::new(),
    }
}

fn apply_track_outcome(metadata: &mut MeetingMetadata, outcome: &TrackFinalizeOutcome) {
    if let Some(track) = metadata
        .audio_tracks
        .iter_mut()
        .find(|candidate| candidate.track == outcome.track.name())
    {
        track.status = outcome.status.clone();
        track.error = outcome.error.clone();
    }
}

/// New recording saver using incremental saving strategy
pub struct RecordingSaver {
    /// The mixed track retains the historical field name because callers use
    /// it for stats and the legacy `audio.mp4` result.
    incremental_saver: Option<Arc<AsyncMutex<IncrementalAudioSaver>>>,
    microphone_saver: Option<Arc<AsyncMutex<IncrementalAudioSaver>>>,
    system_audio_saver: Option<Arc<AsyncMutex<IncrementalAudioSaver>>>,
    meeting_folder: Option<PathBuf>,
    meeting_name: Option<String>,
    metadata: Option<MeetingMetadata>,
    transcript_state: Arc<Mutex<TranscriptState>>,
    transcript_persistence_lock: Arc<Mutex<()>>,
    accumulation_task: Option<JoinHandle<AccumulationReport>>,
}

impl RecordingSaver {
    pub fn new() -> Self {
        Self {
            incremental_saver: None,
            microphone_saver: None,
            system_audio_saver: None,
            meeting_folder: None,
            meeting_name: None,
            metadata: None,
            transcript_state: Arc::new(Mutex::new(TranscriptState::default())),
            transcript_persistence_lock: Arc::new(Mutex::new(())),
            accumulation_task: None,
        }
    }

    /// Set the meeting name for this recording session
    pub fn set_meeting_name(&mut self, name: Option<String>) {
        self.meeting_name = name;
    }

    /// Set device information in metadata
    pub fn set_device_info(&mut self, mic_name: Option<String>, sys_name: Option<String>) {
        if let Some(ref mut metadata) = self.metadata {
            metadata.devices.microphone = mic_name;
            metadata.devices.system_audio = sys_name;

            // Write updated metadata to disk if folder exists
            if let Some(folder) = &self.meeting_folder {
                let metadata_clone = metadata.clone();
                if let Err(e) = self.write_metadata(folder, &metadata_clone) {
                    warn!("Failed to update metadata with device info: {}", e);
                }
            }
        }
    }

    /// Append a revision event and update the latest materialized transcript.
    ///
    /// Event history is idempotent by `event_id`. Legacy payloads without an
    /// explicit event id remain compatible through `sequence_id` and exact
    /// payload de-duplication.
    pub fn add_transcript_segment(&self, segment: TranscriptSegment) {
        self.transcript_persistence_sink()
            .add_transcript_segment(segment);
    }

    pub fn transcript_persistence_sink(&self) -> TranscriptPersistenceSink {
        TranscriptPersistenceSink {
            transcript_state: self.transcript_state.clone(),
            meeting_folder: self.meeting_folder.clone(),
            persistence_lock: self.transcript_persistence_lock.clone(),
        }
    }

    /// Legacy method for backward compatibility - converts text to basic segment
    pub fn add_transcript_chunk(&self, text: String) {
        let sequence_id = chrono::Utc::now().timestamp_millis().max(0) as u64;
        let update = TranscriptUpdate::from_legacy_chunk(TranscriptChunkInput::new(
            text,
            sequence_id,
            0.0,
            0.0,
            false,
            Some(1.0),
            AudioSource::Unknown,
            "legacy",
        ));
        self.add_transcript_segment(update.into());
    }

    /// Start accumulation with optional incremental saving
    ///
    /// # Arguments
    /// * `auto_save` - If true, creates checkpoints and enables saving. If false, audio chunks are discarded.
    /// * `topology` - Capture sources expected on the shared frame timeline.
    pub fn start_accumulation(
        &mut self,
        auto_save: bool,
        topology: CaptureTopology,
    ) -> Result<mpsc::UnboundedSender<AudioChunk>> {
        if self.accumulation_task.is_some() {
            return Err(anyhow::anyhow!(
                "Audio accumulation is already active; refusing to orphan the existing task"
            ));
        }
        let meeting_name = self
            .meeting_name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("Meeting name must be set before audio accumulation"))?;

        if auto_save {
            info!(
                "Initializing multi-track audio saver (microphone: {}, system: {})",
                topology.microphone, topology.system_audio
            );
        } else {
            info!(
                "Starting recording without audio saving (auto-save DISABLED - transcripts only)"
            );
        }

        // Even transcript-only sessions need their meeting folder and
        // metadata. Audio savers are created only for selected tracks.
        self.initialize_meeting_folder(&meeting_name, auto_save, topology)?;
        info!("Successfully initialized recording persistence");

        // Create the one ingress channel used by the synchronized pipeline
        // only after persistence is known to be ready. The receiver task
        // classifies each chunk by DeviceType.
        let (sender, receiver) = mpsc::unbounded_channel::<AudioChunk>();
        let savers = TrackSavers {
            mixed: self.incremental_saver.clone(),
            microphone: self.microphone_saver.clone(),
            system_audio: self.system_audio_saver.clone(),
        };
        self.accumulation_task = Some(tokio::spawn(drain_audio_chunks(
            receiver, auto_save, savers,
        )));

        Ok(sender)
    }

    /// Initialize meeting folder structure and metadata
    ///
    /// # Arguments
    /// * `meeting_name` - Name of the meeting
    /// * `create_checkpoints` - Whether to create .checkpoints/ directory and IncrementalAudioSaver
    fn initialize_meeting_folder(
        &mut self,
        meeting_name: &str,
        create_checkpoints: bool,
        topology: CaptureTopology,
    ) -> Result<()> {
        // Load preferences to get base recordings folder
        let base_folder = super::recording_preferences::get_default_recordings_folder();

        // Create meeting folder structure (with or without .checkpoints/ subdirectory)
        let meeting_folder = create_meeting_folder(&base_folder, meeting_name, create_checkpoints)?;

        self.incremental_saver = None;
        self.microphone_saver = None;
        self.system_audio_saver = None;
        let mut track_initialization_errors: Vec<(RecordingTrack, String)> = Vec::new();

        // Initialize an isolated saver for every enabled output. The mixed
        // file remains `audio.mp4` for existing consumers.
        if create_checkpoints {
            for track in RecordingTrack::ALL {
                if !track.enabled(topology) {
                    continue;
                }
                let saver = IncrementalAudioSaver::new_with_layout(
                    meeting_folder.clone(),
                    48_000,
                    track.checkpoint_subdirectory(),
                    track.file_name(),
                );
                let saver = match saver {
                    Ok(saver) => Arc::new(AsyncMutex::new(saver)),
                    Err(error) => {
                        let message = error.to_string();
                        error!("Failed to initialize {} saver: {}", track.name(), message);
                        track_initialization_errors.push((track, message));
                        continue;
                    }
                };
                match track {
                    RecordingTrack::Mixed => self.incremental_saver = Some(saver),
                    RecordingTrack::Microphone => self.microphone_saver = Some(saver),
                    RecordingTrack::SystemAudio => self.system_audio_saver = Some(saver),
                }
            }
            info!("Incremental track savers initialized");
        } else {
            info!("Skipped incremental audio savers (auto-save disabled)");
        }

        let audio_tracks = RecordingTrack::ALL
            .into_iter()
            .map(|track| {
                let enabled = track.enabled(topology);
                let initialization_error = track_initialization_errors
                    .iter()
                    .find(|(failed_track, _)| *failed_track == track)
                    .map(|(_, error)| error.clone());
                AudioTrackMetadata {
                    track: track.name().to_string(),
                    file: track.file_name().to_string(),
                    enabled,
                    status: if initialization_error.is_some() {
                        "failed"
                    } else if !enabled {
                        "not_captured"
                    } else if create_checkpoints {
                        "recording"
                    } else {
                        "disabled"
                    }
                    .to_string(),
                    error: initialization_error,
                }
            })
            .collect();

        // Create initial metadata
        let metadata = MeetingMetadata {
            version: "2.0".to_string(),
            meeting_id: None, // Will be set by backend
            meeting_name: Some(meeting_name.to_string()),
            created_at: chrono::Utc::now().to_rfc3339(),
            completed_at: None,
            duration_seconds: None,
            devices: DeviceInfo {
                microphone: None, // Could be enhanced to store actual device names
                system_audio: None,
            },
            audio_file: if create_checkpoints && (topology.microphone || topology.system_audio) {
                "audio.mp4".to_string()
            } else {
                "".to_string()
            },
            transcript_file: "transcripts.json".to_string(),
            sample_rate: 48_000,
            status: "recording".to_string(),
            audio_tracks,
            timeline: Some(AudioTimelineMetadata {
                duration_frames: 0,
                sample_rate: 48_000,
                timebase: "1/48000".to_string(),
            }),
        };

        // Retain the exact folder in memory before the fallible write. If the
        // write fails, startup rollback can still identify and preserve the
        // partially initialized recovery folder.
        self.meeting_folder = Some(meeting_folder.clone());
        self.metadata = Some(metadata.clone());

        // Write initial metadata.json
        self.write_metadata(&meeting_folder, &metadata)?;

        if track_initialization_errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Failed to initialize required audio track saver(s): {}",
                track_initialization_errors
                    .iter()
                    .map(|(track, error)| format!("{}: {}", track.name(), error))
                    .collect::<Vec<_>>()
                    .join("; ")
            ))
        }
    }

    /// Write metadata.json to disk (atomic write with temp file)
    fn write_metadata(&self, folder: &PathBuf, metadata: &MeetingMetadata) -> Result<()> {
        let json_string = serde_json::to_string_pretty(metadata)?;
        write_atomic(folder, "metadata.json", json_string.as_bytes())
    }

    /// Write transcripts.json to disk (atomic write with temp file and validation)
    fn write_transcripts_json(&self, folder: &PathBuf) -> Result<()> {
        write_transcripts_json_from_state(&self.transcript_state, folder)
    }

    /// Write every unique transcript revision as NDJSON in arrival order.
    ///
    /// Rewriting via an atomic replacement is intentional: an interrupted
    /// append can leave a truncated final line, while this file is the local
    /// recovery source for deterministic replay.
    fn write_transcript_events_ndjson(&self, folder: &PathBuf) -> Result<()> {
        write_transcript_events_ndjson_from_state(&self.transcript_state, folder)
    }

    // in frontend/src-tauri/src/audio/recording_saver.rs
    pub fn get_stats(&self) -> (usize, u32) {
        if let Some(ref saver) = self.incremental_saver {
            if let Ok(guard) = saver.try_lock() {
                (guard.get_checkpoint_count() as usize, 48000)
            } else {
                (0, 48000)
            }
        } else {
            (0, 48000)
        }
    }

    /// Roll back persistence after recording startup fails.
    ///
    /// The caller must first stop capture/pipeline work and drop every clone of
    /// the recording ingress sender. This method then drains the receiver to
    /// EOF, writes an explicit error state, and releases in-memory savers
    /// without finalizing audio or deleting any checkpoint data.
    pub async fn rollback_failed_start_after_ingress_closed(
        &mut self,
        reason: &str,
    ) -> Result<Option<String>, String> {
        let reason = if reason.trim().is_empty() {
            "Recording startup failed"
        } else {
            reason.trim()
        };

        let (accumulation_report, task_error) = if let Some(task) = self.accumulation_task.take() {
            match task.await {
                Ok(report) => (report, None),
                Err(error) => (
                    AccumulationReport::default(),
                    Some(format!(
                        "Audio accumulation task failed during rollback: {error}"
                    )),
                ),
            }
        } else {
            (AccumulationReport::default(), None)
        };

        let folder_path = self
            .meeting_folder
            .as_ref()
            .map(|folder| folder.to_string_lossy().to_string());
        let mut persistence_errors = Vec::new();

        if let (Some(folder), Some(mut metadata)) = (&self.meeting_folder, self.metadata.clone()) {
            metadata.status = "error".to_string();
            metadata.completed_at = None;
            let sample_rate = accumulation_report
                .sample_rate
                .max(metadata.sample_rate)
                .max(1);
            metadata.timeline = Some(AudioTimelineMetadata {
                duration_frames: accumulation_report.max_end_frame,
                sample_rate,
                timebase: format!("1/{sample_rate}"),
            });

            for track in &mut metadata.audio_tracks {
                if !track.enabled || track.status == "failed" {
                    continue;
                }
                let recording_track = match track.track.as_str() {
                    "mixed" => Some(RecordingTrack::Mixed),
                    "microphone" => Some(RecordingTrack::Microphone),
                    "system_audio" => Some(RecordingTrack::SystemAudio),
                    _ => None,
                };
                let accumulation_error = recording_track
                    .and_then(|track| accumulation_report.track(track).error.clone());
                let failure_detail = join_error_messages(
                    Some(join_error_messages(
                        Some(reason.to_string()),
                        accumulation_error,
                    )),
                    task_error.clone(),
                );
                track.status = "interrupted".to_string();
                track.error = Some(join_error_messages(
                    track.error.take(),
                    Some(failure_detail),
                ));
            }

            if let Err(error) = self.write_metadata(folder, &metadata) {
                persistence_errors.push(format!("Failed to persist rollback metadata: {error}"));
            } else {
                self.metadata = Some(metadata);
            }

            match self.transcript_persistence_lock.lock() {
                Ok(_persistence_guard) => {
                    if let Err(error) = self.write_transcripts_json(folder) {
                        persistence_errors
                            .push(format!("Failed to preserve rollback transcripts: {error}"));
                    }
                    if let Err(error) = self.write_transcript_events_ndjson(folder) {
                        persistence_errors.push(format!(
                            "Failed to preserve rollback transcript revisions: {error}"
                        ));
                    }
                }
                Err(error) => persistence_errors
                    .push(format!("Failed to lock rollback transcript state: {error}")),
            }
        }

        // Dropping these handles never removes their on-disk checkpoint
        // directories. Recovery remains possible on the next launch.
        self.incremental_saver = None;
        self.microphone_saver = None;
        self.system_audio_saver = None;

        if persistence_errors.is_empty() {
            Ok(folder_path)
        } else {
            Err(persistence_errors.join("; "))
        }
    }

    /// Stop and save using incremental saving approach
    ///
    /// # Arguments
    /// * `app` - Tauri app handle for emitting events
    /// * `recording_duration` - Actual recording duration in seconds (from RecordingState)
    pub async fn stop_and_save<R: Runtime>(
        &mut self,
        app: &AppHandle<R>,
        recording_duration: Option<f64>,
    ) -> Result<Option<String>, String> {
        info!("Stopping recording saver");

        // The pipeline owns every sender clone. Once it has flushed and
        // dropped them, awaiting this task guarantees that all queued tail
        // chunks have reached their per-track saver. No boolean can truncate
        // the receiver loop before EOF.
        let mut accumulation_report = AccumulationReport::default();
        let accumulation_task_error = if let Some(task) = self.accumulation_task.take() {
            match task.await {
                Ok(report) => {
                    accumulation_report = report;
                    None
                }
                Err(error) => {
                    let message = format!("Audio accumulation task failed: {error}");
                    error!("{}", message);
                    Some(message)
                }
            }
        } else {
            None
        };

        let savers = TrackSavers {
            mixed: self.incremental_saver.clone(),
            microphone: self.microphone_saver.clone(),
            system_audio: self.system_audio_saver.clone(),
        };
        let mut track_outcomes = Vec::new();
        for track in RecordingTrack::ALL {
            let accumulation_error = join_error_messages(
                accumulation_report.track(track).error.clone(),
                accumulation_task_error.clone(),
            );
            let accumulation_error = (!accumulation_error.is_empty()).then_some(accumulation_error);
            if let Some(outcome) =
                finalize_track_saver(track, savers.for_track(track).cloned(), accumulation_error)
                    .await
            {
                if outcome.succeeded {
                    if let Some(path) = &outcome.path {
                        info!("Finalized {} audio: {}", track.name(), path.display());
                    }
                } else if let Some(error) = &outcome.error {
                    error!("Failed to finalize {} audio: {}", track.name(), error);
                }
                track_outcomes.push(outcome);
            }
        }

        let final_audio_path = track_outcomes
            .iter()
            .find(|outcome| outcome.track == RecordingTrack::Mixed)
            .and_then(|outcome| outcome.path.clone());
        let mixed_failure = track_outcomes
            .iter()
            .find(|outcome| outcome.track == RecordingTrack::Mixed && !outcome.succeeded)
            .and_then(|outcome| outcome.error.clone())
            .or_else(|| {
                self.metadata.as_ref().and_then(|metadata| {
                    metadata
                        .audio_tracks
                        .iter()
                        .find(|track| track.track == RecordingTrack::Mixed.name() && track.enabled)
                        .filter(|track| track.status == "failed")
                        .and_then(|track| track.error.clone())
                })
            });

        // Save final transcripts.json with validation
        if let Some(folder) = &self.meeting_folder {
            let _persistence_guard = self
                .transcript_persistence_lock
                .lock()
                .map_err(|error| format!("Failed to lock transcript persistence: {}", error))?;
            if let Err(e) = self.write_transcripts_json(folder) {
                error!("❌ Failed to write final transcripts: {}", e);
                return Err(format!("Failed to save transcripts: {}", e));
            }
            if let Err(e) = self.write_transcript_events_ndjson(folder) {
                error!("Failed to write final transcript revision history: {}", e);
                return Err(format!("Failed to save transcript revision history: {}", e));
            }

            // Verify transcripts were written correctly
            let transcript_path = folder.join("transcripts.json");
            if !transcript_path.exists() {
                error!(
                    "❌ Transcript file was not created at: {}",
                    transcript_path.display()
                );
                return Err("Transcript file verification failed".to_string());
            }
            info!(
                "✅ Transcripts saved and verified at: {}",
                transcript_path.display()
            );
        }

        // Update metadata to completed status with actual recording duration
        if let (Some(folder), Some(mut metadata)) = (&self.meeting_folder, self.metadata.clone()) {
            for outcome in &track_outcomes {
                apply_track_outcome(&mut metadata, outcome);
            }
            for recording_track in RecordingTrack::ALL {
                if track_outcomes
                    .iter()
                    .any(|outcome| outcome.track == recording_track)
                {
                    continue;
                }
                let Some(accumulation_error) =
                    accumulation_report.track(recording_track).error.clone()
                else {
                    continue;
                };
                if let Some(track) = metadata
                    .audio_tracks
                    .iter_mut()
                    .find(|track| track.track == recording_track.name() && track.enabled)
                {
                    track.status = "failed".to_string();
                    track.error = Some(join_error_messages(
                        track.error.take(),
                        Some(accumulation_error),
                    ));
                }
            }
            if let Some(task_error) = &accumulation_task_error {
                for track in &mut metadata.audio_tracks {
                    if track.enabled && track.status == "recording" {
                        track.status = "failed".to_string();
                        track.error = Some(task_error.clone());
                    }
                }
            }

            // A raw-track failure is intentionally represented on that track
            // while the meeting remains completed if the compatibility mixed
            // file and transcripts are durable.
            metadata.status = if mixed_failure.is_some() {
                "error"
            } else {
                "completed"
            }
            .to_string();
            metadata.completed_at = Some(chrono::Utc::now().to_rfc3339());

            let timeline_sample_rate = if accumulation_report.sample_rate > 0 {
                accumulation_report.sample_rate
            } else {
                metadata.sample_rate.max(1)
            };
            let duration_frames = if accumulation_report.max_end_frame > 0 {
                accumulation_report.max_end_frame
            } else {
                recording_duration
                    .filter(|duration| duration.is_finite() && *duration > 0.0)
                    .map(|duration| (duration * timeline_sample_rate as f64).round() as u64)
                    .unwrap_or(0)
            };
            metadata.timeline = Some(AudioTimelineMetadata {
                duration_frames,
                sample_rate: timeline_sample_rate,
                timebase: format!("1/{timeline_sample_rate}"),
            });

            // Prefer RecordingState's active duration for the legacy field,
            // then the frame clock, then the final visible transcript.
            metadata.duration_seconds = recording_duration
                .or_else(|| {
                    (duration_frames > 0)
                        .then_some(duration_frames as f64 / timeline_sample_rate as f64)
                })
                .or_else(|| {
                    if let Ok(state) = self.transcript_state.lock() {
                        state
                            .materialized
                            .last()
                            .map(|segment| segment.audio_end_time)
                    } else {
                        None
                    }
                });

            if let Err(e) = self.write_metadata(folder, &metadata) {
                error!("❌ Failed to update metadata to completed: {}", e);
                return Err(format!("Failed to update metadata: {}", e));
            }
            self.metadata = Some(metadata.clone());

            info!(
                "✅ Metadata updated with duration: {:?}s",
                metadata.duration_seconds
            );
        }

        // Checkpoints are the last-resort recovery source. Delete their exact
        // per-track directories only after every configured track finalized
        // and both transcript files plus metadata were committed. Any early
        // return above deliberately leaves all checkpoints intact.
        if !track_outcomes.is_empty() && track_outcomes.iter().all(|outcome| outcome.succeeded) {
            if let Some(folder) = &self.meeting_folder {
                let checkpoint_root = folder.join(".checkpoints");
                for outcome in &track_outcomes {
                    let checkpoint_directory = folder.join(outcome.track.checkpoint_subdirectory());
                    if checkpoint_directory.exists() {
                        if let Err(error) = std::fs::remove_dir_all(&checkpoint_directory) {
                            warn!(
                                "Failed to clean finalized {} checkpoints {}: {}",
                                outcome.track.name(),
                                checkpoint_directory.display(),
                                error
                            );
                        }
                    }
                }
                if checkpoint_root.exists() {
                    if let Err(error) = std::fs::remove_dir(&checkpoint_root) {
                        warn!(
                            "Finalized checkpoint root retained (not empty or unavailable) {}: {}",
                            checkpoint_root.display(),
                            error
                        );
                    }
                }
            }
        }

        // The legacy recording-saved event describes a finalized audio file,
        // so transcript-only mode intentionally does not emit a fake path.
        if let Some(audio_path) = &final_audio_path {
            let save_event = serde_json::json!({
                "audio_file": audio_path.to_string_lossy(),
                "transcript_file": self.meeting_folder.as_ref()
                    .map(|f| f.join("transcripts.json").to_string_lossy().to_string()),
                "meeting_name": self.meeting_name,
                "meeting_folder": self.meeting_folder.as_ref()
                    .map(|f| f.to_string_lossy().to_string())
            });

            if let Err(e) = app.emit("recording-saved", &save_event) {
                warn!("Failed to emit recording-saved event: {}", e);
            }
        }

        // Preserve an explicit stop boundary for summary binding before the
        // saver drops its trusted in-memory history. The registry correlates
        // by the unique backend session and exact folder; it never guesses the
        // most recently stopped recording.
        if let Ok(state) = self.transcript_state.lock() {
            crate::summary::live::mark_trusted_recording_pending(
                &state.event_history,
                self.meeting_folder.clone(),
            );
        }

        // Clean up in-memory transcript state after both files are durable.
        if let Ok(mut state) = self.transcript_state.lock() {
            state.materialized.clear();
            state.event_history.clear();
        }

        self.incremental_saver = None;
        self.microphone_saver = None;
        self.system_audio_saver = None;

        if let Some(error) = mixed_failure {
            return Err(format!("Failed to finalize mixed audio: {error}"));
        }

        Ok(final_audio_path.map(|path| path.to_string_lossy().to_string()))
    }

    /// Get the meeting folder path (for passing to backend)
    pub fn get_meeting_folder(&self) -> Option<&PathBuf> {
        self.meeting_folder.as_ref()
    }

    /// Get accumulated transcript segments (for reload sync)
    pub fn get_transcript_segments(&self) -> Vec<TranscriptSegment> {
        if let Ok(state) = self.transcript_state.lock() {
            state.visible_materialized()
        } else {
            Vec::new()
        }
    }

    /// Get every unique revision received in arrival order for reload replay.
    pub fn get_transcript_events(&self) -> Vec<TranscriptSegment> {
        if let Ok(state) = self.transcript_state.lock() {
            state.event_history.clone()
        } else {
            Vec::new()
        }
    }

    /// Get meeting name (for reload sync)
    pub fn get_meeting_name(&self) -> Option<String> {
        self.meeting_name.clone()
    }
}

fn write_atomic(folder: &PathBuf, file_name: &str, bytes: &[u8]) -> Result<()> {
    let destination = folder.join(file_name);
    let mut temp = tempfile::Builder::new()
        .prefix(".meetily-write-")
        .tempfile_in(folder)?;
    temp.write_all(bytes)?;
    temp.as_file_mut().sync_all()?;
    temp.persist(&destination).map_err(|error| {
        anyhow::anyhow!(
            "Failed to atomically replace {}: {}",
            destination.display(),
            error.error
        )
    })?;
    Ok(())
}

fn write_transcripts_json_from_state(
    transcript_state: &Arc<Mutex<TranscriptState>>,
    folder: &PathBuf,
) -> Result<()> {
    let segments = transcript_state
        .lock()
        .map_err(|_| anyhow::anyhow!("Failed to lock transcript state"))?
        .visible_materialized();
    let json = serde_json::json!({
        "version": "2.0",
        "segments": segments,
        "last_updated": chrono::Utc::now().to_rfc3339(),
        "total_segments": segments.len()
    });
    let json_string = serde_json::to_string_pretty(&json)?;
    write_atomic(folder, "transcripts.json", json_string.as_bytes())?;
    info!(
        "Successfully wrote transcripts.json with {} segments",
        segments.len()
    );
    Ok(())
}

fn write_transcript_events_ndjson_from_state(
    transcript_state: &Arc<Mutex<TranscriptState>>,
    folder: &PathBuf,
) -> Result<()> {
    let events = transcript_state
        .lock()
        .map_err(|_| anyhow::anyhow!("Failed to lock transcript event history"))?
        .event_history
        .clone();
    let mut ndjson = String::new();
    for event in &events {
        ndjson.push_str(&serde_json::to_string(event)?);
        ndjson.push('\n');
    }
    write_atomic(folder, "transcript-events.ndjson", ndjson.as_bytes())?;
    info!(
        "Successfully wrote transcript-events.ndjson with {} revisions",
        events.len()
    );
    Ok(())
}

fn append_transcript_event_ndjson(folder: &Path, event: &TranscriptSegment) -> Result<()> {
    let destination = folder.join("transcript-events.ndjson");
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(destination)?;
    file.write_all(&line)?;
    file.sync_data()?;
    Ok(())
}

/// Read and replay the durable transcript history for a recording folder.
///
/// The caller supplies a folder path rather than an arbitrary file path. Both
/// the folder and the selected recovery file are canonicalized and must remain
/// below Meetily's recordings root, including through symlinks/junctions.
#[tauri::command]
pub async fn read_recording_transcript_recovery(
    meeting_folder: String,
) -> std::result::Result<TranscriptRecoveryResult, String> {
    let recordings_root = super::recording_preferences::get_default_recordings_folder();
    let meeting_folder = PathBuf::from(meeting_folder);

    tauri::async_runtime::spawn_blocking(move || {
        read_recording_transcript_recovery_from_root(&recordings_root, &meeting_folder)
    })
    .await
    .map_err(|error| format!("Transcript recovery task failed: {error}"))?
}

fn read_recording_transcript_recovery_from_root(
    recordings_root: &Path,
    meeting_folder: &Path,
) -> std::result::Result<TranscriptRecoveryResult, String> {
    let canonical_root = canonical_directory(recordings_root, "recordings root")?;
    let canonical_folder = validate_recording_folder_from_root(&canonical_root, meeting_folder)?;

    let ndjson_path = resolve_recovery_file(
        &canonical_root,
        &canonical_folder,
        "transcript-events.ndjson",
    )?;
    let json_path = resolve_recovery_file(&canonical_root, &canonical_folder, "transcripts.json")?;

    if let Some(ndjson_path) = ndjson_path {
        let ndjson_result = read_transcript_events_ndjson(&ndjson_path)?;
        if !ndjson_result.events.is_empty() || json_path.is_none() {
            return Ok(ndjson_result);
        }

        let mut fallback = read_transcripts_json(
            json_path
                .as_deref()
                .expect("JSON path was checked before fallback"),
        )?;
        let mut warnings = ndjson_result.warnings;
        warnings.push(recovery_warning(
            "ndjson_no_valid_events",
            None,
            "transcript-events.ndjson contained no valid events; used transcripts.json",
        ));
        warnings.append(&mut fallback.warnings);
        fallback.warnings = warnings;
        return Ok(fallback);
    }

    if let Some(json_path) = json_path {
        let mut fallback = read_transcripts_json(&json_path)?;
        fallback.warnings.insert(
            0,
            recovery_warning(
                "ndjson_missing",
                None,
                "transcript-events.ndjson is missing; used transcripts.json",
            ),
        );
        return Ok(fallback);
    }

    Err("No transcript recovery file exists in the recording folder".to_string())
}

/// Resolve a recording directory created by the native saver and prove that it
/// remains below the configured recordings root. Recording startup uses this
/// before persisting its crash-recovery correlation.
pub(crate) fn validate_recording_folder(
    meeting_folder: &Path,
) -> std::result::Result<PathBuf, String> {
    let recordings_root = super::recording_preferences::get_default_recordings_folder();
    let canonical_root = canonical_directory(&recordings_root, "recordings root")?;
    validate_recording_folder_from_root(&canonical_root, meeting_folder)
}

fn validate_recording_folder_from_root(
    canonical_root: &Path,
    meeting_folder: &Path,
) -> std::result::Result<PathBuf, String> {
    let canonical_folder = canonical_directory(meeting_folder, "recording folder")?;
    if !canonical_folder.starts_with(canonical_root) {
        return Err("Recording folder is outside the configured recordings root".to_string());
    }
    Ok(canonical_folder)
}

fn canonical_directory(path: &Path, label: &str) -> std::result::Result<PathBuf, String> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("Failed to resolve {label}: {error}"))?;
    if !canonical.is_dir() {
        return Err(format!("{label} is not a directory"));
    }
    Ok(canonical)
}

fn resolve_recovery_file(
    canonical_root: &Path,
    canonical_folder: &Path,
    file_name: &str,
) -> std::result::Result<Option<PathBuf>, String> {
    let candidate = canonical_folder.join(file_name);
    match std::fs::symlink_metadata(&candidate) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Failed to inspect {file_name}: {error}")),
    }

    let canonical_file = std::fs::canonicalize(&candidate)
        .map_err(|error| format!("Failed to resolve {file_name}: {error}"))?;
    if !canonical_file.starts_with(canonical_root) {
        return Err(format!("{file_name} resolves outside the recordings root"));
    }
    if !canonical_file.is_file() {
        return Err(format!("{file_name} is not a regular file"));
    }
    ensure_recovery_file_size(&canonical_file, file_name)?;
    Ok(Some(canonical_file))
}

fn ensure_recovery_file_size(path: &Path, file_name: &str) -> std::result::Result<(), String> {
    let size = std::fs::metadata(path)
        .map_err(|error| format!("Failed to inspect {file_name}: {error}"))?
        .len();
    if size > MAX_TRANSCRIPT_RECOVERY_FILE_BYTES {
        return Err(format!(
            "{file_name} exceeds the {} MiB recovery limit",
            MAX_TRANSCRIPT_RECOVERY_FILE_BYTES / (1024 * 1024)
        ));
    }
    Ok(())
}

fn read_transcript_events_ndjson(
    path: &Path,
) -> std::result::Result<TranscriptRecoveryResult, String> {
    let file = File::open(path)
        .map_err(|error| format!("Failed to open transcript-events.ndjson: {error}"))?;
    let mut reader = BufReader::new(file);
    let mut state = TranscriptState::default();
    let mut warnings = Vec::new();
    let mut line_bytes = Vec::new();
    let mut line_number = 0usize;

    loop {
        line_bytes.clear();
        let bytes_read = reader
            .read_until(b'\n', &mut line_bytes)
            .map_err(|error| format!("Failed to read transcript-events.ndjson: {error}"))?;
        if bytes_read == 0 {
            break;
        }
        line_number += 1;

        if line_bytes.len() > MAX_TRANSCRIPT_RECOVERY_LINE_BYTES {
            warnings.push(recovery_warning(
                "ndjson_line_too_large",
                Some(line_number),
                "Skipped an oversized transcript event line",
            ));
            continue;
        }

        let trimmed = trim_ascii_whitespace(&line_bytes);
        if trimmed.is_empty() {
            continue;
        }

        let value = match serde_json::from_slice::<serde_json::Value>(trimmed) {
            Ok(value) => value,
            Err(error) => {
                warnings.push(recovery_warning(
                    "invalid_ndjson_line",
                    Some(line_number),
                    format!("Skipped an invalid transcript event: {error}"),
                ));
                continue;
            }
        };

        match transcript_segment_from_recovery_value(value) {
            Ok(segment) => {
                apply_recovered_segment(&mut state, &mut warnings, Some(line_number), segment)
            }
            Err(error) => warnings.push(recovery_warning(
                "invalid_transcript_event",
                Some(line_number),
                format!("Skipped an invalid transcript event: {error}"),
            )),
        }
    }

    Ok(TranscriptRecoveryResult {
        events: state.event_history,
        source: TranscriptRecoverySource::TranscriptEventsNdjson,
        warnings,
    })
}

fn read_transcripts_json(path: &Path) -> std::result::Result<TranscriptRecoveryResult, String> {
    let file =
        File::open(path).map_err(|error| format!("Failed to open transcripts.json: {error}"))?;
    let document: serde_json::Value = serde_json::from_reader(BufReader::new(file))
        .map_err(|error| format!("Failed to parse transcripts.json: {error}"))?;
    let segments = document
        .get("segments")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "transcripts.json does not contain a segments array".to_string())?;

    let mut state = TranscriptState::default();
    let mut warnings = Vec::new();
    for (index, value) in segments.iter().cloned().enumerate() {
        let line = index + 1;
        match transcript_segment_from_recovery_value(value) {
            Ok(segment) => apply_recovered_segment(&mut state, &mut warnings, Some(line), segment),
            Err(error) => warnings.push(recovery_warning(
                "invalid_json_segment",
                Some(line),
                format!("Skipped an invalid transcripts.json segment: {error}"),
            )),
        }
    }

    Ok(TranscriptRecoveryResult {
        events: state.event_history,
        source: TranscriptRecoverySource::TranscriptsJson,
        warnings,
    })
}

fn transcript_segment_from_recovery_value(
    mut value: serde_json::Value,
) -> std::result::Result<TranscriptSegment, String> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| "transcript event must be a JSON object".to_string())?;

    if !object.get("text").is_some_and(serde_json::Value::is_string) {
        return Err("text must be a string".to_string());
    }

    let timestamp_is_missing = object
        .get("timestamp")
        .and_then(serde_json::Value::as_str)
        .map_or(true, str::is_empty);
    if timestamp_is_missing {
        if let Some(display_time) = object
            .get("display_time")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        {
            object.insert(
                "timestamp".to_string(),
                serde_json::Value::String(display_time),
            );
        }
    }

    let utterance_is_missing = object
        .get("utterance_id")
        .and_then(serde_json::Value::as_str)
        .map_or(true, str::is_empty);
    if utterance_is_missing {
        if let Some(id) = object
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .map(str::to_string)
        {
            object.insert("utterance_id".to_string(), serde_json::Value::String(id));
        }
    }

    // Older transcripts.json files serialize absent numeric/boolean fields as
    // null, while TranscriptUpdate uses concrete values with serde defaults.
    for field in [
        "timestamp",
        "source",
        "sequence_id",
        "chunk_start_time",
        "is_partial",
        "confidence",
        "audio_start_time",
        "audio_end_time",
        "duration",
        "schema_version",
        "revision",
    ] {
        if object.get(field).is_some_and(serde_json::Value::is_null) {
            object.remove(field);
        }
    }

    serde_json::from_value::<TranscriptUpdate>(value)
        .map(TranscriptSegment::from)
        .map_err(|error| error.to_string())
}

fn apply_recovered_segment(
    state: &mut TranscriptState,
    warnings: &mut Vec<TranscriptRecoveryWarning>,
    line: Option<usize>,
    segment: TranscriptSegment,
) {
    let event_id = segment.event_id.clone();
    let utterance_id = segment.materialization_key();
    let revision = segment.revision;
    match state.apply(segment) {
        TranscriptApplyResult::Duplicate => warnings.push(recovery_warning(
            "duplicate_event_id",
            line,
            format!("Ignored duplicate transcript event {event_id}"),
        )),
        TranscriptApplyResult::ConflictIgnored => warnings.push(recovery_warning(
            "revision_conflict_ignored",
            line,
            format!(
                "Retained the deterministic winner for utterance {utterance_id} revision {revision}"
            ),
        )),
        TranscriptApplyResult::Added
        | TranscriptApplyResult::Updated
        | TranscriptApplyResult::Stale => {}
    }
}

fn recovery_warning(
    code: impl Into<String>,
    line: Option<usize>,
    message: impl Into<String>,
) -> TranscriptRecoveryWarning {
    TranscriptRecoveryWarning {
        code: code.into(),
        line,
        message: message.into(),
    }
}

fn trim_ascii_whitespace(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(|byte| byte.is_ascii_whitespace()) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(|byte| byte.is_ascii_whitespace()) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

impl Default for RecordingSaver {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn segment(
        event_id: &str,
        revision: u64,
        event_kind: &str,
        is_stable: bool,
        text: &str,
    ) -> TranscriptSegment {
        let update: TranscriptUpdate = serde_json::from_value(json!({
            "text": text,
            "timestamp": "12:00:00",
            "source": "Audio",
            "sequence_id": 7,
            "chunk_start_time": 1.0,
            "confidence": 0.9,
            "audio_start_time": 1.0,
            "audio_end_time": 2.0,
            "duration": 1.0,
            "schema_version": 1,
            "event_id": event_id,
            "utterance_id": "utterance-7",
            "revision": revision,
            "event_kind": event_kind,
            "is_stable": is_stable,
            "start_ms": 1000,
            "end_ms": 2000,
            "audio_source": "mixed",
            "created_at": format!("2026-01-01T00:00:0{}Z", revision.min(9))
        }))
        .expect("test transcript update should deserialize");
        update.into()
    }

    #[test]
    fn duplicate_event_is_idempotent() {
        let mut state = TranscriptState::default();
        let event = segment("event-1", 0, "final", true, "hello");

        assert_eq!(state.apply(event.clone()), TranscriptApplyResult::Added);
        assert_eq!(state.apply(event), TranscriptApplyResult::Duplicate);
        assert_eq!(state.materialized.len(), 1);
        assert_eq!(state.event_history.len(), 1);
    }

    #[test]
    fn diarization_metadata_round_trips_with_nested_value_as_canonical() {
        let update: TranscriptUpdate = serde_json::from_value(json!({
            "text": "多人测试",
            "timestamp": "12:00:00",
            "source": "Audio",
            "sequence_id": 8,
            "audio_start_time": 2.0,
            "audio_end_time": 3.0,
            "duration": 1.0,
            "asr_provider": "deepgram",
            "asr_model": "nova-3",
            "diarization": {
                "provider": "moss-worker",
                "model": "MOSS-Transcribe-Diarize",
                "model_revision": "fixture-sha",
                "revision": 7,
                "window_id": "window-2",
                "window_start_frame": 96000,
                "window_end_frame": 144000,
                "status": "provisional"
            },
            "diarization_provider": "must-not-win",
            "diarization_revision": 99
        }))
        .expect("deserialize update");

        let segment = TranscriptSegment::from(update);
        assert_eq!(segment.asr_provider.as_deref(), Some("deepgram"));
        assert_eq!(segment.asr_model.as_deref(), Some("nova-3"));
        assert_eq!(segment.diarization_provider.as_deref(), Some("moss-worker"));
        assert_eq!(segment.diarization_revision, Some(7));
        assert_eq!(segment.speaker_confidence, None);

        let encoded = serde_json::to_string(&segment).expect("serialize transcript segment");
        let decoded: TranscriptSegment =
            serde_json::from_str(&encoded).expect("deserialize transcript segment");
        assert_eq!(decoded, segment);
    }

    #[test]
    fn stale_revision_is_audited_but_does_not_replace_latest() {
        let mut state = TranscriptState::default();
        assert_eq!(
            state.apply(segment("event-2", 2, "final", true, "latest")),
            TranscriptApplyResult::Added
        );
        assert_eq!(
            state.apply(segment("event-1", 1, "correction", true, "stale")),
            TranscriptApplyResult::Stale
        );

        assert_eq!(state.materialized.len(), 1);
        assert_eq!(state.materialized[0].revision, 2);
        assert_eq!(state.materialized[0].text, "latest");
        assert_eq!(state.event_history.len(), 2);
    }

    #[test]
    fn stable_final_replaces_partial_at_same_revision() {
        let mut state = TranscriptState::default();
        assert_eq!(
            state.apply(segment("partial-1", 0, "partial", false, "hel")),
            TranscriptApplyResult::Added
        );
        assert_eq!(
            state.apply(segment("final-1", 0, "final", true, "hello")),
            TranscriptApplyResult::Updated
        );

        assert_eq!(state.materialized.len(), 1);
        assert_eq!(state.materialized[0].text, "hello");
        assert!(state.materialized[0].is_stable);
        assert_eq!(state.event_history.len(), 2);
    }

    #[test]
    fn legacy_sequence_id_still_materializes_updates() {
        let partial: TranscriptUpdate = serde_json::from_value(json!({
            "text": "hel",
            "timestamp": "12:00:00",
            "sequence_id": 7,
            "is_partial": true
        }))
        .unwrap();
        let final_update: TranscriptUpdate = serde_json::from_value(json!({
            "text": "hello",
            "timestamp": "12:00:01",
            "sequence_id": 7,
            "is_partial": false
        }))
        .unwrap();
        let mut state = TranscriptState::default();

        assert_eq!(state.apply(partial.into()), TranscriptApplyResult::Added);
        assert_eq!(
            state.apply(final_update.into()),
            TranscriptApplyResult::Updated
        );
        assert_eq!(state.materialized.len(), 1);
        assert_eq!(state.materialized[0].sequence_id, 7);
        assert_eq!(state.materialized[0].text, "hello");
    }

    #[test]
    fn retraction_is_audited_but_hidden_until_a_newer_revision_restores_it() {
        let mut state = TranscriptState::default();
        assert_eq!(
            state.apply(segment("event-1", 1, "final", true, "visible")),
            TranscriptApplyResult::Added
        );
        assert_eq!(
            state.apply(segment("event-2", 3, "retraction", true, "")),
            TranscriptApplyResult::Updated
        );
        assert!(state.visible_materialized().is_empty());

        assert_eq!(
            state.apply(segment("event-stale", 2, "correction", true, "stale")),
            TranscriptApplyResult::Stale
        );
        assert!(state.visible_materialized().is_empty());

        assert_eq!(
            state.apply(segment("event-4", 4, "correction", true, "restored")),
            TranscriptApplyResult::Updated
        );
        assert_eq!(state.visible_materialized()[0].text, "restored");
        assert_eq!(state.event_history.len(), 4);
    }

    fn audio_chunk(device_type: DeviceType, start_frame: u64) -> AudioChunk {
        AudioChunk {
            data: vec![0.25; 2_400],
            sample_rate: 48_000,
            timestamp: start_frame as f64 / 48_000.0,
            chunk_id: start_frame / 2_400,
            device_type,
            start_frame: Some(start_frame),
            end_frame: Some(start_frame + 2_400),
        }
    }

    #[test]
    fn device_types_route_to_independent_tracks() {
        assert_eq!(
            recording_track_for_device(&DeviceType::Mixed),
            RecordingTrack::Mixed
        );
        assert_eq!(
            recording_track_for_device(&DeviceType::Microphone),
            RecordingTrack::Microphone
        );
        assert_eq!(
            recording_track_for_device(&DeviceType::System),
            RecordingTrack::SystemAudio
        );
    }

    #[tokio::test]
    async fn accumulator_drains_every_chunk_after_sender_closes() {
        let (sender, receiver) = mpsc::unbounded_channel();
        sender.send(audio_chunk(DeviceType::Microphone, 0)).unwrap();
        sender.send(audio_chunk(DeviceType::System, 2_400)).unwrap();
        sender.send(audio_chunk(DeviceType::Mixed, 4_800)).unwrap();
        drop(sender);

        let report = drain_audio_chunks(receiver, false, TrackSavers::default()).await;

        assert_eq!(report.chunks_received, 3);
        assert_eq!(report.microphone.chunks_received, 1);
        assert_eq!(report.system_audio.chunks_received, 1);
        assert_eq!(report.mixed.chunks_received, 1);
        assert_eq!(report.max_end_frame, 7_200);
        assert_eq!(report.sample_rate, 48_000);
    }

    #[test]
    fn accumulation_refuses_missing_meeting_name_without_side_effects() {
        let mut saver = RecordingSaver::new();

        let error = saver
            .start_accumulation(false, CaptureTopology::microphone_only())
            .unwrap_err();

        assert!(error.to_string().contains("Meeting name must be set"));
        assert!(saver.accumulation_task.is_none());
        assert!(saver.meeting_folder.is_none());
    }

    #[tokio::test]
    async fn accumulation_refuses_to_replace_an_existing_task() {
        let mut saver = RecordingSaver::new();
        saver.set_meeting_name(Some("must-not-start".to_string()));
        saver.accumulation_task = Some(tokio::spawn(async { AccumulationReport::default() }));

        let error = saver
            .start_accumulation(false, CaptureTopology::microphone_only())
            .unwrap_err();

        assert!(error.to_string().contains("already active"));
        if let Some(task) = saver.accumulation_task.take() {
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn failed_start_rollback_marks_error_and_preserves_checkpoints() {
        let directory = tempfile::tempdir().unwrap();
        let meeting_folder = directory.path().join("meeting");
        let checkpoint_directory = meeting_folder.join(".checkpoints/mixed");
        std::fs::create_dir_all(&checkpoint_directory).unwrap();
        std::fs::write(
            checkpoint_directory.join("audio_chunk_000.mp4"),
            b"recoverable-audio",
        )
        .unwrap();

        let mut saver = RecordingSaver::new();
        saver.meeting_folder = Some(meeting_folder.clone());
        saver.metadata = Some(MeetingMetadata {
            version: "2.0".to_string(),
            meeting_id: None,
            meeting_name: Some("rollback".to_string()),
            created_at: "2026-09-01T00:00:00Z".to_string(),
            completed_at: None,
            duration_seconds: None,
            devices: DeviceInfo {
                microphone: Some("test microphone".to_string()),
                system_audio: None,
            },
            audio_file: "audio.mp4".to_string(),
            transcript_file: "transcripts.json".to_string(),
            sample_rate: 48_000,
            status: "recording".to_string(),
            audio_tracks: vec![AudioTrackMetadata {
                track: "mixed".to_string(),
                file: "audio.mp4".to_string(),
                enabled: true,
                status: "recording".to_string(),
                error: None,
            }],
            timeline: None,
        });
        let (sender, receiver) = mpsc::unbounded_channel();
        sender.send(audio_chunk(DeviceType::Mixed, 0)).unwrap();
        drop(sender);
        saver.accumulation_task = Some(tokio::spawn(drain_audio_chunks(
            receiver,
            false,
            TrackSavers::default(),
        )));

        let folder = saver
            .rollback_failed_start_after_ingress_closed("VAD initialization failed")
            .await
            .unwrap();

        assert_eq!(folder, Some(meeting_folder.to_string_lossy().to_string()));
        assert!(checkpoint_directory.join("audio_chunk_000.mp4").exists());
        let metadata: MeetingMetadata =
            serde_json::from_slice(&std::fs::read(meeting_folder.join("metadata.json")).unwrap())
                .unwrap();
        assert_eq!(metadata.status, "error");
        assert_eq!(metadata.audio_tracks[0].status, "interrupted");
        assert!(metadata.audio_tracks[0]
            .error
            .as_deref()
            .unwrap()
            .contains("VAD initialization failed"));
        assert!(saver.accumulation_task.is_none());
    }

    #[test]
    fn version_one_metadata_deserializes_without_v2_fields() {
        let metadata: MeetingMetadata = serde_json::from_value(json!({
            "version": "1.0",
            "meeting_id": null,
            "meeting_name": "legacy",
            "created_at": "2026-01-01T00:00:00Z",
            "completed_at": null,
            "duration_seconds": 1.0,
            "devices": { "microphone": null, "system_audio": null },
            "audio_file": "audio.mp4",
            "transcript_file": "transcripts.json",
            "sample_rate": 48000,
            "status": "completed"
        }))
        .unwrap();

        assert!(metadata.audio_tracks.is_empty());
        assert!(metadata.timeline.is_none());
    }

    #[test]
    fn transcript_sink_remains_durable_without_recording_manager_owner() {
        let directory = tempfile::tempdir().unwrap();
        let mut saver = RecordingSaver::new();
        saver.meeting_folder = Some(directory.path().to_path_buf());
        let sink = saver.transcript_persistence_sink();
        drop(saver);

        sink.add_transcript_segment(segment("event-final", 0, "final", true, "尾部字幕"));

        let materialized = std::fs::read_to_string(directory.path().join("transcripts.json"))
            .expect("materialized transcript should be persisted");
        let revisions = std::fs::read_to_string(directory.path().join("transcript-events.ndjson"))
            .expect("revision stream should be persisted");
        assert!(materialized.contains("尾部字幕"));
        assert!(revisions.contains("event-final"));
    }

    #[test]
    fn recovery_replays_ndjson_and_tolerates_duplicate_and_corrupt_tail() {
        let sandbox = tempfile::tempdir().unwrap();
        let recordings_root = sandbox.path().join("recordings");
        let meeting_folder = recordings_root.join("meeting-a");
        std::fs::create_dir_all(&meeting_folder).unwrap();

        let partial = segment("event-partial", 0, "partial", false, "hel");
        let final_event = segment("event-final", 1, "final", true, "hello");
        let contents = format!(
            "{}\n{}\n{}\n{{\"event_id\":",
            serde_json::to_string(&partial).unwrap(),
            serde_json::to_string(&final_event).unwrap(),
            serde_json::to_string(&final_event).unwrap(),
        );
        std::fs::write(meeting_folder.join("transcript-events.ndjson"), contents).unwrap();

        let recovered =
            read_recording_transcript_recovery_from_root(&recordings_root, &meeting_folder)
                .unwrap();

        assert_eq!(
            recovered.source,
            TranscriptRecoverySource::TranscriptEventsNdjson
        );
        assert_eq!(recovered.events.len(), 2);
        assert_eq!(recovered.events[0].revision, 0);
        assert_eq!(recovered.events[1].revision, 1);
        assert!(recovered
            .warnings
            .iter()
            .any(|warning| warning.code == "duplicate_event_id"));
        assert!(recovered
            .warnings
            .iter()
            .any(|warning| warning.code == "invalid_ndjson_line"));
    }

    #[test]
    fn recovery_falls_back_to_legacy_transcripts_json() {
        let sandbox = tempfile::tempdir().unwrap();
        let recordings_root = sandbox.path().join("recordings");
        let meeting_folder = recordings_root.join("meeting-b");
        std::fs::create_dir_all(&meeting_folder).unwrap();
        let legacy = json!({
            "version": "1.0",
            "segments": [{
                "id": "legacy-segment-4",
                "text": "legacy transcript",
                "timestamp": "2026-01-01T00:00:00Z",
                "audio_start_time": 1.25,
                "audio_end_time": 2.5,
                "duration": 1.25,
                "sequence_id": 4
            }]
        });
        std::fs::write(
            meeting_folder.join("transcripts.json"),
            serde_json::to_vec_pretty(&legacy).unwrap(),
        )
        .unwrap();

        let recovered =
            read_recording_transcript_recovery_from_root(&recordings_root, &meeting_folder)
                .unwrap();

        assert_eq!(recovered.source, TranscriptRecoverySource::TranscriptsJson);
        assert_eq!(recovered.events.len(), 1);
        assert_eq!(recovered.events[0].utterance_id, "legacy-segment-4");
        assert_eq!(recovered.events[0].text, "legacy transcript");
        assert!(recovered.events[0].is_stable);
        assert!(recovered
            .warnings
            .iter()
            .any(|warning| warning.code == "ndjson_missing"));
    }

    #[test]
    fn recovery_rejects_folder_outside_recordings_root() {
        let sandbox = tempfile::tempdir().unwrap();
        let recordings_root = sandbox.path().join("recordings");
        let outside_folder = sandbox.path().join("outside");
        std::fs::create_dir_all(&recordings_root).unwrap();
        std::fs::create_dir_all(&outside_folder).unwrap();
        std::fs::write(
            outside_folder.join("transcripts.json"),
            br#"{"version":"1.0","segments":[]}"#,
        )
        .unwrap();

        let error = read_recording_transcript_recovery_from_root(&recordings_root, &outside_folder)
            .unwrap_err();
        assert!(error.contains("outside the configured recordings root"));
    }
}
