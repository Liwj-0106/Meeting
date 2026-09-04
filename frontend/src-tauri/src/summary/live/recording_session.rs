//! Trusted recording-session bridge for live summary.
//!
//! The renderer never submits transcript text or evidence here. Every event
//! originates after `TranscriptPersistenceSink` accepted the canonical Rust
//! event. A random public scope labels UI state while a separate one-time
//! handle and exact recording-folder correlation authorize save-time binding.

use super::{
    DispatchOutcome, LiveSummaryActor, LiveSummaryActorConfig, LiveSummaryActorError,
    LiveSummaryDispatchState, LiveSummaryFrontendError, LiveSummaryLifecycle, LiveSummaryProvider,
    LiveSummaryProviderAvailability, LiveSummaryProviderFactory, LiveSummaryProviderResponse,
    LiveSummaryPublicSnapshot, LiveSummaryRevision, LiveSummaryScope, SummaryResponseOutcome,
    SummaryResponsePreparation, SummarySourceChange, SummarySubmitOutcome,
};
use crate::audio::recording_saver::TranscriptSegment;
use crate::audio::transcription::event::TranscriptEvent;
use crate::database::repositories::recording_session_binding::RecordingSessionBindingRepository;
use once_cell::sync::OnceCell;
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, Mutex};
use uuid::Uuid;

const DEFAULT_MAX_SESSIONS: usize = 4;
const DEFAULT_MAX_EVENTS_PER_SESSION: usize = 20_000;
const DEFAULT_PENDING_TTL_MS: u64 = 30 * 60 * 1_000;
const DEFAULT_ACTIVE_TTL_MS: u64 = 12 * 60 * 60 * 1_000;
const USED_HANDLE_TTL_MS: u64 = 60 * 60 * 1_000;
const MAX_USED_HANDLE_TOMBSTONES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingSummaryState {
    Active,
    PendingBinding,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingLiveSummarySnapshot {
    pub state: RecordingSummaryState,
    pub bindable: bool,
    pub summary: LiveSummaryPublicSnapshot,
}

/// Intentionally serializable because the renderer must return this exact
/// Rust-issued value once. It does not implement `Debug` or `Display`.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingSummaryBindingTicket {
    pub scope_id: String,
    pub handle: String,
}

pub(crate) struct PreparedRecordingSummaryBinding {
    pub(crate) scope_id: String,
    pub(crate) source_session_id: String,
    pub(crate) recording_folder_hash: String,
    pub(crate) trusted_segments: Vec<TranscriptSegment>,
}

