//! Session-scoped diarization sidecar runtime.
//!
//! This actor is deliberately a best-effort branch beside recording and ASR.
//! Rust capture/transcription code is the only producer of ingress messages;
//! no Tauri command accepts transcript corrections from a renderer.

use super::moss_protocol::{
    MossSegment, MossWorkerRequest, MossWorkerResponse, MOSS_WORKER_SCHEMA_VERSION,
};
use super::process_transport::{MossJsonlProcessFactory, MossProcessTransportConfig};
use super::speaker_stabilizer::{
    stabilize_speakers, SpeakerStabilizerConfig, StabilizedObservation,
};
use super::worker_supervisor::{
    MossWorkerError, MossWorkerErrorCode, MossWorkerSupervisor, MossWorkerSupervisorConfig,
};
use crate::audio::recording_saver::{TranscriptPersistenceSink, TranscriptSegment};
use crate::audio::recording_state::AudioChunk;
use crate::audio::transcription::{
    DiarizationMetadata, DiarizationStatus, SpeakerMetadata, SpeakerStatus, TranscriptEvent,
    TranscriptEventKind, TranscriptUpdate,
};
use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const CANONICAL_SAMPLE_RATE: u64 = 48_000;
const SNAPSHOT_SAMPLE_RATE: u32 = 16_000;
const MAX_WINDOW_FRAMES: u64 = 90 * CANONICAL_SAMPLE_RATE;
const DEFAULT_CACHE_FRAMES: u64 = 180 * CANONICAL_SAMPLE_RATE;
const DEFAULT_OVERLAP_FRAMES: u64 = 15 * CANONICAL_SAMPLE_RATE;
const DEFAULT_QUEUE_CAPACITY: usize = 64;
const MIN_SNAPSHOT_SAMPLES: usize = 800;
const MAX_MODEL_REVISION_BYTES: u64 = 512;
// MOSS is a long-form correction model. Keep only one materialized window in
// flight and coalesce pressure to the newest stable transcript boundary.
const MAX_IN_FLIGHT_WINDOWS: usize = 1;
// Crash leftovers are derived snapshots, never source recordings. Startup
// cleanup is deliberately direct-child-only, age-gated, and work-bounded.
const ORPHAN_SNAPSHOT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_ORPHAN_SNAPSHOT_SCAN: usize = 256;
const MAX_ORPHAN_SNAPSHOT_DELETIONS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiarizationRuntimeAvailability {
    Starting,
    Ready,
    Busy,
    Unavailable,
    Stopping,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiarizationRuntimeStatus {
    pub availability: DiarizationRuntimeAvailability,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    pub session_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiarizationIngressErrorCode {
    QueueFull,
    RuntimeClosed,
    Unavailable,
    InvalidAudio,
    InvalidTranscript,
    SessionMismatch,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DiarizationIngressError {
    pub code: DiarizationIngressErrorCode,
}

impl fmt::Debug for DiarizationIngressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiarizationIngressError")
            .field("code", &self.code)
            .finish()
    }
}

impl fmt::Display for DiarizationIngressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.code {
            DiarizationIngressErrorCode::QueueFull => "diarization queue is full",
            DiarizationIngressErrorCode::RuntimeClosed => "diarization runtime is closed",
            DiarizationIngressErrorCode::Unavailable => "diarization runtime is unavailable",
            DiarizationIngressErrorCode::InvalidAudio => "invalid trusted audio chunk",
            DiarizationIngressErrorCode::InvalidTranscript => {
                "invalid trusted stable transcript event"
            }
            DiarizationIngressErrorCode::SessionMismatch => {
                "transcript event belongs to another recording session"
            }
        })
    }
}

impl std::error::Error for DiarizationIngressError {}

#[derive(Debug, thiserror::Error)]
pub enum DiarizationRuntimeStartError {
    #[error("invalid diarization session configuration")]
    InvalidConfiguration,
    #[error("failed to create diarization session storage")]
    StorageUnavailable,
    #[error("async runtime is unavailable")]
    RuntimeUnavailable,
}

#[derive(Clone)]
pub struct DiarizationSessionIngress {
    sender: mpsc::Sender<IngressMessage>,
    status: watch::Receiver<DiarizationRuntimeStatus>,
    session_id: Arc<str>,
}

impl DiarizationSessionIngress {
    /// Best-effort capture ingress. It never waits for disk, Python, or a model.
    pub fn try_push_audio_chunk(&self, chunk: AudioChunk) -> Result<(), DiarizationIngressError> {
        self.ensure_available()?;
        let buffered =
            BufferedAudioChunk::try_from(chunk).map_err(|_| DiarizationIngressError {
                code: DiarizationIngressErrorCode::InvalidAudio,
            })?;
        self.try_send(IngressMessage::Audio(buffered))
    }

    /// Accept a canonical update produced by trusted Rust ASR code.
    pub fn try_push_transcript_update(
        &self,
        update: &TranscriptUpdate,
    ) -> Result<(), DiarizationIngressError> {
        self.try_push_transcript_event(update.clone().normalize())
    }

    /// This is crate-internal by construction: there is no Tauri command that
    /// exposes it to the renderer.
    pub fn try_push_transcript_event(
        &self,
        event: TranscriptEvent,
    ) -> Result<(), DiarizationIngressError> {
        self.ensure_available()?;
        if event.session_id.as_deref() != Some(self.session_id.as_ref()) {
            return Err(DiarizationIngressError {
                code: DiarizationIngressErrorCode::SessionMismatch,
            });
        }
        if !event.is_stable
            || event.event_kind != TranscriptEventKind::Final
            || event.text.trim().is_empty()
            || event.end_ms <= event.start_ms
            || event
                .end_ms
                .saturating_sub(event.start_ms)
                .saturating_mul(48)
                > MAX_WINDOW_FRAMES
        {
            return Err(DiarizationIngressError {
                code: DiarizationIngressErrorCode::InvalidTranscript,
            });
        }
        self.try_send(IngressMessage::Transcript(event))
    }

    pub fn status(&self) -> DiarizationRuntimeStatus {
        self.status.borrow().clone()
    }

    pub fn subscribe_status(&self) -> watch::Receiver<DiarizationRuntimeStatus> {
        self.status.clone()
    }

    fn ensure_available(&self) -> Result<(), DiarizationIngressError> {
        if self.status.borrow().availability == DiarizationRuntimeAvailability::Unavailable {
            return Err(DiarizationIngressError {
                code: DiarizationIngressErrorCode::Unavailable,
            });
        }
        Ok(())
    }

    fn try_send(&self, message: IngressMessage) -> Result<(), DiarizationIngressError> {
        self.sender.try_send(message).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => DiarizationIngressError {
                code: DiarizationIngressErrorCode::QueueFull,
            },
            mpsc::error::TrySendError::Closed(_) => DiarizationIngressError {
                code: DiarizationIngressErrorCode::RuntimeClosed,
            },
        })
    }
}

#[derive(Debug, Clone)]
pub struct DiarizationSessionConfig {
    pub session_id: String,
    pub meeting_id: Option<String>,
    pub meeting_folder: PathBuf,
    pub model_revision: String,
    pub queue_capacity: usize,
    pub cache_frames: u64,
    pub overlap_frames: u64,
}

impl DiarizationSessionConfig {
    pub fn new(
        session_id: impl Into<String>,
        meeting_folder: impl Into<PathBuf>,
        model_revision: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            meeting_id: None,
            meeting_folder: meeting_folder.into(),
            model_revision: model_revision.into(),
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            cache_frames: DEFAULT_CACHE_FRAMES,
            overlap_frames: DEFAULT_OVERLAP_FRAMES,
        }
    }

    fn validate(&self) -> Result<(), DiarizationRuntimeStartError> {
        if !safe_identifier(&self.session_id)
            || self
                .meeting_id
                .as_deref()
                .is_some_and(|value| !safe_identifier(value))
            || !safe_revision(&self.model_revision)
            || self.queue_capacity == 0
            || self.cache_frames < MAX_WINDOW_FRAMES
            || self.overlap_frames >= MAX_WINDOW_FRAMES
            || !self.meeting_folder.is_absolute()
        {
            return Err(DiarizationRuntimeStartError::InvalidConfiguration);
        }
        Ok(())
    }
}

pub trait CanonicalTranscriptSink: Send + Sync {
    fn persist(&self, update: TranscriptUpdate);
}

impl CanonicalTranscriptSink for TranscriptPersistenceSink {
    fn persist(&self, update: TranscriptUpdate) {
        self.add_transcript_segment(TranscriptSegment::from(update));
    }
}

pub trait CanonicalTranscriptEmitter: Send + Sync {
    fn emit(&self, update: &TranscriptUpdate) -> Result<(), ()>;
}

struct TauriTranscriptEmitter<R: Runtime> {
    app: AppHandle<R>,
}

impl<R: Runtime> CanonicalTranscriptEmitter for TauriTranscriptEmitter<R> {
    fn emit(&self, update: &TranscriptUpdate) -> Result<(), ()> {
        self.app.emit("transcript-update", update).map_err(|_| ())
    }
}

