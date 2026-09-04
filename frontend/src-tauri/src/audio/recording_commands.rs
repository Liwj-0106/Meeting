// audio/recording_commands.rs
//
// Slim Tauri command layer for recording functionality.
// Delegates to transcription and recording modules for actual implementation.

use anyhow::Result;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::database::{
    models::{StreamingLatencyMode, TranscriptProvider},
    repositories::{
        recording_session_binding::RecordingSessionBindingRepository,
        setting::{SettingsRepository, TranscriptApiKey},
    },
};

use super::diarization::{DiarizationSessionIngress, DiarizationSessionRuntime};
use super::recording_saver::{
    validate_recording_folder, TranscriptPersistenceSink, TranscriptSegment,
};

use super::timeline_event::{
    activate_audio_timeline_session, active_audio_timeline_session, active_audio_timeline_snapshot,
    deactivate_audio_timeline_session, AudioEventSeverity, AudioTimelineEvent,
    AudioTimelineEventDraft, AudioTimelineEventKind, AudioTimelineSession, AudioTrack,
    AudioTrackState,
};
use super::{
    default_input_device,  // Get default microphone
    default_output_device, // Get default system audio
    parse_audio_device,
    AudioStreamStartupFailure,
    DeviceEvent,
    DeviceMonitorType,
    RecordingManager,
};

// Import transcription modules
use super::transcription::streaming::{
    build_deepgram_url, build_openai_realtime_url, run_deepgram_supervisor, run_openai_supervisor,
    AsrHealthEvent, DeepgramOptions, DeepgramSupervisorConfig, DeepgramSupervisorOutput,
    OpenAiRealtimeDelay, OpenAiRealtimeOptions, OpenAiRealtimeTurnDetection,
    OpenAiSupervisorConfig, OpenAiSupervisorOutput, SharedAsrHealthState, StreamingAsrCommand,
    StreamingTranscriptContext,
};
use super::transcription::{self, reset_speech_detected_flag};

/// Explicit cross-platform protocol value used by the device selectors.
/// `None` means "use the current system default"; this value means "do not
/// create a stream for this source".
const DISABLED_AUDIO_DEVICE: &str = "disabled";

fn is_disabled_audio_device(device_name: &str) -> bool {
    device_name.eq_ignore_ascii_case(DISABLED_AUDIO_DEVICE)
}

// Re-export TranscriptUpdate for backward compatibility
pub use super::transcription::TranscriptUpdate;

// ============================================================================
// GLOBAL STATE
// ============================================================================

// Simple recording state tracking
static IS_RECORDING: AtomicBool = AtomicBool::new(false);

// Global recording manager and transcription task to keep them alive during recording
static RECORDING_MANAGER: Mutex<Option<RecordingManager>> = Mutex::new(None);
static TRANSCRIPTION_TASK: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
static ACTIVE_TRANSCRIPTION_PROVIDER: Mutex<Option<TranscriptProvider>> = Mutex::new(None);
static ACTIVE_ASR_HEALTH: Mutex<Option<SharedAsrHealthState>> = Mutex::new(None);
static ACTIVE_DIARIZATION_RUNTIME: Mutex<Option<DiarizationSessionRuntime>> = Mutex::new(None);

const STREAMING_ASR_QUEUE_CAPACITY: usize = 640;

// Serializes operations that temporarily take ownership of RECORDING_MANAGER.
// Unlike the std mutex above, this guard is safe to hold across the async
// device rebuild and shutdown operations. The std guard is always released
// immediately after take/restore.
static RECORDING_MANAGER_OPERATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct AudioHealthMonitorTask {
    session_id: String,
    cancellation: CancellationToken,
    handle: JoinHandle<()>,
}

static AUDIO_HEALTH_MONITOR_TASK: Mutex<Option<AudioHealthMonitorTask>> = Mutex::new(None);

enum PreparedTranscriptionMode {
    Local(TranscriptProvider),
    Deepgram {
        options: DeepgramOptions,
        api_key: TranscriptApiKey,
    },
    OpenAi {
        options: OpenAiRealtimeOptions,
        api_key: TranscriptApiKey,
    },
}

struct PreparedTranscriptionSession {
    mode: PreparedTranscriptionMode,
    streaming_receiver: Option<mpsc::Receiver<StreamingAsrCommand>>,
}

impl PreparedTranscriptionSession {
    fn provider(&self) -> TranscriptProvider {
        match &self.mode {
            PreparedTranscriptionMode::Local(provider) => *provider,
            PreparedTranscriptionMode::Deepgram { .. } => TranscriptProvider::Deepgram,
            PreparedTranscriptionMode::OpenAi { .. } => TranscriptProvider::OpenAi,
        }
    }

    fn attach_streaming_ingress(&mut self, manager: &mut RecordingManager) {
        if matches!(
            self.mode,
            PreparedTranscriptionMode::Deepgram { .. } | PreparedTranscriptionMode::OpenAi { .. }
        ) {
            let (sender, receiver) = mpsc::channel(STREAMING_ASR_QUEUE_CAPACITY);
            manager.set_streaming_asr_sender(sender);
            self.streaming_receiver = Some(receiver);
        }
    }
}

fn latency_endpointing(mode: StreamingLatencyMode) -> (u32, Option<u32>) {
    match mode {
        StreamingLatencyMode::Minimal => (100, Some(1_000)),
        StreamingLatencyMode::Low => (200, Some(1_000)),
        StreamingLatencyMode::Balanced => (300, Some(1_200)),
        StreamingLatencyMode::High => (700, Some(2_000)),
    }
}

fn openai_realtime_delay(mode: StreamingLatencyMode) -> OpenAiRealtimeDelay {
    match mode {
        StreamingLatencyMode::Minimal => OpenAiRealtimeDelay::Minimal,
        StreamingLatencyMode::Low => OpenAiRealtimeDelay::Low,
        StreamingLatencyMode::Balanced => OpenAiRealtimeDelay::Medium,
        StreamingLatencyMode::High => OpenAiRealtimeDelay::High,
    }
}

async fn prepare_transcription_session<R: Runtime>(
    app: &AppHandle<R>,
) -> Result<PreparedTranscriptionSession, String> {
    let state = app.state::<crate::state::AppState>();
    let runtime = SettingsRepository::get_transcript_runtime_config(state.db_manager.pool())
        .await
        .map_err(|error| format!("Failed to load transcription configuration: {error}"))?;

    let mode = match runtime.provider {
        TranscriptProvider::LocalWhisper | TranscriptProvider::Parakeet => {
            transcription::validate_transcription_model_ready(app).await?;
            PreparedTranscriptionMode::Local(runtime.provider)
        }
        TranscriptProvider::Deepgram => {
            let provider = runtime.streaming_config.providers.deepgram;
            let (endpointing_ms, utterance_end_ms) = latency_endpointing(provider.latency_mode);
            let options = DeepgramOptions {
                endpoint_override: provider.endpoint_override,
                model: provider.model,
                language: provider.language,
                interim_results: true,
                endpointing_ms,
                utterance_end_ms,
                diarize: provider.diarization,
                keyterms: provider.keywords,
            };
            build_deepgram_url(&options)
                .map_err(|error| format!("Deepgram 流式转写配置无效：{error}"))?;
            let api_key = runtime.api_key.ok_or_else(|| {
                "尚未配置 Deepgram API 密钥，请先在转写设置中保存密钥。".to_string()
            })?;
            PreparedTranscriptionMode::Deepgram { options, api_key }
        }
        TranscriptProvider::OpenAi => {
            let provider = runtime.streaming_config.providers.openai;
            let languages = if provider.language.eq_ignore_ascii_case("auto") {
                Vec::new()
            } else {
                vec![provider.language]
            };
            let options = OpenAiRealtimeOptions {
                endpoint_override: provider.endpoint_override,
                model: provider.model,
                prompt: None,
                languages,
                keywords: provider.keywords,
                delay: Some(openai_realtime_delay(provider.latency_mode)),
                // The canonical local VAD emits explicit Commit commands. Keeping
                // provider VAD disabled prevents two endpointing clocks from
                // racing and assigning one item to different audio ranges.
                turn_detection: OpenAiRealtimeTurnDetection::Disabled,
            };
            build_openai_realtime_url(&options)
                .map_err(|error| format!("OpenAI Realtime 流式转写配置无效：{error}"))?;
            let api_key = runtime.api_key.ok_or_else(|| {
                "尚未配置 OpenAI API 密钥，请先在转写设置中保存密钥。".to_string()
            })?;
            PreparedTranscriptionMode::OpenAi { options, api_key }
        }
    };

    Ok(PreparedTranscriptionSession {
        mode,
        streaming_receiver: None,
    })
}

fn replace_active_asr_health(health: Option<SharedAsrHealthState>) {
    let mut active = ACTIVE_ASR_HEALTH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *active = health;
}

fn set_active_transcription_provider(provider: Option<TranscriptProvider>) {
    let mut active = ACTIVE_TRANSCRIPTION_PROVIDER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *active = provider;
}

fn take_active_transcription_provider() -> Option<TranscriptProvider> {
    ACTIVE_TRANSCRIPTION_PROVIDER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
}

fn start_active_diarization_runtime<R: Runtime>(
    app: &AppHandle<R>,
    meeting_folder: Option<PathBuf>,
    session_id: &str,
    transcript_sink: TranscriptPersistenceSink,
) -> Option<DiarizationSessionIngress> {
    if let Some(previous) = ACTIVE_DIARIZATION_RUNTIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        previous.shutdown_in_background();
    }

    let Some(meeting_folder) = meeting_folder else {
        let _ = app.emit(
            "diarization-status",
            serde_json::json!({
                "availability": "unavailable",
                "error_code": "audio_unavailable",
                "session_id": session_id,
            }),
        );
        return None;
    };
    match DiarizationSessionRuntime::start_portable(
        app.clone(),
        meeting_folder,
        session_id.to_string(),
        transcript_sink,
    ) {
        Ok(runtime) => {
            let ingress = runtime.ingress();
            let _ = app.emit("diarization-status", runtime.status());
            *ACTIVE_DIARIZATION_RUNTIME
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(runtime);
            Some(ingress)
        }
        Err(error) => {
            warn!("Diarization runtime is unavailable: {}", error);
            let _ = app.emit(
                "diarization-status",
                serde_json::json!({
                    "availability": "unavailable",
                    "error_code": "invalid_configuration",
                    "session_id": session_id,
                }),
            );
            None
        }
    }
}

fn stop_active_diarization_runtime() {
    if let Some(runtime) = ACTIVE_DIARIZATION_RUNTIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        // Cleanup is bounded and detached. Recording stop never waits for a
        // model result, while the runtime-owned sink has already persisted any
        // correction emitted before cancellation.
        runtime.shutdown_in_background();
    }
}

async fn drain_unused_local_transcription(
    mut receiver: mpsc::UnboundedReceiver<super::recording_state::AudioChunk>,
    diarization_ingress: Option<DiarizationSessionIngress>,
) {
    while let Some(chunk) = receiver.recv().await {
        if let Some(ingress) = &diarization_ingress {
            let _ = ingress.try_push_audio_chunk(chunk);
        }
    }
}

async fn persist_asr_health_event(meeting_folder: &Option<PathBuf>, event: &AsrHealthEvent) {
    let Some(folder) = meeting_folder else {
        return;
    };
    let path = folder.join("asr-events.ndjson");
    let mut line = match serde_json::to_vec(event) {
        Ok(line) => line,
        Err(error) => {
            warn!("Failed to encode ASR health event: {}", error);
            return;
        }
    };
    line.push(b'\n');
    match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
    {
        Ok(mut file) => {
            if let Err(error) = file.write_all(&line).await {
                warn!("Failed to persist ASR health event: {}", error);
            }
        }
        Err(error) => warn!("Failed to open ASR health timeline: {}", error),
    }
}

