//! Session supervisor for Deepgram live transcription.
//!
//! The supervisor owns reconnect/replay policy but remains independent from
//! Tauri, persistence, and recording lifecycle commands. Integration code
//! supplies the bounded audio-command receiver and forwards the typed outputs.

use super::deepgram::{
    DeepgramConnection, DeepgramControl, DeepgramError, DeepgramNetworkErrorKind, DeepgramOptions,
    DeepgramProviderError, DeepgramServerEvent,
};
use super::health::{
    AsrHealthApplyResult, AsrHealthEvent, AsrHealthEventKind, AsrHealthReducer, AsrHealthSeverity,
    AsrHealthSnapshot, AsrHealthState,
};
use super::normalizer::{
    NormalizeOutcome, StreamingTranscriptContext, StreamingTranscriptNormalizer,
};
use super::protocol::{
    ProviderTranscriptEvent, ProviderTranscriptWord, StreamingAsrCommand, StreamingAsrProvider,
    StreamingAudioFrame, STREAMING_AUDIO_SAMPLE_RATE,
};
use super::ring_buffer::{RingBufferError, StreamingAudioRingBuffer};
use crate::audio::transcription::{AudioSource, TranscriptUpdate};
use rand::random;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{self, Instant};
use uuid::Uuid;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(4);
const FINAL_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const MIN_BACKOFF_MS: u64 = 500;
const MAX_BACKOFF_MS: u64 = 8_000;
const BACKOFF_JITTER_PERCENT: i64 = 20;

/// Credentials are deliberately private and redacted from `Debug` output.
/// The value is moved directly into the actor and never enters an event or
/// transport error.
pub struct DeepgramSupervisorConfig {
    pub session_id: String,
    pub options: DeepgramOptions,
    pub transcript_context: StreamingTranscriptContext,
    pub first_transcript_sequence: u64,
    api_key: String,
}

impl DeepgramSupervisorConfig {
    pub fn new(
        session_id: impl Into<String>,
        options: DeepgramOptions,
        transcript_context: StreamingTranscriptContext,
        first_transcript_sequence: u64,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            options,
            transcript_context,
            first_transcript_sequence,
            api_key: api_key.into(),
        }
    }
}