#[derive(Clone)]
enum WorkerBackend {
    Supervisor(MossWorkerSupervisor),
    Unavailable(MossWorkerError),
}

impl WorkerBackend {
    fn error_code(&self) -> Option<MossWorkerErrorCode> {
        match self {
            Self::Supervisor(_) => None,
            Self::Unavailable(error) => Some(error.code),
        }
    }

    fn request_shutdown(&self) {
        if let Self::Supervisor(supervisor) = self {
            supervisor.request_shutdown();
        }
    }
}

pub struct DiarizationSessionRuntime {
    ingress: DiarizationSessionIngress,
    cancellation: CancellationToken,
    backend: WorkerBackend,
    task: Option<JoinHandle<()>>,
}

impl DiarizationSessionRuntime {
    /// Assemble the production portable paths. Missing Python/model artifacts
    /// produce a live but explicitly unavailable sidecar; they never fall back
    /// to a system Python installation or a synthetic result.
    pub fn start_portable<R: Runtime>(
        app: AppHandle<R>,
        meeting_folder: PathBuf,
        session_id: String,
        sink: TranscriptPersistenceSink,
    ) -> Result<Self, DiarizationRuntimeStartError> {
        let (config, backend) = portable_backend(&app, meeting_folder, session_id)?;
        let emitter: Arc<dyn CanonicalTranscriptEmitter> = Arc::new(TauriTranscriptEmitter { app });
        Self::start_with_backend(config, backend, Arc::new(sink), emitter)
    }

    pub fn ingress(&self) -> DiarizationSessionIngress {
        self.ingress.clone()
    }

    pub fn status(&self) -> DiarizationRuntimeStatus {
        self.ingress.status()
    }

    pub fn request_shutdown(&self) {
        self.cancellation.cancel();
        self.backend.request_shutdown();
    }

    pub fn shutdown_in_background(mut self) {
        self.request_shutdown();
        if let Some(task) = self.task.take() {
            tokio::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
            });
        }
    }

    fn start_with_backend(
        config: DiarizationSessionConfig,
        backend: WorkerBackend,
        sink: Arc<dyn CanonicalTranscriptSink>,
        emitter: Arc<dyn CanonicalTranscriptEmitter>,
    ) -> Result<Self, DiarizationRuntimeStartError> {
        tokio::runtime::Handle::try_current()
            .map_err(|_| DiarizationRuntimeStartError::RuntimeUnavailable)?;
        config.validate()?;
        // Missing production model/runtime is a normal fail-closed state. Do
        // not create sidecar storage or accept audio until a real backend is
        // available.
        let journal = match &backend {
            WorkerBackend::Supervisor(_) => Some(SessionJournal::open(&config)?),
            WorkerBackend::Unavailable(_) => None,
        };
        let (sender, receiver) = mpsc::channel(config.queue_capacity);
        let cancellation = CancellationToken::new();
        let availability = if backend.error_code().is_some() {
            DiarizationRuntimeAvailability::Unavailable
        } else {
            DiarizationRuntimeAvailability::Starting
        };
        let (status_sender, status) = watch::channel(DiarizationRuntimeStatus {
            availability,
            error_code: backend.error_code().map(worker_error_code),
            session_id: config.session_id.clone(),
        });
        let ingress = DiarizationSessionIngress {
            sender,
            status,
            session_id: Arc::from(config.session_id.as_str()),
        };
        let task = tokio::spawn(run_session_actor(
            config,
            backend.clone(),
            journal,
            receiver,
            cancellation.clone(),
            status_sender,
            sink,
            emitter,
        ));
        Ok(Self {
            ingress,
            cancellation,
            backend,
            task: Some(task),
        })
    }
}

impl Drop for DiarizationSessionRuntime {
    fn drop(&mut self) {
        self.request_shutdown();
    }
}

fn portable_backend<R: Runtime>(
    app: &AppHandle<R>,
    meeting_folder: PathBuf,
    session_id: String,
) -> Result<(DiarizationSessionConfig, WorkerBackend), DiarizationRuntimeStartError> {
    let data_dir = crate::storage::app_data_dir(app)
        .map_err(|_| DiarizationRuntimeStartError::InvalidConfiguration)?;
    let portable_root = data_dir
        .parent()
        .filter(|_| {
            data_dir
                .file_name()
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("app-data"))
        })
        .map(Path::to_path_buf)
        .unwrap_or_else(|| data_dir.clone());
    let model_root = data_dir.join("models/moss-transcribe-diarize");
    let revision_file = model_root.join("model-revision.txt");
    let model_revision = read_model_revision(&revision_file)
        .unwrap_or_else(|| "moss-model-not-installed".to_string());
    let config =
        DiarizationSessionConfig::new(session_id, meeting_folder.clone(), model_revision.clone());

    if !model_root.join("config.json").is_file() || !revision_file.is_file() {
        return Ok((
            config,
            WorkerBackend::Unavailable(MossWorkerError::new(
                MossWorkerErrorCode::ModelNotInstalled,
                false,
            )),
        ));
    }

    #[cfg(windows)]
    let python_executable = portable_root.join("conda-envs/moss-td/python.exe");
    #[cfg(not(windows))]
    let python_executable = portable_root.join("conda-envs/moss-td/bin/python");
    let Some(executable_root) = python_executable.parent().map(Path::to_path_buf) else {
        return Err(DiarizationRuntimeStartError::InvalidConfiguration);
    };
    if !python_executable.is_file() {
        return Ok((
            config,
            WorkerBackend::Unavailable(MossWorkerError::new(
                MossWorkerErrorCode::RuntimeUnavailable,
                false,
            )),
        ));
    }

    let bundled_worker = app
        .path()
        .resource_dir()
        .ok()
        .map(|root| root.join("workers/moss_worker.py"));
    let staged_worker = portable_root.join("runtime/workers/moss_worker.py");
    let mut worker_script = bundled_worker
        .filter(|path| path.is_file())
        .or_else(|| staged_worker.is_file().then_some(staged_worker));
    // Development may run before the portable runtime is staged. Release
    // builds never embed or depend on the checkout's compile-time path.
    #[cfg(debug_assertions)]
    if worker_script.is_none() {
        let source_worker =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("workers/moss_worker.py");
        worker_script = source_worker.is_file().then_some(source_worker);
    }
    let worker_script = worker_script.ok_or(DiarizationRuntimeStartError::InvalidConfiguration)?;
    let worker_root = worker_script
        .parent()
        .map(Path::to_path_buf)
        .ok_or(DiarizationRuntimeStartError::InvalidConfiguration)?;
    let temp_root = data_dir.join("temp/moss-td").join(&config.session_id);
    let process_config = MossProcessTransportConfig::new(
        python_executable,
        executable_root,
        worker_script,
        worker_root,
        &portable_root,
        model_root,
        &meeting_folder,
        temp_root,
        model_revision,
    );
    let factory = MossJsonlProcessFactory::new(process_config)
        .map_err(|_| DiarizationRuntimeStartError::InvalidConfiguration)?;
    let mut supervisor_config = MossWorkerSupervisorConfig::new(&meeting_folder);
    supervisor_config.queue_capacity = 2;
    // The one-time local model load is independent from per-window inference.
    // It runs in the sidecar supervisor and never blocks capture or persistence.
    supervisor_config.startup_timeout = Duration::from_secs(300);
    let supervisor = MossWorkerSupervisor::start(supervisor_config, Arc::new(factory))
        .map_err(|_| DiarizationRuntimeStartError::RuntimeUnavailable)?;
    Ok((config, WorkerBackend::Supervisor(supervisor)))
}

fn read_model_revision(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_MODEL_REVISION_BYTES {
        return None;
    }
    let value = fs::read_to_string(path).ok()?;
    let value = value.trim();
    safe_revision(value).then(|| value.to_string())
}

enum IngressMessage {
    Audio(BufferedAudioChunk),
    Transcript(TranscriptEvent),
}

#[derive(Debug, Clone)]
struct BufferedAudioChunk {
    samples: Vec<f32>,
    sample_rate: u32,
    start_frame: u64,
    end_frame: u64,
}

impl TryFrom<AudioChunk> for BufferedAudioChunk {
    type Error = ();

    fn try_from(chunk: AudioChunk) -> Result<Self, Self::Error> {
        let start_frame = chunk.start_frame.ok_or(())?;
        let end_frame = chunk.end_frame.ok_or(())?;
        if chunk.data.is_empty()
            || !(8_000..=192_000).contains(&chunk.sample_rate)
            || end_frame <= start_frame
            || end_frame.saturating_sub(start_frame) > MAX_WINDOW_FRAMES
            || chunk.data.iter().any(|sample| !sample.is_finite())
        {
            return Err(());
        }
        let expected_frames = (chunk.data.len() as u128)
            .saturating_mul(CANONICAL_SAMPLE_RATE as u128)
            / u128::from(chunk.sample_rate);
        let actual_frames = u128::from(end_frame.saturating_sub(start_frame));
        if expected_frames.abs_diff(actual_frames) > 8 {
            return Err(());
        }
        Ok(Self {
            samples: chunk.data,
            sample_rate: chunk.sample_rate,
            start_frame,
            end_frame,
        })
    }
}