async fn forward_deepgram_outputs<R: Runtime>(
    app: AppHandle<R>,
    mut outputs: mpsc::UnboundedReceiver<DeepgramSupervisorOutput>,
    meeting_folder: Option<PathBuf>,
    transcript_sink: TranscriptPersistenceSink,
    diarization_ingress: Option<DiarizationSessionIngress>,
) {
    while let Some(output) = outputs.recv().await {
        match output {
            DeepgramSupervisorOutput::Transcript(update) => {
                transcript_sink.add_transcript_segment(TranscriptSegment::from(update.clone()));
                if let Some(ingress) = &diarization_ingress {
                    let _ = ingress.try_push_transcript_update(&update);
                }
                if let Err(error) = app.emit("transcript-update", &update) {
                    warn!("Failed to emit streaming transcript update: {}", error);
                }
            }
            DeepgramSupervisorOutput::Health(event) => {
                if let Err(error) = app.emit("asr-health-update", &event) {
                    warn!("Failed to emit ASR health update: {}", error);
                }
                persist_asr_health_event(&meeting_folder, &event).await;
            }
        }
    }
}

async fn forward_openai_outputs<R: Runtime>(
    app: AppHandle<R>,
    mut outputs: mpsc::UnboundedReceiver<OpenAiSupervisorOutput>,
    meeting_folder: Option<PathBuf>,
    transcript_sink: TranscriptPersistenceSink,
    diarization_ingress: Option<DiarizationSessionIngress>,
) {
    while let Some(output) = outputs.recv().await {
        match output {
            OpenAiSupervisorOutput::Transcript(update) => {
                transcript_sink.add_transcript_segment(TranscriptSegment::from(update.clone()));
                if let Some(ingress) = &diarization_ingress {
                    let _ = ingress.try_push_transcript_update(&update);
                }
                if let Err(error) = app.emit("transcript-update", &update) {
                    warn!("Failed to emit streaming transcript update: {}", error);
                }
            }
            OpenAiSupervisorOutput::Health(event) => {
                if let Err(error) = app.emit("asr-health-update", &event) {
                    warn!("Failed to emit ASR health update: {}", error);
                }
                persist_asr_health_event(&meeting_folder, &event).await;
            }
        }
    }
}

fn start_prepared_transcription_task<R: Runtime>(
    app: AppHandle<R>,
    prepared: PreparedTranscriptionSession,
    local_receiver: mpsc::UnboundedReceiver<super::recording_state::AudioChunk>,
    meeting_folder: Option<PathBuf>,
    session_id: String,
    transcript_sink: TranscriptPersistenceSink,
    diarization_ingress: Option<DiarizationSessionIngress>,
) -> JoinHandle<()> {
    match prepared.mode {
        PreparedTranscriptionMode::Local(_) => {
            replace_active_asr_health(None);
            transcription::start_transcription_task(
                app,
                local_receiver,
                session_id,
                transcript_sink,
                diarization_ingress,
            )
        }
        PreparedTranscriptionMode::Deepgram { options, api_key } => {
            let streaming_receiver = prepared
                .streaming_receiver
                .expect("streaming ingress must be attached before recording starts");
            let health = SharedAsrHealthState::default();
            replace_active_asr_health(Some(health.clone()));
            let transcript_context = StreamingTranscriptContext {
                meeting_id: None,
                session_id: Some(session_id.clone()),
                default_language: if options.language == "auto" {
                    None
                } else {
                    Some(options.language.clone())
                },
                trace_id: Some(session_id.clone()),
            };
            let config = DeepgramSupervisorConfig::new(
                session_id,
                options,
                transcript_context,
                0,
                api_key.expose_secret(),
            );

            tokio::spawn(async move {
                let (output_sender, output_receiver) = mpsc::unbounded_channel();
                let supervisor =
                    run_deepgram_supervisor(config, streaming_receiver, output_sender, health);
                let forwarder = forward_deepgram_outputs(
                    app,
                    output_receiver,
                    meeting_folder,
                    transcript_sink,
                    diarization_ingress.clone(),
                );
                let local_drain =
                    drain_unused_local_transcription(local_receiver, diarization_ingress);
                let (exit, (), ()) = tokio::join!(supervisor, forwarder, local_drain);
                info!("Deepgram streaming supervisor exited: {:?}", exit);
            })
        }
        PreparedTranscriptionMode::OpenAi { options, api_key } => {
            let streaming_receiver = prepared
                .streaming_receiver
                .expect("streaming ingress must be attached before recording starts");
            let health = SharedAsrHealthState::default();
            replace_active_asr_health(Some(health.clone()));
            let transcript_context = StreamingTranscriptContext {
                meeting_id: None,
                session_id: Some(session_id.clone()),
                default_language: if options.languages.len() == 1 {
                    options.languages.first().cloned()
                } else {
                    None
                },
                trace_id: Some(session_id.clone()),
            };
            let config = OpenAiSupervisorConfig::new(
                session_id,
                options,
                transcript_context,
                0,
                api_key.expose_secret(),
            );

            tokio::spawn(async move {
                let (output_sender, output_receiver) = mpsc::unbounded_channel();
                let supervisor =
                    run_openai_supervisor(config, streaming_receiver, output_sender, health);
                let forwarder = forward_openai_outputs(
                    app,
                    output_receiver,
                    meeting_folder,
                    transcript_sink,
                    diarization_ingress.clone(),
                );
                let local_drain =
                    drain_unused_local_transcription(local_receiver, diarization_ingress);
                let (exit, (), ()) = tokio::join!(supervisor, forwarder, local_drain);
                info!("OpenAI Realtime streaming supervisor exited: {:?}", exit);
            })
        }
    }
}