impl std::fmt::Debug for PreparedRecordingSummaryBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedRecordingSummaryBinding")
            .field("scope_id", &self.scope_id)
            .field("source_session_id", &self.source_session_id)
            .field("recording_folder_hash", &self.recording_folder_hash)
            .field("trusted_segment_count", &self.trusted_segments.len())
            .finish()
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RecordingSummaryRegistryError {
    #[error("trusted transcript event has no recording session")]
    MissingSourceSession,
    #[error("trusted transcript event is not canonical")]
    InvalidTranscriptEvent,
    #[error("recording summary registry reached its session capacity")]
    SessionCapacityReached,
    #[error("recording summary session reached its transcript capacity")]
    EventCapacityReached,
    #[error("recording folder correlation is missing")]
    MissingFolderCorrelation,
    #[error("recording folder does not match the trusted session")]
    FolderMismatch,
    #[error("recording summary binding handle is invalid")]
    InvalidHandle,
    #[error("recording summary binding handle was already used")]
    AlreadyBound,
    #[error("recording summary binding handle expired")]
    Expired,
    #[error("recording summary session is still active")]
    SessionStillActive,
    #[error("recording summary binding is already in progress")]
    BindingInProgress,
    #[error("saved meeting does not match the pending binding")]
    MeetingMismatch,
    #[error(transparent)]
    Actor(#[from] LiveSummaryActorError),
}

impl RecordingSummaryRegistryError {
    pub(crate) fn frontend_error(&self) -> LiveSummaryFrontendError {
        let (code, message, retryable) = match self {
            Self::MissingSourceSession | Self::InvalidTranscriptEvent => (
                "live_summary_recording_event_invalid",
                "录音转写没有形成可绑定的可信总结范围。",
                false,
            ),
            Self::SessionCapacityReached | Self::EventCapacityReached => (
                "live_summary_recording_capacity_reached",
                "实时总结已达到本机会话容量，录音和转写仍会继续保存。",
                false,
            ),
            Self::MissingFolderCorrelation | Self::FolderMismatch => (
                "live_summary_recording_folder_mismatch",
                "录音目录与实时总结会话不匹配，已拒绝绑定。",
                false,
            ),
            Self::InvalidHandle => (
                "live_summary_binding_handle_invalid",
                "实时总结绑定凭据无效。",
                false,
            ),
            Self::AlreadyBound => (
                "live_summary_binding_already_used",
                "该实时总结会话已经绑定。",
                false,
            ),
            Self::Expired => (
                "live_summary_binding_expired",
                "待绑定的实时总结会话已经过期。",
                false,
            ),
            Self::SessionStillActive => (
                "live_summary_recording_still_active",
                "录音仍在进行，暂不能绑定会议。",
                false,
            ),
            Self::BindingInProgress => (
                "live_summary_binding_in_progress",
                "该实时总结会话正在绑定，请稍后重试。",
                true,
            ),
            Self::MeetingMismatch => (
                "live_summary_binding_meeting_mismatch",
                "保存后的会议与待绑定会话不匹配。",
                false,
            ),
            Self::Actor(_) => (
                "live_summary_recording_actor_failed",
                "实时总结无法处理该条可信转写，录音和字幕不受影响。",
                false,
            ),
        };
        LiveSummaryFrontendError {
            code: code.to_string(),
            message: message.to_string(),
            retryable,
        }
    }
}

pub(crate) enum RecordingSummaryIngress {
    Transcript {
        segment: TranscriptSegment,
        meeting_folder: Option<PathBuf>,
    },
    Pending {
        source_session_id: String,
        meeting_folder: Option<PathBuf>,
    },
    Barrier(oneshot::Sender<()>),
}

static RECORDING_SUMMARY_INGRESS: OnceCell<mpsc::UnboundedSender<RecordingSummaryIngress>> =
    OnceCell::new();

pub(crate) fn install_recording_summary_ingress(
    sender: mpsc::UnboundedSender<RecordingSummaryIngress>,
) {
    let _ = RECORDING_SUMMARY_INGRESS.set(sender);
}

pub(crate) fn observe_trusted_transcript(
    segment: TranscriptSegment,
    meeting_folder: Option<PathBuf>,
) {
    if let Some(sender) = RECORDING_SUMMARY_INGRESS.get() {
        let _ = sender.send(RecordingSummaryIngress::Transcript {
            segment,
            meeting_folder,
        });
    }
}

pub(crate) fn mark_trusted_recording_pending(
    segments: &[TranscriptSegment],
    meeting_folder: Option<PathBuf>,
) {
    let session_ids = segments
        .iter()
        .filter_map(|segment| segment.session_id.as_deref())
        .filter(|value| !value.trim().is_empty())
        .collect::<HashSet<_>>();
    let Some(source_session_id) = session_ids.iter().next() else {
        return;
    };
    if session_ids.len() != 1 {
        return;
    }
    if let Some(sender) = RECORDING_SUMMARY_INGRESS.get() {
        let _ = sender.send(RecordingSummaryIngress::Pending {
            source_session_id: (*source_session_id).to_string(),
            meeting_folder,
        });
    }
}

#[derive(Debug, Clone, Copy)]
struct RegistryLimits {
    max_sessions: usize,
    max_events_per_session: usize,
    pending_ttl_ms: u64,
    active_ttl_ms: u64,
}

impl Default for RegistryLimits {
    fn default() -> Self {
        Self {
            max_sessions: DEFAULT_MAX_SESSIONS,
            max_events_per_session: DEFAULT_MAX_EVENTS_PER_SESSION,
            pending_ttl_ms: DEFAULT_PENDING_TTL_MS,
            active_ttl_ms: DEFAULT_ACTIVE_TTL_MS,
        }
    }
}

struct RecordingSession<P: LiveSummaryProvider> {
    source_session_id: String,
    scope: LiveSummaryScope,
    binding_handle: String,
    folder_key: Option<String>,
    state: RecordingSummaryState,
    created_at_ms: u64,
    updated_at_ms: u64,
    pending_at_ms: Option<u64>,
    binding_in_progress: bool,
    saved_meeting_id: Option<String>,
    bound_revision: Option<LiveSummaryRevision>,
    template_id: String,
    actor: LiveSummaryActor<P>,
    latest_revision: Option<LiveSummaryRevision>,
    last_error: Option<LiveSummaryFrontendError>,
    dispatch_state: LiveSummaryDispatchState,
    trusted_segments: Vec<TranscriptSegment>,
    trusted_history_complete: bool,
    seen_event_ids: HashSet<String>,
    deferred_changes: VecDeque<SummarySourceChange>,
}

impl<P: LiveSummaryProvider> RecordingSession<P> {
    fn apply_dispatch(&mut self, dispatch: DispatchOutcome) {
        match dispatch {
            DispatchOutcome::Started { .. } | DispatchOutcome::WaitingForInFlight => {
                self.dispatch_state = LiveSummaryDispatchState::InFlight;
                self.last_error = None;
            }
            DispatchOutcome::Idle => self.dispatch_state = LiveSummaryDispatchState::Idle,
            DispatchOutcome::Deferred { error } => {
                self.dispatch_state = LiveSummaryDispatchState::Deferred;
                self.last_error = Some(LiveSummaryFrontendError {
                    code: error.code.to_string(),
                    message: "实时总结模型暂时不可用，可信转写仍会保留。".to_string(),
                    retryable: error.retryable,
                });
            }
        }
    }

    fn drain_deferred(&mut self) -> Result<(), LiveSummaryActorError> {
        while let Some(change) = self.deferred_changes.pop_front() {
            match self.actor.submit_change(change)? {
                SummarySubmitOutcome::Accepted { dispatch, .. } => self.apply_dispatch(dispatch),
                SummarySubmitOutcome::RetryRequired { event, .. } => {
                    self.deferred_changes.push_front(event);
                    self.dispatch_state = LiveSummaryDispatchState::Deferred;
                    break;
                }
                SummarySubmitOutcome::Duplicate { .. } | SummarySubmitOutcome::Stale { .. } => {}
                SummarySubmitOutcome::IgnoredPartial { .. }
                | SummarySubmitOutcome::IgnoredUnsupported { .. } => {}
            }
        }
        Ok(())
    }

    fn snapshot(&self, provider: LiveSummaryProviderAvailability) -> RecordingLiveSummarySnapshot {
        let (summary_revision, generation) = self.actor.counters();
        let lifecycle = if self.actor.is_finalized() {
            LiveSummaryLifecycle::Finalized
        } else if self.dispatch_state == LiveSummaryDispatchState::InFlight {
            LiveSummaryLifecycle::Generating
        } else if !provider.available {
            LiveSummaryLifecycle::Unavailable
        } else if self.last_error.is_some() {
            LiveSummaryLifecycle::Error
        } else {
            LiveSummaryLifecycle::Active
        };
        let scope_id = self.scope.id().to_string();
        RecordingLiveSummarySnapshot {
            state: self.state,
            bindable: self.state == RecordingSummaryState::PendingBinding
                && self.folder_key.is_some()
                && !self.binding_in_progress,
            summary: LiveSummaryPublicSnapshot {
                schema_version: 1,
                scope: self.scope.clone(),
                meeting_id: None,
                session_scope_id: Some(scope_id),
                template_id: Some(self.template_id.clone()),
                lifecycle,
                dispatch_state: self.dispatch_state.clone(),
                provider,
                transcript_cursor: self.actor.transcript_cursor(),
                summary_revision,
                generation,
                finalized: self.actor.is_finalized(),
                recovered: false,
                updated_at: self
                    .latest_revision
                    .as_ref()
                    .map(|revision| revision.created_at.clone()),
                last_complete_revision: self.latest_revision.clone(),
                items: self.actor.current_items().to_vec(),
                error: self.last_error.clone(),
            },
        }
    }
}

struct UsedHandle {
    hash: String,
    expires_at_ms: u64,
    outcome: UsedHandleOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsedHandleOutcome {
    Bound,
    Expired,
}

struct RegistryState<P: LiveSummaryProvider> {
    sessions: HashMap<String, RecordingSession<P>>,
    used_handles: VecDeque<UsedHandle>,
}

impl<P: LiveSummaryProvider> Default for RegistryState<P> {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            used_handles: VecDeque::new(),
        }
    }
}

pub struct RecordingLiveSummaryRegistry<F: LiveSummaryProviderFactory> {
    factory: F,
    queue_capacity: usize,
    limits: RegistryLimits,
    state: Mutex<RegistryState<F::Provider>>,
}

impl<F: LiveSummaryProviderFactory> RecordingLiveSummaryRegistry<F> {
    pub fn new(factory: F, queue_capacity: usize) -> Self {
        Self {
            factory,
            queue_capacity,
            limits: RegistryLimits::default(),
            state: Mutex::new(RegistryState::default()),
        }
    }

    #[cfg(test)]
    fn with_limits(factory: F, queue_capacity: usize, limits: RegistryLimits) -> Self {
        Self {
            factory,
            queue_capacity,
            limits,
            state: Mutex::new(RegistryState::default()),
        }
    }