#[derive(Default)]
struct AudioWindowCache {
    chunks: VecDeque<BufferedAudioChunk>,
    max_frames: u64,
}

impl AudioWindowCache {
    fn new(max_frames: u64) -> Self {
        Self {
            chunks: VecDeque::new(),
            max_frames,
        }
    }

    fn push(&mut self, chunk: BufferedAudioChunk) {
        let newest_end = chunk.end_frame;
        self.chunks.push_back(chunk);
        let keep_from = newest_end.saturating_sub(self.max_frames);
        while self
            .chunks
            .front()
            .is_some_and(|candidate| candidate.end_frame <= keep_from)
        {
            self.chunks.pop_front();
        }
    }

    fn render(&self, start_frame: u64, end_frame: u64) -> Option<RenderedAudioWindow> {
        if end_frame <= start_frame || end_frame.saturating_sub(start_frame) > MAX_WINDOW_FRAMES {
            return None;
        }
        let sample_count = canonical_frames_to_samples(
            end_frame.saturating_sub(start_frame),
            SNAPSHOT_SAMPLE_RATE,
        )?;
        if sample_count < MIN_SNAPSHOT_SAMPLES {
            return None;
        }
        let mut samples = vec![0.0_f32; sample_count];
        let mut counts = vec![0_u8; sample_count];
        let mut copied = 0usize;
        for chunk in &self.chunks {
            let overlap_start = start_frame.max(chunk.start_frame);
            let overlap_end = end_frame.min(chunk.end_frame);
            if overlap_end <= overlap_start {
                continue;
            }
            let output_start = canonical_frames_to_samples(
                overlap_start.saturating_sub(start_frame),
                SNAPSHOT_SAMPLE_RATE,
            )?;
            let output_end = canonical_frames_to_samples(
                overlap_end.saturating_sub(start_frame),
                SNAPSHOT_SAMPLE_RATE,
            )?
            .min(sample_count);
            for output_index in output_start..output_end {
                let absolute_frame = start_frame.saturating_add(
                    (output_index as u64).saturating_mul(CANONICAL_SAMPLE_RATE)
                        / u64::from(SNAPSHOT_SAMPLE_RATE),
                );
                let source_position = absolute_frame.saturating_sub(chunk.start_frame) as f64
                    * f64::from(chunk.sample_rate)
                    / CANONICAL_SAMPLE_RATE as f64;
                let source_index = source_position.floor() as usize;
                if source_index >= chunk.samples.len() {
                    continue;
                }
                let next_index = source_index.saturating_add(1).min(chunk.samples.len() - 1);
                let fraction = (source_position - source_index as f64) as f32;
                let value = chunk.samples[source_index]
                    + (chunk.samples[next_index] - chunk.samples[source_index]) * fraction;
                let count = counts[output_index];
                samples[output_index] = if count == 0 {
                    value
                } else {
                    (samples[output_index] * f32::from(count) + value)
                        / f32::from(count.saturating_add(1))
                };
                counts[output_index] = count.saturating_add(1);
                copied = copied.saturating_add(1);
            }
        }
        (copied >= MIN_SNAPSHOT_SAMPLES).then_some(RenderedAudioWindow {
            start_frame,
            end_frame,
            samples,
        })
    }
}

fn canonical_frames_to_samples(frames: u64, sample_rate: u32) -> Option<usize> {
    let samples = u128::from(frames).saturating_mul(u128::from(sample_rate))
        / u128::from(CANONICAL_SAMPLE_RATE);
    usize::try_from(samples).ok()
}

struct RenderedAudioWindow {
    start_frame: u64,
    end_frame: u64,
    samples: Vec<f32>,
}