#[tauri::command]
pub async fn get_streaming_asr_health_snapshot() -> Vec<AsrHealthEvent> {
    ACTIVE_ASR_HEALTH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .map(SharedAsrHealthState::event_snapshot)
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
struct AudioSourceConfiguration {
    microphone_label: Option<String>,
    system_label: Option<String>,
    startup_failures: Vec<AudioStreamStartupFailure>,
}

impl AudioSourceConfiguration {
    fn detail(&self) -> String {
        match (&self.microphone_label, &self.system_label) {
            (Some(_), Some(_)) => "Microphone and system audio capture are active".to_string(),
            (Some(_), None) => "Microphone-only capture is active".to_string(),
            (None, Some(_)) => "System-audio-only capture is active".to_string(),
            (None, None) => "No audio capture route is active".to_string(),
        }
    }
}

fn lock_recording_manager() -> std::sync::MutexGuard<'static, Option<RecordingManager>> {
    RECORDING_MANAGER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Cancellation-safe temporary ownership of the global manager.
///
/// Tokio timeout/select may drop a reconnect future at any suspension point.
/// Restoring in Drop prevents that cancellation from dropping the only live
/// manager and its still-healthy capture route.
struct RecordingManagerLease {
    manager: Option<RecordingManager>,
}

impl RecordingManagerLease {
    fn take() -> Option<Self> {
        let manager = {
            let mut manager_guard = lock_recording_manager();
            manager_guard.take()
        }?;
        Some(Self {
            manager: Some(manager),
        })
    }

    fn manager_mut(&mut self) -> &mut RecordingManager {
        self.manager
            .as_mut()
            .expect("recording manager lease must own its manager")
    }
}

impl Drop for RecordingManagerLease {
    fn drop(&mut self) {
        let Some(manager) = self.manager.take() else {
            return;
        };
        let mut manager_guard = lock_recording_manager();
        if manager_guard.is_none() {
            *manager_guard = Some(manager);
        } else {
            error!("Recording manager lease found an unexpected replacement; preserving the registered manager");
        }
    }
}

fn publish_audio_health_event<R: Runtime>(
    app: &AppHandle<R>,
    session: &Arc<AudioTimelineSession>,
    draft: AudioTimelineEventDraft,
) -> Result<Option<AudioTimelineEvent>, String> {
    let event = session
        .record(draft)
        .map_err(|error| format!("Failed to persist audio source health: {error}"))?;
    if let Some(ref event) = event {
        app.emit("audio-source-health", event)
            .map_err(|error| format!("Failed to emit audio source health: {error}"))?;
    }
    Ok(event)
}

fn emit_pending_audio_health_events<R: Runtime>(
    app: &AppHandle<R>,
    session: &Arc<AudioTimelineSession>,
    maximum: usize,
) {
    let pending = match session.drain_pending_events(maximum) {
        Ok(events) => events,
        Err(error) => {
            warn!("Failed to drain persisted audio health events: {}", error);
            return;
        }
    };
    for event in pending {
        if let Err(error) = app.emit("audio-source-health", &event) {
            warn!(
                "Failed to emit persisted audio health event {}: {}",
                event.event_id, error
            );
        }
    }
}

fn prepare_audio_health_session(
    manager: &RecordingManager,
) -> Result<Arc<AudioTimelineSession>, String> {
    let meeting_folder = manager
        .get_meeting_folder()
        .ok_or_else(|| "Meeting folder was not initialized for the audio timeline".to_string())?;
    let session_id = manager.get_state().get_session_id();
    if session_id.is_empty() {
        return Err("Recording session ID was not initialized".to_string());
    }

    let (session, warnings) = AudioTimelineSession::open(meeting_folder, session_id)
        .map_err(|error| format!("Failed to open audio timeline: {error}"))?;
    for warning in warnings {
        warn!(
            "Audio timeline recovery warning at line {} ({}): {}",
            warning.line, warning.code, warning.detail
        );
    }
    Ok(session)
}

async fn register_recording_start_anchor<R: Runtime>(
    app: &AppHandle<R>,
    source_session_id: &str,
    meeting_folder: Option<&std::path::Path>,
) -> Result<(), String> {
    let meeting_folder = meeting_folder
        .ok_or_else(|| "Recording folder was not initialized for crash recovery".to_string())?;
    let canonical_folder = validate_recording_folder(meeting_folder)?;
    let folder_hash = RecordingSessionBindingRepository::recording_folder_hash(&canonical_folder)
        .map_err(|error| format!("Failed to correlate recording folder: {error}"))?;
    let state = app.state::<crate::state::AppState>();
    RecordingSessionBindingRepository::ensure_pending(
        state.db_manager.pool(),
        source_session_id,
        &folder_hash,
    )
    .await
    .map_err(|error| format!("Failed to persist recording recovery anchor: {error}"))?;
    Ok(())
}

async fn reject_unanchored_recording_start(
    manager: &mut RecordingManager,
    internal_error: String,
) -> String {
    error!("Recording start recovery anchor failed: {internal_error}");
    let rollback_warning = manager
        .rollback_started_recording("Required crash-recovery anchor could not be persisted")
        .await
        .err();
    match rollback_warning {
        Some(warning) => {
            error!("Recording start rollback warning: {warning}");
            "录音未启动：无法持久化崩溃恢复状态，且启动回滚未完整完成。请检查存储后重试。"
                .to_string()
        }
        None => "录音未启动：无法持久化崩溃恢复状态。请检查存储后重试。".to_string(),
    }
}

fn initial_audio_health_drafts(sources: &AudioSourceConfiguration) -> Vec<AudioTimelineEventDraft> {
    let mut drafts = Vec::with_capacity(6 + sources.startup_failures.len());
    let mut session_started = AudioTimelineEventDraft::new(
        AudioTrack::Mixed,
        AudioTimelineEventKind::SessionStarted,
        AudioTrackState::Starting,
        0,
    );
    session_started.code = "audio_session_started".to_string();
    session_started.detail = Some(sources.detail());
    drafts.push(session_started);

    if let Some(device_label) = sources.microphone_label.clone() {
        let mut configured = AudioTimelineEventDraft::new(
            AudioTrack::Microphone,
            AudioTimelineEventKind::TrackConfigured,
            AudioTrackState::Ready,
            0,
        );
        configured.code = "audio_track_configured".to_string();
        configured.device_label = Some(device_label.clone());
        drafts.push(configured);

        let mut healthy = AudioTimelineEventDraft::new(
            AudioTrack::Microphone,
            AudioTimelineEventKind::StateChanged,
            AudioTrackState::Healthy,
            0,
        );
        healthy.code = "audio_track_healthy".to_string();
        healthy.device_label = Some(device_label);
        drafts.push(healthy);
    }

    if let Some(device_label) = sources.system_label.clone() {
        let mut configured = AudioTimelineEventDraft::new(
            AudioTrack::System,
            AudioTimelineEventKind::TrackConfigured,
            AudioTrackState::Ready,
            0,
        );
        configured.code = "audio_track_configured".to_string();
        configured.device_label = Some(device_label.clone());
        drafts.push(configured);

        let mut healthy = AudioTimelineEventDraft::new(
            AudioTrack::System,
            AudioTimelineEventKind::StateChanged,
            AudioTrackState::Healthy,
            0,
        );
        healthy.code = "audio_track_healthy".to_string();
        healthy.device_label = Some(device_label);
        drafts.push(healthy);
    }

    for failure in &sources.startup_failures {
        let track = match failure.device_type {
            super::RecordingDeviceType::Microphone => AudioTrack::Microphone,
            super::RecordingDeviceType::System => AudioTrack::System,
            super::RecordingDeviceType::Mixed => AudioTrack::Mixed,
        };
        let mut interrupted = AudioTimelineEventDraft::new(
            track,
            AudioTimelineEventKind::StreamInterrupted,
            AudioTrackState::Degraded,
            0,
        );
        interrupted.severity = AudioEventSeverity::Warning;
        interrupted.code = "audio_stream_start_failed".to_string();
        interrupted.device_label = Some(failure.device_label.clone());
        interrupted.detail = Some(format!(
            "The selected capture route could not start; the healthy route remains active: {}",
            failure.detail
        ));
        drafts.push(interrupted);
    }

    let mut mixed_healthy = AudioTimelineEventDraft::new(
        AudioTrack::Mixed,
        AudioTimelineEventKind::StateChanged,
        AudioTrackState::Healthy,
        0,
    );
    mixed_healthy.code = "audio_track_healthy".to_string();
    mixed_healthy.detail = Some("Synchronized transcription mix is active".to_string());
    drafts.push(mixed_healthy);
    drafts
}

fn emit_initial_audio_health<R: Runtime>(
    app: &AppHandle<R>,
    session: &Arc<AudioTimelineSession>,
    sources: &AudioSourceConfiguration,
) -> Result<(), String> {
    for draft in initial_audio_health_drafts(sources) {
        publish_audio_health_event(app, session, draft)?;
    }
    Ok(())
}

fn monitor_track(device_type: &DeviceMonitorType) -> AudioTrack {
    match device_type {
        DeviceMonitorType::Microphone => AudioTrack::Microphone,
        DeviceMonitorType::SystemAudio => AudioTrack::System,
    }
}

enum ProcessedDeviceEvent {
    Lost {
        device_name: String,
        device_type: DeviceMonitorType,
        at_frame: u64,
    },
    Recovered {
        device_name: String,
        device_type: DeviceMonitorType,
        at_frame: u64,
    },
    RecoveryFailed {
        device_name: String,
        device_type: DeviceMonitorType,
        at_frame: u64,
        detail: String,
        exhausted: bool,
    },
}

async fn process_device_event(event: DeviceEvent) -> Option<ProcessedDeviceEvent> {
    let _operation_guard = RECORDING_MANAGER_OPERATION.lock().await;
    let mut manager_lease = RecordingManagerLease::take()?;
    let manager = manager_lease.manager_mut();

    let result = match event {
        DeviceEvent::DeviceDisconnected {
            device_name,
            device_type,
        } => {
            manager
                .handle_device_disconnect(device_name.clone(), device_type.clone())
                .await;
            ProcessedDeviceEvent::Lost {
                device_name,
                device_type,
                at_frame: manager.get_state().get_media_end_frame(),
            }
        }
        DeviceEvent::DeviceReconnected {
            device_name,
            device_type,
        } => {
            let reconnect_result = manager
                .handle_device_reconnect(device_name.clone(), device_type.clone())
                .await;
            let at_frame = manager.get_state().get_media_end_frame();
            match reconnect_result {
                Ok(()) => ProcessedDeviceEvent::Recovered {
                    device_name,
                    device_type,
                    at_frame,
                },
                Err(error) => ProcessedDeviceEvent::RecoveryFailed {
                    device_name,
                    device_type,
                    at_frame,
                    detail: error.to_string(),
                    exhausted: false,
                },
            }
        }
        DeviceEvent::DeviceListChanged => return None,
    };
    Some(result)
}

fn emit_processed_device_event<R: Runtime>(
    app: &AppHandle<R>,
    session: &Arc<AudioTimelineSession>,
    event: ProcessedDeviceEvent,
    attempt: Option<u32>,
) {
    let mut draft = match event {
        ProcessedDeviceEvent::Lost {
            device_name,
            device_type,
            at_frame,
        } => {
            let mut draft = AudioTimelineEventDraft::new(
                monitor_track(&device_type),
                AudioTimelineEventKind::DeviceLost,
                AudioTrackState::Interrupted,
                at_frame,
            );
            draft.severity = AudioEventSeverity::Warning;
            draft.code = "audio_device_lost".to_string();
            draft.device_label = Some(device_name);
            draft.detail =
                Some("The capture route stopped; the other route remains active".to_string());
            draft
        }
        ProcessedDeviceEvent::Recovered {
            device_name,
            device_type,
            at_frame,
        } => {
            let mut draft = AudioTimelineEventDraft::new(
                monitor_track(&device_type),
                AudioTimelineEventKind::DeviceRecovered,
                AudioTrackState::Healthy,
                at_frame,
            );
            draft.code = "audio_device_recovered".to_string();
            draft.device_label = Some(device_name);
            draft.detail = Some(
                "The capture route was rebuilt without restarting the healthy route".to_string(),
            );
            draft
        }
        ProcessedDeviceEvent::RecoveryFailed {
            device_name,
            device_type,
            at_frame,
            detail,
            exhausted,
        } => {
            let mut draft = AudioTimelineEventDraft::new(
                monitor_track(&device_type),
                AudioTimelineEventKind::StateChanged,
                if exhausted {
                    AudioTrackState::Degraded
                } else {
                    AudioTrackState::Recovering
                },
                at_frame,
            );
            draft.severity = AudioEventSeverity::Warning;
            draft.code = if exhausted {
                "audio_device_reconnect_exhausted"
            } else {
                "audio_device_reconnect_failed"
            }
            .to_string();
            draft.device_label = Some(device_name);
            draft.detail = Some(detail);
            draft
        }
    };
    draft.attempt = attempt;

    if let Err(error) = publish_audio_health_event(app, session, draft) {
        warn!("Failed to publish device health transition: {}", error);
    }
}

fn current_media_frame() -> u64 {
    lock_recording_manager()
        .as_ref()
        .map(|manager| manager.get_state().get_media_end_frame())
        .unwrap_or(0)
}

fn emit_restart_scheduled<R: Runtime>(
    app: &AppHandle<R>,
    session: &Arc<AudioTimelineSession>,
    device_name: &str,
    device_type: &DeviceMonitorType,
    attempt: u32,
    delay: Duration,
) {
    let mut draft = AudioTimelineEventDraft::new(
        monitor_track(device_type),
        AudioTimelineEventKind::StreamRestartScheduled,
        AudioTrackState::Recovering,
        current_media_frame(),
    );
    draft.code = "audio_stream_restart_scheduled".to_string();
    draft.attempt = Some(attempt);
    draft.device_label = Some(device_name.to_string());
    draft.detail = Some(format!(
        "Capture-route rebuild attempt {attempt} scheduled after {} ms",
        delay.as_millis()
    ));
    if let Err(error) = publish_audio_health_event(app, session, draft) {
        warn!("Failed to publish stream restart schedule: {}", error);
    }
}

async fn supervise_device_event<R: Runtime>(
    app: &AppHandle<R>,
    session: Option<&Arc<AudioTimelineSession>>,
    cancellation: &CancellationToken,
    event: DeviceEvent,
) {
    const MAX_RECONNECT_ATTEMPTS: u32 = 4;
    const REBUILD_TIMEOUT: Duration = Duration::from_secs(8);

    if let DeviceEvent::DeviceReconnected {
        ref device_name,
        ref device_type,
    } = event
    {
        for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
            let backoff = if attempt == 1 {
                Duration::ZERO
            } else {
                Duration::from_millis(250_u64.saturating_mul(1_u64 << (attempt - 2)))
            };
            if let Some(session) = session {
                emit_restart_scheduled(app, session, device_name, device_type, attempt, backoff);
            }
            if !backoff.is_zero() {
                tokio::select! {
                    _ = cancellation.cancelled() => return,
                    _ = tokio::time::sleep(backoff) => {}
                }
            }

            let outcome = tokio::select! {
                _ = cancellation.cancelled() => return,
                outcome = tokio::time::timeout(REBUILD_TIMEOUT, process_device_event(event.clone())) => outcome,
            };
            let exhausted = attempt == MAX_RECONNECT_ATTEMPTS;
            match outcome {
                Ok(Some(processed @ ProcessedDeviceEvent::Recovered { .. })) => {
                    if let Some(session) = session {
                        emit_processed_device_event(app, session, processed, Some(attempt));
                    }
                    return;
                }
                Ok(Some(mut processed @ ProcessedDeviceEvent::RecoveryFailed { .. })) => {
                    if let ProcessedDeviceEvent::RecoveryFailed {
                        exhausted: event_exhausted,
                        ..
                    } = &mut processed
                    {
                        *event_exhausted = exhausted;
                    }
                    if let Some(session) = session {
                        emit_processed_device_event(app, session, processed, Some(attempt));
                    }
                }
                Ok(Some(processed)) => {
                    if let Some(session) = session {
                        emit_processed_device_event(app, session, processed, Some(attempt));
                    }
                    return;
                }
                Ok(None) => return,
                Err(_) => {
                    if let Some(session) = session {
                        emit_processed_device_event(
                            app,
                            session,
                            ProcessedDeviceEvent::RecoveryFailed {
                                device_name: device_name.clone(),
                                device_type: device_type.clone(),
                                at_frame: current_media_frame(),
                                detail: format!(
                                    "Capture-route rebuild attempt {attempt} timed out after {} seconds",
                                    REBUILD_TIMEOUT.as_secs()
                                ),
                                exhausted,
                            },
                            Some(attempt),
                        );
                    }
                }
            }
        }
        return;
    }

    let outcome = tokio::select! {
        _ = cancellation.cancelled() => return,
        outcome = tokio::time::timeout(REBUILD_TIMEOUT, process_device_event(event)) => outcome,
    };
    match outcome {
        Ok(Some(processed)) => {
            if let Some(session) = session {
                emit_processed_device_event(app, session, processed, None);
            }
        }
        Ok(None) => {}
        Err(_) => warn!("Timed out while applying an audio device event"),
    }
}