    /// Create the trusted summary scope at the recording start boundary. The
    /// backend ASR session and exact recording folder both originate in Rust;
    /// the renderer receives only the opaque public scope and one-time handle.
    pub async fn prepare_session(
        &self,
        source_session_id: &str,
        meeting_folder: &Path,
    ) -> Result<
        (RecordingSummaryBindingTicket, RecordingLiveSummarySnapshot),
        RecordingSummaryRegistryError,
    > {
        let requested_folder = folder_key(meeting_folder)
            .ok_or(RecordingSummaryRegistryError::MissingFolderCorrelation)?;
        let mut state = self.state.lock().await;
        let now = now_ms();
        self.prune_locked(&mut state, now);

        if let Some(session) = state.sessions.get(source_session_id) {
            if session.folder_key.as_deref() != Some(requested_folder.as_str()) {
                return Err(RecordingSummaryRegistryError::FolderMismatch);
            }
            if session.state != RecordingSummaryState::Active {
                return Err(RecordingSummaryRegistryError::SessionStillActive);
            }
            return Ok((
                RecordingSummaryBindingTicket {
                    scope_id: session.scope.id().to_string(),
                    handle: session.binding_handle.clone(),
                },
                session.snapshot(self.factory.availability()),
            ));
        }

        self.ensure_capacity_locked(&mut state, now)?;
        let scope_id = format!("recording-summary-{}", Uuid::new_v4());
        let binding_handle = format!("summary-bind-{}-{}", Uuid::new_v4(), Uuid::new_v4());
        let actor = LiveSummaryActor::new(
            LiveSummaryActorConfig::for_recording_session(
                scope_id.clone(),
                source_session_id,
                self.queue_capacity,
            ),
            self.factory.create(),
        )?;
        let session = RecordingSession {
            source_session_id: source_session_id.to_string(),
            scope: LiveSummaryScope::recording_session(scope_id.clone()),
            binding_handle: binding_handle.clone(),
            folder_key: Some(requested_folder),
            state: RecordingSummaryState::Active,
            created_at_ms: now,
            updated_at_ms: now,
            pending_at_ms: None,
            binding_in_progress: false,
            saved_meeting_id: None,
            bound_revision: None,
            template_id: "standard_meeting".to_string(),
            actor,
            latest_revision: None,
            last_error: None,
            dispatch_state: LiveSummaryDispatchState::Idle,
            trusted_segments: Vec::new(),
            trusted_history_complete: true,
            seen_event_ids: HashSet::new(),
            deferred_changes: VecDeque::new(),
        };
        let snapshot = session.snapshot(self.factory.availability());
        state
            .sessions
            .insert(source_session_id.to_string(), session);
        Ok((
            RecordingSummaryBindingTicket {
                scope_id,
                handle: binding_handle,
            },
            snapshot,
        ))
    }

    pub async fn accept_segment(
        &self,
        segment: TranscriptSegment,
        meeting_folder: Option<PathBuf>,
    ) -> Result<Option<RecordingLiveSummarySnapshot>, RecordingSummaryRegistryError> {
        self.accept_segment_at(segment, meeting_folder, now_ms())
            .await
    }

    /// Rebuild a pending in-memory actor from the bounded, native recording
    /// recovery log after an application restart. The caller supplies the
    /// source session recovered from the durable folder correlation; every
    /// event is validated before the registry is mutated.
    pub async fn recover_pending_session(
        &self,
        source_session_id: &str,
        meeting_folder: &Path,
        trusted_segments: Vec<TranscriptSegment>,
    ) -> Result<
        (RecordingSummaryBindingTicket, RecordingLiveSummarySnapshot),
        RecordingSummaryRegistryError,
    > {
        let requested_folder = folder_key(meeting_folder)
            .ok_or(RecordingSummaryRegistryError::MissingFolderCorrelation)?;
        let mut unique_event_ids = HashSet::new();
        for segment in &trusted_segments {
            let event = canonical_event(segment)?;
            if event.meeting_id.is_some() || event.session_id.as_deref() != Some(source_session_id)
            {
                return Err(RecordingSummaryRegistryError::InvalidTranscriptEvent);
            }
            unique_event_ids.insert(event.event_id);
        }
        if unique_event_ids.len() > self.limits.max_events_per_session {
            return Err(RecordingSummaryRegistryError::EventCapacityReached);
        }

        {
            let mut state = self.state.lock().await;
            let now = now_ms();
            self.prune_locked(&mut state, now);
            if let Some(session) = state.sessions.get_mut(source_session_id) {
                if session.folder_key.as_deref() != Some(requested_folder.as_str()) {
                    return Err(RecordingSummaryRegistryError::FolderMismatch);
                }
                if session.binding_in_progress {
                    return Err(RecordingSummaryRegistryError::BindingInProgress);
                }
                if !session.trusted_history_complete {
                    return Err(RecordingSummaryRegistryError::EventCapacityReached);
                }
                if session.state == RecordingSummaryState::Active {
                    session.state = RecordingSummaryState::PendingBinding;
                    session.pending_at_ms = Some(now);
                    session.updated_at_ms = now;
                    let dispatch = session.actor.request_final_reconcile();
                    session.apply_dispatch(dispatch);
                }
                return Ok((
                    RecordingSummaryBindingTicket {
                        scope_id: session.scope.id().to_string(),
                        handle: session.binding_handle.clone(),
                    },
                    session.snapshot(self.factory.availability()),
                ));
            }
        }

        let (ticket, _) = self
            .prepare_session(source_session_id, meeting_folder)
            .await?;
        for segment in trusted_segments {
            if let Err(error) = self
                .accept_segment(segment, Some(meeting_folder.to_path_buf()))
                .await
            {
                self.state.lock().await.sessions.remove(source_session_id);
                return Err(error);
            }
        }
        let pending = match self
            .mark_pending(source_session_id, Some(meeting_folder.to_path_buf()))
            .await
        {
            Ok(pending) => pending,
            Err(error) => {
                self.state.lock().await.sessions.remove(source_session_id);
                return Err(error);
            }
        };
        Ok((ticket, pending))
    }