fn is_generated_snapshot_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(identifier) = name
        .strip_prefix("job-")
        .and_then(|value| value.strip_suffix(".wav"))
    else {
        return false;
    };
    identifier.len() == 32
        && identifier
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Best-effort cleanup for snapshots left behind by a crashed sidecar job.
///
/// Only direct, regular children matching the exact generated filename are
/// considered. Fresh files, symlinks, nested paths, and user-named WAV files
/// are never removed. Both directory scanning and deletion are bounded so
/// opening a session cannot turn into unbounded filesystem work.
fn cleanup_orphan_snapshots(snapshot_root: &Path, now: SystemTime) -> Result<usize, ()> {
    let entries = fs::read_dir(snapshot_root).map_err(|_| ())?;
    let mut candidates = Vec::new();
    for entry in entries.take(MAX_ORPHAN_SNAPSHOT_SCAN).flatten() {
        if !is_generated_snapshot_name(&entry.file_name()) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() || file_type.is_symlink() {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age >= ORPHAN_SNAPSHOT_TTL {
            candidates.push((modified, entry.path()));
        }
    }
    candidates.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let mut removed = 0usize;
    for (_, path) in candidates.into_iter().take(MAX_ORPHAN_SNAPSHOT_DELETIONS) {
        if fs::remove_file(path).is_ok() {
            removed = removed.saturating_add(1);
        }
    }
    Ok(removed)
}

#[derive(Clone)]
struct SessionJournal {
    journal_path: PathBuf,
    snapshot_root: PathBuf,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl SessionJournal {
    fn open(config: &DiarizationSessionConfig) -> Result<Self, DiarizationRuntimeStartError> {
        crate::storage::validate_portable_storage_path(
            &config.meeting_folder,
            "diarization meeting folder",
        )
        .map_err(|_| DiarizationRuntimeStartError::StorageUnavailable)?;
        let canonical_meeting_folder = fs::canonicalize(&config.meeting_folder)
            .map_err(|_| DiarizationRuntimeStartError::StorageUnavailable)?;
        crate::storage::validate_portable_storage_path(
            &canonical_meeting_folder,
            "canonical diarization meeting folder",
        )
        .map_err(|_| DiarizationRuntimeStartError::StorageUnavailable)?;
        // Keep protocol paths in the same lexical form as the configured
        // meeting root. On Windows, `canonicalize` adds a verbatim prefix;
        // mixing that form with a non-verbatim supervisor root causes a safe
        // request to fail lexical containment before canonical dispatch.
        let root = config.meeting_folder.join(".diarization");
        let snapshot_root = root.join("audio");
        fs::create_dir_all(&snapshot_root)
            .map_err(|_| DiarizationRuntimeStartError::StorageUnavailable)?;
        let canonical_snapshot_root = fs::canonicalize(&snapshot_root)
            .map_err(|_| DiarizationRuntimeStartError::StorageUnavailable)?;
        if !canonical_snapshot_root.starts_with(&canonical_meeting_folder) {
            return Err(DiarizationRuntimeStartError::StorageUnavailable);
        }
        crate::storage::validate_portable_storage_path(
            &canonical_snapshot_root,
            "diarization snapshot folder",
        )
        .map_err(|_| DiarizationRuntimeStartError::StorageUnavailable)?;
        // This is intentionally best effort: inability to remove an old
        // derived snapshot must not make capture or the canonical transcript
        // unavailable. Each live job still deletes its own exact snapshot.
        let _ = cleanup_orphan_snapshots(&snapshot_root, SystemTime::now());
        Ok(Self {
            journal_path: root.join("diarization-jobs.ndjson"),
            snapshot_root,
            lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    async fn append(&self, entry: &SessionJournalEntry) -> Result<(), ()> {
        let mut line = serde_json::to_vec(entry).map_err(|_| ())?;
        line.push(b'\n');
        let _guard = self.lock.lock().await;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.journal_path)
            .await
            .map_err(|_| ())?;
        file.write_all(&line).await.map_err(|_| ())?;
        file.sync_data().await.map_err(|_| ())?;
        Ok(())
    }

    fn snapshot_path(&self, job_id: &str) -> PathBuf {
        self.snapshot_root.join(format!("{job_id}.wav"))
    }
}

#[derive(Serialize)]
#[serde(tag = "record_type", rename_all = "snake_case")]
enum SessionJournalEntry {
    Job {
        schema: u16,
        session_id: String,
        meeting_id: Option<String>,
        job_id: String,
        window_id: String,
        window_start_frame: u64,
        window_end_frame: u64,
        model_revision: String,
        status: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_code: Option<String>,
        created_at: String,
    },
    Result {
        schema: u16,
        session_id: String,
        meeting_id: Option<String>,
        result_revision_id: String,
        revision: u64,
        job_id: String,
        window_id: String,
        window_start_frame: u64,
        window_end_frame: u64,
        model_revision: String,
        status: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_code: Option<String>,
        segments: Vec<MossSegment>,
        created_at: String,
    },
}

struct PendingWindow {
    job_id: String,
    window_id: String,
    result_revision_id: String,
    base_events: Vec<TranscriptEvent>,
    rendered: RenderedAudioWindow,
}

struct JobCompletion {
    pending: PendingWindow,
    response: Result<MossWorkerResponse, MossWorkerError>,
    latency_ms: u64,
}

#[allow(clippy::too_many_arguments)]
fn try_launch_window_job(
    config: &DiarizationSessionConfig,
    backend: &WorkerBackend,
    journal: Option<&SessionJournal>,
    audio: &AudioWindowCache,
    recent_events: &VecDeque<TranscriptEvent>,
    current: &TranscriptEvent,
    completion_sender: &mpsc::UnboundedSender<JobCompletion>,
) -> bool {
    let (Some(journal), Some(pending)) = (
        journal,
        prepare_window(config, audio, recent_events, current),
    ) else {
        return false;
    };
    tokio::spawn(run_window_job(
        config.clone(),
        backend.clone(),
        journal.clone(),
        pending,
        completion_sender.clone(),
    ));
    true
}

#[allow(clippy::too_many_arguments)]
async fn run_session_actor(
    config: DiarizationSessionConfig,
    backend: WorkerBackend,
    journal: Option<SessionJournal>,
    mut receiver: mpsc::Receiver<IngressMessage>,
    cancellation: CancellationToken,
    status_sender: watch::Sender<DiarizationRuntimeStatus>,
    sink: Arc<dyn CanonicalTranscriptSink>,
    emitter: Arc<dyn CanonicalTranscriptEmitter>,
) {
    let mut audio = AudioWindowCache::new(config.cache_frames);
    let mut recent_events: VecDeque<TranscriptEvent> = VecDeque::new();
    let mut latest_events: HashMap<String, TranscriptEvent> = HashMap::new();
    let mut observations: Vec<StabilizedObservation> = Vec::new();
    let mut next_speaker_index = 1_u64;
    let (completion_sender, mut completion_receiver) = mpsc::unbounded_channel::<JobCompletion>();
    let mut in_flight_windows = 0usize;
    let mut coalesced_event: Option<TranscriptEvent> = None;
    let initial_availability = if backend.error_code().is_some() {
        DiarizationRuntimeAvailability::Unavailable
    } else {
        DiarizationRuntimeAvailability::Ready
    };
    publish_status(
        &status_sender,
        &config.session_id,
        initial_availability,
        backend.error_code(),
    );

    loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => break,
            Some(completion) = completion_receiver.recv() => {
                in_flight_windows = in_flight_windows.saturating_sub(1);
                match completion.response {
                    Ok(response) => {
                        let corrections = build_canonical_corrections(
                            &config,
                            &completion.pending,
                            &response,
                            completion.latency_ms,
                            &mut latest_events,
                            &mut observations,
                            &mut next_speaker_index,
                        );
                        for event in corrections {
                            let update = transcript_update_from_event(&event);
                            // The session file is the authority while meeting_id is None.
                            // Persist before notifying any renderer consumer.
                            sink.persist(update.clone());
                            if emitter.emit(&update).is_err() {
                                publish_status(
                                    &status_sender,
                                    &config.session_id,
                                    DiarizationRuntimeAvailability::Ready,
                                    Some(MossWorkerErrorCode::InvalidResponse),
                                );
                            }
                        }
                        publish_status(
                            &status_sender,
                            &config.session_id,
                            DiarizationRuntimeAvailability::Ready,
                            None,
                        );
                    }
                    Err(error) => {
                        let availability = if matches!(
                            error.code,
                            MossWorkerErrorCode::ModelNotInstalled
                                | MossWorkerErrorCode::RuntimeUnavailable
                        ) {
                            DiarizationRuntimeAvailability::Unavailable
                        } else {
                            DiarizationRuntimeAvailability::Ready
                        };
                        publish_status(
                            &status_sender,
                            &config.session_id,
                            availability,
                            Some(error.code),
                        );
                    }
                }
                // Pressure is represented by one latest boundary, not one
                // task per final transcript. Only now, after the active job
                // has completed and removed its WAV, may another window be
                // rendered and materialized.
                if let Some(event) = coalesced_event.take() {
                    if in_flight_windows < MAX_IN_FLIGHT_WINDOWS
                        && try_launch_window_job(
                            &config,
                            &backend,
                            journal.as_ref(),
                            &audio,
                            &recent_events,
                            &event,
                            &completion_sender,
                        )
                    {
                        in_flight_windows = in_flight_windows.saturating_add(1);
                        publish_status(
                            &status_sender,
                            &config.session_id,
                            DiarizationRuntimeAvailability::Busy,
                            None,
                        );
                    }
                }
            }
            message = receiver.recv() => {
                let Some(message) = message else { break; };
                match message {
                    IngressMessage::Audio(chunk) => {
                        if backend.error_code().is_none() {
                            audio.push(chunk);
                        }
                    }
                    IngressMessage::Transcript(event) => {
                        if backend.error_code().is_some() {
                            continue;
                        }
                        latest_events
                            .entry(event.utterance_id.clone())
                            .and_modify(|current| {
                                if event.revision >= current.revision {
                                    *current = event.clone();
                                }
                            })
                            .or_insert_with(|| event.clone());
                        recent_events.retain(|candidate| candidate.utterance_id != event.utterance_id);
                        recent_events.push_back(event.clone());
                        prune_recent_events(&mut recent_events, event.end_ms, config.cache_frames);
                        if in_flight_windows >= MAX_IN_FLIGHT_WINDOWS {
                            // Last-write-wins is intentional: when the active
                            // long-form correction finishes, the newest stable
                            // boundary covers the intervening overlap window.
                            coalesced_event = Some(event);
                        } else if try_launch_window_job(
                            &config,
                            &backend,
                            journal.as_ref(),
                            &audio,
                            &recent_events,
                            &event,
                            &completion_sender,
                        ) {
                            in_flight_windows = in_flight_windows.saturating_add(1);
                            publish_status(
                                &status_sender,
                                &config.session_id,
                                DiarizationRuntimeAvailability::Busy,
                                None,
                            );
                        }
                    }
                }
            }
        }
    }

    backend.request_shutdown();
    publish_status(
        &status_sender,
        &config.session_id,
        DiarizationRuntimeAvailability::Stopped,
        None,
    );
}

fn prune_recent_events(
    events: &mut VecDeque<TranscriptEvent>,
    current_end_ms: u64,
    cache_frames: u64,
) {
    let cache_ms = cache_frames / 48;
    let keep_after = current_end_ms.saturating_sub(cache_ms);
    while events
        .front()
        .is_some_and(|event| event.end_ms < keep_after)
    {
        events.pop_front();
    }
}

fn prepare_window(
    config: &DiarizationSessionConfig,
    audio: &AudioWindowCache,
    recent_events: &VecDeque<TranscriptEvent>,
    current: &TranscriptEvent,
) -> Option<PendingWindow> {
    let current_start = milliseconds_to_frames(current.start_ms);
    let current_end = milliseconds_to_frames(current.end_ms);
    let overlap_start = current_start.saturating_sub(config.overlap_frames);
    let mut base_events: Vec<_> = recent_events
        .iter()
        .filter(|event| {
            let end = milliseconds_to_frames(event.end_ms);
            let start = milliseconds_to_frames(event.start_ms);
            end >= overlap_start && start < current_end
        })
        .cloned()
        .collect();
    base_events.sort_by(|left, right| {
        left.start_ms
            .cmp(&right.start_ms)
            .then_with(|| left.end_ms.cmp(&right.end_ms))
            .then_with(|| left.utterance_id.cmp(&right.utterance_id))
    });
    let window_start = base_events
        .first()
        .map(|event| milliseconds_to_frames(event.start_ms))
        .unwrap_or(current_start);
    let window_start = window_start.max(current_end.saturating_sub(MAX_WINDOW_FRAMES));
    base_events.retain(|event| milliseconds_to_frames(event.end_ms) > window_start);
    let rendered = audio.render(window_start, current_end)?;
    let job_id = format!("job-{}", Uuid::new_v4().simple());
    let window_id = format!("window-{}", Uuid::new_v4().simple());
    let result_revision_id = format!("result-{}", Uuid::new_v4().simple());
    Some(PendingWindow {
        job_id,
        window_id,
        result_revision_id,
        base_events,
        rendered,
    })
}

async fn run_window_job(
    config: DiarizationSessionConfig,
    backend: WorkerBackend,
    journal: SessionJournal,
    pending: PendingWindow,
    completion_sender: mpsc::UnboundedSender<JobCompletion>,
) {
    let created_at = now_timestamp();
    let queued = SessionJournalEntry::Job {
        schema: 1,
        session_id: config.session_id.clone(),
        meeting_id: config.meeting_id.clone(),
        job_id: pending.job_id.clone(),
        window_id: pending.window_id.clone(),
        window_start_frame: pending.rendered.start_frame,
        window_end_frame: pending.rendered.end_frame,
        model_revision: config.model_revision.clone(),
        status: "queued",
        error_code: None,
        created_at: created_at.clone(),
    };
    let _ = journal.append(&queued).await;

    let snapshot_path = journal.snapshot_path(&pending.job_id);
    let samples = pending.rendered.samples.clone();
    let snapshot_for_write = snapshot_path.clone();
    if tokio::task::spawn_blocking(move || write_pcm16_wav(&snapshot_for_write, &samples))
        .await
        .ok()
        .and_then(Result::ok)
        .is_none()
    {
        let error = MossWorkerError::new(MossWorkerErrorCode::AudioUnavailable, true);
        append_failed_result(&config, &journal, &pending, error.code).await;
        let _ = tokio::fs::remove_file(&snapshot_path).await;
        let _ = completion_sender.send(JobCompletion {
            pending,
            response: Err(error),
            latency_ms: 0,
        });
        return;
    }

    let request = MossWorkerRequest {
        schema: MOSS_WORKER_SCHEMA_VERSION,
        job: pending.job_id.clone(),
        session: config.session_id.clone(),
        window_start_frame: pending.rendered.start_frame,
        window_end_frame: pending.rendered.end_frame,
        audio_path: snapshot_path.clone(),
        prompt: None,
        model_revision: config.model_revision.clone(),
    };
    let started = Instant::now();
    let response = match backend {
        WorkerBackend::Unavailable(error) => Err(error),
        WorkerBackend::Supervisor(supervisor) => match supervisor.try_submit(request) {
            Ok(job) => {
                let running = SessionJournalEntry::Job {
                    schema: 1,
                    session_id: config.session_id.clone(),
                    meeting_id: config.meeting_id.clone(),
                    job_id: pending.job_id.clone(),
                    window_id: pending.window_id.clone(),
                    window_start_frame: pending.rendered.start_frame,
                    window_end_frame: pending.rendered.end_frame,
                    model_revision: config.model_revision.clone(),
                    status: "running",
                    error_code: None,
                    created_at: now_timestamp(),
                };
                let _ = journal.append(&running).await;
                job.wait().await
            }
            Err(error) => Err(error),
        },
    };
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match &response {
        Ok(worker_response) => {
            let result = SessionJournalEntry::Result {
                schema: 1,
                session_id: config.session_id.clone(),
                meeting_id: config.meeting_id.clone(),
                result_revision_id: pending.result_revision_id.clone(),
                revision: 1,
                job_id: pending.job_id.clone(),
                window_id: pending.window_id.clone(),
                window_start_frame: pending.rendered.start_frame,
                window_end_frame: pending.rendered.end_frame,
                model_revision: config.model_revision.clone(),
                status: "succeeded",
                error_code: None,
                segments: worker_response.segments.clone(),
                created_at: now_timestamp(),
            };
            let _ = journal.append(&result).await;
        }
        Err(error) => append_failed_result(&config, &journal, &pending, error.code).await,
    }
    // A completed request no longer needs its derived PCM snapshot. Delete
    // only this internally generated file; a crash may leave an orphan for a
    // later bounded cleanup pass.
    let _ = tokio::fs::remove_file(&snapshot_path).await;
    let _ = completion_sender.send(JobCompletion {
        pending,
        response,
        latency_ms,
    });
}

async fn append_failed_result(
    config: &DiarizationSessionConfig,
    journal: &SessionJournal,
    pending: &PendingWindow,
    code: MossWorkerErrorCode,
) {
    let result = SessionJournalEntry::Result {
        schema: 1,
        session_id: config.session_id.clone(),
        meeting_id: config.meeting_id.clone(),
        result_revision_id: pending.result_revision_id.clone(),
        revision: 1,
        job_id: pending.job_id.clone(),
        window_id: pending.window_id.clone(),
        window_start_frame: pending.rendered.start_frame,
        window_end_frame: pending.rendered.end_frame,
        model_revision: config.model_revision.clone(),
        status: if code == MossWorkerErrorCode::JobCancelled {
            "cancelled"
        } else {
            "failed"
        },
        error_code: Some(worker_error_code(code)),
        segments: Vec::new(),
        created_at: now_timestamp(),
    };
    let _ = journal.append(&result).await;
}

#[allow(clippy::too_many_arguments)]
fn build_canonical_corrections(
    config: &DiarizationSessionConfig,
    pending: &PendingWindow,
    response: &MossWorkerResponse,
    latency_ms: u64,
    latest_events: &mut HashMap<String, TranscriptEvent>,
    previous_observations: &mut Vec<StabilizedObservation>,
    next_speaker_index: &mut u64,
) -> Vec<TranscriptEvent> {
    let stabilization = stabilize_speakers(
        previous_observations,
        &response.segments,
        &SpeakerStabilizerConfig {
            next_speaker_index: *next_speaker_index,
            ..SpeakerStabilizerConfig::default()
        },
    );
    *next_speaker_index = stabilization.next_speaker_index;
    let window_keep_from = response
        .window_end_frame
        .saturating_sub(DEFAULT_CACHE_FRAMES);
    previous_observations.retain(|observation| observation.end_frame > window_keep_from);
    previous_observations.extend(stabilization.observations.clone());

    let mut corrections = Vec::new();
    for base in &pending.base_events {
        let Some(current) = latest_events.get(&base.utterance_id).cloned() else {
            continue;
        };
        let start_frame = milliseconds_to_frames(base.start_ms);
        let end_frame = milliseconds_to_frames(base.end_ms);
        let mut mapped: Vec<_> = stabilization
            .observations
            .iter()
            .filter(|observation| {
                interval_overlap(
                    start_frame,
                    end_frame,
                    observation.start_frame,
                    observation.end_frame,
                ) > 0
            })
            .cloned()
            .collect();
        mapped.sort_by(|left, right| {
            left.start_frame
                .cmp(&right.start_frame)
                .then_with(|| left.end_frame.cmp(&right.end_frame))
                .then_with(|| left.speaker.speaker_id.cmp(&right.speaker.speaker_id))
        });
        if mapped.is_empty() {
            continue;
        }
        let corrected_text = join_segment_text(mapped.iter().map(|item| item.text.as_str()));
        let corrected_text = if corrected_text.trim().is_empty() {
            current.text.clone()
        } else {
            corrected_text
        };
        let mut speaker = dominant_speaker(&mapped, start_frame, end_frame);
        if current
            .speaker
            .as_ref()
            .is_some_and(|speaker| is_user_protected(&speaker.status))
        {
            speaker = current.speaker.clone();
        }
        let text_changed = corrected_text.trim() != current.text.trim();
        let speaker_changed = speaker.as_ref().map(|value| &value.speaker_id)
            != current.speaker.as_ref().map(|value| &value.speaker_id);
        if !text_changed && !speaker_changed {
            continue;
        }
        let Some(revision) = current.revision.checked_add(1) else {
            continue;
        };
        let diarization_revision = current
            .diarization
            .as_ref()
            .map(|metadata| metadata.revision)
            .unwrap_or(0)
            .saturating_add(1);
        let event_kind = if text_changed {
            TranscriptEventKind::Correction
        } else {
            TranscriptEventKind::SpeakerUpdate
        };
        let event = TranscriptEvent {
            schema_version: current.schema_version.max(1),
            event_id: format!(
                "moss-{}-{}-{}",
                pending.job_id,
                revision,
                short_event_fingerprint(&current.utterance_id, event_kind.as_str())
            ),
            meeting_id: config.meeting_id.clone(),
            session_id: Some(config.session_id.clone()),
            utterance_id: current.utterance_id.clone(),
            revision,
            event_kind,
            is_stable: true,
            start_ms: current.start_ms,
            end_ms: current.end_ms,
            text: corrected_text,
            language: current.language.clone(),
            audio_source: current.audio_source.clone(),
            speaker,
            asr: current.asr.clone(),
            diarization: Some(DiarizationMetadata {
                provider: "moss-worker".to_string(),
                model: Some("MOSS-Transcribe-Diarize".to_string()),
                model_revision: Some(config.model_revision.clone()),
                revision: diarization_revision,
                window_id: Some(pending.window_id.clone()),
                window_start_frame: Some(response.window_start_frame),
                window_end_frame: Some(response.window_end_frame),
                status: DiarizationStatus::Resolved,
                latency_ms: Some(latency_ms),
            }),
            replaces_event_id: Some(current.event_id.clone()),
            provider_event_id: Some(pending.result_revision_id.clone()),
            created_at: now_timestamp(),
            trace_id: current.trace_id.clone(),
            sequence_id: current.sequence_id,
        };
        latest_events.insert(event.utterance_id.clone(), event.clone());
        corrections.push(event);
    }
    corrections
}

fn dominant_speaker(
    observations: &[StabilizedObservation],
    utterance_start: u64,
    utterance_end: u64,
) -> Option<SpeakerMetadata> {
    let mut durations: BTreeMap<String, (u64, SpeakerMetadata)> = BTreeMap::new();
    for observation in observations {
        let duration = interval_overlap(
            utterance_start,
            utterance_end,
            observation.start_frame,
            observation.end_frame,
        );
        let entry = durations
            .entry(observation.speaker.speaker_id.clone())
            .or_insert_with(|| (0, observation.speaker.clone()));
        entry.0 = entry.0.saturating_add(duration);
    }
    durations
        .into_values()
        .max_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| right.1.speaker_id.cmp(&left.1.speaker_id))
        })
        .map(|(_, speaker)| speaker)
}