fn start_audio_health_monitor<R: Runtime>(
    app: AppHandle<R>,
    session: Option<Arc<AudioTimelineSession>>,
) -> Result<(), String> {
    let mut task_guard = AUDIO_HEALTH_MONITOR_TASK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if task_guard.is_some() {
        return Err("An audio health supervisor is already active".to_string());
    }

    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let task_session = session.clone();
    let task_session_id = session
        .as_ref()
        .map(|session| session.session_id().to_string())
        .unwrap_or_else(|| "unpersisted-session".to_string());
    let task_log_session_id = task_session_id.clone();
    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(250));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = task_cancellation.cancelled() => break,
                _ = interval.tick() => {}
            }

            if task_cancellation.is_cancelled()
                || task_session
                    .as_ref()
                    .is_some_and(|session| !session.is_accepting_events())
            {
                break;
            }

            if let Some(ref session) = task_session {
                emit_pending_audio_health_events(&app, session, 64);
            }

            let events = {
                let mut manager_guard = lock_recording_manager();
                let mut events = Vec::new();
                if let Some(manager) = manager_guard.as_mut() {
                    while events.len() < 32 {
                        match manager.poll_device_events() {
                            Some(event) => events.push(event),
                            None => break,
                        }
                    }
                }
                events
            };

            for event in events {
                if task_cancellation.is_cancelled() {
                    break;
                }
                supervise_device_event(&app, task_session.as_ref(), &task_cancellation, event)
                    .await;
            }
        }
        info!(
            "Audio health supervisor stopped for session {}",
            task_log_session_id
        );
    });

    *task_guard = Some(AudioHealthMonitorTask {
        session_id: task_session_id,
        cancellation,
        handle,
    });
    Ok(())
}

fn begin_audio_health_session<R: Runtime>(
    app: &AppHandle<R>,
    session: Option<Arc<AudioTimelineSession>>,
    sources: &AudioSourceConfiguration,
) {
    let supervisor_session =
        session.and_then(
            |session| match emit_initial_audio_health(app, &session, sources) {
                Ok(()) => match activate_audio_timeline_session(session.clone()) {
                    Ok(()) => Some(session),
                    Err(error) => {
                        warn!("Failed to activate audio timeline: {}", error);
                        session.close();
                        None
                    }
                },
                Err(error) => {
                    warn!("Failed to initialize audio source health: {}", error);
                    session.close();
                    None
                }
            },
        );

    // Device recovery remains available even if health-file creation failed.
    if let Err(error) = start_audio_health_monitor(app.clone(), supervisor_session) {
        warn!("Failed to start audio health supervisor: {}", error);
    }
}

async fn stop_audio_health_monitor() {
    let task = {
        AUDIO_HEALTH_MONITOR_TASK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    };
    if let Some(task) = task {
        task.cancellation.cancel();
        if let Err(error) = task.handle.await {
            warn!(
                "Audio health supervisor for session {} did not exit cleanly: {}",
                task.session_id, error
            );
        }
    }
}

async fn clear_stale_audio_health_session() {
    stop_audio_health_monitor().await;
    match active_audio_timeline_session() {
        Ok(Some(session)) => {
            if let Err(error) = deactivate_audio_timeline_session(session.session_id()) {
                warn!("Failed to clear stale audio timeline: {}", error);
            }
        }
        Ok(None) => {}
        Err(error) => warn!("Failed to inspect stale audio timeline: {}", error),
    }
}

fn finish_audio_health_session<R: Runtime>(
    app: &AppHandle<R>,
    session: &Arc<AudioTimelineSession>,
    at_frame: u64,
) {
    let history = match session.event_snapshot() {
        Ok(events) => events,
        Err(error) => {
            warn!(
                "Failed to snapshot source tracks before closing the audio timeline: {}",
                error
            );
            Vec::new()
        }
    };

    for draft in final_audio_health_drafts(&history, at_frame) {
        if let Err(error) = publish_audio_health_event(app, session, draft) {
            warn!(
                "Failed to publish an audio session stop boundary: {}",
                error
            );
        }
    }
    if let Err(error) = deactivate_audio_timeline_session(session.session_id()) {
        warn!("Failed to deactivate the audio timeline: {}", error);
    }
}

fn final_audio_health_drafts(
    history: &[AudioTimelineEvent],
    at_frame: u64,
) -> Vec<AudioTimelineEventDraft> {
    let mut drafts = Vec::with_capacity(3);
    for track in [AudioTrack::Microphone, AudioTrack::System] {
        let latest = history.iter().rev().find(|event| event.track == track);
        let Some(latest) = latest else {
            continue;
        };

        let mut stopped = AudioTimelineEventDraft::new(
            track,
            AudioTimelineEventKind::StateChanged,
            AudioTrackState::Stopped,
            at_frame,
        );
        stopped.code = "audio_track_stopped".to_string();
        stopped.recoverable = false;
        stopped.device_label = latest.device_label.clone();
        stopped.detail = Some("Audio capture track completed".to_string());
        drafts.push(stopped);
    }

    let mut stopped = AudioTimelineEventDraft::new(
        AudioTrack::Mixed,
        AudioTimelineEventKind::SessionStopped,
        AudioTrackState::Stopped,
        at_frame,
    );
    stopped.code = "audio_session_stopped".to_string();
    stopped.recoverable = false;
    stopped.detail = Some("Audio capture and final synchronizer drain completed".to_string());
    drafts.push(stopped);
    drafts
}

// ============================================================================
// PUBLIC TYPES
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct RecordingArgs {
    pub save_path: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct TranscriptionStatus {
    pub chunks_in_queue: usize,
    pub is_processing: bool,
    pub last_activity_ms: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingStartResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_binding_ticket: Option<crate::summary::live::RecordingSummaryBindingTicket>,
}

fn shutdown_status(warnings: &[String]) -> &'static str {
    if warnings.is_empty() {
        "completed"
    } else {
        "recoverable_error"
    }
}

fn recording_stopped_payload(
    folder_path: Option<String>,
    meeting_name: Option<String>,
    save_status: &str,
    warnings: &[String],
) -> serde_json::Value {
    let status = shutdown_status(warnings);
    let message = if status == "completed" {
        "Recording stopped; frontend can persist the completed transcript"
    } else {
        "Recording stopped with recoverable warnings; recovery data was retained"
    };

    serde_json::json!({
        "message": message,
        "folder_path": folder_path,
        "meeting_name": meeting_name,
        "status": status,
        "save_status": save_status,
        "warnings": warnings,
    })
}

// ============================================================================
// RECORDING COMMANDS
// ============================================================================

/// Start recording with default devices
pub async fn start_recording<R: Runtime>(
    app: AppHandle<R>,
) -> Result<RecordingStartResult, String> {
    start_recording_with_meeting_name(app, None).await
}

/// Start recording with default devices and optional meeting name
pub async fn start_recording_with_meeting_name<R: Runtime>(
    app: AppHandle<R>,
    meeting_name: Option<String>,
) -> Result<RecordingStartResult, String> {
    info!(
        "Starting recording with default devices (meeting_name_present={})",
        meeting_name.is_some()
    );

    // Hold the lifecycle guard through the complete public start boundary so a
    // concurrent stop cannot observe a half-installed manager/task/timeline.
    let _engine_lifecycle_guard = super::common::acquire_engine_lifecycle_lock().await;

    // Check if already recording
    let current_recording_state = IS_RECORDING.load(Ordering::SeqCst);
    info!("🔍 IS_RECORDING state check: {}", current_recording_state);
    if current_recording_state {
        return Err("Recording already in progress".to_string());
    }
    clear_stale_audio_health_session().await;
    replace_active_asr_health(None);
    set_active_transcription_provider(None);

    // Resolve the complete local or streaming transcription plan before audio
    // hardware is opened. Invalid cloud configuration therefore fails closed
    // without leaving a half-started recording behind.
    info!("🔍 Preparing transcription session before starting recording...");
    let mut prepared_transcription = match prepare_transcription_session(&app).await {
        Ok(prepared) => prepared,
        Err(validation_error) => {
            error!("Model validation failed: {}", validation_error);

            // Emit error event for frontend - actionable: false to show toast instead of modal
            // (download progress is already shown in top-right toast)
            let _ = app.emit(
                "transcription-error",
                serde_json::json!({
                    "error": &validation_error,
                    "userMessage": validation_error,
                    "actionable": false
                }),
            );

            return Err(validation_error);
        }
    };
    info!("✅ Transcription session preparation passed");

    // Async-first approach - no more blocking operations!
    info!("🚀 Starting async recording initialization");

    // Create new recording manager
    let mut manager = RecordingManager::new();
    prepared_transcription.attach_streaming_ingress(&mut manager);

    // Load recording preferences to get auto_save AND device preferences
    let (auto_save, preferred_mic_name, preferred_system_name) =
        match super::recording_preferences::load_recording_preferences(&app).await {
            Ok(prefs) => {
                info!("📋 Loaded recording preferences: auto_save={}, preferred_mic={:?}, preferred_system={:?}",
                      prefs.auto_save, prefs.preferred_mic_device, prefs.preferred_system_device);
                (
                    prefs.auto_save,
                    prefs.preferred_mic_device,
                    prefs.preferred_system_device,
                )
            }
            Err(e) => {
                warn!(
                    "Failed to load recording preferences, using defaults: {}",
                    e
                );
                (true, None, None)
            }
        };

    // ============================================================================
    // MICROPHONE DEVICE RESOLUTION: Preference → Default → Error
    // ============================================================================
    let microphone_device = match preferred_mic_name {
        Some(pref_name) if is_disabled_audio_device(&pref_name) => {
            info!("🎤 Microphone capture is disabled by preference");
            None
        }
        Some(pref_name) => {
            info!("🎤 Attempting to use preferred microphone: '{}'", pref_name);
            match parse_audio_device(&pref_name) {
                Ok(device) => {
                    info!("✅ Using preferred microphone: '{}'", device.name);
                    Some(Arc::new(device))
                }
                Err(e) => {
                    warn!(
                        "⚠️ Preferred microphone '{}' not available: {}",
                        pref_name, e
                    );
                    warn!("   Falling back to system default microphone...");
                    match default_input_device() {
                        Ok(device) => {
                            info!("✅ Using default microphone: '{}'", device.name);
                            Some(Arc::new(device))
                        }
                        Err(default_err) => {
                            warn!("⚠️ No microphone available (preferred and default both failed): {}", default_err);
                            warn!("   Recording can still continue if system audio is available");
                            None
                        }
                    }
                }
            }
        }
        None => {
            info!("🎤 No microphone preference set, using system default");
            match default_input_device() {
                Ok(device) => {
                    info!("✅ Using default microphone: '{}'", device.name);
                    Some(Arc::new(device))
                }
                Err(e) => {
                    warn!("⚠️ No default microphone available: {}", e);
                    warn!("   Recording can still continue if system audio is available");
                    None
                }
            }
        }
    };

    // ============================================================================
    // SYSTEM AUDIO DEVICE RESOLUTION: Preference → Default → None (optional)
    // ============================================================================
    let system_device = match preferred_system_name {
        Some(pref_name) if is_disabled_audio_device(&pref_name) => {
            info!("🔊 System audio capture is disabled by preference");
            None
        }
        Some(pref_name) => {
            info!(
                "🔊 Attempting to use preferred system audio: '{}'",
                pref_name
            );
            match parse_audio_device(&pref_name) {
                Ok(device) => {
                    info!("✅ Using preferred system audio: '{}'", device.name);
                    Some(Arc::new(device))
                }
                Err(e) => {
                    warn!(
                        "⚠️ Preferred system audio '{}' not available: {}",
                        pref_name, e
                    );
                    warn!("   Falling back to system default...");
                    match default_output_device() {
                        Ok(device) => {
                            info!("✅ Using default system audio: '{}'", device.name);
                            Some(Arc::new(device))
                        }
                        Err(default_err) => {
                            warn!("⚠️ No system audio available (preferred and default both failed): {}", default_err);
                            warn!("   Recording will continue with microphone only");
                            None // System audio is optional
                        }
                    }
                }
            }
        }
        None => {
            info!("🔊 No system audio preference set, using system default");
            match default_output_device() {
                Ok(device) => {
                    info!("✅ Using default system audio: '{}'", device.name);
                    Some(Arc::new(device))
                }
                Err(e) => {
                    warn!("⚠️ No default system audio available: {}", e);
                    warn!("   Recording will continue with microphone only");
                    None // System audio is optional
                }
            }
        }
    };

    if microphone_device.is_none() && system_device.is_none() {
        return Err(
            "No audio source is available. Enable a microphone or system audio device.".to_string(),
        );
    }
    // Always ensure a meeting name is set so incremental saver initializes
    let effective_meeting_name = meeting_name.clone().unwrap_or_else(|| {
        // Example: Meeting 2025-10-03_08-25-23
        let now = chrono::Local::now();
        format!("Meeting {}", now.format("%Y-%m-%d_%H-%M-%S"))
    });
    manager.set_meeting_name(Some(effective_meeting_name));

    // Set up error callback
    let app_for_error = app.clone();
    manager.set_error_callback(move |error| {
        let _ = app_for_error.emit("recording-error", error.user_message());
    });

    // Start recording with resolved devices (replaces start_recording_with_defaults_and_auto_save call)
    let transcription_receiver = manager
        .start_recording(microphone_device, system_device, auto_save)
        .await
        .map_err(|e| format!("Failed to start recording: {}", e))?;
    let transcript_sink = manager.transcript_persistence_sink();
    let audio_sources = AudioSourceConfiguration {
        microphone_label: manager
            .get_state()
            .get_microphone_device()
            .map(|device| device.name.clone()),
        system_label: manager
            .get_state()
            .get_system_device()
            .map(|device| device.name.clone()),
        startup_failures: manager.stream_startup_failures(),
    };
    let audio_health_session = match prepare_audio_health_session(&manager) {
        Ok(session) => Some(session),
        Err(error) => {
            warn!("Audio source health timeline is unavailable: {}", error);
            None
        }
    };
    let asr_session_id = manager.get_state().get_session_id();
    let meeting_folder_for_asr = manager.get_meeting_folder();
    if let Err(error) =
        register_recording_start_anchor(&app, &asr_session_id, meeting_folder_for_asr.as_deref())
            .await
    {
        return Err(reject_unanchored_recording_start(&mut manager, error).await);
    }
    let active_transcription_provider = prepared_transcription.provider();
    let diarization_ingress = start_active_diarization_runtime(
        &app,
        meeting_folder_for_asr.clone(),
        &asr_session_id,
        transcript_sink.clone(),
    );

    // Store the manager globally to keep it alive
    {
        let mut global_manager = RECORDING_MANAGER.lock().unwrap();
        *global_manager = Some(manager);
    }

    // Set recording flag and reset speech detection flag
    info!("🔍 Setting IS_RECORDING to true and resetting SPEECH_DETECTED_EMITTED");
    IS_RECORDING.store(true, Ordering::SeqCst);
    reset_speech_detected_flag(); // Reset for new recording session

    // Emit success event
    if let Err(error) = app.emit(
        "recording-started",
        serde_json::json!({
            "message": "Recording started successfully with parallel processing",
            "devices": ["Default Microphone", "Default System Audio"],
            "workers": 3
        }),
    ) {
        // The command result is also a public success signal. An event-delivery
        // failure must not return an error after resources are already live.
        warn!("Failed to emit recording-started: {}", error);
    }

    // `recording-started` is the public session boundary. Health events are
    // deliberately emitted only after it and before the monitor task can
    // publish disconnects, preserving deterministic startup order.
    begin_audio_health_session(&app, audio_health_session, &audio_sources);

    // Create the summary scope after the public start boundary, but before
    // transcription drains its first frame. A summary configuration failure
    // is deliberately non-fatal to recording.
    let summary_binding_ticket = match meeting_folder_for_asr.as_deref() {
        Some(folder) => match crate::summary::live::commands::prepare_trusted_recording_summary(
            &app,
            &asr_session_id,
            folder,
        )
        .await
        {
            Ok(ticket) => Some(ticket),
            Err(error) => {
                warn!("Live summary session was not prepared: {}", error.code);
                None
            }
        },
        None => None,
    };

    // Start transcription after the public recording boundary. Trusted Rust
    // sources persist directly before their renderer-facing notification.
    let task_handle = start_prepared_transcription_task(
        app.clone(),
        prepared_transcription,
        transcription_receiver,
        meeting_folder_for_asr,
        asr_session_id,
        transcript_sink,
        diarization_ingress,
    );
    {
        let mut global_task = TRANSCRIPTION_TASK.lock().unwrap();
        *global_task = Some(task_handle);
    }
    set_active_transcription_provider(Some(active_transcription_provider));

    // Update tray menu to reflect recording state
    crate::tray::update_tray_menu(&app);

    info!("✅ Recording started successfully with async-first approach");

    Ok(RecordingStartResult {
        summary_binding_ticket,
    })
}