    async fn accept_segment_at(
        &self,
        segment: TranscriptSegment,
        meeting_folder: Option<PathBuf>,
        now: u64,
    ) -> Result<Option<RecordingLiveSummarySnapshot>, RecordingSummaryRegistryError> {
        let event = canonical_event(&segment)?;
        let source_session_id = event
            .session_id
            .clone()
            .filter(|value| !value.trim().is_empty())
            .ok_or(RecordingSummaryRegistryError::MissingSourceSession)?;
        if event.meeting_id.is_some() {
            return Ok(None);
        }
        let incoming_folder = meeting_folder.as_deref().and_then(folder_key);
        let mut state = self.state.lock().await;
        self.prune_locked(&mut state, now);
        if !state.sessions.contains_key(&source_session_id) {
            self.ensure_capacity_locked(&mut state, now)?;
            let scope_id = format!("recording-summary-{}", Uuid::new_v4());
            let binding_handle = format!("summary-bind-{}-{}", Uuid::new_v4(), Uuid::new_v4());
            let actor = LiveSummaryActor::new(
                LiveSummaryActorConfig::for_recording_session(
                    scope_id.clone(),
                    source_session_id.clone(),
                    self.queue_capacity,
                ),
                self.factory.create(),
            )?;
            state.sessions.insert(
                source_session_id.clone(),
                RecordingSession {
                    source_session_id: source_session_id.clone(),
                    scope: LiveSummaryScope::recording_session(scope_id),
                    binding_handle,
                    folder_key: incoming_folder.clone(),
                    state: RecordingSummaryState::Active,
                    created_at_ms: now,
                    updated_at_ms: now,
                    pending_at_ms: None,
                    binding_in_progress: false,
                    saved_meeting_id: None,
                    bound_revision: None,
                    template_id: "standard_meeting".to_string(),
                    actor,
                    latest_revision: None,
                    last_error: None,
                    dispatch_state: LiveSummaryDispatchState::Idle,
                    trusted_segments: Vec::new(),
                    trusted_history_complete: true,
                    seen_event_ids: HashSet::new(),
                    deferred_changes: VecDeque::new(),
                },
            );
        }
        let session = state
            .sessions
            .get_mut(&source_session_id)
            .expect("recording session was inserted or already existed");
        if session.state != RecordingSummaryState::Active {
            return Err(RecordingSummaryRegistryError::InvalidTranscriptEvent);
        }
        if let Some(incoming_folder) = incoming_folder {
            match session.folder_key.as_deref() {
                Some(existing) if existing != incoming_folder => {
                    return Err(RecordingSummaryRegistryError::FolderMismatch)
                }
                None => session.folder_key = Some(incoming_folder),
                _ => {}
            }
        }
        session.updated_at_ms = now;
        if session.seen_event_ids.insert(event.event_id.clone()) {
            if session.trusted_segments.len() >= self.limits.max_events_per_session {
                session.trusted_history_complete = false;
                session.last_error =
                    Some(RecordingSummaryRegistryError::EventCapacityReached.frontend_error());
                return Err(RecordingSummaryRegistryError::EventCapacityReached);
            }
            session.trusted_segments.push(segment);
        }
        match session.actor.submit_event(event)? {
            SummarySubmitOutcome::Accepted { dispatch, .. } => session.apply_dispatch(dispatch),
            SummarySubmitOutcome::RetryRequired { event, .. } => {
                session.deferred_changes.push_back(event);
                session.dispatch_state = LiveSummaryDispatchState::Deferred;
            }
            SummarySubmitOutcome::Duplicate { .. }
            | SummarySubmitOutcome::Stale { .. }
            | SummarySubmitOutcome::IgnoredPartial { .. }
            | SummarySubmitOutcome::IgnoredUnsupported { .. } => {}
        }
        Ok(Some(session.snapshot(self.factory.availability())))
    }

    pub async fn mark_pending(
        &self,
        source_session_id: &str,
        meeting_folder: Option<PathBuf>,
    ) -> Result<RecordingLiveSummarySnapshot, RecordingSummaryRegistryError> {
        self.mark_pending_at(source_session_id, meeting_folder, now_ms())
            .await
    }

    async fn mark_pending_at(
        &self,
        source_session_id: &str,
        meeting_folder: Option<PathBuf>,
        now: u64,
    ) -> Result<RecordingLiveSummarySnapshot, RecordingSummaryRegistryError> {
        let incoming_folder = meeting_folder
            .as_deref()
            .and_then(folder_key)
            .ok_or(RecordingSummaryRegistryError::MissingFolderCorrelation)?;
        let mut state = self.state.lock().await;
        self.prune_locked(&mut state, now);
        let session = state
            .sessions
            .get_mut(source_session_id)
            .ok_or(RecordingSummaryRegistryError::InvalidHandle)?;
        match session.folder_key.as_deref() {
            Some(existing) if existing != incoming_folder => {
                return Err(RecordingSummaryRegistryError::FolderMismatch)
            }
            None => session.folder_key = Some(incoming_folder),
            _ => {}
        }
        if session.state == RecordingSummaryState::PendingBinding {
            return Ok(session.snapshot(self.factory.availability()));
        }
        session.state = RecordingSummaryState::PendingBinding;
        session.pending_at_ms = Some(now);
        session.updated_at_ms = now;
        let dispatch = session.actor.request_final_reconcile();
        session.apply_dispatch(dispatch);
        Ok(session.snapshot(self.factory.availability()))
    }

    /// Stop-time fallback for transcript-free recordings. The exact folder is
    /// already unique and Rust-created, so no renderer identity is consulted.
    pub async fn mark_pending_by_folder(
        &self,
        meeting_folder: &Path,
    ) -> Result<RecordingLiveSummarySnapshot, RecordingSummaryRegistryError> {
        let requested_folder = folder_key(meeting_folder)
            .ok_or(RecordingSummaryRegistryError::MissingFolderCorrelation)?;
        let mut state = self.state.lock().await;
        let now = now_ms();
        self.prune_locked(&mut state, now);
        let session = state
            .sessions
            .values_mut()
            .find(|session| session.folder_key.as_deref() == Some(requested_folder.as_str()))
            .ok_or(RecordingSummaryRegistryError::FolderMismatch)?;
        if session.state == RecordingSummaryState::PendingBinding {
            return Ok(session.snapshot(self.factory.availability()));
        }
        session.state = RecordingSummaryState::PendingBinding;
        session.pending_at_ms = Some(now);
        session.updated_at_ms = now;
        let dispatch = session.actor.request_final_reconcile();
        session.apply_dispatch(dispatch);
        Ok(session.snapshot(self.factory.availability()))
    }

    pub async fn reconfigure(&self, template_id: String) -> Result<(), LiveSummaryActorError> {
        let availability = self.factory.availability();
        let mut state = self.state.lock().await;
        for session in state.sessions.values_mut() {
            let same_provider = session.actor.provider().provider_id() == availability.provider
                && session.actor.provider().model_id() == availability.model.as_deref();
            session.template_id = template_id.clone();
            if same_provider {
                continue;
            }
            let mut actor = LiveSummaryActor::new(
                LiveSummaryActorConfig::for_recording_session(
                    session.scope.id(),
                    &session.source_session_id,
                    self.queue_capacity,
                ),
                self.factory.create(),
            )?;
            for segment in &session.trusted_segments {
                let event = canonical_event(segment)
                    .map_err(|_| LiveSummaryActorError::InvalidStableEvent("invalid replay"))?;
                let _ = actor.submit_event(event)?;
            }
            session.actor = actor;
            session.latest_revision = None;
            session.last_error = None;
            session.dispatch_state = if session.actor.in_flight_request().is_some() {
                LiveSummaryDispatchState::InFlight
            } else {
                LiveSummaryDispatchState::Idle
            };
        }
        Ok(())
    }