impl fmt::Debug for DeepgramSupervisorConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeepgramSupervisorConfig")
            .field("session_id", &self.session_id)
            .field("options", &self.options)
            .field("transcript_context", &self.transcript_context)
            .field("first_transcript_sequence", &self.first_transcript_sequence)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DeepgramSupervisorOutput {
    Transcript(TranscriptUpdate),
    Health(AsrHealthEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepgramSupervisorExit {
    Stopped,
    InputClosed,
    Fatal { code: String },
}

/// Cloneable, lock-contained reducer used by command adapters to expose a
/// session health snapshot without giving them access to actor internals.
#[derive(Debug, Clone, Default)]
pub struct SharedAsrHealthState {
    reducer: Arc<Mutex<AsrHealthReducer>>,
}

impl SharedAsrHealthState {
    pub fn snapshot(&self) -> Vec<AsrHealthSnapshot> {
        lock_reducer(&self.reducer).snapshot()
    }

    pub fn snapshot_for(&self, provider: &StreamingAsrProvider) -> Option<AsrHealthSnapshot> {
        lock_reducer(&self.reducer).snapshot_for(provider).cloned()
    }

    pub fn event_history(&self) -> Vec<AsrHealthEvent> {
        lock_reducer(&self.reducer).event_history().to_vec()
    }

    /// Full revision-safe event stream used by the frontend's listener-first
    /// snapshot handshake. This intentionally returns events, not the compact
    /// per-provider presentation snapshot.
    pub fn event_snapshot(&self) -> Vec<AsrHealthEvent> {
        self.event_history()
    }

    pub(crate) fn next_sequence(&self) -> u64 {
        lock_reducer(&self.reducer)
            .last_sequence()
            .and_then(|value| value.checked_add(1))
            .unwrap_or(0)
    }

    pub(crate) fn apply(&self, event: AsrHealthEvent) -> AsrHealthApplyResult {
        lock_reducer(&self.reducer).apply(event)
    }
}

fn lock_reducer(reducer: &Mutex<AsrHealthReducer>) -> MutexGuard<'_, AsrHealthReducer> {
    reducer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run one Deepgram session until `Stop`, sender EOF, or an unrecoverable
/// provider failure. The caller should use a bounded command channel; this
/// actor keeps consuming it during reconnect backoff so recording stays
/// isolated from provider latency.
pub async fn run_deepgram_supervisor(
    config: DeepgramSupervisorConfig,
    mut commands: mpsc::Receiver<StreamingAsrCommand>,
    outputs: mpsc::UnboundedSender<DeepgramSupervisorOutput>,
    health: SharedAsrHealthState,
) -> DeepgramSupervisorExit {
    let mut actor = DeepgramSupervisor {
        health_sequence: health.next_sequence(),
        normalizer: StreamingTranscriptNormalizer::new(
            config.transcript_context.clone(),
            config.first_transcript_sequence,
        ),
        ring: StreamingAudioRingBuffer::thirty_seconds(),
        config,
        commands: &mut commands,
        outputs: &outputs,
        health: &health,
        reconnect_attempt: 0,
        latest_frame: 0,
        audio_source: AudioSource::Mixed,
        finalize_pending: false,
        finalized_since_audio: false,
    };
    actor.run().await
}

struct DeepgramSupervisor<'a> {
    config: DeepgramSupervisorConfig,
    commands: &'a mut mpsc::Receiver<StreamingAsrCommand>,
    outputs: &'a mpsc::UnboundedSender<DeepgramSupervisorOutput>,
    health: &'a SharedAsrHealthState,
    normalizer: StreamingTranscriptNormalizer,
    ring: StreamingAudioRingBuffer,
    health_sequence: u64,
    reconnect_attempt: u32,
    latest_frame: u64,
    audio_source: AudioSource,
    finalize_pending: bool,
    finalized_since_audio: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandAction {
    Continue,
    Reconnect,
    Stop,
    InputClosed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionExit {
    Reconnect,
    Stop,
    InputClosed,
    Fatal,
}

impl DeepgramSupervisor<'_> {
    async fn run(&mut self) -> DeepgramSupervisorExit {
        self.publish_health(
            AsrHealthEventKind::SessionStarting,
            AsrHealthState::Connecting,
            AsrHealthSeverity::Info,
            "deepgram_session_starting",
            true,
            None,
            0,
            None,
        );

        loop {
            match self.wait_for_audio().await {
                CommandAction::Continue => {}
                CommandAction::Stop => return self.finish_without_connection(false),
                CommandAction::InputClosed => return self.finish_without_connection(true),
                CommandAction::Reconnect => continue,
            }

            let stable_boundary = self
                .normalizer
                .last_stable_frame()
                .or_else(|| self.ring.oldest_frame())
                .unwrap_or(self.latest_frame);
            let replay = match self.ring.replay_from_stable_boundary(stable_boundary) {
                Ok(Some(replay)) => replay,
                Ok(None) => continue,
                Err(_) => {
                    self.publish_health(
                        AsrHealthEventKind::BufferOverflow,
                        AsrHealthState::Degraded,
                        AsrHealthSeverity::Warning,
                        "deepgram_replay_boundary_unavailable",
                        true,
                        None,
                        0,
                        None,
                    );
                    self.ring.clear();
                    continue;
                }
            };
            let connection_origin = replay.actual_start_frame;
            let connection_id = format!("deepgram-{}", Uuid::new_v4());
            self.publish_health(
                AsrHealthEventKind::ConnectionChanged,
                AsrHealthState::Connecting,
                AsrHealthSeverity::Info,
                "deepgram_connecting",
                true,
                Some(connection_origin),
                0,
                None,
            );

            let connect_result = time::timeout(
                CONNECT_TIMEOUT,
                DeepgramConnection::connect_with_source(
                    &self.config.options,
                    &self.config.api_key,
                    connection_id,
                    connection_origin,
                    self.audio_source.clone(),
                ),
            )
            .await;
            let mut connection = match connect_result {
                Ok(Ok(connection)) => connection,
                Ok(Err(error)) => {
                    if !self.handle_connection_error(&error) {
                        return DeepgramSupervisorExit::Fatal {
                            code: deepgram_error_code(&error).to_string(),
                        };
                    }
                    match self.reconnect_backoff().await {
                        CommandAction::Continue | CommandAction::Reconnect => continue,
                        CommandAction::Stop => return self.finish_without_connection(false),
                        CommandAction::InputClosed => return self.finish_without_connection(true),
                    }
                }
                Err(_) => {
                    self.publish_provider_failure(
                        "deepgram_connect_timeout",
                        true,
                        AsrHealthSeverity::Warning,
                    );
                    match self.reconnect_backoff().await {
                        CommandAction::Continue | CommandAction::Reconnect => continue,
                        CommandAction::Stop => return self.finish_without_connection(false),
                        CommandAction::InputClosed => return self.finish_without_connection(true),
                    }
                }
            };

            let replaying = self.reconnect_attempt > 0;
            if replaying {
                self.publish_health(
                    AsrHealthEventKind::ReplayStarted,
                    AsrHealthState::Replaying,
                    AsrHealthSeverity::Info,
                    if replay.history_truncated {
                        "deepgram_replay_history_truncated"
                    } else {
                        "deepgram_replay_started"
                    },
                    true,
                    Some(replay.actual_start_frame),
                    0,
                    None,
                );
            }

            let replay_frame = StreamingAudioFrame::new(
                0,
                replay.actual_start_frame,
                replay.samples,
                self.audio_source.clone(),
            );
            if let Err(error) = connection.send_audio(&replay_frame).await {
                if !self.handle_connection_error(&error) {
                    return DeepgramSupervisorExit::Fatal {
                        code: deepgram_error_code(&error).to_string(),
                    };
                }
                match self.reconnect_backoff().await {
                    CommandAction::Continue | CommandAction::Reconnect => continue,
                    CommandAction::Stop => return self.finish_without_connection(false),
                    CommandAction::InputClosed => return self.finish_without_connection(true),
                }
            }
            self.finalized_since_audio = false;

            self.reconnect_attempt = 0;
            self.publish_health(
                AsrHealthEventKind::ConnectionChanged,
                AsrHealthState::Streaming,
                AsrHealthSeverity::Info,
                if replaying {
                    "deepgram_replay_complete"
                } else {
                    "deepgram_streaming"
                },
                true,
                Some(replay.actual_start_frame),
                0,
                None,
            );

            if self.finalize_pending {
                if let Err(error) = connection.send_control(DeepgramControl::Finalize).await {
                    if !self.handle_connection_error(&error) {
                        return DeepgramSupervisorExit::Fatal {
                            code: deepgram_error_code(&error).to_string(),
                        };
                    }
                    match self.reconnect_backoff().await {
                        CommandAction::Continue | CommandAction::Reconnect => continue,
                        CommandAction::Stop => return self.finish_without_connection(false),
                        CommandAction::InputClosed => return self.finish_without_connection(true),
                    }
                }
                self.finalize_pending = false;
                self.finalized_since_audio = true;
            }

            match self.run_connection(&mut connection).await {
                ConnectionExit::Stop => {
                    self.graceful_close(&mut connection).await;
                    return self.finish_without_connection(false);
                }
                ConnectionExit::InputClosed => {
                    self.graceful_close(&mut connection).await;
                    return self.finish_without_connection(true);
                }
                ConnectionExit::Fatal => {
                    return DeepgramSupervisorExit::Fatal {
                        code: "deepgram_unrecoverable_failure".to_string(),
                    }
                }
                ConnectionExit::Reconnect => match self.reconnect_backoff().await {
                    CommandAction::Continue | CommandAction::Reconnect => continue,
                    CommandAction::Stop => return self.finish_without_connection(false),
                    CommandAction::InputClosed => return self.finish_without_connection(true),
                },
            }
        }
    }

    async fn wait_for_audio(&mut self) -> CommandAction {
        if self.ring.len_frames() > 0 {
            return CommandAction::Continue;
        }
        loop {
            let Some(command) = self.commands.recv().await else {
                return CommandAction::InputClosed;
            };
            match self.ingest_disconnected_command(command) {
                CommandAction::Continue if self.ring.len_frames() == 0 => continue,
                action => return action,
            }
        }
    }

    async fn run_connection(&mut self, connection: &mut DeepgramConnection) -> ConnectionExit {
        let keepalive = time::sleep(KEEPALIVE_INTERVAL);
        tokio::pin!(keepalive);
        loop {
            tokio::select! {
                biased;
                command = self.commands.recv() => {
                    let Some(command) = command else {
                        return ConnectionExit::InputClosed;
                    };
                    match command {
                        StreamingAsrCommand::Audio(frame) => {
                            if self.push_audio(&frame) {
                                return ConnectionExit::Reconnect;
                            }
                            if let Err(error) = connection.send_audio(&frame).await {
                                return if self.handle_connection_error(&error) {
                                    ConnectionExit::Reconnect
                                } else {
                                    ConnectionExit::Fatal
                                };
                            }
                            self.finalized_since_audio = false;
                        }
                        StreamingAsrCommand::Commit { .. } => {
                            self.finalize_pending = true;
                            if let Err(error) = connection.send_control(DeepgramControl::Finalize).await {
                                return if self.handle_connection_error(&error) {
                                    ConnectionExit::Reconnect
                                } else {
                                    ConnectionExit::Fatal
                                };
                            }
                            self.finalize_pending = false;
                            self.finalized_since_audio = true;
                        }
                        StreamingAsrCommand::Flush { reason: _ } => {
                            self.finalize_pending = true;
                            if let Err(error) = connection.send_control(DeepgramControl::Finalize).await {
                                return if self.handle_connection_error(&error) {
                                    ConnectionExit::Reconnect
                                } else {
                                    ConnectionExit::Fatal
                                };
                            }
                            self.finalize_pending = false;
                            self.finalized_since_audio = true;
                        }
                        StreamingAsrCommand::Reconnect { attempt, .. } => {
                            self.reconnect_attempt = self.reconnect_attempt.max(attempt);
                            return ConnectionExit::Reconnect;
                        }
                        StreamingAsrCommand::Stop => return ConnectionExit::Stop,
                    }
                }
                provider_event = connection.recv() => {
                    match provider_event {
                        Ok(Some(event)) => {
                            if self.handle_provider_event(event) {
                                return ConnectionExit::Reconnect;
                            }
                        }
                        Ok(None) => {
                            self.publish_provider_failure(
                                "deepgram_connection_closed",
                                true,
                                AsrHealthSeverity::Warning,
                            );
                            return ConnectionExit::Reconnect;
                        }
                        Err(error) => {
                            return if self.handle_connection_error(&error) {
                                ConnectionExit::Reconnect
                            } else {
                                ConnectionExit::Fatal
                            };
                        }
                    }
                }
                () = &mut keepalive => {
                    if let Err(error) = connection.send_control(DeepgramControl::KeepAlive).await {
                        return if self.handle_connection_error(&error) {
                            ConnectionExit::Reconnect
                        } else {
                            ConnectionExit::Fatal
                        };
                    }
                    keepalive.as_mut().reset(Instant::now() + KEEPALIVE_INTERVAL);
                }
            }
        }
    }

    async fn graceful_close(&mut self, connection: &mut DeepgramConnection) {
        if !self.finalized_since_audio {
            let _ = connection.send_control(DeepgramControl::Finalize).await;
            self.finalized_since_audio = true;
        }
        let deadline = Instant::now() + FINAL_DRAIN_TIMEOUT;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match time::timeout(remaining, connection.recv()).await {
                Ok(Ok(Some(event))) => {
                    if self.handle_provider_event(event) {
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = connection.send_control(DeepgramControl::CloseStream).await;
        let _ = connection.close_websocket().await;
    }

    /// Returns true when the provider explicitly reported a failed stream and
    /// the caller should replace the connection.
    fn handle_provider_event(&mut self, event: DeepgramServerEvent) -> bool {
        match event {
            DeepgramServerEvent::Transcript(transcript) => {
                let Some(mut event) =
                    trim_replayed_transcript(transcript.event, self.normalizer.last_stable_frame())
                else {
                    return false;
                };
                event.latency_ms = transcript_latency_ms(&event, self.latest_frame);
                if event.model.is_none() {
                    event.model = Some(self.config.options.model.clone());
                }
                if let Some(speaker) = event.speaker_id.as_mut() {
                    if !speaker.starts_with("speaker-") {
                        *speaker = format!("speaker-{speaker}");
                    }
                }
                for word in &mut event.words {
                    if let Some(speaker) = word.speaker_id.as_mut() {
                        if !speaker.starts_with("speaker-") {
                            *speaker = format!("speaker-{speaker}");
                        }
                    }
                }
                match self.normalizer.normalize(event) {
                    Ok(NormalizeOutcome::Emitted(update)) => {
                        let _ = self
                            .outputs
                            .send(DeepgramSupervisorOutput::Transcript(update));
                    }
                    Ok(NormalizeOutcome::Duplicate | NormalizeOutcome::IgnoredAfterFinal) => {}
                    Err(_) => self.publish_provider_failure(
                        "deepgram_normalization_failed",
                        true,
                        AsrHealthSeverity::Warning,
                    ),
                }
                false
            }
            DeepgramServerEvent::ProviderError(error) => {
                self.handle_provider_error(error);
                true
            }
            DeepgramServerEvent::EmptyResult(_)
            | DeepgramServerEvent::UtteranceEnd(_)
            | DeepgramServerEvent::SpeechStarted(_)
            | DeepgramServerEvent::Metadata(_)
            | DeepgramServerEvent::Unknown { .. } => false,
        }
    }

    fn handle_provider_error(&mut self, error: DeepgramProviderError) {
        let code = error
            .code
            .as_deref()
            .and_then(sanitize_provider_code)
            .unwrap_or("deepgram_provider_error");
        self.publish_provider_failure(code, true, AsrHealthSeverity::Warning);
    }

    fn handle_connection_error(&mut self, error: &DeepgramError) -> bool {
        let recoverable = deepgram_error_recoverable(error);
        self.publish_provider_failure(
            deepgram_error_code(error),
            recoverable,
            if recoverable {
                AsrHealthSeverity::Warning
            } else {
                AsrHealthSeverity::Fatal
            },
        );
        recoverable
    }

    async fn reconnect_backoff(&mut self) -> CommandAction {
        self.reconnect_attempt = self.reconnect_attempt.saturating_add(1);
        let stable_boundary = self
            .normalizer
            .last_stable_frame()
            .or_else(|| self.ring.oldest_frame())
            .unwrap_or(self.latest_frame);
        let replay_from = stable_boundary
            .saturating_sub(super::ring_buffer::DEFAULT_REPLAY_PREROLL_FRAMES)
            .max(self.ring.oldest_frame().unwrap_or(0));
        self.publish_health(
            AsrHealthEventKind::ReconnectScheduled,
            AsrHealthState::Backoff,
            AsrHealthSeverity::Warning,
            "deepgram_reconnect_scheduled",
            true,
            Some(replay_from),
            0,
            None,
        );

        let jitter = (random::<u16>() as f64 / u16::MAX as f64) * 2.0 - 1.0;
        let delay = reconnect_backoff_duration(self.reconnect_attempt, jitter);
        let timer = time::sleep(delay);
        tokio::pin!(timer);
        loop {
            tokio::select! {
                () = &mut timer => return CommandAction::Continue,
                command = self.commands.recv() => {
                    let Some(command) = command else {
                        return CommandAction::InputClosed;
                    };
                    match self.ingest_disconnected_command(command) {
                        CommandAction::Continue | CommandAction::Reconnect => {}
                        action => return action,
                    }
                }
            }
        }
    }

    fn ingest_disconnected_command(&mut self, command: StreamingAsrCommand) -> CommandAction {
        match command {
            StreamingAsrCommand::Audio(frame) => {
                self.push_audio(&frame);
                CommandAction::Continue
            }
            StreamingAsrCommand::Commit { .. } => {
                self.finalize_pending = true;
                CommandAction::Continue
            }
            StreamingAsrCommand::Flush { reason: _ } => {
                self.finalize_pending = true;
                CommandAction::Continue
            }
            StreamingAsrCommand::Reconnect { attempt, .. } => {
                self.reconnect_attempt = self.reconnect_attempt.max(attempt);
                CommandAction::Reconnect
            }
            StreamingAsrCommand::Stop => CommandAction::Stop,
        }
    }

    /// Returns true when the incoming frame exposed a queue-loss discontinuity
    /// and the current provider connection must be replaced.
    fn push_audio(&mut self, frame: &StreamingAudioFrame) -> bool {
        let end_frame = frame.end_frame().unwrap_or(frame.origin_frame);
        self.latest_frame = self.latest_frame.max(end_frame);
        self.audio_source = frame.source.clone();
        match self.ring.push(frame) {
            Ok(report) => {
                if report.incoming_prefix_discarded > 0 {
                    self.publish_health(
                        AsrHealthEventKind::BufferOverflow,
                        AsrHealthState::Degraded,
                        AsrHealthSeverity::Warning,
                        "deepgram_ring_frame_truncated",
                        true,
                        Some(report.oldest_frame),
                        report.incoming_prefix_discarded,
                        None,
                    );
                }
                false
            }
            Err(RingBufferError::NonContiguousTimeline {
                expected_frame,
                received_frame,
            }) => {
                let dropped = received_frame.saturating_sub(expected_frame);
                self.ring.clear();
                let _ = self.ring.push(frame);
                self.publish_health(
                    AsrHealthEventKind::BufferOverflow,
                    AsrHealthState::Degraded,
                    AsrHealthSeverity::Warning,
                    "deepgram_ingress_gap",
                    true,
                    Some(frame.origin_frame),
                    dropped,
                    None,
                );
                true
            }
            Err(_) => {
                self.ring.clear();
                let _ = self.ring.push(frame);
                self.publish_health(
                    AsrHealthEventKind::BufferOverflow,
                    AsrHealthState::Degraded,
                    AsrHealthSeverity::Warning,
                    "deepgram_invalid_audio_sequence",
                    true,
                    Some(frame.origin_frame),
                    frame.samples.len() as u64,
                    None,
                );
                true
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_health(
        &mut self,
        kind: AsrHealthEventKind,
        state: AsrHealthState,
        severity: AsrHealthSeverity,
        code: &str,
        recoverable: bool,
        replay_from_frame: Option<u64>,
        dropped_frames: u64,
        detail: Option<String>,
    ) {
        let mut event = AsrHealthEvent::new(
            self.config.session_id.clone(),
            self.health_sequence,
            StreamingAsrProvider::Deepgram,
            kind,
            state,
            self.latest_frame,
        );
        self.health_sequence = self.health_sequence.saturating_add(1);
        event.severity = severity;
        event.code = code.to_string();
        event.recoverable = recoverable;
        event.attempt = self.reconnect_attempt;
        event.replay_from_frame = replay_from_frame;
        event.dropped_frames = dropped_frames;
        event.detail = detail;
        if self.health.apply(event.clone()) == AsrHealthApplyResult::Applied {
            let _ = self.outputs.send(DeepgramSupervisorOutput::Health(event));
        }
    }

    fn publish_provider_failure(
        &mut self,
        code: &str,
        recoverable: bool,
        severity: AsrHealthSeverity,
    ) {
        self.publish_health(
            AsrHealthEventKind::ProviderError,
            if recoverable {
                AsrHealthState::Degraded
            } else {
                AsrHealthState::Failed
            },
            severity,
            code,
            recoverable,
            None,
            0,
            None,
        );
    }

    fn finish_without_connection(&mut self, input_closed: bool) -> DeepgramSupervisorExit {
        self.publish_health(
            AsrHealthEventKind::SessionStopped,
            AsrHealthState::Stopped,
            AsrHealthSeverity::Info,
            if input_closed {
                "deepgram_input_closed"
            } else {
                "deepgram_session_stopped"
            },
            false,
            None,
            0,
            None,
        );
        if input_closed {
            DeepgramSupervisorExit::InputClosed
        } else {
            DeepgramSupervisorExit::Stopped
        }
    }
}

fn transcript_latency_ms(event: &ProviderTranscriptEvent, latest_frame: u64) -> Option<u64> {
    let (_, end_frame) = event.time.absolute_frame_range().ok()?;
    Some(latest_frame.saturating_sub(end_frame) / (STREAMING_AUDIO_SAMPLE_RATE as u64 / 1_000))
}

/// Suppress replay results that are wholly behind the last stable canonical
/// boundary. If word timestamps exist, a crossing result is trimmed to the
/// not-yet-stable suffix; no timing is fabricated when the provider omitted
/// word metadata.
fn trim_replayed_transcript(
    mut event: ProviderTranscriptEvent,
    stable_boundary: Option<u64>,
) -> Option<ProviderTranscriptEvent> {
    let Some(stable_boundary) = stable_boundary else {
        return Some(event);
    };
    let (start_frame, end_frame) = event.time.absolute_frame_range().ok()?;
    if end_frame <= stable_boundary {
        return None;
    }
    if start_frame >= stable_boundary || event.words.is_empty() {
        return Some(event);
    }
    if event
        .words
        .iter()
        .any(|word| word.start_ms.is_none() || word.end_ms.is_none())
    {
        return Some(event);
    }

    let origin = event.time.origin_frame;
    let retained: Vec<ProviderTranscriptWord> = event
        .words
        .into_iter()
        .filter(|word| {
            word.end_ms
                .and_then(|milliseconds| provider_ms_to_absolute_frame(origin, milliseconds))
                .is_none_or(|end| end > stable_boundary)
        })
        .collect();
    if retained.is_empty() {
        return None;
    }
    let first_start = retained.first().and_then(|word| word.start_ms);
    let last_end = retained.last().and_then(|word| word.end_ms);
    if let (Some(start), Some(end)) = (first_start, last_end) {
        event.time.provider_start_ms = start;
        event.time.provider_end_ms = end.max(start);
    }
    event.text = join_provider_words(&retained);
    event.words = retained;
    (!event.text.trim().is_empty()).then_some(event)
}

fn provider_ms_to_absolute_frame(origin_frame: u64, milliseconds: f64) -> Option<u64> {
    if !milliseconds.is_finite() || milliseconds < 0.0 {
        return None;
    }
    let offset = milliseconds * STREAMING_AUDIO_SAMPLE_RATE as f64 / 1_000.0;
    if !offset.is_finite() || offset > u64::MAX as f64 {
        return None;
    }
    origin_frame.checked_add(offset.round() as u64)
}

fn join_provider_words(words: &[ProviderTranscriptWord]) -> String {
    let values: Vec<&str> = words
        .iter()
        .map(|word| word.punctuated_text.as_deref().unwrap_or(&word.text))
        .filter(|value| !value.trim().is_empty())
        .collect();
    if values
        .iter()
        .all(|value| value.chars().all(is_cjk_or_punctuation))
    {
        values.concat()
    } else {
        values.join(" ")
    }
}

fn is_cjk_or_punctuation(character: char) -> bool {
    matches!(character,
        '\u{3000}'..='\u{303f}' |
        '\u{3040}'..='\u{30ff}' |
        '\u{3400}'..='\u{4dbf}' |
        '\u{4e00}'..='\u{9fff}' |
        '\u{ff00}'..='\u{ffef}'
    )
}

fn reconnect_backoff_duration(attempt: u32, jitter_unit: f64) -> Duration {
    let exponent = attempt.saturating_sub(1).min(4);
    let base = MIN_BACKOFF_MS
        .saturating_mul(1_u64 << exponent)
        .min(MAX_BACKOFF_MS);
    let bounded_jitter = jitter_unit.clamp(-1.0, 1.0);
    let jitter = (base as f64 * BACKOFF_JITTER_PERCENT as f64 / 100.0 * bounded_jitter).round();
    Duration::from_millis((base as i64 + jitter as i64).max(1) as u64)
}

fn deepgram_error_recoverable(error: &DeepgramError) -> bool {
    match error {
        DeepgramError::Transport { kind, .. } => !matches!(
            kind,
            DeepgramNetworkErrorKind::Authentication
                | DeepgramNetworkErrorKind::Rejected
                | DeepgramNetworkErrorKind::Tls
                | DeepgramNetworkErrorKind::Protocol
        ),
        DeepgramError::Configuration(_)
        | DeepgramError::MissingCredential
        | DeepgramError::InvalidCredential
        | DeepgramError::MissingConnectionId
        | DeepgramError::InvalidPcm16Payload
        | DeepgramError::Protocol(_)
        | DeepgramError::Pcm(_) => false,
        DeepgramError::Parse(_)
        | DeepgramError::UtteranceIndexOverflow
        | DeepgramError::UnexpectedBinaryMessage => true,
    }
}

fn deepgram_error_code(error: &DeepgramError) -> &'static str {
    match error {
        DeepgramError::Configuration(_) => "deepgram_invalid_configuration",
        DeepgramError::Parse(_) => "deepgram_invalid_provider_message",
        DeepgramError::Protocol(_) => "deepgram_protocol_error",
        DeepgramError::Pcm(_) => "deepgram_audio_encoding_error",
        DeepgramError::MissingCredential => "deepgram_missing_credential",
        DeepgramError::InvalidCredential => "deepgram_invalid_credential",
        DeepgramError::MissingConnectionId => "deepgram_missing_connection_id",
        DeepgramError::UtteranceIndexOverflow => "deepgram_utterance_overflow",
        DeepgramError::InvalidPcm16Payload => "deepgram_invalid_pcm_payload",
        DeepgramError::UnexpectedBinaryMessage => "deepgram_unexpected_binary_message",
        DeepgramError::Transport { kind, .. } => match kind {
            DeepgramNetworkErrorKind::Authentication => "deepgram_authentication_failed",
            DeepgramNetworkErrorKind::RateLimited => "deepgram_rate_limited",
            DeepgramNetworkErrorKind::ServiceUnavailable => "deepgram_service_unavailable",
            DeepgramNetworkErrorKind::Timeout => "deepgram_network_timeout",
            DeepgramNetworkErrorKind::Disconnected => "deepgram_disconnected",
            DeepgramNetworkErrorKind::Tls => "deepgram_tls_failed",
            DeepgramNetworkErrorKind::Protocol => "deepgram_websocket_protocol_error",
            DeepgramNetworkErrorKind::Rejected => "deepgram_connection_rejected",
            DeepgramNetworkErrorKind::Other => "deepgram_network_error",
        },
    }
}

fn sanitize_provider_code(code: &str) -> Option<&str> {
    let code = code.trim();
    (!code.is_empty()
        && code.len() <= 80
        && code
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_')))
    .then_some(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::transcription::streaming::{
        ProviderTimeRange, ProviderTranscriptKind, StreamingAsrProvider,
        STREAMING_ASR_SCHEMA_VERSION,
    };

    fn provider_event(start_ms: f64, end_ms: f64) -> ProviderTranscriptEvent {
        ProviderTranscriptEvent {
            schema_version: STREAMING_ASR_SCHEMA_VERSION,
            provider: StreamingAsrProvider::Deepgram,
            connection_id: "connection-1".to_string(),
            provider_event_id: None,
            utterance_key: "utterance-1".to_string(),
            kind: ProviderTranscriptKind::Final,
            text: "old new".to_string(),
            time: ProviderTimeRange {
                origin_frame: 0,
                provider_start_ms: start_ms,
                provider_end_ms: end_ms,
            },
            confidence: Some(0.9),
            language: Some("en".to_string()),
            speaker_id: None,
            speaker_confidence: None,
            model: Some("nova-3".to_string()),
            latency_ms: None,
            audio_source: AudioSource::Mixed,
            trace_id: None,
            words: vec![
                ProviderTranscriptWord {
                    text: "old".to_string(),
                    punctuated_text: None,
                    start_ms: Some(start_ms),
                    end_ms: Some(1_000.0),
                    confidence: Some(0.9),
                    language: None,
                    speaker_id: Some("0".to_string()),
                    speaker_confidence: Some(0.8),
                },
                ProviderTranscriptWord {
                    text: "new".to_string(),
                    punctuated_text: None,
                    start_ms: Some(1_000.0),
                    end_ms: Some(end_ms),
                    confidence: Some(0.9),
                    language: None,
                    speaker_id: Some("1".to_string()),
                    speaker_confidence: Some(0.8),
                },
            ],
        }
    }

    #[test]
    fn config_debug_never_contains_api_key() {
        let config = DeepgramSupervisorConfig::new(
            "session-1",
            DeepgramOptions::default(),
            StreamingTranscriptContext::default(),
            0,
            "secret-that-must-not-appear",
        );
        let debug = format!("{config:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("secret-that-must-not-appear"));
    }

    #[test]
    fn backoff_uses_half_one_two_four_eight_second_bases_with_bounded_jitter() {
        assert_eq!(
            reconnect_backoff_duration(1, 0.0),
            Duration::from_millis(500)
        );
        assert_eq!(reconnect_backoff_duration(2, 0.0), Duration::from_secs(1));
        assert_eq!(reconnect_backoff_duration(3, 0.0), Duration::from_secs(2));
        assert_eq!(reconnect_backoff_duration(4, 0.0), Duration::from_secs(4));
        assert_eq!(reconnect_backoff_duration(5, 0.0), Duration::from_secs(8));
        assert_eq!(
            reconnect_backoff_duration(9, -1.0),
            Duration::from_millis(6_400)
        );
        assert_eq!(
            reconnect_backoff_duration(9, 1.0),
            Duration::from_millis(9_600)
        );
    }

    #[test]
    fn replay_event_before_stable_boundary_is_dropped() {
        let event = provider_event(0.0, 900.0);
        assert!(trim_replayed_transcript(event, Some(48_000)).is_none());
    }

    #[test]
    fn first_connection_transcript_is_not_dropped_without_a_stable_boundary() {
        let event = provider_event(0.0, 900.0);
        assert!(trim_replayed_transcript(event, None).is_some());
    }

    #[test]
    fn crossing_replay_event_keeps_only_timestamped_suffix() {
        let event = provider_event(500.0, 1_500.0);
        let trimmed = trim_replayed_transcript(event, Some(48_000)).unwrap();
        assert_eq!(trimmed.text, "new");
        assert_eq!(trimmed.words.len(), 1);
        assert_eq!(trimmed.time.provider_start_ms, 1_000.0);
        assert_eq!(trimmed.time.provider_end_ms, 1_500.0);
    }

    #[test]
    fn health_state_keeps_reducer_snapshot_and_history_reusable() {
        let state = SharedAsrHealthState::default();
        let mut event = AsrHealthEvent::new(
            "session-1",
            0,
            StreamingAsrProvider::Deepgram,
            AsrHealthEventKind::SessionStarting,
            AsrHealthState::Connecting,
            0,
        );
        event.event_id = "health-1".to_string();
        assert_eq!(state.apply(event.clone()), AsrHealthApplyResult::Applied);
        assert_eq!(state.apply(event), AsrHealthApplyResult::Duplicate);
        assert_eq!(state.snapshot().len(), 1);
        assert_eq!(state.event_history().len(), 1);
        assert_eq!(state.event_snapshot().len(), 1);
    }

    #[tokio::test]
    async fn stop_before_audio_never_opens_network_and_emits_complete_health_history() {
        let (command_tx, command_rx) = mpsc::channel(1);
        let (output_tx, mut output_rx) = mpsc::unbounded_channel();
        command_tx.send(StreamingAsrCommand::Stop).await.unwrap();
        drop(command_tx);
        let health = SharedAsrHealthState::default();
        let exit = run_deepgram_supervisor(
            DeepgramSupervisorConfig::new(
                "session-1",
                DeepgramOptions::default(),
                StreamingTranscriptContext::default(),
                0,
                "unused-secret",
            ),
            command_rx,
            output_tx,
            health.clone(),
        )
        .await;

        assert_eq!(exit, DeepgramSupervisorExit::Stopped);
        let outputs: Vec<_> = std::iter::from_fn(|| output_rx.try_recv().ok()).collect();
        assert_eq!(outputs.len(), 2);
        assert!(outputs
            .iter()
            .all(|output| matches!(output, DeepgramSupervisorOutput::Health(_))));
        let history = health.event_snapshot();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].state, AsrHealthState::Connecting);
        assert_eq!(history[1].state, AsrHealthState::Stopped);
    }
}