/// Start recording with specific devices
pub async fn start_recording_with_devices<R: Runtime>(
    app: AppHandle<R>,
    mic_device_name: Option<String>,
    system_device_name: Option<String>,
) -> Result<RecordingStartResult, String> {
    start_recording_with_devices_and_meeting(app, mic_device_name, system_device_name, None).await
}

/// Start recording with specific devices and optional meeting name
pub async fn start_recording_with_devices_and_meeting<R: Runtime>(
    app: AppHandle<R>,
    mic_device_name: Option<String>,
    system_device_name: Option<String>,
    meeting_name: Option<String>,
) -> Result<RecordingStartResult, String> {
    info!(
        "Starting recording with specific devices: mic_selected={}, system_selected={}, meeting_name_present={}",
        mic_device_name.is_some(),
        system_device_name.is_some(),
        meeting_name.is_some()
    );

    // Hold the lifecycle guard through the complete public start boundary so a
    // concurrent stop cannot observe a half-installed manager/task/timeline.
    let _engine_lifecycle_guard = super::common::acquire_engine_lifecycle_lock().await;

    // Check if already recording
    let current_recording_state = IS_RECORDING.load(Ordering::SeqCst);
    info!("🔍 IS_RECORDING state check: {}", current_recording_state);
    if current_recording_state {
        return Err("Recording already in progress".to_string());
    }
    clear_stale_audio_health_session().await;
    replace_active_asr_health(None);
    set_active_transcription_provider(None);

    info!("🔍 Preparing transcription session before starting recording...");
    let mut prepared_transcription = match prepare_transcription_session(&app).await {
        Ok(prepared) => prepared,
        Err(validation_error) => {
            error!(
                "Transcription session preparation failed: {}",
                validation_error
            );
            let _ = app.emit(
                "transcription-error",
                serde_json::json!({
                    "error": &validation_error,
                    "userMessage": validation_error,
                    "actionable": false
                }),
            );
            return Err(validation_error);
        }
    };
    info!("✅ Transcription session preparation passed");

    // Resolve each source independently. `None` is the system default; the
    // explicit "disabled" value is the only way to omit that stream.
    let mic_device = match mic_device_name.as_deref() {
        Some(name) if is_disabled_audio_device(name) => {
            info!("🎤 Microphone capture explicitly disabled");
            None
        }
        Some(name) => {
            Some(Arc::new(parse_audio_device(name).map_err(|e| {
                format!("Invalid microphone device '{}': {}", name, e)
            })?))
        }
        None => match default_input_device() {
            Ok(device) => {
                info!("✅ Using default microphone: '{}'", device.name);
                Some(Arc::new(device))
            }
            Err(e) => {
                warn!("⚠️ No default microphone available: {}", e);
                None
            }
        },
    };

    let system_device = match system_device_name.as_deref() {
        Some(name) if is_disabled_audio_device(name) => {
            info!("🔊 System audio capture explicitly disabled");
            None
        }
        Some(name) => {
            Some(Arc::new(parse_audio_device(name).map_err(|e| {
                format!("Invalid system device '{}': {}", name, e)
            })?))
        }
        None => match default_output_device() {
            Ok(device) => {
                info!("✅ Using default system audio: '{}'", device.name);
                Some(Arc::new(device))
            }
            Err(e) => {
                warn!("⚠️ No default system audio available: {}", e);
                None
            }
        },
    };

    if mic_device.is_none() && system_device.is_none() {
        return Err(
            "No audio source is available. Enable a microphone or system audio device.".to_string(),
        );
    }
    // Async-first approach for custom devices - no more blocking operations!
    info!("🚀 Starting async recording initialization with custom devices");

    // Create new recording manager
    let mut manager = RecordingManager::new();
    prepared_transcription.attach_streaming_ingress(&mut manager);

    // Load recording preferences to check auto_save setting
    let auto_save = match super::recording_preferences::load_recording_preferences(&app).await {
        Ok(prefs) => {
            info!(
                "📋 Loaded recording preferences: auto_save={}",
                prefs.auto_save
            );
            prefs.auto_save
        }
        Err(e) => {
            warn!(
                "Failed to load recording preferences, defaulting to auto_save=true: {}",
                e
            );
            true // Default to saving if preferences can't be loaded
        }
    };

    // Always ensure a meeting name is set so incremental saver initializes
    let effective_meeting_name = meeting_name.clone().unwrap_or_else(|| {
        let now = chrono::Local::now();
        format!("Meeting {}", now.format("%Y-%m-%d_%H-%M-%S"))
    });
    manager.set_meeting_name(Some(effective_meeting_name));

    // Set up error callback
    let app_for_error = app.clone();
    manager.set_error_callback(move |error| {
        let _ = app_for_error.emit("recording-error", error.user_message());
    });

    // Start recording with specified devices and auto_save setting
    let transcription_receiver = manager
        .start_recording(mic_device, system_device, auto_save)
        .await
        .map_err(|e| format!("Failed to start recording: {}", e))?;
    let transcript_sink = manager.transcript_persistence_sink();
    let audio_sources = AudioSourceConfiguration {
        microphone_label: manager
            .get_state()
            .get_microphone_device()
            .map(|device| device.name.clone()),
        system_label: manager
            .get_state()
            .get_system_device()
            .map(|device| device.name.clone()),
        startup_failures: manager.stream_startup_failures(),
    };
    let audio_health_session = match prepare_audio_health_session(&manager) {
        Ok(session) => Some(session),
        Err(error) => {
            warn!("Audio source health timeline is unavailable: {}", error);
            None
        }
    };
    let asr_session_id = manager.get_state().get_session_id();
    let meeting_folder_for_asr = manager.get_meeting_folder();
    if let Err(error) =
        register_recording_start_anchor(&app, &asr_session_id, meeting_folder_for_asr.as_deref())
            .await
    {
        return Err(reject_unanchored_recording_start(&mut manager, error).await);
    }
    let active_transcription_provider = prepared_transcription.provider();
    let diarization_ingress = start_active_diarization_runtime(
        &app,
        meeting_folder_for_asr.clone(),
        &asr_session_id,
        transcript_sink.clone(),
    );

    // Store the manager globally to keep it alive
    {
        let mut global_manager = RECORDING_MANAGER.lock().unwrap();
        *global_manager = Some(manager);
    }

    // Set recording flag and reset speech detection flag
    info!("🔍 Setting IS_RECORDING to true and resetting SPEECH_DETECTED_EMITTED");
    IS_RECORDING.store(true, Ordering::SeqCst);
    reset_speech_detected_flag(); // Reset for new recording session

    // Emit success event
    if let Err(error) = app.emit(
        "recording-started",
        serde_json::json!({
            "message": "Recording started with custom devices and parallel processing",
            "devices": [
                mic_device_name.unwrap_or_else(|| "Default Microphone".to_string()),
                system_device_name.unwrap_or_else(|| "Default System Audio".to_string())
            ],
            "workers": 3
        }),
    ) {
        warn!("Failed to emit recording-started: {}", error);
    }

    begin_audio_health_session(&app, audio_health_session, &audio_sources);

    let summary_binding_ticket = match meeting_folder_for_asr.as_deref() {
        Some(folder) => match crate::summary::live::commands::prepare_trusted_recording_summary(
            &app,
            &asr_session_id,
            folder,
        )
        .await
        {
            Ok(ticket) => Some(ticket),
            Err(error) => {
                warn!("Live summary session was not prepared: {}", error.code);
                None
            }
        },
        None => None,
    };

    let task_handle = start_prepared_transcription_task(
        app.clone(),
        prepared_transcription,
        transcription_receiver,
        meeting_folder_for_asr,
        asr_session_id,
        transcript_sink,
        diarization_ingress,
    );
    {
        let mut global_task = TRANSCRIPTION_TASK.lock().unwrap();
        *global_task = Some(task_handle);
    }
    set_active_transcription_provider(Some(active_transcription_provider));

    // Update tray menu to reflect recording state
    crate::tray::update_tray_menu(&app);

    info!("✅ Recording started with custom devices using async-first approach");

    Ok(RecordingStartResult {
        summary_binding_ticket,
    })
}