fn is_user_protected(status: &SpeakerStatus) -> bool {
    matches!(
        status,
        SpeakerStatus::UserConfirmed | SpeakerStatus::Renamed | SpeakerStatus::Merged
    )
}

fn interval_overlap(left_start: u64, left_end: u64, right_start: u64, right_end: u64) -> u64 {
    left_end
        .min(right_end)
        .saturating_sub(left_start.max(right_start))
}

fn join_segment_text<'a>(segments: impl Iterator<Item = &'a str>) -> String {
    let mut output = String::new();
    for segment in segments {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let needs_space = output
            .chars()
            .last()
            .zip(segment.chars().next())
            .is_some_and(|(left, right)| {
                left.is_ascii_alphanumeric() && right.is_ascii_alphanumeric()
            });
        if needs_space {
            output.push(' ');
        }
        output.push_str(segment);
    }
    output
}

fn transcript_update_from_event(event: &TranscriptEvent) -> TranscriptUpdate {
    let speaker = event.speaker.clone();
    let asr = event.asr.clone();
    let diarization = event.diarization.clone();
    let timestamp = chrono::DateTime::parse_from_rfc3339(&event.created_at)
        .map(|value| value.format("%H:%M:%S").to_string())
        .unwrap_or_default();
    let audio_start_time = event.start_ms as f64 / 1_000.0;
    let audio_end_time = event.end_ms as f64 / 1_000.0;
    TranscriptUpdate {
        text: event.text.clone(),
        timestamp,
        source: event.audio_source.legacy_label().to_string(),
        sequence_id: event.sequence_id.unwrap_or_default(),
        chunk_start_time: audio_start_time,
        is_partial: false,
        confidence: asr
            .as_ref()
            .and_then(|value| value.confidence)
            .unwrap_or(0.85),
        audio_start_time,
        audio_end_time,
        duration: (audio_end_time - audio_start_time).max(0.0),
        schema_version: event.schema_version,
        event_id: Some(event.event_id.clone()),
        meeting_id: event.meeting_id.clone(),
        session_id: event.session_id.clone(),
        utterance_id: Some(event.utterance_id.clone()),
        revision: event.revision,
        event_kind: Some(event.event_kind.clone()),
        is_stable: Some(event.is_stable),
        start_ms: Some(event.start_ms),
        end_ms: Some(event.end_ms),
        language: event.language.clone(),
        audio_source: Some(event.audio_source.clone()),
        speaker: speaker.clone(),
        speaker_id: speaker.as_ref().map(|value| value.speaker_id.clone()),
        speaker_local_label: speaker.as_ref().and_then(|value| value.local_label.clone()),
        speaker_display_name: speaker
            .as_ref()
            .and_then(|value| value.display_name.clone()),
        speaker_confidence: speaker.as_ref().and_then(|value| value.confidence),
        speaker_status: speaker.as_ref().map(|value| value.status.clone()),
        asr: asr.clone(),
        asr_provider: asr.as_ref().map(|value| value.provider.clone()),
        asr_model: asr.as_ref().and_then(|value| value.model.clone()),
        asr_confidence: asr.as_ref().and_then(|value| value.confidence),
        asr_latency_ms: asr.as_ref().and_then(|value| value.latency_ms),
        diarization: diarization.clone(),
        diarization_provider: diarization.as_ref().map(|value| value.provider.clone()),
        diarization_model: diarization.as_ref().and_then(|value| value.model.clone()),
        diarization_model_revision: diarization
            .as_ref()
            .and_then(|value| value.model_revision.clone()),
        diarization_revision: diarization.as_ref().map(|value| value.revision),
        diarization_window_id: diarization
            .as_ref()
            .and_then(|value| value.window_id.clone()),
        diarization_window_start_frame: diarization
            .as_ref()
            .and_then(|value| value.window_start_frame),
        diarization_window_end_frame: diarization
            .as_ref()
            .and_then(|value| value.window_end_frame),
        diarization_status: diarization.as_ref().map(|value| value.status.clone()),
        diarization_latency_ms: diarization.as_ref().and_then(|value| value.latency_ms),
        replaces_event_id: event.replaces_event_id.clone(),
        provider_event_id: event.provider_event_id.clone(),
        created_at: Some(event.created_at.clone()),
        trace_id: event.trace_id.clone(),
    }
}