    pub async fn handle_provider_response(
        &self,
        scope_id: &str,
        response: LiveSummaryProviderResponse,
    ) -> Result<RecordingLiveSummarySnapshot, RecordingSummaryRegistryError> {
        let mut state = self.state.lock().await;
        let session = state
            .sessions
            .values_mut()
            .find(|session| session.scope.id() == scope_id)
            .ok_or(RecordingSummaryRegistryError::InvalidHandle)?;
        match session.actor.prepare_response(response) {
            SummaryResponsePreparation::Ready(prepared) => {
                match session.actor.commit_prepared_response(prepared)? {
                    SummaryResponseOutcome::Committed {
                        revision,
                        next_dispatch,
                    } => {
                        session.latest_revision = Some(revision);
                        session.apply_dispatch(next_dispatch);
                    }
                    _ => unreachable!("prepared response always commits"),
                }
            }
            SummaryResponsePreparation::Resolved(outcome) => match outcome {
                SummaryResponseOutcome::Committed { .. } => unreachable!(),
                SummaryResponseOutcome::IgnoredStale { next_dispatch, .. } => {
                    session.apply_dispatch(next_dispatch)
                }
                SummaryResponseOutcome::Rejected {
                    error,
                    next_dispatch,
                } => {
                    session.apply_dispatch(next_dispatch);
                    session.last_error =
                        Some(super::LiveSummaryCoordinatorError::Actor(error).frontend_error());
                }
                SummaryResponseOutcome::ProviderFailed {
                    error,
                    next_dispatch,
                } => {
                    session.apply_dispatch(next_dispatch);
                    session.last_error = Some(LiveSummaryFrontendError {
                        code: error.code.to_string(),
                        message: "实时总结模型调用失败，已保留上一版可信结果。".to_string(),
                        retryable: error.retryable,
                    });
                }
            },
        }
        session.drain_deferred()?;
        Ok(session.snapshot(self.factory.availability()))
    }

    pub async fn list_snapshots(&self) -> Vec<RecordingLiveSummarySnapshot> {
        let mut state = self.state.lock().await;
        self.prune_locked(&mut state, now_ms());
        let provider = self.factory.availability();
        let mut snapshots = state
            .sessions
            .values()
            .map(|session| session.snapshot(provider.clone()))
            .collect::<Vec<_>>();
        snapshots.sort_by_key(|snapshot| snapshot.summary.session_scope_id.clone());
        snapshots
    }

    pub async fn issue_binding_ticket(
        &self,
        meeting_folder: &Path,
    ) -> Result<RecordingSummaryBindingTicket, RecordingSummaryRegistryError> {
        let requested_folder = folder_key(meeting_folder)
            .ok_or(RecordingSummaryRegistryError::MissingFolderCorrelation)?;
        let mut state = self.state.lock().await;
        self.prune_locked(&mut state, now_ms());
        let session = state
            .sessions
            .values()
            .find(|session| session.folder_key.as_deref() == Some(requested_folder.as_str()))
            .ok_or(RecordingSummaryRegistryError::FolderMismatch)?;
        if session.state != RecordingSummaryState::PendingBinding {
            return Err(RecordingSummaryRegistryError::SessionStillActive);
        }
        if session.binding_in_progress {
            return Err(RecordingSummaryRegistryError::BindingInProgress);
        }
        Ok(RecordingSummaryBindingTicket {
            scope_id: session.scope.id().to_string(),
            handle: session.binding_handle.clone(),
        })
    }

    pub(crate) async fn prepare_binding(
        &self,
        handle: &str,
        meeting_folder: &Path,
    ) -> Result<PreparedRecordingSummaryBinding, RecordingSummaryRegistryError> {
        let requested_folder = folder_key(meeting_folder)
            .ok_or(RecordingSummaryRegistryError::MissingFolderCorrelation)?;
        let mut state = self.state.lock().await;
        self.prune_locked(&mut state, now_ms());
        if let Some(outcome) = used_handle_outcome(&state.used_handles, handle) {
            return Err(match outcome {
                UsedHandleOutcome::Bound => RecordingSummaryRegistryError::AlreadyBound,
                UsedHandleOutcome::Expired => RecordingSummaryRegistryError::Expired,
            });
        }
        let session = state
            .sessions
            .values_mut()
            .find(|session| constant_time_eq(&session.binding_handle, handle))
            .ok_or(RecordingSummaryRegistryError::InvalidHandle)?;
        if session.folder_key.as_deref() != Some(requested_folder.as_str()) {
            return Err(RecordingSummaryRegistryError::FolderMismatch);
        }
        if session.state != RecordingSummaryState::PendingBinding {
            return Err(RecordingSummaryRegistryError::SessionStillActive);
        }
        if session.binding_in_progress {
            return Err(RecordingSummaryRegistryError::BindingInProgress);
        }
        if !session.trusted_history_complete {
            return Err(RecordingSummaryRegistryError::EventCapacityReached);
        }
        session.binding_in_progress = true;
        Ok(PreparedRecordingSummaryBinding {
            scope_id: session.scope.id().to_string(),
            source_session_id: session.source_session_id.clone(),
            recording_folder_hash: requested_folder,
            trusted_segments: session.trusted_segments.clone(),
        })
    }

    pub async fn prepare_bound_revision(
        &self,
        handle: &str,
        meeting_id: &str,
    ) -> Result<Option<LiveSummaryRevision>, RecordingSummaryRegistryError> {
        let mut state = self.state.lock().await;
        let session = state
            .sessions
            .values_mut()
            .find(|session| constant_time_eq(&session.binding_handle, handle))
            .ok_or(RecordingSummaryRegistryError::InvalidHandle)?;
        if !session.binding_in_progress {
            return Err(RecordingSummaryRegistryError::InvalidHandle);
        }
        match session.saved_meeting_id.as_deref() {
            Some(existing) if existing != meeting_id => {
                return Err(RecordingSummaryRegistryError::MeetingMismatch)
            }
            None => session.saved_meeting_id = Some(meeting_id.to_string()),
            _ => {}
        }
        if session.bound_revision.is_none() {
            session.bound_revision = session
                .latest_revision
                .as_ref()
                .map(|revision| {
                    session
                        .actor
                        .rebind_revision_to_meeting(revision, meeting_id)
                        .map_err(RecordingSummaryRegistryError::from)
                })
                .transpose()?;
        }
        Ok(session.bound_revision.clone())
    }

    pub async fn abort_binding(&self, handle: &str) {
        let mut state = self.state.lock().await;
        if let Some(session) = state
            .sessions
            .values_mut()
            .find(|session| constant_time_eq(&session.binding_handle, handle))
        {
            session.binding_in_progress = false;
        }
    }

