//! Session supervisor for OpenAI Realtime transcription.
//!
//! OpenAI transcript delta/completed events identify content by
//! `(item_id, content_index)`, but do not include transcript timestamps,
//! confidence, word timing, or speaker identity. This supervisor therefore
//! emits only local canonical-frame ranges observed by Meetily. Those ranges
//! are useful for UI ordering, but are not provider timestamps. Manual
//! commits, server-VAD observations, and reconnect replay are kept as distinct
//! mapping states so uncertainty cannot silently become provider metadata.
//!
//! This actor owns reconnect/replay and graceful shutdown only. It never
//! starts a local fallback provider.

use super::health::{
    AsrHealthApplyResult, AsrHealthEvent, AsrHealthEventKind, AsrHealthSeverity, AsrHealthState,
};
use super::normalizer::StreamingTranscriptContext;
use super::openai_realtime::{
    OpenAiProviderError, OpenAiRealtimeConnection, OpenAiRealtimeControl, OpenAiRealtimeError,
    OpenAiRealtimeNetworkErrorKind, OpenAiRealtimeOptions, OpenAiRealtimeServerEvent,
    OpenAiTranscriptCompletedUpdate, OpenAiTranscriptDeltaUpdate,
};
use super::protocol::{
    StreamingAsrCommand, StreamingAsrProvider, StreamingAudioFrame, STREAMING_AUDIO_SAMPLE_RATE,
};
use super::ring_buffer::{
    RingBufferError, StreamingAudioRingBuffer, DEFAULT_REPLAY_PREROLL_FRAMES,
};
use super::supervisor::SharedAsrHealthState;
use crate::audio::transcription::{
    AudioSource, TranscriptChunkInput, TranscriptEventKind, TranscriptUpdate,
};
use rand::random;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
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

/// Private credentials are moved directly into the connection attempt and
/// are always redacted from `Debug`.
pub struct OpenAiSupervisorConfig {
    pub session_id: String,
    pub options: OpenAiRealtimeOptions,
    pub transcript_context: StreamingTranscriptContext,
    pub first_transcript_sequence: u64,
    api_key: String,
}