fn write_pcm16_wav(path: &Path, samples: &[f32]) -> Result<(), ()> {
    let data_bytes = u32::try_from(samples.len().saturating_mul(2)).map_err(|_| ())?;
    let riff_size = 36_u32.checked_add(data_bytes).ok_or(())?;
    let mut temp = tempfile::Builder::new()
        .prefix(".moss-window-")
        .tempfile_in(path.parent().ok_or(())?)
        .map_err(|_| ())?;
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&riff_size.to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16_u32.to_le_bytes());
    header.extend_from_slice(&1_u16.to_le_bytes());
    header.extend_from_slice(&1_u16.to_le_bytes());
    header.extend_from_slice(&SNAPSHOT_SAMPLE_RATE.to_le_bytes());
    header.extend_from_slice(&(SNAPSHOT_SAMPLE_RATE * 2).to_le_bytes());
    header.extend_from_slice(&2_u16.to_le_bytes());
    header.extend_from_slice(&16_u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data_bytes.to_le_bytes());
    temp.write_all(&header).map_err(|_| ())?;
    for sample in samples {
        let sample = sample.clamp(-1.0, 1.0);
        let pcm = if sample >= 0.0 {
            (sample * i16::MAX as f32).round() as i16
        } else {
            (sample * -(i16::MIN as f32)).round() as i16
        };
        temp.write_all(&pcm.to_le_bytes()).map_err(|_| ())?;
    }
    temp.as_file_mut().sync_all().map_err(|_| ())?;
    temp.persist(path).map_err(|_| ())?;
    Ok(())
}

fn publish_status(
    sender: &watch::Sender<DiarizationRuntimeStatus>,
    session_id: &str,
    availability: DiarizationRuntimeAvailability,
    error: Option<MossWorkerErrorCode>,
) {
    let _ = sender.send(DiarizationRuntimeStatus {
        availability,
        error_code: error.map(worker_error_code),
        session_id: session_id.to_string(),
    });
}

fn worker_error_code(code: MossWorkerErrorCode) -> String {
    use MossWorkerErrorCode as Code;
    match code {
        Code::InvalidConfiguration => "invalid_configuration",
        Code::RuntimeUnavailable => "runtime_unavailable",
        Code::ModelNotInstalled => "model_not_installed",
        Code::InvalidRequest => "invalid_request",
        Code::AudioUnavailable => "audio_unavailable",
        Code::AudioOutsideRoot => "audio_outside_root",
        Code::ModelRevisionMismatch => "model_revision_mismatch",
        Code::QueueFull => "queue_full",
        Code::WorkerUnavailable => "worker_unavailable",
        Code::CircuitOpen => "circuit_open",
        Code::StartupTimeout => "startup_timeout",
        Code::HandshakeRejected => "handshake_rejected",
        Code::WorkerCrashed => "worker_crashed",
        Code::JobTimeout => "job_timeout",
        Code::JobCancelled => "job_cancelled",
        Code::InvalidResponse => "invalid_response",
        Code::CancellationFailed => "cancellation_failed",
        Code::SupervisorStopping => "supervisor_stopping",
        Code::SupervisorStopped => "supervisor_stopped",
        Code::ShutdownTimeout => "shutdown_timeout",
        Code::ShutdownFailed => "shutdown_failed",
    }
    .to_string()
}

fn safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
}

fn safe_revision(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_./:".contains(&byte))
}

fn milliseconds_to_frames(milliseconds: u64) -> u64 {
    milliseconds.saturating_mul(48)
}