/// Stop recording, drain pending work, and report any recoverable shutdown failure.
pub async fn stop_recording<R: Runtime>(
    app: AppHandle<R>,
    _args: RecordingArgs,
) -> Result<(), String> {
    info!("🛑 Starting bounded recording shutdown and persistence");

    // Serialize the complete stop transition with live starts and batch model
    // unloads. The guard is intentionally held until shutdown finishes.
    let _engine_lifecycle_guard = super::common::acquire_engine_lifecycle_lock().await;

    // Check if recording is active
    if !IS_RECORDING.load(Ordering::SeqCst) {
        info!("Recording was not active");
        clear_stale_audio_health_session().await;
        replace_active_asr_health(None);
        set_active_transcription_provider(None);
        return Ok(());
    }
    let active_transcription_provider = take_active_transcription_provider();

    let audio_health_session = match active_audio_timeline_session() {
        Ok(session) => session,
        Err(error) => {
            warn!("Failed to access the active audio timeline: {}", error);
            None
        }
    };
    // Stop and join the sole DeviceMonitor consumer before moving the manager.
    // Any event already being processed restores the manager first.
    stop_audio_health_monitor().await;
    let _manager_operation_guard = RECORDING_MANAGER_OPERATION.lock().await;

    // Emit shutdown progress to frontend
    let _ = app.emit(
        "recording-shutdown-progress",
        serde_json::json!({
            "stage": "stopping_audio",
            "message": "Stopping audio capture...",
            "progress": 20
        }),
    );

    // Step 1: Stop audio capture immediately (no more new chunks) with proper error handling
    let manager_for_cleanup = {
        let mut global_manager = lock_recording_manager();
        global_manager.take()
    };

    let mut shutdown_warnings = Vec::<String>::new();
    let mut manager_for_cleanup = manager_for_cleanup;

    if let Some(ref mut manager) = manager_for_cleanup {
        // Use FORCE FLUSH to immediately process all accumulated audio - eliminates 30s delay!
        info!("🚀 Using FORCE FLUSH to eliminate pipeline accumulation delays");
        match manager.stop_streams_and_force_flush().await {
            Ok(()) => {
                info!("✅ Audio capture stopped and the pipeline drain completed");
            }
            Err(error) => {
                let warning = format!("Audio capture or pipeline shutdown failed: {error}");
                error!("❌ {}", warning);
                shutdown_warnings.push(warning);
            }
        }
    } else {
        let warning = "Recording manager was unavailable during shutdown".to_string();
        warn!("{}", warning);
        shutdown_warnings.push(warning);
    }

    // Capture has been requested to stop and the manager will never be put
    // back into the active slot. Close the public flag even if a driver or
    // pipeline reported an error; subsequent cleanup works on the local owner.
    IS_RECORDING.store(false, Ordering::SeqCst);

    let final_audio_frame = manager_for_cleanup
        .as_ref()
        .map(|manager| manager.get_state().get_media_end_frame())
        .unwrap_or(0);
    if let Some(ref session) = audio_health_session {
        // force_flush_and_stop may persist a final-drain gap after the
        // supervisor was cancelled. Deliver it before the stop boundary.
        emit_pending_audio_health_events(&app, session, usize::MAX);
        finish_audio_health_session(&app, session, final_audio_frame);
    }

    // Step 2: Signal transcription workers to finish processing queued chunks
    let _ = app.emit(
        "recording-shutdown-progress",
        serde_json::json!({
            "stage": "processing_transcripts",
            "message": "Processing remaining transcript chunks...",
            "progress": 40
        }),
    );

    // Wait for the transcription task with a bounded timeout. A timeout is
    // reported as recoverable because audio/checkpoint data remains on disk.
    let transcription_task = {
        let mut global_task = TRANSCRIPTION_TASK.lock().unwrap();
        global_task.take()
    };

    if let Some(mut task_handle) = transcription_task {
        info!("⏳ Waiting for queued transcription chunks to drain");

        // Enhanced progress monitoring during shutdown
        let progress_app = app.clone();
        let progress_task = tokio::spawn(async move {
            let last_update = std::time::Instant::now();

            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                // Emit periodic progress updates during shutdown
                let elapsed = last_update.elapsed().as_secs();
                let _ = progress_app.emit(
                    "recording-shutdown-progress",
                    serde_json::json!({
                        "stage": "processing_transcripts",
                        "message": format!("Processing transcripts... ({}s elapsed)", elapsed),
                        "progress": 40,
                        "detailed": true,
                        "elapsed_seconds": elapsed
                    }),
                );
            }
        });

        // Wait up to 10 minutes for transcription completion to prevent indefinite hangs
        match tokio::time::timeout(
            tokio::time::Duration::from_secs(600), // 10 minutes max
            &mut task_handle,
        )
        .await
        {
            Ok(Ok(())) => {
                info!("✅ Transcription worker drained its queued chunks");
            }
            Ok(Err(e)) => {
                warn!("⚠️ Transcription task completed with error: {:?}", e);
                shutdown_warnings.push(format!("Transcription worker failed during shutdown: {e}"));
            }
            Err(_) => {
                warn!("⏱️ Transcription timeout (10 minutes) reached; aborting and joining the worker before model unload");
                task_handle.abort();
                if let Err(error) = task_handle.await {
                    if !error.is_cancelled() {
                        warn!("Transcription worker failed while aborting: {:?}", error);
                    }
                }
                let _ = app.emit(
                    "recording-error",
                    "Transcription did not finish before shutdown; the recording was preserved for recovery.",
                );
                shutdown_warnings
                    .push("Transcription did not finish before the shutdown timeout".to_string());
            }
        }

        // Stop progress monitoring
        progress_task.abort();
    } else {
        info!("ℹ️ No transcription task found to wait for");
    }

    // No renderer event is trusted for persistence. Once every Rust ASR source
    // has drained, cancel the optional sidecar without waiting for the model.
    stop_active_diarization_runtime();

    // Step 3: Unload the transcription model after the worker exits or is aborted
    let _ = app.emit(
        "recording-shutdown-progress",
        serde_json::json!({
            "stage": "unloading_model",
            "message": "Unloading speech recognition model...",
            "progress": 70
        }),
    );

    info!("🧠 Transcription worker has exited; unloading the transcription model");

    // Unload only the provider that actually owned this recording. Settings
    // can change while a meeting is running, and cloud sessions do not own a
    // local model that should be unloaded.
    match active_transcription_provider {
        Some(TranscriptProvider::Parakeet) => {
            info!("🦜 Unloading Parakeet model...");
            let engine_clone = {
                let engine_guard = crate::parakeet_engine::commands::PARAKEET_ENGINE
                    .lock()
                    .unwrap();
                engine_guard.as_ref().cloned()
            };

            if let Some(engine) = engine_clone {
                let current_model = engine
                    .get_current_model()
                    .await
                    .unwrap_or_else(|| "unknown".to_string());
                info!("Current Parakeet model before unload: '{}'", current_model);

                if engine.unload_model().await {
                    info!(
                        "✅ Parakeet model '{}' unloaded successfully",
                        current_model
                    );
                } else {
                    warn!("⚠️ Failed to unload Parakeet model '{}'", current_model);
                }
            } else {
                warn!("⚠️ No Parakeet engine found to unload model");
            }
        }
        Some(TranscriptProvider::LocalWhisper) => {
            info!("🎤 Unloading Whisper model...");
            let engine_clone = {
                let engine_guard = crate::whisper_engine::commands::WHISPER_ENGINE
                    .lock()
                    .unwrap();
                engine_guard.as_ref().cloned()
            };

            if let Some(engine) = engine_clone {
                let current_model = engine
                    .get_current_model()
                    .await
                    .unwrap_or_else(|| "unknown".to_string());
                info!("Current Whisper model before unload: '{}'", current_model);

                if engine.unload_model().await {
                    info!("✅ Whisper model '{}' unloaded successfully", current_model);
                } else {
                    warn!("⚠️ Failed to unload Whisper model '{}'", current_model);
                }
            } else {
                warn!("⚠️ No Whisper engine found to unload model");
            }
        }
        Some(TranscriptProvider::Deepgram | TranscriptProvider::OpenAi) => {
            info!("☁️ Online transcription session exited; no local ASR model to unload");
        }
        None => {
            warn!("⚠️ Active transcription provider was unavailable; skipping model unload");
        }
    }

    // Step 3.5: Track meeting ended analytics with privacy-safe metadata
    // Extract all data from manager BEFORE any async operations to avoid Send issues
    let analytics_data = if let Some(ref manager) = manager_for_cleanup {
        let state = manager.get_state();
        let stats = state.get_stats();

        Some((
            manager.get_recording_duration(),
            manager.get_active_recording_duration().unwrap_or(0.0),
            manager.get_total_pause_duration(),
            manager.get_transcript_segments().len() as u64,
            state.has_fatal_error(),
            state.get_microphone_device().map(|d| d.name.clone()),
            state.get_system_device().map(|d| d.name.clone()),
            stats.chunks_processed,
        ))
    } else {
        None
    };

    // Now perform async analytics tracking without holding manager reference
    if let Some((
        total_duration,
        active_duration,
        pause_duration,
        transcript_segments_count,
        had_fatal_error,
        mic_device_name,
        sys_device_name,
        chunks_processed,
    )) = analytics_data
    {
        info!("📊 Collecting analytics for meeting end");

        // Helper function to classify device type from device name (privacy-safe)
        fn classify_device_type(device_name: &str) -> &'static str {
            let name_lower = device_name.to_lowercase();
            // Check for Bluetooth keywords
            if name_lower.contains("bluetooth")
                || name_lower.contains("airpods")
                || name_lower.contains("beats")
                || name_lower.contains("headphones")
                || name_lower.contains("bt ")
                || name_lower.contains("wireless")
            {
                "Bluetooth"
            } else {
                "Wired"
            }
        }

        // Get transcription model info (already loaded above for model unload)
        let transcription_config = match crate::api::api::api_get_transcript_config(
            app.clone(),
            app.clone().state(),
            None,
        )
        .await
        {
            Ok(Some(config)) => Some((config.provider, config.model)),
            _ => None,
        };

        let (transcription_provider, transcription_model) =
            transcription_config.unwrap_or_else(|| ("unknown".to_string(), "unknown".to_string()));

        // Get summary model info from API
        let summary_config =
            match crate::api::api::api_get_model_config(app.clone(), app.clone().state(), None)
                .await
            {
                Ok(Some(config)) => Some((config.provider, config.model)),
                _ => None,
            };

        let (summary_provider, summary_model) =
            summary_config.unwrap_or_else(|| ("unknown".to_string(), "unknown".to_string()));

        // Classify device types (privacy-safe)
        let microphone_device_type = mic_device_name
            .as_ref()
            .map(|name| classify_device_type(name))
            .unwrap_or("Unknown");

        let system_audio_device_type = sys_device_name
            .as_ref()
            .map(|name| classify_device_type(name))
            .unwrap_or("Unknown");

        // Track meeting ended event with privacy-safe data
        match crate::analytics::commands::track_meeting_ended(
            transcription_provider.clone(),
            transcription_model.clone(),
            summary_provider.clone(),
            summary_model.clone(),
            total_duration,
            active_duration,
            pause_duration,
            microphone_device_type.to_string(),
            system_audio_device_type.to_string(),
            chunks_processed,
            transcript_segments_count,
            had_fatal_error,
        )
        .await
        {
            Ok(_) => info!("✅ Analytics tracked successfully for meeting end"),
            Err(e) => warn!("⚠️ Failed to track analytics: {}", e),
        }
    }

    // Step 4: Finalize recording state and cleanup resources safely
    let _ = app.emit(
        "recording-shutdown-progress",
        serde_json::json!({
            "stage": "finalizing",
            "message": "Finalizing recording and cleaning up resources...",
            "progress": 90
        }),
    );

    // Perform final cleanup with the manager if available. A save error is
    // recoverable: the manager still releases in-memory state while the saver
    // keeps any checkpoint data whose final persistence was not verified.
    let mut save_status = "recoverable_error";
    let (meeting_folder, meeting_name) = if let Some(mut manager) = manager_for_cleanup {
        info!("🧹 Performing final cleanup and saving recording data");

        // Extract meeting info BEFORE async operations
        let meeting_folder = manager.get_meeting_folder();
        let meeting_name = manager.get_meeting_name();

        match tokio::time::timeout(
            tokio::time::Duration::from_secs(300), // 5 minutes max for file I/O
            manager.save_recording_only(&app),
        )
        .await
        {
            Ok(Ok(_)) => {
                info!("✅ Recording data saved successfully during cleanup");
                save_status = "completed";
            }
            Ok(Err(e)) => {
                let warning =
                    format!("Recording persistence failed; recovery data was retained: {e}");
                warn!("⚠️ {}", warning);
                shutdown_warnings.push(warning);
            }
            Err(_) => {
                let warning =
                    "Recording persistence timed out after 5 minutes; recovery data was retained"
                        .to_string();
                warn!("⏱️ {}", warning);
                shutdown_warnings.push(warning);
            }
        }

        (meeting_folder, meeting_name)
    } else {
        info!("ℹ️ No recording manager available for cleanup");
        (None, None)
    };

    if let Some(folder) = meeting_folder.as_deref() {
        if let Err(error) =
            crate::summary::live::commands::mark_trusted_recording_summary_pending_by_folder(
                &app, folder,
            )
            .await
        {
            warn!(
                "Live summary session did not reach pending binding: {}",
                error.code
            );
        }
    }

    info!("🔍 Recording lifecycle flag is closed");

    // Step 4.5: Prepare metadata for frontend (NO database save)
    // NOTE: We do NOT save to database here. The frontend will save after all transcripts are displayed.
    // This ensures the user sees all transcripts streaming in before the database save happens.
    let (folder_path_str, meeting_name_str) = match (&meeting_folder, &meeting_name) {
        (Some(path), Some(name)) => (Some(path.to_string_lossy().to_string()), Some(name.clone())),
        _ => (None, None),
    };

    info!("📤 Preparing recording metadata for frontend save");
    info!(
        "   folder_present={}, meeting_name_present={}",
        folder_path_str.is_some(),
        meeting_name_str.is_some()
    );

    // Database save removed - frontend will handle this after receiving all transcripts
    info!("ℹ️ Skipping database save in Rust - frontend will save after all transcripts received");

    let final_status = shutdown_status(&shutdown_warnings);
    let final_message = if final_status == "completed" {
        "Recording stopped and persistence completed"
    } else {
        "Recording stopped with recoverable warnings"
    };

    if !shutdown_warnings.is_empty() {
        let error_message = format!(
            "Recording stopped with recoverable warnings: {}",
            shutdown_warnings.join("; ")
        );
        if let Err(error) = app.emit("recording-error", error_message) {
            warn!("Failed to emit recording-error after shutdown: {}", error);
        }
    }

    // Step 5: Complete the lifecycle even when persistence needs recovery.
    let _ = app.emit(
        "recording-shutdown-progress",
        serde_json::json!({
            "stage": "complete",
            "message": final_message,
            "progress": 100,
            "status": final_status,
            "save_status": save_status,
            "warnings": &shutdown_warnings,
        }),
    );

    // Preserve the legacy metadata fields while adding explicit status fields.
    // Existing frontends can continue saving the transcript; newer clients can
    // surface recovery state without parsing an English message.
    let stopped_payload = recording_stopped_payload(
        folder_path_str,
        meeting_name_str,
        save_status,
        &shutdown_warnings,
    );
    let stopped_event_result = app.emit("recording-stopped", stopped_payload);

    // Update tray menu to reflect stopped state
    crate::tray::update_tray_menu(&app);

    // The stopped health event was already emitted and persisted by the
    // supervisor. Drop only the in-memory active snapshot after the complete
    // recording boundary so a WebView reload during shutdown can still hydrate.
    replace_active_asr_health(None);
    set_active_transcription_provider(None);

    stopped_event_result.map_err(|error| error.to_string())?;

    if final_status == "completed" {
        info!("Recording shutdown and persistence completed");
    } else {
        warn!(
            "Recording shutdown completed with recoverable warnings: {}",
            shutdown_warnings.join("; ")
        );
    }
    Ok(())
}