    pub async fn complete_binding(
        &self,
        handle: &str,
        meeting_id: &str,
    ) -> Result<(), RecordingSummaryRegistryError> {
        let mut state = self.state.lock().await;
        let source_session_id = state
            .sessions
            .iter()
            .find(|(_, session)| constant_time_eq(&session.binding_handle, handle))
            .map(|(source_session_id, _)| source_session_id.clone())
            .ok_or(RecordingSummaryRegistryError::InvalidHandle)?;
        let session = state
            .sessions
            .get(&source_session_id)
            .expect("binding session was found");
        if !session.binding_in_progress || session.saved_meeting_id.as_deref() != Some(meeting_id) {
            return Err(RecordingSummaryRegistryError::MeetingMismatch);
        }
        let handle_hash = super::source_text_hash(&session.binding_handle);
        state.sessions.remove(&source_session_id);
        remember_used_handle(
            &mut state.used_handles,
            handle_hash,
            UsedHandleOutcome::Bound,
            now_ms(),
        );
        Ok(())
    }

    fn ensure_capacity_locked(
        &self,
        state: &mut RegistryState<F::Provider>,
        now: u64,
    ) -> Result<(), RecordingSummaryRegistryError> {
        if state.sessions.len() < self.limits.max_sessions {
            return Ok(());
        }
        let evict = state
            .sessions
            .iter()
            .filter(|(_, session)| session.state == RecordingSummaryState::PendingBinding)
            .min_by_key(|(_, session)| session.pending_at_ms.unwrap_or(session.updated_at_ms))
            .map(|(key, _)| key.clone());
        if let Some(key) = evict {
            if let Some(session) = state.sessions.remove(&key) {
                remember_used_handle(
                    &mut state.used_handles,
                    super::source_text_hash(&session.binding_handle),
                    UsedHandleOutcome::Expired,
                    now,
                );
                return Ok(());
            }
        }
        Err(RecordingSummaryRegistryError::SessionCapacityReached)
    }