fn now_timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn short_event_fingerprint(utterance_id: &str, kind: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in utterance_id.bytes().chain(kind.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::diarization::{
        MossTransportError, MossWorkerHandshake, MossWorkerTransport, MossWorkerTransportFactory,
    };
    use crate::audio::transcription::{AsrMetadata, AudioSource};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::sync::Notify;

    #[derive(Default)]
    struct CollectSink(Mutex<Vec<TranscriptUpdate>>);

    impl CanonicalTranscriptSink for CollectSink {
        fn persist(&self, update: TranscriptUpdate) {
            self.0.lock().unwrap().push(update);
        }
    }

    #[derive(Default)]
    struct CollectEmitter(Mutex<Vec<TranscriptUpdate>>);

    impl CanonicalTranscriptEmitter for CollectEmitter {
        fn emit(&self, update: &TranscriptUpdate) -> Result<(), ()> {
            self.0.lock().unwrap().push(update.clone());
            Ok(())
        }
    }

    struct FixtureFactory;

    struct FixtureTransport {
        alive: bool,
    }

    #[derive(Default)]
    struct GatedFixtureState {
        execute_calls: AtomicUsize,
        first_started: Notify,
        release_first: Notify,
    }

    struct GatedFixtureFactory {
        state: Arc<GatedFixtureState>,
    }

    struct GatedFixtureTransport {
        alive: bool,
        state: Arc<GatedFixtureState>,
    }

    #[async_trait]
    impl MossWorkerTransportFactory for FixtureFactory {
        async fn spawn(&self) -> Result<Box<dyn MossWorkerTransport>, MossTransportError> {
            Ok(Box::new(FixtureTransport { alive: true }))
        }
    }

    #[async_trait]
    impl MossWorkerTransport for FixtureTransport {
        async fn handshake(&mut self) -> Result<MossWorkerHandshake, MossTransportError> {
            Ok(MossWorkerHandshake {
                schema: MOSS_WORKER_SCHEMA_VERSION,
                worker_revision: "fixture-worker-v1".to_string(),
                model_revision: "fixture-model-v1".to_string(),
                max_in_flight: 1,
            })
        }

        async fn execute(
            &mut self,
            request: &MossWorkerRequest,
        ) -> Result<MossWorkerResponse, MossTransportError> {
            Ok(MossWorkerResponse {
                schema: request.schema,
                job: request.job.clone(),
                session: request.session.clone(),
                window_start_frame: request.window_start_frame,
                window_end_frame: request.window_end_frame,
                segments: vec![MossSegment {
                    start_frame: request.window_start_frame,
                    end_frame: request.window_end_frame,
                    speaker: "S01".to_string(),
                    text: "corrected fixture".to_string(),
                }],
            })
        }

        async fn cancel(&mut self, _job: &str) -> Result<(), MossTransportError> {
            Ok(())
        }

        async fn shutdown(&mut self) -> Result<(), MossTransportError> {
            self.alive = false;
            Ok(())
        }

        fn is_alive(&self) -> bool {
            self.alive
        }
    }

    #[async_trait]
    impl MossWorkerTransportFactory for GatedFixtureFactory {
        async fn spawn(&self) -> Result<Box<dyn MossWorkerTransport>, MossTransportError> {
            Ok(Box::new(GatedFixtureTransport {
                alive: true,
                state: self.state.clone(),
            }))
        }
    }

    #[async_trait]
    impl MossWorkerTransport for GatedFixtureTransport {
        async fn handshake(&mut self) -> Result<MossWorkerHandshake, MossTransportError> {
            Ok(MossWorkerHandshake {
                schema: MOSS_WORKER_SCHEMA_VERSION,
                worker_revision: "fixture-worker-v1".to_string(),
                model_revision: "fixture-model-v1".to_string(),
                max_in_flight: 1,
            })
        }

        async fn execute(
            &mut self,
            request: &MossWorkerRequest,
        ) -> Result<MossWorkerResponse, MossTransportError> {
            let call_index = self.state.execute_calls.fetch_add(1, Ordering::SeqCst);
            if call_index == 0 {
                self.state.first_started.notify_one();
                self.state.release_first.notified().await;
            }
            Ok(MossWorkerResponse {
                schema: request.schema,
                job: request.job.clone(),
                session: request.session.clone(),
                window_start_frame: request.window_start_frame,
                window_end_frame: request.window_end_frame,
                segments: vec![MossSegment {
                    start_frame: request.window_start_frame,
                    end_frame: request.window_end_frame,
                    speaker: "S01".to_string(),
                    text: format!("coalesced fixture {call_index}"),
                }],
            })
        }

        async fn cancel(&mut self, _job: &str) -> Result<(), MossTransportError> {
            Ok(())
        }

        async fn shutdown(&mut self) -> Result<(), MossTransportError> {
            self.alive = false;
            self.state.release_first.notify_waiters();
            Ok(())
        }

        fn is_alive(&self) -> bool {
            self.alive
        }
    }

    fn final_event(session: &str, text: &str) -> TranscriptEvent {
        TranscriptEvent {
            schema_version: 1,
            event_id: "asr-event-1".to_string(),
            meeting_id: None,
            session_id: Some(session.to_string()),
            utterance_id: "utterance-1".to_string(),
            revision: 0,
            event_kind: TranscriptEventKind::Final,
            is_stable: true,
            start_ms: 0,
            end_ms: 1_000,
            text: text.to_string(),
            language: Some("en".to_string()),
            audio_source: AudioSource::Mixed,
            speaker: None,
            asr: Some(AsrMetadata {
                provider: "fixture-asr".to_string(),
                model: Some("fixture".to_string()),
                confidence: Some(0.9),
                latency_ms: Some(10),
            }),
            diarization: None,
            replaces_event_id: None,
            provider_event_id: None,
            created_at: "2026-09-02T00:00:00.000Z".to_string(),
            trace_id: Some("trace-1".to_string()),
            sequence_id: Some(1),
        }
    }

    fn numbered_final_event(session: &str, index: usize) -> TranscriptEvent {
        let mut event = final_event(session, &format!("original fixture {index}"));
        event.event_id = format!("asr-event-{index}");
        event.utterance_id = format!("utterance-{index}");
        event.sequence_id = Some(index as u64);
        event
    }

    fn audio_chunk() -> AudioChunk {
        AudioChunk {
            data: vec![0.1; 16_000],
            sample_rate: 16_000,
            timestamp: 0.0,
            chunk_id: 1,
            device_type: crate::audio::recording_state::DeviceType::Mixed,
            start_frame: Some(0),
            end_frame: Some(48_000),
        }
    }

    async fn wait_for_emission(emitter: &CollectEmitter) -> Option<TranscriptUpdate> {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(update) = emitter.0.lock().unwrap().first().cloned() {
                    return Some(update);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or(None)
    }

    #[tokio::test]
    async fn synthetic_audio_reaches_supervisor_and_emits_canonical_correction() {
        let directory = tempfile::tempdir().unwrap();
        let meeting = directory.path().join("meeting");
        fs::create_dir_all(&meeting).unwrap();
        let mut supervisor_config = MossWorkerSupervisorConfig::new(&meeting);
        supervisor_config.health_check_interval = Duration::from_millis(10);
        let supervisor = MossWorkerSupervisor::start(supervisor_config, Arc::new(FixtureFactory))
            .expect("start fixture supervisor");
        let sink = Arc::new(CollectSink::default());
        let emitter = Arc::new(CollectEmitter::default());
        let runtime = DiarizationSessionRuntime::start_with_backend(
            DiarizationSessionConfig::new("session-fixture", &meeting, "fixture-model-v1"),
            WorkerBackend::Supervisor(supervisor),
            sink.clone(),
            emitter.clone(),
        )
        .expect("start session runtime");
        let ingress = runtime.ingress();
        ingress.try_push_audio_chunk(audio_chunk()).unwrap();
        ingress
            .try_push_transcript_event(final_event("session-fixture", "original fixture"))
            .unwrap();

        let update = wait_for_emission(&emitter).await.unwrap_or_else(|| {
            let journal = fs::read_to_string(meeting.join(".diarization/diarization-jobs.ndjson"))
                .unwrap_or_else(|_| "<journal unavailable>".to_string());
            panic!(
                "fixture runtime should emit; status={:?}; journal={journal}",
                runtime.status()
            )
        });
        assert_eq!(update.event_kind, Some(TranscriptEventKind::Correction));
        assert_eq!(update.text, "corrected fixture");
        assert_eq!(update.revision, 1);
        assert_eq!(update.meeting_id, None);
        assert_eq!(update.session_id.as_deref(), Some("session-fixture"));
        assert_eq!(update.speaker_id.as_deref(), Some("speaker-001"));
        assert_eq!(update.diarization_provider.as_deref(), Some("moss-worker"));
        assert_eq!(sink.0.lock().unwrap().as_slice(), &[update.clone()]);

        let journal =
            fs::read_to_string(meeting.join(".diarization/diarization-jobs.ndjson")).unwrap();
        assert!(journal.contains("\"status\":\"queued\""));
        assert!(journal.contains("\"status\":\"running\""));
        assert!(journal.contains("\"status\":\"succeeded\""));
        assert!(journal.contains("\"meeting_id\":null"));
        assert!(!journal.to_ascii_lowercase().contains("stderr"));
        assert!(!journal.to_ascii_lowercase().contains("secret"));
        assert_eq!(
            fs::read_dir(meeting.join(".diarization/audio"))
                .unwrap()
                .count(),
            0,
            "completed jobs must delete their derived WAV snapshot"
        );
        runtime.shutdown_in_background();
    }

    #[tokio::test]
    async fn busy_actor_coalesces_to_latest_without_materializing_extra_wavs() {
        let directory = tempfile::tempdir().unwrap();
        let meeting = directory.path().join("meeting");
        fs::create_dir_all(&meeting).unwrap();
        let state = Arc::new(GatedFixtureState::default());
        let mut supervisor_config = MossWorkerSupervisorConfig::new(&meeting);
        supervisor_config.health_check_interval = Duration::from_millis(10);
        let supervisor = MossWorkerSupervisor::start(
            supervisor_config,
            Arc::new(GatedFixtureFactory {
                state: state.clone(),
            }),
        )
        .expect("start gated fixture supervisor");
        let sink = Arc::new(CollectSink::default());
        let emitter = Arc::new(CollectEmitter::default());
        let mut session_config =
            DiarizationSessionConfig::new("session-coalesce", &meeting, "fixture-model-v1");
        session_config.queue_capacity = 32;
        let runtime = DiarizationSessionRuntime::start_with_backend(
            session_config,
            WorkerBackend::Supervisor(supervisor),
            sink,
            emitter,
        )
        .expect("start coalescing session runtime");
        let ingress = runtime.ingress();
        ingress.try_push_audio_chunk(audio_chunk()).unwrap();
        ingress
            .try_push_transcript_event(numbered_final_event("session-coalesce", 1))
            .unwrap();

        tokio::time::timeout(Duration::from_secs(3), state.first_started.notified())
            .await
            .expect("first fixture window should reach the worker");
        for index in 2..=16 {
            ingress
                .try_push_transcript_event(numbered_final_event("session-coalesce", index))
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            while ingress.sender.capacity() != 32 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("actor should drain and coalesce ingress while the worker is busy");

        assert_eq!(
            fs::read_dir(meeting.join(".diarization/audio"))
                .unwrap()
                .count(),
            1,
            "only the active window may have a materialized WAV"
        );

        state.release_first.notify_one();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let journal =
                    fs::read_to_string(meeting.join(".diarization/diarization-jobs.ndjson"))
                        .unwrap_or_default();
                if state.execute_calls.load(Ordering::SeqCst) == 2
                    && journal.matches("\"status\":\"succeeded\"").count() == 2
                    && fs::read_dir(meeting.join(".diarization/audio"))
                        .map(|entries| entries.count() == 0)
                        .unwrap_or(false)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the active and latest coalesced windows should finish");

        let journal =
            fs::read_to_string(meeting.join(".diarization/diarization-jobs.ndjson")).unwrap();
        assert_eq!(state.execute_calls.load(Ordering::SeqCst), 2);
        assert_eq!(journal.matches("\"status\":\"queued\"").count(), 2);
        assert!(!journal.contains("queue_full"));
        runtime.shutdown_in_background();
    }

    #[tokio::test]
    async fn unavailable_backend_is_explicit_and_never_fakes_a_result() {
        let directory = tempfile::tempdir().unwrap();
        let meeting = directory.path().join("meeting");
        fs::create_dir_all(&meeting).unwrap();
        let sink = Arc::new(CollectSink::default());
        let emitter = Arc::new(CollectEmitter::default());
        let runtime = DiarizationSessionRuntime::start_with_backend(
            DiarizationSessionConfig::new("session-unavailable", &meeting, "missing-model"),
            WorkerBackend::Unavailable(MossWorkerError::new(
                MossWorkerErrorCode::ModelNotInstalled,
                false,
            )),
            sink.clone(),
            emitter.clone(),
        )
        .unwrap();
        assert_eq!(
            runtime.status().availability,
            DiarizationRuntimeAvailability::Unavailable
        );
        let ingress = runtime.ingress();
        for _ in 0..32 {
            assert_eq!(
                ingress
                    .try_push_audio_chunk(audio_chunk())
                    .unwrap_err()
                    .code,
                DiarizationIngressErrorCode::Unavailable
            );
            assert_eq!(
                ingress
                    .try_push_transcript_event(final_event("session-unavailable", "original"))
                    .unwrap_err()
                    .code,
                DiarizationIngressErrorCode::Unavailable
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(emitter.0.lock().unwrap().is_empty());
        assert!(sink.0.lock().unwrap().is_empty());
        assert!(
            !meeting.join(".diarization").exists(),
            "an unavailable backend must not create jobs or audio snapshots"
        );
        runtime.shutdown_in_background();
    }

    #[test]
    fn ingress_rejects_partial_and_cross_session_events() {
        let (sender, _receiver) = mpsc::channel(1);
        let (_status_sender, status) = watch::channel(DiarizationRuntimeStatus {
            availability: DiarizationRuntimeAvailability::Ready,
            error_code: None,
            session_id: "session-one".to_string(),
        });
        let ingress = DiarizationSessionIngress {
            sender,
            status,
            session_id: Arc::from("session-one"),
        };
        let mut partial = final_event("session-one", "partial");
        partial.event_kind = TranscriptEventKind::Partial;
        partial.is_stable = false;
        assert_eq!(
            ingress.try_push_transcript_event(partial).unwrap_err().code,
            DiarizationIngressErrorCode::InvalidTranscript
        );
        assert_eq!(
            ingress
                .try_push_transcript_event(final_event("session-two", "wrong session"))
                .unwrap_err()
                .code,
            DiarizationIngressErrorCode::SessionMismatch
        );
    }

    #[test]
    fn audio_cache_renders_synthetic_vad_window_as_pcm() {
        let mut cache = AudioWindowCache::new(DEFAULT_CACHE_FRAMES);
        cache.push(BufferedAudioChunk::try_from(audio_chunk()).unwrap());
        let rendered = cache.render(0, 48_000).expect("render audio window");
        assert_eq!(rendered.samples.len(), 16_000);
        assert!(rendered
            .samples
            .iter()
            .all(|sample| (*sample - 0.1).abs() < 0.001));
    }

    #[test]
    fn orphan_cleanup_is_ttl_gated_and_never_descends_or_touches_user_wavs() {
        let directory = tempfile::tempdir().unwrap();
        let snapshot_root = directory.path().join("meeting/.diarization/audio");
        let nested = snapshot_root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let generated = snapshot_root.join("job-00000000000000000000000000000001.wav");
        let user_wav = snapshot_root.join("recording.wav");
        let near_match = snapshot_root.join("job-00000000000000000000000000000002.WAV");
        let nested_generated = nested.join("job-00000000000000000000000000000003.wav");
        for path in [&generated, &user_wav, &near_match, &nested_generated] {
            fs::write(path, b"fixture").unwrap();
        }

        assert_eq!(
            cleanup_orphan_snapshots(&snapshot_root, SystemTime::now()).unwrap(),
            0,
            "fresh snapshots must survive startup cleanup"
        );
        let future = SystemTime::now() + ORPHAN_SNAPSHOT_TTL + Duration::from_secs(1);
        assert_eq!(cleanup_orphan_snapshots(&snapshot_root, future).unwrap(), 1);
        assert!(!generated.exists());
        assert!(user_wav.exists());
        assert!(near_match.exists());
        assert!(nested_generated.exists());
    }

    #[test]
    fn orphan_cleanup_deletion_count_is_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let snapshot_root = directory.path().join("meeting/.diarization/audio");
        fs::create_dir_all(&snapshot_root).unwrap();
        let fixture_count = MAX_ORPHAN_SNAPSHOT_DELETIONS + 5;
        for index in 0..fixture_count {
            fs::write(
                snapshot_root.join(format!("job-{index:032x}.wav")),
                b"fixture",
            )
            .unwrap();
        }
        let future = SystemTime::now() + ORPHAN_SNAPSHOT_TTL + Duration::from_secs(1);
        assert_eq!(
            cleanup_orphan_snapshots(&snapshot_root, future).unwrap(),
            MAX_ORPHAN_SNAPSHOT_DELETIONS
        );
        let remaining = fs::read_dir(&snapshot_root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| is_generated_snapshot_name(&entry.file_name()))
            .count();
        assert_eq!(remaining, 5);
    }

    #[test]
    fn protected_user_speaker_is_not_overwritten() {
        let mut current = final_event("session-protected", "same text");
        current.speaker = Some(SpeakerMetadata {
            speaker_id: "speaker-user".to_string(),
            local_label: None,
            display_name: Some("User name".to_string()),
            confidence: None,
            status: SpeakerStatus::Renamed,
        });
        let rendered = RenderedAudioWindow {
            start_frame: 0,
            end_frame: 48_000,
            samples: vec![0.0; 16_000],
        };
        let pending = PendingWindow {
            job_id: "job-protected".to_string(),
            window_id: "window-protected".to_string(),
            result_revision_id: "result-protected".to_string(),
            base_events: vec![current.clone()],
            rendered,
        };
        let response = MossWorkerResponse {
            schema: 1,
            job: "job-protected".to_string(),
            session: "session-protected".to_string(),
            window_start_frame: 0,
            window_end_frame: 48_000,
            segments: vec![MossSegment {
                start_frame: 0,
                end_frame: 48_000,
                speaker: "S99".to_string(),
                text: "same text".to_string(),
            }],
        };
        let mut latest = HashMap::from([(current.utterance_id.clone(), current)]);
        let mut observations = Vec::new();
        let mut next_speaker = 1;
        let events = build_canonical_corrections(
            &DiarizationSessionConfig::new(
                "session-protected",
                PathBuf::from("D:/fixture"),
                "fixture-model-v1",
            ),
            &pending,
            &response,
            12,
            &mut latest,
            &mut observations,
            &mut next_speaker,
        );
        assert!(
            events.is_empty(),
            "model must not replace a renamed speaker"
        );
    }
}