/// Check if recording is active
pub async fn is_recording() -> bool {
    IS_RECORDING.load(Ordering::SeqCst)
}

/// Get recording statistics
pub async fn get_transcription_status() -> TranscriptionStatus {
    TranscriptionStatus {
        chunks_in_queue: 0,
        is_processing: IS_RECORDING.load(Ordering::SeqCst),
        last_activity_ms: 0,
    }
}

/// Pause the current recording
#[tauri::command]
pub async fn pause_recording<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    info!("Pausing recording");

    // Check if currently recording
    if !IS_RECORDING.load(Ordering::SeqCst) {
        return Err("No recording is currently active".to_string());
    }

    // Access the recording manager and pause it
    let manager_guard = RECORDING_MANAGER.lock().unwrap();
    if let Some(manager) = manager_guard.as_ref() {
        manager.pause_recording().map_err(|e| e.to_string())?;

        // Emit pause event to frontend
        app.emit(
            "recording-paused",
            serde_json::json!({
                "message": "Recording paused"
            }),
        )
        .map_err(|e| e.to_string())?;

        // Update tray menu to reflect paused state
        crate::tray::update_tray_menu(&app);

        info!("Recording paused successfully");
        Ok(())
    } else {
        Err("No recording manager found".to_string())
    }
}

/// Resume the current recording
#[tauri::command]
pub async fn resume_recording<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    info!("Resuming recording");

    // Check if currently recording
    if !IS_RECORDING.load(Ordering::SeqCst) {
        return Err("No recording is currently active".to_string());
    }

    // Access the recording manager and resume it
    let manager_guard = RECORDING_MANAGER.lock().unwrap();
    if let Some(manager) = manager_guard.as_ref() {
        manager.resume_recording().map_err(|e| e.to_string())?;

        // Emit resume event to frontend
        app.emit(
            "recording-resumed",
            serde_json::json!({
                "message": "Recording resumed"
            }),
        )
        .map_err(|e| e.to_string())?;

        // Update tray menu to reflect resumed state
        crate::tray::update_tray_menu(&app);

        info!("Recording resumed successfully");
        Ok(())
    } else {
        Err("No recording manager found".to_string())
    }
}

/// Check if recording is currently paused
#[tauri::command]
pub async fn is_recording_paused() -> bool {
    let manager_guard = RECORDING_MANAGER.lock().unwrap();
    if let Some(manager) = manager_guard.as_ref() {
        manager.is_paused()
    } else {
        false
    }
}

/// Get detailed recording state
#[tauri::command]
pub async fn get_recording_state() -> serde_json::Value {
    let is_recording = IS_RECORDING.load(Ordering::SeqCst);
    let manager_guard = RECORDING_MANAGER.lock().unwrap();

    if let Some(manager) = manager_guard.as_ref() {
        serde_json::json!({
            "is_recording": is_recording,
            "is_paused": manager.is_paused(),
            "is_active": manager.is_active(),
            "recording_duration": manager.get_recording_duration(),
            "active_duration": manager.get_active_recording_duration(),
            "total_pause_duration": manager.get_total_pause_duration(),
            "current_pause_duration": manager.get_current_pause_duration()
        })
    } else {
        serde_json::json!({
            "is_recording": is_recording,
            "is_paused": false,
            "is_active": false,
            "recording_duration": null,
            "active_duration": null,
            "total_pause_duration": 0.0,
            "current_pause_duration": null
        })
    }
}

/// Return the complete ordered health timeline for the one active recording.
///
/// The WebView subscribes before invoking this command. Any event concurrent
/// with the snapshot is therefore either included here or delivered live, and
/// duplicate delivery is harmless because consumers reduce by `event_id`.
#[tauri::command]
pub fn get_audio_source_health_snapshot() -> Result<Vec<AudioTimelineEvent>, String> {
    active_audio_timeline_snapshot()
        .map_err(|error| format!("Failed to snapshot audio source health: {error}"))
}

/// Get the meeting folder path for the current recording
/// Returns the path if a meeting name was set and folder structure initialized
#[tauri::command]
pub async fn get_meeting_folder_path() -> Result<Option<String>, String> {
    let manager_guard = RECORDING_MANAGER.lock().unwrap();
    if let Some(manager) = manager_guard.as_ref() {
        Ok(manager
            .get_meeting_folder()
            .map(|p| p.to_string_lossy().to_string()))
    } else {
        Ok(None)
    }
}

/// Get every accumulated transcript revision from the current recording.
/// Callers materialize the latest utterance view during deterministic replay.
#[tauri::command]
pub async fn get_transcript_history(
) -> Result<Vec<crate::audio::recording_saver::TranscriptSegment>, String> {
    let manager_guard = RECORDING_MANAGER.lock().unwrap();

    if let Some(manager) = manager_guard.as_ref() {
        Ok(manager.get_transcript_events())
    } else {
        Ok(Vec::new()) // No recording active, return empty
    }
}

/// Get meeting name from current recording session
/// Used for syncing frontend state after page reload during active recording
#[tauri::command]
pub async fn get_recording_meeting_name() -> Result<Option<String>, String> {
    let manager_guard = RECORDING_MANAGER.lock().unwrap();

    if let Some(manager) = manager_guard.as_ref() {
        Ok(manager.get_meeting_name())
    } else {
        Ok(None)
    }
}

// ============================================================================
// DEVICE MONITORING COMMANDS (AirPods/Bluetooth disconnect/reconnect support)
// ============================================================================

/// Response structure for device events
#[derive(Debug, Serialize, Clone)]
#[serde(tag = "type")]
pub enum DeviceEventResponse {
    DeviceDisconnected {
        device_name: String,
        device_type: String,
    },
    DeviceReconnected {
        device_name: String,
        device_type: String,
    },
    DeviceListChanged,
}