impl OpenAiSupervisorConfig {
    pub fn new(
        session_id: impl Into<String>,
        options: OpenAiRealtimeOptions,
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

impl fmt::Debug for OpenAiSupervisorConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiSupervisorConfig")
            .field("session_id", &self.session_id)
            .field("options", &self.options)
            .field("transcript_context", &self.transcript_context)
            .field("first_transcript_sequence", &self.first_transcript_sequence)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum OpenAiSupervisorOutput {
    Transcript(TranscriptUpdate),
    Health(AsrHealthEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenAiSupervisorExit {
    Stopped,
    InputClosed,
    Fatal { code: String },
}

/// Run one OpenAI Realtime transcription session until `Stop`, command EOF,
/// or an unrecoverable configuration/authentication/protocol failure.
pub async fn run_openai_supervisor(
    config: OpenAiSupervisorConfig,
    mut commands: mpsc::Receiver<StreamingAsrCommand>,
    outputs: mpsc::UnboundedSender<OpenAiSupervisorOutput>,
    health: SharedAsrHealthState,
) -> OpenAiSupervisorExit {
    let mut transcript_context = config.transcript_context.clone();
    if transcript_context.session_id.is_none() {
        transcript_context.session_id = Some(config.session_id.clone());
    }
    let assembler = OpenAiTranscriptAssembler::new(
        transcript_context,
        config.options.model.clone(),
        config.first_transcript_sequence,
    );
    let mut actor = OpenAiSupervisor {
        health_sequence: health.next_sequence(),
        config,
        commands: &mut commands,
        outputs: &outputs,
        health: &health,
        assembler,
        mapper: OpenAiItemMapper::default(),
        ring: StreamingAudioRingBuffer::thirty_seconds(),
        reconnect_attempt: 0,
        latest_frame: 0,
        last_stable_frame: None,
        audio_source: AudioSource::Mixed,
        commit_pending_through: None,
        warned_time_bases: HashSet::new(),
    };
    actor.run().await
}

struct OpenAiSupervisor<'a> {
    config: OpenAiSupervisorConfig,
    commands: &'a mut mpsc::Receiver<StreamingAsrCommand>,
    outputs: &'a mpsc::UnboundedSender<OpenAiSupervisorOutput>,
    health: &'a SharedAsrHealthState,
    assembler: OpenAiTranscriptAssembler,
    mapper: OpenAiItemMapper,
    ring: StreamingAudioRingBuffer,
    health_sequence: u64,
    reconnect_attempt: u32,
    latest_frame: u64,
    last_stable_frame: Option<u64>,
    audio_source: AudioSource,
    commit_pending_through: Option<u64>,
    warned_time_bases: HashSet<OpenAiCanonicalTimeBasis>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandAction {
    Continue,
    Reconnect,
    Stop,
    InputClosed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConnectionExit {
    Reconnect,
    Stop,
    InputClosed,
    Fatal { code: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProviderAction {
    Continue,
    Reconnect,
    Fatal { code: String },
}

impl OpenAiSupervisor<'_> {
    async fn run(&mut self) -> OpenAiSupervisorExit {
        self.publish_health(
            AsrHealthEventKind::SessionStarting,
            AsrHealthState::Connecting,
            AsrHealthSeverity::Info,
            "openai_realtime_session_starting",
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
                .last_stable_frame
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
                        "openai_realtime_replay_boundary_unavailable",
                        true,
                        None,
                        0,
                        None,
                    );
                    self.ring.clear();
                    continue;
                }
            };
            let replaying = self.reconnect_attempt > 0;
            self.mapper.start_connection(replaying);
            self.publish_health(
                AsrHealthEventKind::ConnectionChanged,
                AsrHealthState::Connecting,
                AsrHealthSeverity::Info,
                "openai_realtime_connecting",
                true,
                Some(replay.actual_start_frame),
                0,
                None,
            );

            let connect_result = time::timeout(
                CONNECT_TIMEOUT,
                OpenAiRealtimeConnection::connect(&self.config.options, &self.config.api_key),
            )
            .await;
            let mut connection = match connect_result {
                Ok(Ok(connection)) => connection,
                Ok(Err(error)) => {
                    if !self.handle_connection_error(&error) {
                        return OpenAiSupervisorExit::Fatal {
                            code: openai_error_code(&error).to_string(),
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
                        "openai_realtime_connect_timeout",
                        true,
                        AsrHealthSeverity::Warning,
                        None,
                    );
                    match self.reconnect_backoff().await {
                        CommandAction::Continue | CommandAction::Reconnect => continue,
                        CommandAction::Stop => return self.finish_without_connection(false),
                        CommandAction::InputClosed => return self.finish_without_connection(true),
                    }
                }
            };

            if replaying {
                self.publish_health(
                    AsrHealthEventKind::ReplayStarted,
                    AsrHealthState::Replaying,
                    AsrHealthSeverity::Info,
                    if replay.history_truncated {
                        "openai_realtime_replay_history_truncated"
                    } else {
                        "openai_realtime_replay_started"
                    },
                    true,
                    Some(replay.actual_start_frame),
                    0,
                    Some(
                        "Replay has no provider timestamps; item-to-frame deduplication is ambiguous"
                            .to_string(),
                    ),
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
                    return OpenAiSupervisorExit::Fatal {
                        code: openai_error_code(&error).to_string(),
                    };
                }
                match self.reconnect_backoff().await {
                    CommandAction::Continue | CommandAction::Reconnect => continue,
                    CommandAction::Stop => return self.finish_without_connection(false),
                    CommandAction::InputClosed => return self.finish_without_connection(true),
                }
            }
            self.mapper.note_audio(
                replay.actual_start_frame,
                replay.end_frame_exclusive,
                replaying,
            );

            let current_attempt = self.reconnect_attempt;
            self.reconnect_attempt = 0;
            self.publish_health(
                AsrHealthEventKind::ConnectionChanged,
                AsrHealthState::Streaming,
                AsrHealthSeverity::Info,
                if replaying {
                    "openai_realtime_replay_sent"
                } else {
                    "openai_realtime_streaming"
                },
                true,
                Some(replay.actual_start_frame),
                0,
                replaying.then(|| {
                    format!(
                        "Replay attempt {current_attempt} was sent; transcript overlap remains provider-untimestamped"
                    )
                }),
            );

            if let Some(through_frame) = self.commit_pending_through.take() {
                if let Err(error) = self
                    .commit_provider_buffer(&mut connection, through_frame)
                    .await
                {
                    if !self.handle_connection_error(&error) {
                        return OpenAiSupervisorExit::Fatal {
                            code: openai_error_code(&error).to_string(),
                        };
                    }
                    match self.reconnect_backoff().await {
                        CommandAction::Continue | CommandAction::Reconnect => continue,
                        CommandAction::Stop => return self.finish_without_connection(false),
                        CommandAction::InputClosed => return self.finish_without_connection(true),
                    }
                }
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
                ConnectionExit::Fatal { code } => {
                    let _ = connection.close_websocket().await;
                    return OpenAiSupervisorExit::Fatal { code };
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

    async fn run_connection(
        &mut self,
        connection: &mut OpenAiRealtimeConnection,
    ) -> ConnectionExit {
        let keepalive = time::sleep(KEEPALIVE_INTERVAL);
        tokio::pin!(keepalive);
        loop {
            tokio::select! {
                biased;
                () = &mut keepalive => {
                    if let Err(error) = connection.send_ping().await {
                        return self.connection_error_exit(&error);
                    }
                    keepalive.as_mut().reset(Instant::now() + KEEPALIVE_INTERVAL);
                }
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
                                return self.connection_error_exit(&error);
                            }
                            if let Ok(end_frame) = frame.end_frame() {
                                self.mapper.note_audio(frame.origin_frame, end_frame, false);
                            }
                        }
                        StreamingAsrCommand::Commit { through_frame } => {
                            if let Err(error) = self.commit_provider_buffer(connection, through_frame).await {
                                return self.connection_error_exit(&error);
                            }
                        }
                        StreamingAsrCommand::Flush { reason: _ } => {
                            if let Err(error) = self.commit_provider_buffer(connection, self.latest_frame).await {
                                return self.connection_error_exit(&error);
                            }
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
                        Ok(Some(event)) => match self.handle_provider_event(event) {
                            ProviderAction::Continue => {}
                            ProviderAction::Reconnect => return ConnectionExit::Reconnect,
                            ProviderAction::Fatal { code } => return ConnectionExit::Fatal { code },
                        },
                        Ok(None) => {
                            self.publish_provider_failure(
                                "openai_realtime_connection_closed",
                                true,
                                AsrHealthSeverity::Warning,
                                None,
                            );
                            return ConnectionExit::Reconnect;
                        }
                        Err(error) => return self.connection_error_exit(&error),
                    }
                }
            }
        }
    }

    async fn commit_provider_buffer(
        &mut self,
        connection: &mut OpenAiRealtimeConnection,
        requested_through_frame: u64,
    ) -> Result<(), OpenAiRealtimeError> {
        if !self.mapper.has_uncommitted_audio() {
            return Ok(());
        }
        connection
            .send_control(OpenAiRealtimeControl::Commit)
            .await?;
        if let Some(sealed) = self.mapper.seal_manual_commit(requested_through_frame) {
            if sealed.boundary_widened {
                self.publish_health(
                    AsrHealthEventKind::ProviderError,
                    AsrHealthState::Degraded,
                    AsrHealthSeverity::Warning,
                    "openai_realtime_commit_boundary_widened",
                    true,
                    None,
                    0,
                    Some(format!(
                        "Requested through frame {}; OpenAI committed the full sent buffer through local frame {}",
                        requested_through_frame, sealed.range.end_frame
                    )),
                );
            }
        }
        Ok(())
    }

    async fn graceful_close(&mut self, connection: &mut OpenAiRealtimeConnection) {
        if self.mapper.has_uncommitted_audio()
            && connection
                .send_control(OpenAiRealtimeControl::Commit)
                .await
                .is_ok()
        {
            let _ = self.mapper.seal_manual_commit(self.latest_frame);
        }

        let deadline = Instant::now() + FINAL_DRAIN_TIMEOUT;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match time::timeout(remaining, connection.recv()).await {
                Ok(Ok(Some(event))) => {
                    if !matches!(self.handle_provider_event(event), ProviderAction::Continue) {
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = connection.send_control(OpenAiRealtimeControl::Clear).await;
        self.mapper.clear_provider_buffer();
        let _ = connection.close_websocket().await;
    }

    fn handle_provider_event(&mut self, event: OpenAiRealtimeServerEvent) -> ProviderAction {
        match event {
            OpenAiRealtimeServerEvent::SpeechStarted(event) => {
                self.mapper
                    .note_speech_started(event.item_id, self.latest_frame);
                ProviderAction::Continue
            }
            OpenAiRealtimeServerEvent::SpeechStopped(event) => {
                self.mapper
                    .note_speech_stopped(event.item_id, self.latest_frame);
                ProviderAction::Continue
            }
            OpenAiRealtimeServerEvent::AudioBufferCommitted(event) => {
                let range = self.mapper.bind_committed_item(
                    event.item_id.clone(),
                    event.previous_item_id,
                    self.latest_frame,
                    self.last_stable_frame,
                );
                if self.assembler.bind_item_range(&event.item_id, range) {
                    self.publish_health(
                        AsrHealthEventKind::ProviderError,
                        AsrHealthState::Degraded,
                        AsrHealthSeverity::Warning,
                        "openai_realtime_late_item_mapping",
                        true,
                        None,
                        0,
                        Some(
                            "Commit-to-item mapping arrived after a final transcript; the emitted local range was not silently rewritten"
                                .to_string(),
                        ),
                    );
                }
                self.warn_time_basis(range.basis);
                ProviderAction::Continue
            }
            OpenAiRealtimeServerEvent::AudioBufferCleared(_) => {
                self.mapper.clear_provider_buffer();
                ProviderAction::Continue
            }
            OpenAiRealtimeServerEvent::TranscriptDelta(update) => {
                let range = self.range_for_item(&update.event.item_id);
                if let Some(output) =
                    self.assembler
                        .apply_delta(update, range, self.audio_source.clone())
                {
                    self.warn_time_basis(range.basis);
                    let _ = self
                        .outputs
                        .send(OpenAiSupervisorOutput::Transcript(output));
                }
                ProviderAction::Continue
            }
            OpenAiRealtimeServerEvent::TranscriptCompleted(update) => {
                let range = self.range_for_item(&update.event.item_id);
                if let Some(output) =
                    self.assembler
                        .apply_completed(update, range, self.audio_source.clone())
                {
                    self.warn_time_basis(range.basis);
                    if matches!(
                        range.basis,
                        OpenAiCanonicalTimeBasis::ManualCommit
                            | OpenAiCanonicalTimeBasis::ServerVadObserved
                    ) {
                        self.last_stable_frame =
                            Some(self.last_stable_frame.unwrap_or(0).max(range.end_frame));
                    }
                    let _ = self
                        .outputs
                        .send(OpenAiSupervisorOutput::Transcript(output));
                }
                ProviderAction::Continue
            }
            OpenAiRealtimeServerEvent::TranscriptFailed(update) => {
                self.assembler.mark_failed(
                    &update.event.item_id,
                    update.event.content_index,
                    update.event.event_id.as_deref(),
                );
                let code = provider_error_code(&update.event.error)
                    .unwrap_or("openai_realtime_transcription_failed");
                self.publish_provider_failure(
                    code,
                    true,
                    AsrHealthSeverity::Warning,
                    Some("One provider item failed; no final transcript was invented".to_string()),
                );
                ProviderAction::Continue
            }
            OpenAiRealtimeServerEvent::Error(event) => {
                let recoverable = provider_error_recoverable(&event.error);
                let code = provider_error_code(&event.error)
                    .unwrap_or("openai_realtime_provider_error")
                    .to_string();
                self.publish_provider_failure(
                    &code,
                    recoverable,
                    if recoverable {
                        AsrHealthSeverity::Warning
                    } else {
                        AsrHealthSeverity::Fatal
                    },
                    None,
                );
                if recoverable {
                    ProviderAction::Reconnect
                } else {
                    ProviderAction::Fatal { code }
                }
            }
            OpenAiRealtimeServerEvent::RateLimitsUpdated(event) => {
                if event
                    .rate_limits
                    .iter()
                    .any(|limit| limit.remaining.is_some_and(|remaining| remaining <= 0.0))
                {
                    self.publish_provider_failure(
                        "openai_realtime_rate_limit_exhausted",
                        true,
                        AsrHealthSeverity::Warning,
                        None,
                    );
                }
                ProviderAction::Continue
            }
            OpenAiRealtimeServerEvent::SessionCreated(_)
            | OpenAiRealtimeServerEvent::SessionUpdated(_)
            | OpenAiRealtimeServerEvent::TranscriptionSessionCreated(_)
            | OpenAiRealtimeServerEvent::TranscriptionSessionUpdated(_)
            | OpenAiRealtimeServerEvent::Unknown { .. } => ProviderAction::Continue,
        }
    }

    fn range_for_item(&self, item_id: &str) -> LocalFrameRange {
        self.mapper.range_for(item_id).unwrap_or_else(|| {
            self.mapper
                .provisional_range(self.latest_frame, self.last_stable_frame)
        })
    }

    fn warn_time_basis(&mut self, basis: OpenAiCanonicalTimeBasis) {
        if !self.warned_time_bases.insert(basis) {
            return;
        }
        let (kind, code, severity, detail) = match basis {
            OpenAiCanonicalTimeBasis::ManualCommit => (
                AsrHealthEventKind::ConnectionChanged,
                "openai_realtime_manual_commit_local_timing",
                AsrHealthSeverity::Info,
                "Transcript time uses the local canonical audio buffer committed by Meetily; it is not a provider transcript timestamp",
            ),
            OpenAiCanonicalTimeBasis::ServerVadObserved => (
                AsrHealthEventKind::ConnectionChanged,
                "openai_realtime_server_vad_local_timing",
                AsrHealthSeverity::Info,
                "Transcript time uses local frame observations around server VAD; it is not a provider transcript timestamp",
            ),
            OpenAiCanonicalTimeBasis::ReplayAmbiguous => (
                AsrHealthEventKind::ProviderError,
                "openai_realtime_replay_item_mapping_ambiguous",
                AsrHealthSeverity::Warning,
                "Replay can create new provider item IDs without timestamps; overlap cannot be trimmed exactly",
            ),
            OpenAiCanonicalTimeBasis::UnmappedLocalWindow => (
                AsrHealthEventKind::ProviderError,
                "openai_realtime_item_mapping_unavailable",
                AsrHealthSeverity::Warning,
                "No commit-to-item mapping was available; transcript time uses a conservative local audio window",
            ),
        };
        self.publish_health(
            kind,
            if severity == AsrHealthSeverity::Info {
                AsrHealthState::Streaming
            } else {
                AsrHealthState::Degraded
            },
            severity,
            code,
            true,
            None,
            0,
            Some(detail.to_string()),
        );
    }

    fn connection_error_exit(&mut self, error: &OpenAiRealtimeError) -> ConnectionExit {
        if self.handle_connection_error(error) {
            ConnectionExit::Reconnect
        } else {
            ConnectionExit::Fatal {
                code: openai_error_code(error).to_string(),
            }
        }
    }

    fn handle_connection_error(&mut self, error: &OpenAiRealtimeError) -> bool {
        let recoverable = openai_error_recoverable(error);
        self.publish_provider_failure(
            openai_error_code(error),
            recoverable,
            if recoverable {
                AsrHealthSeverity::Warning
            } else {
                AsrHealthSeverity::Fatal
            },
            None,
        );
        recoverable
    }

    async fn reconnect_backoff(&mut self) -> CommandAction {
        self.reconnect_attempt = self.reconnect_attempt.saturating_add(1);
        let stable_boundary = self
            .last_stable_frame
            .or_else(|| self.ring.oldest_frame())
            .unwrap_or(self.latest_frame);
        let replay_from = stable_boundary
            .saturating_sub(DEFAULT_REPLAY_PREROLL_FRAMES)
            .max(self.ring.oldest_frame().unwrap_or(0));
        self.publish_health(
            AsrHealthEventKind::ReconnectScheduled,
            AsrHealthState::Backoff,
            AsrHealthSeverity::Warning,
            "openai_realtime_reconnect_scheduled",
            true,
            Some(replay_from),
            0,
            Some(
                "Commands continue buffering during backoff; replay has no exact transcript timestamps"
                    .to_string(),
            ),
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
            StreamingAsrCommand::Commit { through_frame } => {
                self.commit_pending_through =
                    Some(self.commit_pending_through.unwrap_or(0).max(through_frame));
                CommandAction::Continue
            }
            StreamingAsrCommand::Flush { reason: _ } => {
                self.commit_pending_through = Some(self.latest_frame);
                CommandAction::Continue
            }
            StreamingAsrCommand::Reconnect { attempt, .. } => {
                self.reconnect_attempt = self.reconnect_attempt.max(attempt);
                CommandAction::Reconnect
            }
            StreamingAsrCommand::Stop => CommandAction::Stop,
        }
    }

    /// Returns true when a discontinuity requires a fresh provider connection.
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
                        "openai_realtime_ring_frame_truncated",
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
                    "openai_realtime_ingress_gap",
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
                    "openai_realtime_invalid_audio_sequence",
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
            StreamingAsrProvider::OpenAiRealtime,
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
            let _ = self.outputs.send(OpenAiSupervisorOutput::Health(event));
        }
    }

    fn publish_provider_failure(
        &mut self,
        code: &str,
        recoverable: bool,
        severity: AsrHealthSeverity,
        detail: Option<String>,
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
            detail,
        );
    }

    fn finish_without_connection(&mut self, input_closed: bool) -> OpenAiSupervisorExit {
        self.publish_health(
            AsrHealthEventKind::SessionStopped,
            AsrHealthState::Stopped,
            AsrHealthSeverity::Info,
            if input_closed {
                "openai_realtime_input_closed"
            } else {
                "openai_realtime_session_stopped"
            },
            false,
            None,
            0,
            None,
        );
        if input_closed {
            OpenAiSupervisorExit::InputClosed
        } else {
            OpenAiSupervisorExit::Stopped
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum OpenAiCanonicalTimeBasis {
    /// Exact range of the local audio buffer at a client commit boundary.
    ManualCommit,
    /// Bounds observed on Meetily's local frame clock around server VAD.
    ServerVadObserved,
    /// Range contains replay overlap which cannot be trimmed without provider
    /// transcript timestamps.
    ReplayAmbiguous,
    /// Conservative local range used before a commit-to-item mapping exists.
    UnmappedLocalWindow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LocalFrameRange {
    start_frame: u64,
    end_frame: u64,
    basis: OpenAiCanonicalTimeBasis,
}

impl LocalFrameRange {
    fn new(start_frame: u64, end_frame: u64, basis: OpenAiCanonicalTimeBasis) -> Self {
        Self {
            start_frame,
            end_frame: end_frame.max(start_frame),
            basis,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BufferWindow {
    start_frame: u64,
    end_frame: u64,
    replay_ambiguous: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SealedCommit {
    range: LocalFrameRange,
    boundary_widened: bool,
}

#[derive(Debug, Default)]
struct OpenAiItemMapper {
    current_buffer: Option<BufferWindow>,
    pending_manual_commits: VecDeque<LocalFrameRange>,
    speech_starts: HashMap<String, u64>,
    speech_ranges: HashMap<String, LocalFrameRange>,
    item_ranges: HashMap<String, LocalFrameRange>,
    previous_items: HashMap<String, Option<String>>,
    connection_replay_ambiguous: bool,
}

impl OpenAiItemMapper {
    fn start_connection(&mut self, replay_ambiguous: bool) {
        self.current_buffer = None;
        self.pending_manual_commits.clear();
        self.speech_starts.clear();
        self.speech_ranges.clear();
        self.connection_replay_ambiguous = replay_ambiguous;
    }

    fn note_audio(&mut self, start_frame: u64, end_frame: u64, replay_ambiguous: bool) {
        let end_frame = end_frame.max(start_frame);
        match &mut self.current_buffer {
            Some(buffer) => {
                buffer.start_frame = buffer.start_frame.min(start_frame);
                buffer.end_frame = buffer.end_frame.max(end_frame);
                buffer.replay_ambiguous |= replay_ambiguous;
            }
            None => {
                self.current_buffer = Some(BufferWindow {
                    start_frame,
                    end_frame,
                    replay_ambiguous: replay_ambiguous || self.connection_replay_ambiguous,
                });
            }
        }
    }

    fn has_uncommitted_audio(&self) -> bool {
        self.current_buffer
            .is_some_and(|buffer| buffer.end_frame > buffer.start_frame)
    }

    fn seal_manual_commit(&mut self, requested_through_frame: u64) -> Option<SealedCommit> {
        let buffer = self.current_buffer.take()?;
        let basis = if buffer.replay_ambiguous {
            OpenAiCanonicalTimeBasis::ReplayAmbiguous
        } else {
            OpenAiCanonicalTimeBasis::ManualCommit
        };
        let range = LocalFrameRange::new(buffer.start_frame, buffer.end_frame, basis);
        self.pending_manual_commits.push_back(range);
        self.connection_replay_ambiguous = false;
        Some(SealedCommit {
            range,
            boundary_widened: requested_through_frame < buffer.end_frame,
        })
    }

    fn note_speech_started(&mut self, item_id: String, latest_frame: u64) {
        self.speech_starts.entry(item_id).or_insert(latest_frame);
    }

    fn note_speech_stopped(&mut self, item_id: String, latest_frame: u64) {
        let start_frame = self.speech_starts.remove(&item_id).unwrap_or(latest_frame);
        let basis = if self.connection_replay_ambiguous {
            OpenAiCanonicalTimeBasis::ReplayAmbiguous
        } else {
            OpenAiCanonicalTimeBasis::ServerVadObserved
        };
        self.speech_ranges.insert(
            item_id,
            LocalFrameRange::new(start_frame, latest_frame, basis),
        );
    }

    fn bind_committed_item(
        &mut self,
        item_id: String,
        previous_item_id: Option<String>,
        latest_frame: u64,
        stable_frame: Option<u64>,
    ) -> LocalFrameRange {
        let range = self
            .pending_manual_commits
            .pop_front()
            .or_else(|| self.speech_ranges.remove(&item_id))
            .or_else(|| {
                self.current_buffer.take().map(|buffer| {
                    LocalFrameRange::new(
                        buffer.start_frame,
                        buffer.end_frame,
                        if buffer.replay_ambiguous {
                            OpenAiCanonicalTimeBasis::ReplayAmbiguous
                        } else {
                            OpenAiCanonicalTimeBasis::ServerVadObserved
                        },
                    )
                })
            })
            .unwrap_or_else(|| {
                LocalFrameRange::new(
                    stable_frame.unwrap_or(latest_frame),
                    latest_frame,
                    OpenAiCanonicalTimeBasis::UnmappedLocalWindow,
                )
            });
        self.connection_replay_ambiguous = false;
        self.previous_items
            .insert(item_id.clone(), previous_item_id);
        self.item_ranges.insert(item_id, range);
        range
    }

    fn range_for(&self, item_id: &str) -> Option<LocalFrameRange> {
        self.item_ranges.get(item_id).copied()
    }

    fn provisional_range(&self, latest_frame: u64, stable_frame: Option<u64>) -> LocalFrameRange {
        if let Some(buffer) = self.current_buffer {
            return LocalFrameRange::new(
                buffer.start_frame,
                buffer.end_frame,
                if buffer.replay_ambiguous {
                    OpenAiCanonicalTimeBasis::ReplayAmbiguous
                } else {
                    OpenAiCanonicalTimeBasis::UnmappedLocalWindow
                },
            );
        }
        LocalFrameRange::new(
            stable_frame.unwrap_or(latest_frame),
            latest_frame,
            OpenAiCanonicalTimeBasis::UnmappedLocalWindow,
        )
    }

    fn clear_provider_buffer(&mut self) {
        self.current_buffer = None;
        self.pending_manual_commits.clear();
        self.speech_starts.clear();
        self.speech_ranges.clear();
        self.connection_replay_ambiguous = false;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OpenAiItemKey {
    item_id: String,
    content_index: u32,
}

#[derive(Debug, Clone)]
struct OpenAiItemRevisionState {
    sequence_id: u64,
    utterance_id: String,
    revision: u64,
    last_event_id: Option<String>,
    last_text: String,
    finalized: bool,
    range: LocalFrameRange,
}

#[derive(Debug)]
struct OpenAiTranscriptAssembler {
    context: StreamingTranscriptContext,
    model: String,
    next_sequence: u64,
    items: HashMap<OpenAiItemKey, OpenAiItemRevisionState>,
    seen_provider_events: HashSet<String>,
}

impl OpenAiTranscriptAssembler {
    fn new(context: StreamingTranscriptContext, model: String, first_sequence: u64) -> Self {
        Self {
            context,
            model,
            next_sequence: first_sequence,
            items: HashMap::new(),
            seen_provider_events: HashSet::new(),
        }
    }

    fn apply_delta(
        &mut self,
        update: OpenAiTranscriptDeltaUpdate,
        range: LocalFrameRange,
        source: AudioSource,
    ) -> Option<TranscriptUpdate> {
        if self.provider_event_seen(update.event.event_id.as_deref()) {
            return None;
        }
        let key = OpenAiItemKey {
            item_id: update.event.item_id,
            content_index: update.event.content_index,
        };
        let text = self
            .items
            .get(&key)
            .map(|state| {
                let mut text = state.last_text.clone();
                text.push_str(&update.event.delta);
                text
            })
            .unwrap_or(update.accumulated_transcript);
        self.emit(key, text, true, update.event.event_id, range, source, None)
    }

    fn apply_completed(
        &mut self,
        update: OpenAiTranscriptCompletedUpdate,
        range: LocalFrameRange,
        source: AudioSource,
    ) -> Option<TranscriptUpdate> {
        if self.provider_event_seen(update.event.event_id.as_deref()) {
            return None;
        }
        let key = OpenAiItemKey {
            item_id: update.event.item_id,
            content_index: update.event.content_index,
        };
        let text = if update.event.transcript.trim().is_empty() {
            update.streamed_transcript.unwrap_or_default()
        } else {
            update.event.transcript
        };
        let language = update
            .event
            .languages
            .into_iter()
            .find(|value| !value.trim().is_empty());
        self.emit(
            key,
            text,
            false,
            update.event.event_id,
            range,
            source,
            language,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn emit(
        &mut self,
        key: OpenAiItemKey,
        text: String,
        is_partial: bool,
        provider_event_id: Option<String>,
        range: LocalFrameRange,
        source: AudioSource,
        provider_language: Option<String>,
    ) -> Option<TranscriptUpdate> {
        if text.trim().is_empty() {
            return None;
        }
        if !self.items.contains_key(&key) {
            let sequence_id = self.next_sequence;
            self.next_sequence = self.next_sequence.saturating_add(1);
            self.items.insert(
                key.clone(),
                OpenAiItemRevisionState {
                    sequence_id,
                    utterance_id: format!("openai-realtime:{}:{}", key.item_id, key.content_index),
                    revision: 0,
                    last_event_id: None,
                    last_text: String::new(),
                    finalized: false,
                    range,
                },
            );
        }

        let state = self.items.get_mut(&key)?;
        if state.finalized || (is_partial && state.last_text == text) {
            return None;
        }
        state.range = range;
        let revision = if state.last_event_id.is_some() {
            state.revision.saturating_add(1)
        } else {
            0
        };
        let replaces_event_id = state.last_event_id.clone();
        let event_id = Uuid::new_v4().to_string();
        let sequence_id = state.sequence_id;
        let utterance_id = state.utterance_id.clone();
        state.revision = revision;
        state.last_event_id = Some(event_id.clone());
        state.last_text = text.clone();
        state.finalized = !is_partial;

        Some(build_local_transcript_update(
            &self.context,
            &self.model,
            sequence_id,
            utterance_id,
            revision,
            event_id,
            replaces_event_id,
            provider_event_id,
            text,
            is_partial,
            range,
            source,
            provider_language,
        ))
    }

    fn bind_item_range(&mut self, item_id: &str, range: LocalFrameRange) -> bool {
        let mut late_final = false;
        for (key, state) in &mut self.items {
            if key.item_id == item_id {
                if state.finalized {
                    late_final = true;
                } else {
                    state.range = range;
                }
            }
        }
        late_final
    }

    fn mark_failed(&mut self, item_id: &str, content_index: u32, event_id: Option<&str>) {
        if self.provider_event_seen(event_id) {
            return;
        }
        let key = OpenAiItemKey {
            item_id: item_id.to_string(),
            content_index,
        };
        if let Some(state) = self.items.get_mut(&key) {
            state.finalized = true;
        }
    }

    fn provider_event_seen(&mut self, event_id: Option<&str>) -> bool {
        event_id.is_some_and(|event_id| !self.seen_provider_events.insert(event_id.to_string()))
    }
}

#[allow(clippy::too_many_arguments)]
fn build_local_transcript_update(
    context: &StreamingTranscriptContext,
    model: &str,
    sequence_id: u64,
    utterance_id: String,
    revision: u64,
    event_id: String,
    replaces_event_id: Option<String>,
    provider_event_id: Option<String>,
    text: String,
    is_partial: bool,
    range: LocalFrameRange,
    source: AudioSource,
    provider_language: Option<String>,
) -> TranscriptUpdate {
    let start_seconds = range.start_frame as f64 / STREAMING_AUDIO_SAMPLE_RATE as f64;
    let end_seconds = range.end_frame as f64 / STREAMING_AUDIO_SAMPLE_RATE as f64;
    let mut input = TranscriptChunkInput::new(
        text,
        sequence_id,
        start_seconds,
        end_seconds,
        is_partial,
        None,
        source,
        StreamingAsrProvider::OpenAiRealtime.as_str(),
    );
    input.model = Some(model.to_string());
    input.meeting_id = context.meeting_id.clone();
    input.session_id = context.session_id.clone();
    input.language = provider_language.or_else(|| context.default_language.clone());
    input.trace_id = context.trace_id.clone();
    let mut output = TranscriptUpdate::from_legacy_chunk(input);
    output.event_id = Some(event_id);
    output.utterance_id = Some(utterance_id);
    output.revision = revision;
    output.event_kind = Some(if is_partial {
        TranscriptEventKind::Partial
    } else {
        TranscriptEventKind::Final
    });
    output.is_stable = Some(!is_partial);
    output.replaces_event_id = replaces_event_id;
    output.provider_event_id = provider_event_id;

    // The legacy field is concrete, so zero is the compatibility sentinel.
    // Optional ASR metadata remains `None` and never claims provider confidence.
    output.confidence = 0.0;
    output.asr_confidence = None;
    output.asr_latency_ms = None;
    if let Some(asr) = output.asr.as_mut() {
        asr.confidence = None;
        asr.latency_ms = None;
    }
    output.speaker = None;
    output.speaker_id = None;
    output.speaker_local_label = None;
    output.speaker_display_name = None;
    output.speaker_confidence = None;
    output.speaker_status = None;
    output
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

fn openai_error_recoverable(error: &OpenAiRealtimeError) -> bool {
    match error {
        OpenAiRealtimeError::Transport { kind, .. } => !matches!(
            kind,
            OpenAiRealtimeNetworkErrorKind::Authentication
                | OpenAiRealtimeNetworkErrorKind::Rejected
                | OpenAiRealtimeNetworkErrorKind::Tls
                | OpenAiRealtimeNetworkErrorKind::Protocol
        ),
        OpenAiRealtimeError::Parse(_) | OpenAiRealtimeError::UnexpectedBinaryMessage => true,
        OpenAiRealtimeError::Configuration(_)
        | OpenAiRealtimeError::Protocol(_)
        | OpenAiRealtimeError::Pcm(_)
        | OpenAiRealtimeError::Encode
        | OpenAiRealtimeError::MissingCredential
        | OpenAiRealtimeError::InvalidCredential
        | OpenAiRealtimeError::InvalidPcm16Payload
        | OpenAiRealtimeError::EmptyAudioPayload => false,
    }
}

fn openai_error_code(error: &OpenAiRealtimeError) -> &'static str {
    match error {
        OpenAiRealtimeError::Configuration(_) => "openai_realtime_invalid_configuration",
        OpenAiRealtimeError::Parse(_) => "openai_realtime_invalid_provider_message",
        OpenAiRealtimeError::Protocol(_) => "openai_realtime_protocol_error",
        OpenAiRealtimeError::Pcm(_) => "openai_realtime_audio_encoding_error",
        OpenAiRealtimeError::Encode => "openai_realtime_json_encoding_error",
        OpenAiRealtimeError::MissingCredential => "openai_realtime_missing_credential",
        OpenAiRealtimeError::InvalidCredential => "openai_realtime_invalid_credential",
        OpenAiRealtimeError::InvalidPcm16Payload => "openai_realtime_invalid_pcm_payload",
        OpenAiRealtimeError::EmptyAudioPayload => "openai_realtime_empty_audio_payload",
        OpenAiRealtimeError::UnexpectedBinaryMessage => "openai_realtime_unexpected_binary_message",
        OpenAiRealtimeError::Transport { kind, .. } => match kind {
            OpenAiRealtimeNetworkErrorKind::Authentication => {
                "openai_realtime_authentication_failed"
            }
            OpenAiRealtimeNetworkErrorKind::RateLimited => "openai_realtime_rate_limited",
            OpenAiRealtimeNetworkErrorKind::ServiceUnavailable => {
                "openai_realtime_service_unavailable"
            }
            OpenAiRealtimeNetworkErrorKind::Timeout => "openai_realtime_network_timeout",
            OpenAiRealtimeNetworkErrorKind::Disconnected => "openai_realtime_disconnected",
            OpenAiRealtimeNetworkErrorKind::Tls => "openai_realtime_tls_failed",
            OpenAiRealtimeNetworkErrorKind::Protocol => "openai_realtime_websocket_protocol_error",
            OpenAiRealtimeNetworkErrorKind::Rejected => "openai_realtime_connection_rejected",
            OpenAiRealtimeNetworkErrorKind::Other => "openai_realtime_network_error",
        },
    }
}

fn provider_error_recoverable(error: &OpenAiProviderError) -> bool {
    let marker = error
        .error_type
        .as_deref()
        .or(error.code.as_deref())
        .unwrap_or_default()
        .to_ascii_lowercase();
    !(marker.contains("auth")
        || marker.contains("api_key")
        || marker.contains("permission")
        || marker.contains("invalid_request"))
}

fn provider_error_code(error: &OpenAiProviderError) -> Option<&str> {
    error
        .code
        .as_deref()
        .or(error.error_type.as_deref())
        .and_then(sanitize_provider_code)
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
        OpenAiRealtimeTurnDetection, OpenAiTranscriptCompleted, OpenAiTranscriptDelta,
        STREAMING_ASR_SCHEMA_VERSION,
    };

    fn local_range(
        start_frame: u64,
        end_frame: u64,
        basis: OpenAiCanonicalTimeBasis,
    ) -> LocalFrameRange {
        LocalFrameRange::new(start_frame, end_frame, basis)
    }

    fn delta(
        event_id: &str,
        item_id: &str,
        content_index: u32,
        text: &str,
    ) -> OpenAiTranscriptDeltaUpdate {
        OpenAiTranscriptDeltaUpdate {
            event: OpenAiTranscriptDelta {
                event_id: Some(event_id.to_string()),
                item_id: item_id.to_string(),
                content_index,
                delta: text.to_string(),
            },
            accumulated_transcript: text.to_string(),
        }
    }

    fn completed(
        event_id: &str,
        item_id: &str,
        content_index: u32,
        text: &str,
    ) -> OpenAiTranscriptCompletedUpdate {
        OpenAiTranscriptCompletedUpdate {
            event: OpenAiTranscriptCompleted {
                event_id: Some(event_id.to_string()),
                item_id: item_id.to_string(),
                content_index,
                transcript: text.to_string(),
                languages: Vec::new(),
            },
            streamed_transcript: Some(text.to_string()),
        }
    }

    #[test]
    fn config_debug_never_contains_api_key() {
        let config = OpenAiSupervisorConfig::new(
            "session-1",
            OpenAiRealtimeOptions::default(),
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
    fn manual_commit_maps_fifo_item_ids_and_preserves_previous_item_link() {
        let mut mapper = OpenAiItemMapper::default();
        mapper.start_connection(false);
        mapper.note_audio(0, 48_000, false);
        let sealed = mapper.seal_manual_commit(48_000).unwrap();
        assert_eq!(sealed.range.basis, OpenAiCanonicalTimeBasis::ManualCommit);
        mapper.note_audio(48_000, 96_000, false);
        mapper.seal_manual_commit(96_000).unwrap();

        let first = mapper.bind_committed_item("item-a".into(), None, 96_000, None);
        let second = mapper.bind_committed_item(
            "item-b".into(),
            Some("item-a".into()),
            96_000,
            Some(48_000),
        );
        assert_eq!((first.start_frame, first.end_frame), (0, 48_000));
        assert_eq!((second.start_frame, second.end_frame), (48_000, 96_000));
        assert_eq!(
            mapper.previous_items.get("item-b"),
            Some(&Some("item-a".to_string()))
        );
    }

    #[test]
    fn automatic_commit_is_explicitly_local_observed_not_provider_timed() {
        let mut mapper = OpenAiItemMapper::default();
        mapper.start_connection(false);
        mapper.note_speech_started("vad-item".into(), 12_000);
        mapper.note_speech_stopped("vad-item".into(), 60_000);
        let range = mapper.bind_committed_item("vad-item".into(), None, 72_000, None);
        assert_eq!(range.start_frame, 12_000);
        assert_eq!(range.end_frame, 60_000);
        assert_eq!(range.basis, OpenAiCanonicalTimeBasis::ServerVadObserved);
    }

    #[test]
    fn automatic_commit_without_local_observations_stays_explicitly_unmapped() {
        let mut mapper = OpenAiItemMapper::default();
        mapper.start_connection(false);
        let range = mapper.bind_committed_item("unknown-item".into(), None, 72_000, Some(48_000));
        assert_eq!((range.start_frame, range.end_frame), (48_000, 72_000));
        assert_eq!(range.basis, OpenAiCanonicalTimeBasis::UnmappedLocalWindow);
    }

    #[test]
    fn reconnect_replay_mapping_stays_ambiguous_and_is_not_claimed_exact() {
        let mut mapper = OpenAiItemMapper::default();
        mapper.start_connection(true);
        mapper.note_audio(24_000, 96_000, true);
        let sealed = mapper.seal_manual_commit(96_000).unwrap();
        assert_eq!(
            sealed.range.basis,
            OpenAiCanonicalTimeBasis::ReplayAmbiguous
        );
        let mapped = mapper.bind_committed_item("replay-item".into(), None, 96_000, Some(72_000));
        assert_eq!(mapped.basis, OpenAiCanonicalTimeBasis::ReplayAmbiguous);
        assert_eq!((mapped.start_frame, mapped.end_frame), (24_000, 96_000));
    }

    #[test]
    fn item_and_content_index_keep_one_revision_chain_without_fake_metadata() {
        let context = StreamingTranscriptContext {
            meeting_id: Some("meeting-1".into()),
            session_id: Some("session-1".into()),
            default_language: None,
            trace_id: Some("trace-1".into()),
        };
        let mut assembler =
            OpenAiTranscriptAssembler::new(context, "gpt-live-transcribe".into(), 7);
        let range = local_range(48_000, 96_000, OpenAiCanonicalTimeBasis::ManualCommit);
        let partial = assembler
            .apply_delta(
                delta("event-1", "item-1", 0, "hello"),
                range,
                AudioSource::Mixed,
            )
            .unwrap();
        let final_update = assembler
            .apply_completed(
                completed("event-2", "item-1", 0, "hello world"),
                range,
                AudioSource::Mixed,
            )
            .unwrap();

        assert_eq!(partial.sequence_id, final_update.sequence_id);
        assert_eq!(partial.utterance_id, final_update.utterance_id);
        assert_eq!(partial.revision, 0);
        assert_eq!(final_update.revision, 1);
        assert_eq!(final_update.replaces_event_id, partial.event_id);
        assert_eq!(final_update.start_ms, Some(1_000));
        assert_eq!(final_update.end_ms, Some(2_000));
        assert_eq!(final_update.confidence, 0.0);
        assert_eq!(final_update.asr_confidence, None);
        assert_eq!(final_update.asr.as_ref().unwrap().confidence, None);
        assert_eq!(final_update.speaker, None);
        assert_eq!(final_update.speaker_id, None);
        assert_eq!(final_update.asr_latency_ms, None);
    }

    #[test]
    fn completion_order_across_items_does_not_cross_revision_chains() {
        let mut assembler = OpenAiTranscriptAssembler::new(
            StreamingTranscriptContext::default(),
            "gpt-live-transcribe".into(),
            0,
        );
        let range = local_range(0, 48_000, OpenAiCanonicalTimeBasis::ManualCommit);
        let a_partial = assembler
            .apply_delta(delta("d-a", "item-a", 0, "A"), range, AudioSource::Mixed)
            .unwrap();
        let b_partial = assembler
            .apply_delta(delta("d-b", "item-b", 0, "B"), range, AudioSource::Mixed)
            .unwrap();
        let b_final = assembler
            .apply_completed(
                completed("f-b", "item-b", 0, "B!"),
                range,
                AudioSource::Mixed,
            )
            .unwrap();
        let a_final = assembler
            .apply_completed(
                completed("f-a", "item-a", 0, "A!"),
                range,
                AudioSource::Mixed,
            )
            .unwrap();

        assert_eq!(a_final.sequence_id, a_partial.sequence_id);
        assert_eq!(b_final.sequence_id, b_partial.sequence_id);
        assert_ne!(a_final.sequence_id, b_final.sequence_id);
        assert_eq!(a_final.replaces_event_id, a_partial.event_id);
        assert_eq!(b_final.replaces_event_id, b_partial.event_id);
    }

    #[test]
    fn duplicate_delta_event_does_not_pollute_later_partial_text() {
        let mut assembler = OpenAiTranscriptAssembler::new(
            StreamingTranscriptContext::default(),
            "gpt-live-transcribe".into(),
            0,
        );
        let range = local_range(0, 48_000, OpenAiCanonicalTimeBasis::ManualCommit);
        let first = delta("delta-1", "item", 0, "hello");
        assert!(assembler
            .apply_delta(first.clone(), range, AudioSource::Mixed)
            .is_some());
        assert!(assembler
            .apply_delta(first, range, AudioSource::Mixed)
            .is_none());
        let next = OpenAiTranscriptDeltaUpdate {
            event: OpenAiTranscriptDelta {
                event_id: Some("delta-2".to_string()),
                item_id: "item".to_string(),
                content_index: 0,
                delta: " world".to_string(),
            },
            // A defensive test: the connector decoder may already have seen
            // the duplicate frame, so the supervisor relies on the new delta.
            accumulated_transcript: "hellohello world".to_string(),
        };
        let output = assembler
            .apply_delta(next, range, AudioSource::Mixed)
            .unwrap();
        assert_eq!(output.text, "hello world");
    }

    #[test]
    fn same_item_different_content_index_gets_independent_revision_chains() {
        let mut assembler = OpenAiTranscriptAssembler::new(
            StreamingTranscriptContext::default(),
            "gpt-live-transcribe".into(),
            0,
        );
        let range = local_range(0, 1, OpenAiCanonicalTimeBasis::UnmappedLocalWindow);
        let first = assembler
            .apply_delta(delta("d-0", "item", 0, "zero"), range, AudioSource::Mixed)
            .unwrap();
        let second = assembler
            .apply_delta(delta("d-1", "item", 1, "one"), range, AudioSource::Mixed)
            .unwrap();
        assert_ne!(first.sequence_id, second.sequence_id);
        assert_ne!(first.utterance_id, second.utterance_id);
    }

    #[tokio::test]
    async fn stop_before_audio_never_opens_network_and_updates_shared_health() {
        let (command_tx, command_rx) = mpsc::channel(1);
        let (output_tx, mut output_rx) = mpsc::unbounded_channel();
        command_tx.send(StreamingAsrCommand::Stop).await.unwrap();
        drop(command_tx);
        let health = SharedAsrHealthState::default();
        let exit = run_openai_supervisor(
            OpenAiSupervisorConfig::new(
                "session-1",
                OpenAiRealtimeOptions::default(),
                StreamingTranscriptContext::default(),
                0,
                "unused-secret",
            ),
            command_rx,
            output_tx,
            health.clone(),
        )
        .await;

        assert_eq!(exit, OpenAiSupervisorExit::Stopped);
        let outputs: Vec<_> = std::iter::from_fn(|| output_rx.try_recv().ok()).collect();
        assert_eq!(outputs.len(), 2);
        assert!(outputs
            .iter()
            .all(|output| matches!(output, OpenAiSupervisorOutput::Health(_))));
        let history = health.event_snapshot();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].provider, StreamingAsrProvider::OpenAiRealtime);
        assert_eq!(history[0].state, AsrHealthState::Connecting);
        assert_eq!(history[1].state, AsrHealthState::Stopped);
        assert_eq!(history[0].schema_version, STREAMING_ASR_SCHEMA_VERSION);
    }

    #[test]
    fn server_vad_mode_is_not_confused_with_local_manual_commit_mode() {
        let mut options = OpenAiRealtimeOptions::default();
        options.turn_detection = OpenAiRealtimeTurnDetection::ServerVad {
            threshold: 0.5,
            prefix_padding_ms: 300,
            silence_duration_ms: 500,
        };
        assert!(matches!(
            options.turn_detection,
            OpenAiRealtimeTurnDetection::ServerVad { .. }
        ));
    }
}