    fn prune_locked(&self, state: &mut RegistryState<F::Provider>, now: u64) {
        state.used_handles.retain(|entry| entry.expires_at_ms > now);
        let expired = state
            .sessions
            .iter()
            .filter(|(_, session)| {
                let deadline = match session.state {
                    RecordingSummaryState::Active => session
                        .created_at_ms
                        .saturating_add(self.limits.active_ttl_ms),
                    RecordingSummaryState::PendingBinding => session
                        .pending_at_ms
                        .unwrap_or(session.updated_at_ms)
                        .saturating_add(self.limits.pending_ttl_ms),
                };
                deadline <= now && !session.binding_in_progress
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in expired {
            if let Some(session) = state.sessions.remove(&key) {
                remember_used_handle(
                    &mut state.used_handles,
                    super::source_text_hash(&session.binding_handle),
                    UsedHandleOutcome::Expired,
                    now,
                );
            }
        }
    }
}

fn canonical_event(
    segment: &TranscriptSegment,
) -> Result<TranscriptEvent, RecordingSummaryRegistryError> {
    if segment.schema_version == 0
        || segment.event_id.trim().is_empty()
        || segment.utterance_id.trim().is_empty()
        || segment.created_at.trim().is_empty()
        || segment.end_ms < segment.start_ms
    {
        return Err(RecordingSummaryRegistryError::InvalidTranscriptEvent);
    }
    Ok(TranscriptEvent {
        schema_version: segment.schema_version,
        event_id: segment.event_id.clone(),
        meeting_id: segment.meeting_id.clone(),
        session_id: segment.session_id.clone(),
        utterance_id: segment.utterance_id.clone(),
        revision: segment.revision,
        event_kind: segment.event_kind.clone(),
        is_stable: segment.is_stable,
        start_ms: segment.start_ms,
        end_ms: segment.end_ms,
        text: segment.text.clone(),
        language: segment.language.clone(),
        audio_source: segment.audio_source.clone(),
        speaker: segment.speaker.clone(),
        asr: segment.asr.clone(),
        diarization: segment.diarization.clone(),
        replaces_event_id: segment.replaces_event_id.clone(),
        provider_event_id: segment.provider_event_id.clone(),
        created_at: segment.created_at.clone(),
        trace_id: segment.trace_id.clone(),
        sequence_id: Some(segment.sequence_id),
    })
}

fn folder_key(path: &Path) -> Option<String> {
    RecordingSessionBindingRepository::recording_folder_hash(path).ok()
}

pub(crate) fn recording_folder_hash(path: &Path) -> Result<String, RecordingSummaryRegistryError> {
    folder_key(path).ok_or(RecordingSummaryRegistryError::MissingFolderCorrelation)
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.as_bytes()
        .iter()
        .zip(right.as_bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn used_handle_outcome(
    handles: &VecDeque<UsedHandle>,
    candidate: &str,
) -> Option<UsedHandleOutcome> {
    let candidate_hash = super::source_text_hash(candidate);
    handles
        .iter()
        .find(|entry| constant_time_eq(&entry.hash, &candidate_hash))
        .map(|entry| entry.outcome)
}

fn remember_used_handle(
    handles: &mut VecDeque<UsedHandle>,
    hash: String,
    outcome: UsedHandleOutcome,
    now: u64,
) {
    handles.push_back(UsedHandle {
        hash,
        expires_at_ms: now.saturating_add(USED_HANDLE_TTL_MS),
        outcome,
    });
    while handles.len() > MAX_USED_HANDLE_TOMBSTONES {
        handles.pop_front();
    }
}

fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

pub(crate) fn new_recording_summary_ingress() -> (
    mpsc::UnboundedSender<RecordingSummaryIngress>,
    mpsc::UnboundedReceiver<RecordingSummaryIngress>,
) {
    mpsc::unbounded_channel()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::transcription::event::{
        AudioSource, TranscriptEventKind, TRANSCRIPT_EVENT_SCHEMA_VERSION,
    };
    use crate::summary::live::{DeterministicFakeProvider, LiveSummaryProviderAvailability};

    #[derive(Debug, Clone, Copy)]
    struct FakeFactory;

    impl LiveSummaryProviderFactory for FakeFactory {
        type Provider = DeterministicFakeProvider;

        fn create(&self) -> Self::Provider {
            DeterministicFakeProvider::default()
        }

        fn availability(&self) -> LiveSummaryProviderAvailability {
            LiveSummaryProviderAvailability {
                available: true,
                provider: "deterministic-fake".to_string(),
                model: Some("fixture-v1".to_string()),
                error: None,
            }
        }
    }

    fn segment(session: &str, event_id: &str, sequence_id: u64) -> TranscriptSegment {
        TranscriptSegment {
            id: format!("segment-{sequence_id}"),
            text: "可信会议内容".to_string(),
            audio_start_time: sequence_id as f64,
            audio_end_time: sequence_id as f64 + 0.5,
            duration: 0.5,
            display_time: "00:00:01".to_string(),
            confidence: 0.9,
            sequence_id,
            source: "Audio".to_string(),
            chunk_start_time: sequence_id as f64,
            is_partial: false,
            schema_version: TRANSCRIPT_EVENT_SCHEMA_VERSION,
            event_id: event_id.to_string(),
            meeting_id: None,
            session_id: Some(session.to_string()),
            utterance_id: format!("utterance-{sequence_id}"),
            revision: 1,
            event_kind: TranscriptEventKind::Final,
            is_stable: true,
            start_ms: sequence_id * 1_000,
            end_ms: sequence_id * 1_000 + 500,
            language: Some("zh".to_string()),
            audio_source: AudioSource::Mixed,
            speaker: None,
            speaker_id: None,
            speaker_local_label: None,
            speaker_display_name: None,
            speaker_confidence: None,
            speaker_status: None,
            asr: None,
            asr_provider: None,
            asr_model: None,
            asr_confidence: None,
            asr_latency_ms: None,
            diarization: None,
            diarization_provider: None,
            diarization_model: None,
            diarization_model_revision: None,
            diarization_revision: None,
            diarization_window_id: None,
            diarization_window_start_frame: None,
            diarization_window_end_frame: None,
            diarization_status: None,
            diarization_latency_ms: None,
            replaces_event_id: None,
            provider_event_id: None,
            created_at: "2026-09-02T00:00:00.000Z".to_string(),
            trace_id: None,
        }
    }

    #[tokio::test]
    async fn prepared_transcript_free_session_has_stable_opaque_scope_and_exact_folder_binding() {
        let registry = RecordingLiveSummaryRegistry::new(FakeFactory, 16);
        let folder = PathBuf::from("D:/synthetic/prepared-empty");
        let (ticket, active) = registry
            .prepare_session("backend-prepared-empty", &folder)
            .await
            .expect("prepare trusted start boundary");
        assert_eq!(active.state, RecordingSummaryState::Active);
        assert!(!active.bindable);
        assert!(active.summary.meeting_id.is_none());
        assert_eq!(
            active.summary.session_scope_id.as_deref(),
            Some(ticket.scope_id.as_str())
        );
        assert_eq!(active.summary.scope.id(), ticket.scope_id);
        assert!(matches!(
            active.summary.scope,
            LiveSummaryScope::RecordingSession(_)
        ));

        let (same_ticket, same_active) = registry
            .prepare_session("backend-prepared-empty", &folder)
            .await
            .expect("same start boundary is idempotent");
        assert_eq!(same_ticket.handle, ticket.handle);
        assert_eq!(same_active.summary.scope, active.summary.scope);

        let pending = registry
            .mark_pending_by_folder(&folder)
            .await
            .expect("stop exact trusted folder");
        assert_eq!(pending.state, RecordingSummaryState::PendingBinding);
        assert!(pending.bindable);
        let prepared = registry
            .prepare_binding(&ticket.handle, &folder)
            .await
            .expect("bind transcript-free session");
        assert_eq!(prepared.scope_id, ticket.scope_id);
        assert_eq!(prepared.source_session_id, "backend-prepared-empty");
        assert_eq!(prepared.recording_folder_hash.len(), 64);
        assert!(prepared.trusted_segments.is_empty());
        assert!(registry
            .prepare_bound_revision(&ticket.handle, "meeting-empty")
            .await
            .expect("no revision is still a valid binding")
            .is_none());
        registry
            .complete_binding(&ticket.handle, "meeting-empty")
            .await
            .expect("consume one-time handle");
        assert_eq!(
            registry
                .prepare_binding(&ticket.handle, &folder)
                .await
                .expect_err("used handle cannot bind again"),
            RecordingSummaryRegistryError::AlreadyBound
        );

        let public_json = serde_json::to_value(active.summary).expect("serialize session snapshot");
        assert!(public_json.get("meetingId").is_none());
        assert_eq!(
            public_json
                .get("sessionScopeId")
                .and_then(serde_json::Value::as_str),
            Some(ticket.scope_id.as_str())
        );
        assert_eq!(
            public_json
                .get("scope")
                .and_then(|value| value.get("kind"))
                .and_then(serde_json::Value::as_str),
            Some("recording_session")
        );
    }

    #[tokio::test]
    async fn native_history_rebuilds_a_pending_session_after_process_loss() {
        let registry = RecordingLiveSummaryRegistry::new(FakeFactory, 16);
        let folder = PathBuf::from("D:/synthetic/recovered-session");
        let recovered_segments = vec![
            segment("backend-recovered", "event-recovered-one", 1),
            segment("backend-recovered", "event-recovered-two", 2),
        ];

        let (ticket, pending) = registry
            .recover_pending_session("backend-recovered", &folder, recovered_segments)
            .await
            .expect("rebuild pending session");
        assert_eq!(pending.state, RecordingSummaryState::PendingBinding);
        assert!(pending.bindable);
        assert!(pending.summary.meeting_id.is_none());

        let prepared = registry
            .prepare_binding(&ticket.handle, &folder)
            .await
            .expect("reserve recovered session");
        assert_eq!(prepared.source_session_id, "backend-recovered");
        assert_eq!(prepared.trusted_segments.len(), 2);
        assert_eq!(
            prepared.recording_folder_hash,
            recording_folder_hash(&folder).expect("folder hash")
        );
    }

    #[tokio::test]
    async fn recovery_rejects_mixed_session_history_before_mutating_registry() {
        let registry = RecordingLiveSummaryRegistry::new(FakeFactory, 16);
        let folder = PathBuf::from("D:/synthetic/recovered-mixed");
        let error = match registry
            .recover_pending_session(
                "backend-expected",
                &folder,
                vec![segment("backend-other", "event-other", 1)],
            )
            .await
        {
            Ok(_) => panic!("mixed session history must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error, RecordingSummaryRegistryError::InvalidTranscriptEvent);
        assert!(registry.list_snapshots().await.is_empty());
    }

    #[tokio::test]
    async fn session_scope_is_opaque_pending_and_one_time_bound() {
        let registry = RecordingLiveSummaryRegistry::new(FakeFactory, 16);
        let folder = PathBuf::from("D:/synthetic/session-one");
        let snapshot = registry
            .accept_segment(segment("backend-one", "event-one", 1), Some(folder.clone()))
            .await
            .expect("accept trusted event")
            .expect("recording snapshot");
        assert!(snapshot.summary.meeting_id.is_none());
        assert!(matches!(
            snapshot.summary.scope,
            LiveSummaryScope::RecordingSession(_)
        ));
        registry
            .mark_pending("backend-one", Some(folder.clone()))
            .await
            .expect("mark pending");
        let ticket = registry
            .issue_binding_ticket(&folder)
            .await
            .expect("issue exact-folder ticket");
        let prepared = registry
            .prepare_binding(&ticket.handle, &folder)
            .await
            .expect("reserve binding");
        assert_eq!(prepared.scope_id, ticket.scope_id);
        assert_eq!(prepared.trusted_segments.len(), 1);
        registry
            .prepare_bound_revision(&ticket.handle, "meeting-bound")
            .await
            .expect("prepare optional bound revision");
        registry
            .complete_binding(&ticket.handle, "meeting-bound")
            .await
            .expect("complete binding");
        assert_eq!(
            registry
                .prepare_binding(&ticket.handle, &folder)
                .await
                .expect_err("used handle must be rejected"),
            RecordingSummaryRegistryError::AlreadyBound
        );
    }

    #[tokio::test]
    async fn committed_session_revision_rebinds_every_evidence_reference_to_saved_meeting() {
        let registry = RecordingLiveSummaryRegistry::new(FakeFactory, 16);
        let folder = PathBuf::from("D:/synthetic/session-revision");
        let (ticket, started) = registry
            .prepare_session("backend-revision", &folder)
            .await
            .expect("prepare session");
        registry
            .accept_segment(
                segment("backend-revision", "event-revision", 1),
                Some(folder.clone()),
            )
            .await
            .expect("accept trusted event");
        let request = {
            let state = registry.state.lock().await;
            state
                .sessions
                .get("backend-revision")
                .and_then(|session| session.actor.in_flight_request().cloned())
                .expect("session provider request")
        };
        let committed = registry
            .handle_provider_response(
                &ticket.scope_id,
                DeterministicFakeProvider::response_for(&request, now_ms()),
            )
            .await
            .expect("commit session revision");
        let session_revision = committed
            .summary
            .last_complete_revision
            .expect("session revision");
        assert_eq!(session_revision.scope, started.summary.scope);
        assert!(session_revision
            .items
            .iter()
            .all(|summary_item| summary_item
                .evidence
                .iter()
                .all(|evidence| evidence.scope == started.summary.scope)));

        registry
            .mark_pending_by_folder(&folder)
            .await
            .expect("mark pending");
        registry
            .prepare_binding(&ticket.handle, &folder)
            .await
            .expect("reserve exact binding");
        let rebound = registry
            .prepare_bound_revision(&ticket.handle, "meeting-revision")
            .await
            .expect("rewrite revision")
            .expect("rebound head");
        assert_eq!(rebound.scope, LiveSummaryScope::meeting("meeting-revision"));
        assert!(rebound.items.iter().all(|summary_item| summary_item
            .evidence
            .iter()
            .all(|evidence| evidence.scope == LiveSummaryScope::meeting("meeting-revision"))));
        assert_ne!(rebound.snapshot_hash, session_revision.snapshot_hash);
        assert_eq!(
            rebound.items[0].evidence[0].source_event_id,
            session_revision.items[0].evidence[0].source_event_id
        );
        assert_eq!(
            rebound.items[0].evidence[0].source_text_hash,
            session_revision.items[0].evidence[0].source_text_hash
        );
    }

    #[tokio::test]
    async fn wrong_handle_folder_and_cross_meeting_are_rejected() {
        let registry = RecordingLiveSummaryRegistry::new(FakeFactory, 16);
        let folder = PathBuf::from("D:/synthetic/session-two");
        registry
            .accept_segment(segment("backend-two", "event-two", 2), Some(folder.clone()))
            .await
            .expect("accept trusted event");
        registry
            .mark_pending("backend-two", Some(folder.clone()))
            .await
            .expect("mark pending");
        let ticket = registry
            .issue_binding_ticket(&folder)
            .await
            .expect("issue ticket");
        assert_eq!(
            registry
                .prepare_binding("summary-bind-not-issued", &folder)
                .await
                .expect_err("wrong handle"),
            RecordingSummaryRegistryError::InvalidHandle
        );
        assert_eq!(
            registry
                .prepare_binding(&ticket.handle, Path::new("D:/synthetic/other"))
                .await
                .expect_err("wrong folder"),
            RecordingSummaryRegistryError::FolderMismatch
        );
        registry
            .prepare_binding(&ticket.handle, &folder)
            .await
            .expect("reserve correct binding");
        registry
            .prepare_bound_revision(&ticket.handle, "meeting-one")
            .await
            .expect("bind first meeting");
        assert_eq!(
            registry
                .prepare_bound_revision(&ticket.handle, "meeting-two")
                .await
                .expect_err("cross meeting"),
            RecordingSummaryRegistryError::MeetingMismatch
        );
    }

    #[tokio::test]
    async fn incomplete_bounded_history_cannot_replace_legacy_transcript_save() {
        let limits = RegistryLimits {
            max_sessions: 1,
            max_events_per_session: 1,
            pending_ttl_ms: 10_000,
            active_ttl_ms: 10_000,
        };
        let registry = RecordingLiveSummaryRegistry::with_limits(FakeFactory, 16, limits);
        let folder = PathBuf::from("D:/synthetic/session-capacity");
        let (ticket, _) = registry
            .prepare_session("backend-capacity", &folder)
            .await
            .expect("prepare bounded session");
        registry
            .accept_segment(
                segment("backend-capacity", "event-capacity-one", 1),
                Some(folder.clone()),
            )
            .await
            .expect("first trusted event");
        assert_eq!(
            registry
                .accept_segment(
                    segment("backend-capacity", "event-capacity-two", 2),
                    Some(folder.clone()),
                )
                .await
                .expect_err("bounded history must fail closed"),
            RecordingSummaryRegistryError::EventCapacityReached
        );
        registry
            .mark_pending_by_folder(&folder)
            .await
            .expect("mark pending despite summary capacity");
        assert_eq!(
            registry
                .prepare_binding(&ticket.handle, &folder)
                .await
                .expect_err("an incomplete trusted history must never replace renderer fallback"),
            RecordingSummaryRegistryError::EventCapacityReached
        );
        assert_eq!(registry.list_snapshots().await.len(), 1);
    }

    #[tokio::test]
    async fn pending_ttl_and_session_capacity_are_bounded() {
        let base = now_ms();
        let limits = RegistryLimits {
            max_sessions: 1,
            max_events_per_session: 2,
            pending_ttl_ms: 10_000,
            active_ttl_ms: 1_000,
        };
        let registry = RecordingLiveSummaryRegistry::with_limits(FakeFactory, 16, limits);
        let first_folder = PathBuf::from("D:/synthetic/first");
        registry
            .accept_segment_at(
                segment("backend-first", "event-first", 1),
                Some(first_folder.clone()),
                base,
            )
            .await
            .expect("first session");
        registry
            .mark_pending_at("backend-first", Some(first_folder.clone()), base + 1)
            .await
            .expect("first pending");
        let ticket = registry
            .issue_binding_ticket(&first_folder)
            .await
            .expect("ticket before expiry");
        {
            let mut state = registry.state.lock().await;
            registry.prune_locked(&mut state, base + 10_002);
        }
        assert_eq!(
            registry
                .prepare_binding(&ticket.handle, &first_folder)
                .await
                .expect_err("expired handle"),
            RecordingSummaryRegistryError::Expired
        );
        registry
            .accept_segment_at(
                segment("backend-second", "event-second", 2),
                Some(PathBuf::from("D:/synthetic/second")),
                base + 10_003,
            )
            .await
            .expect("expired session released capacity");
    }
}