impl From<DeviceEvent> for DeviceEventResponse {
    fn from(event: DeviceEvent) -> Self {
        match event {
            DeviceEvent::DeviceDisconnected {
                device_name,
                device_type,
            } => DeviceEventResponse::DeviceDisconnected {
                device_name,
                device_type: format!("{:?}", device_type),
            },
            DeviceEvent::DeviceReconnected {
                device_name,
                device_type,
            } => DeviceEventResponse::DeviceReconnected {
                device_name,
                device_type: format!("{:?}", device_type),
            },
            DeviceEvent::DeviceListChanged => DeviceEventResponse::DeviceListChanged,
        }
    }
}

/// Reconnection status information
#[derive(Debug, Serialize, Clone)]
pub struct ReconnectionStatus {
    pub is_reconnecting: bool,
    pub disconnected_device: Option<DisconnectedDeviceInfo>,
}

/// Information about a disconnected device
#[derive(Debug, Serialize, Clone)]
pub struct DisconnectedDeviceInfo {
    pub name: String,
    pub device_type: String,
}

/// Poll for audio device events (disconnect/reconnect)
/// Should be called periodically (every 1-2 seconds) by frontend during recording
#[tauri::command]
pub async fn poll_audio_device_events() -> Result<Option<DeviceEventResponse>, String> {
    let mut manager_guard = RECORDING_MANAGER.lock().unwrap();

    if let Some(manager) = manager_guard.as_mut() {
        if let Some(event) = manager.poll_device_events() {
            info!("📱 Device event polled: {:?}", event);
            Ok(Some(event.into()))
        } else {
            Ok(None)
        }
    } else {
        // Not recording, no events
        Ok(None)
    }
}

/// Get current reconnection status
/// Returns whether the system is attempting to reconnect and which device
#[tauri::command]
pub async fn get_reconnection_status() -> Result<ReconnectionStatus, String> {
    let manager_guard = RECORDING_MANAGER.lock().unwrap();

    if let Some(manager) = manager_guard.as_ref() {
        let state = manager.get_state();
        let disconnected_device = state
            .get_disconnected_device()
            .map(|(device, device_type)| DisconnectedDeviceInfo {
                name: device.name.clone(),
                device_type: format!("{:?}", device_type),
            });

        Ok(ReconnectionStatus {
            is_reconnecting: manager.is_reconnecting(),
            disconnected_device,
        })
    } else {
        // Not recording, no reconnection in progress
        Ok(ReconnectionStatus {
            is_reconnecting: false,
            disconnected_device: None,
        })
    }
}

/// Get information about the active audio output device
/// Used to warn users about Bluetooth playback issues
#[tauri::command]
pub async fn get_active_audio_output() -> Result<super::playback_monitor::AudioOutputInfo, String> {
    super::playback_monitor::get_active_audio_output()
        .await
        .map_err(|e| format!("Failed to get audio output info: {}", e))
}

/// Manually trigger device reconnection attempt
/// Useful for UI "Retry" button
#[tauri::command]
pub async fn attempt_device_reconnect<R: Runtime>(
    app: AppHandle<R>,
    device_name: String,
    device_type: String,
) -> Result<bool, String> {
    // Parse device type first
    let monitor_type = match device_type.as_str() {
        "Microphone" => DeviceMonitorType::Microphone,
        "SystemAudio" => DeviceMonitorType::SystemAudio,
        _ => return Err(format!("Invalid device type: {}", device_type)),
    };

    // Serialize with supervisor/shutdown ownership transfers, then release the
    // std mutex before awaiting device enumeration and stream construction.
    let _operation_guard = RECORDING_MANAGER_OPERATION.lock().await;
    let mut manager_lease =
        RecordingManagerLease::take().ok_or_else(|| "Recording not active".to_string())?;
    let manager = manager_lease.manager_mut();

    let result = manager
        .attempt_device_reconnect(&device_name, monitor_type.clone())
        .await;
    let at_frame = manager.get_state().get_media_end_frame();

    match result {
        Ok(success) => {
            if success {
                info!("✅ Manual reconnection successful");
                manager.get_state().stop_reconnecting();
                if let Ok(Some(session)) = active_audio_timeline_session() {
                    emit_processed_device_event(
                        &app,
                        &session,
                        ProcessedDeviceEvent::Recovered {
                            device_name,
                            device_type: monitor_type,
                            at_frame,
                        },
                        Some(1),
                    );
                }
            } else {
                warn!("❌ Manual reconnection failed - device not available");
            }
            Ok(success)
        }
        Err(e) => {
            error!("Manual reconnection error: {}", e);
            Err(e.to_string())
        }
    }
}

#[cfg(test)]
mod audio_health_protocol_tests {
    use super::*;

    #[test]
    fn streaming_latency_modes_map_to_bounded_endpointing() {
        assert_eq!(
            latency_endpointing(StreamingLatencyMode::Minimal),
            (100, Some(1_000))
        );
        assert_eq!(
            latency_endpointing(StreamingLatencyMode::Low),
            (200, Some(1_000))
        );
        assert_eq!(
            latency_endpointing(StreamingLatencyMode::Balanced),
            (300, Some(1_200))
        );
        assert_eq!(
            latency_endpointing(StreamingLatencyMode::High),
            (700, Some(2_000))
        );

        assert_eq!(
            openai_realtime_delay(StreamingLatencyMode::Minimal),
            OpenAiRealtimeDelay::Minimal
        );
        assert_eq!(
            openai_realtime_delay(StreamingLatencyMode::Low),
            OpenAiRealtimeDelay::Low
        );
        assert_eq!(
            openai_realtime_delay(StreamingLatencyMode::Balanced),
            OpenAiRealtimeDelay::Medium
        );
        assert_eq!(
            openai_realtime_delay(StreamingLatencyMode::High),
            OpenAiRealtimeDelay::High
        );
    }

    #[test]
    fn stage2_shutdown_payload_keeps_legacy_fields_and_reports_success() {
        let payload = recording_stopped_payload(
            Some("D:/recordings/meeting".to_string()),
            Some("Planning".to_string()),
            "completed",
            &[],
        );

        assert_eq!(payload["folder_path"], "D:/recordings/meeting");
        assert_eq!(payload["meeting_name"], "Planning");
        assert_eq!(payload["status"], "completed");
        assert_eq!(payload["save_status"], "completed");
        assert_eq!(payload["warnings"], serde_json::json!([]));
    }

    #[test]
    fn stage2_shutdown_payload_exposes_recoverable_failures_without_hiding_metadata() {
        let warnings = vec!["pipeline drain failed".to_string()];
        let payload = recording_stopped_payload(
            Some("D:/recordings/meeting".to_string()),
            Some("Planning".to_string()),
            "recoverable_error",
            &warnings,
        );

        assert_eq!(payload["folder_path"], "D:/recordings/meeting");
        assert_eq!(payload["meeting_name"], "Planning");
        assert_eq!(payload["status"], "recoverable_error");
        assert_eq!(payload["save_status"], "recoverable_error");
        assert_eq!(payload["warnings"], serde_json::json!(warnings));
    }

    #[test]
    fn dual_source_startup_events_have_a_stable_public_order() {
        let drafts = initial_audio_health_drafts(&AudioSourceConfiguration {
            microphone_label: Some("Test microphone".to_string()),
            system_label: Some("Test system audio".to_string()),
            startup_failures: Vec::new(),
        });

        let protocol: Vec<_> = drafts
            .iter()
            .map(|draft| {
                (
                    draft.track.as_str(),
                    draft.kind.as_str(),
                    draft.state.as_str(),
                )
            })
            .collect();
        assert_eq!(
            protocol,
            vec![
                ("mixed", "session_started", "starting"),
                ("microphone", "track_configured", "ready"),
                ("microphone", "state_changed", "healthy"),
                ("system", "track_configured", "ready"),
                ("system", "state_changed", "healthy"),
                ("mixed", "state_changed", "healthy"),
            ]
        );
    }

    #[test]
    fn disabled_source_is_not_reported_as_configured() {
        let drafts = initial_audio_health_drafts(&AudioSourceConfiguration {
            microphone_label: None,
            system_label: Some("Test system audio".to_string()),
            startup_failures: Vec::new(),
        });

        assert!(drafts
            .iter()
            .all(|draft| draft.track != AudioTrack::Microphone));
        assert_eq!(drafts.first().unwrap().kind.as_str(), "session_started");
        assert_eq!(drafts.last().unwrap().track, AudioTrack::Mixed);
        assert_eq!(drafts.last().unwrap().state, AudioTrackState::Healthy);
    }

    #[test]
    fn unavailable_selected_route_is_reported_without_hiding_healthy_route() {
        let drafts = initial_audio_health_drafts(&AudioSourceConfiguration {
            microphone_label: Some("Healthy microphone".to_string()),
            system_label: None,
            startup_failures: vec![AudioStreamStartupFailure {
                device_type: crate::audio::RecordingDeviceType::System,
                device_label: "Unavailable speaker".to_string(),
                detail: "device is busy".to_string(),
            }],
        });

        assert!(drafts.iter().any(|draft| {
            draft.track == AudioTrack::Microphone && draft.state == AudioTrackState::Healthy
        }));
        assert!(drafts.iter().any(|draft| {
            draft.track == AudioTrack::System
                && draft.kind == AudioTimelineEventKind::StreamInterrupted
                && draft.state == AudioTrackState::Degraded
                && draft.code == "audio_stream_start_failed"
        }));
    }

    #[test]
    fn final_events_stop_each_seen_direct_source_before_the_mixed_session() {
        let mut old_system = AudioTimelineEvent::new(
            "test-session",
            0,
            AudioTrack::System,
            AudioTimelineEventKind::TrackConfigured,
            AudioTrackState::Ready,
            0,
        );
        old_system.device_label = Some("Old output".to_string());
        let mut microphone = AudioTimelineEvent::new(
            "test-session",
            1,
            AudioTrack::Microphone,
            AudioTimelineEventKind::StateChanged,
            AudioTrackState::Healthy,
            480,
        );
        microphone.device_label = Some("Meeting microphone".to_string());
        let mut current_system = AudioTimelineEvent::new(
            "test-session",
            2,
            AudioTrack::System,
            AudioTimelineEventKind::DeviceRecovered,
            AudioTrackState::Healthy,
            960,
        );
        current_system.device_label = Some("Current output".to_string());
        let imported = AudioTimelineEvent::new(
            "test-session",
            3,
            AudioTrack::Imported,
            AudioTimelineEventKind::StateChanged,
            AudioTrackState::Healthy,
            960,
        );

        let drafts =
            final_audio_health_drafts(&[old_system, microphone, current_system, imported], 48_000);
        assert_eq!(
            drafts
                .iter()
                .map(|draft| (
                    draft.track.as_str(),
                    draft.kind.as_str(),
                    draft.state.as_str(),
                ))
                .collect::<Vec<_>>(),
            vec![
                ("microphone", "state_changed", "stopped"),
                ("system", "state_changed", "stopped"),
                ("mixed", "session_stopped", "stopped"),
            ]
        );
        assert_eq!(
            drafts[0].device_label.as_deref(),
            Some("Meeting microphone")
        );
        assert_eq!(drafts[1].device_label.as_deref(), Some("Current output"));
        assert!(drafts.iter().all(|draft| !draft.recoverable));
    }

    #[test]
    fn final_events_do_not_invent_unseen_direct_sources() {
        let mixed = AudioTimelineEvent::new(
            "test-session",
            0,
            AudioTrack::Mixed,
            AudioTimelineEventKind::SessionStarted,
            AudioTrackState::Starting,
            0,
        );

        let drafts = final_audio_health_drafts(&[mixed], 480);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].track, AudioTrack::Mixed);
        assert_eq!(drafts[0].kind, AudioTimelineEventKind::SessionStopped);
    }
}
