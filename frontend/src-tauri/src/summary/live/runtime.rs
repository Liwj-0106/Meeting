//! Runtime coordination for revision-bound live summaries.
//!
//! The coordinator owns one actor per meeting, persists canonical transcript
//! inputs before they can become evidence, and publishes only a constrained
//! frontend projection. Provider prompts, credentials, request payloads and
//! raw responses never appear in the public DTOs.

use super::{
    install_recording_summary_ingress, new_recording_summary_ingress, RecordingLiveSummaryRegistry,
    RecordingSummaryIngress,
};
use super::{
    DispatchOutcome, LiveSummaryActor, LiveSummaryActorConfig, LiveSummaryActorError,
    LiveSummaryItem, LiveSummaryProvider, LiveSummaryProviderError, LiveSummaryProviderResponse,
    LiveSummaryRecoveryState, LiveSummaryRevision, LiveSummaryRevisionType, LiveSummaryScope,
    SummaryResponseOutcome, SummaryResponsePreparation, SummarySourceChange, SummarySubmitOutcome,
};
use super::{LiveSummaryProviderEnvelope, OpenAiCompatibleLiveSummaryProviderFactory};
use crate::audio::transcription::event::TranscriptEvent;
use crate::database::models::StoredTranscriptEvent;
use crate::database::repositories::live_summary::{LiveSummaryRepository, LiveSummaryStoreError};
use crate::database::repositories::transcript_event::{
    TranscriptEventStoreError, TranscriptEventsRepository,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::SqlitePool;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;

pub const LIVE_SUMMARY_STATE_EVENT: &str = "live-summary-state";
const LIVE_SUMMARY_SCHEMA_VERSION: u16 = 1;
const DEFAULT_QUEUE_CAPACITY: usize = 256;
const DEFAULT_TEMPLATE_ID: &str = "standard_meeting";
const PROVIDER_UNAVAILABLE_CODE: &str = "live_summary_provider_unavailable";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSummaryFrontendError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl LiveSummaryFrontendError {
    fn provider_unavailable() -> Self {
        Self {
            code: PROVIDER_UNAVAILABLE_CODE.to_string(),
            message: "实时总结在线模型不可用，请检查模型、密钥和服务地址。".to_string(),
            retryable: false,
        }
    }

    fn from_provider(error: &LiveSummaryProviderError) -> Self {
        Self {
            code: error.code.to_string(),
            message: if error.code == PROVIDER_UNAVAILABLE_CODE {
                "实时总结在线模型不可用，请检查模型、密钥和服务地址。".to_string()
            } else {
                "实时总结暂时不可用，已保留上一版可信结果。".to_string()
            },
            retryable: error.retryable,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveSummaryLifecycle {
    Active,
    Generating,
    Unavailable,
    Finalized,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveSummaryDispatchState {
    Idle,
    InFlight,
    Deferred,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSummaryProviderAvailability {
    pub available: bool,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<LiveSummaryFrontendError>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSummaryPublicSnapshot {
    pub schema_version: u16,
    pub scope: LiveSummaryScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meeting_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_scope_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    pub lifecycle: LiveSummaryLifecycle,
    pub dispatch_state: LiveSummaryDispatchState,
    pub provider: LiveSummaryProviderAvailability,
    pub transcript_cursor: u64,
    pub summary_revision: u64,
    pub generation: u64,
    pub finalized: bool,
    pub recovered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_complete_revision: Option<LiveSummaryRevision>,
    pub items: Vec<LiveSummaryItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<LiveSummaryFrontendError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveSummaryEventKind {
    SessionStarted,
    TranscriptAccepted,
    SummaryCommitted,
    SummaryFailed,
    FinalReconcileRequested,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSummaryEvent {
    pub schema_version: u16,
    pub kind: LiveSummaryEventKind,
    pub snapshot: LiveSummaryPublicSnapshot,
}

impl LiveSummaryEvent {
    pub fn new(kind: LiveSummaryEventKind, snapshot: LiveSummaryPublicSnapshot) -> Self {
        Self {
            schema_version: LIVE_SUMMARY_SCHEMA_VERSION,
            kind,
            snapshot,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartLiveSummarySession {
    pub meeting_id: String,
    pub template_id: Option<String>,
}

pub trait LiveSummaryProviderFactory: Send + Sync + 'static {
    type Provider: LiveSummaryProvider + Send + 'static;

    fn create(&self) -> Self::Provider;
    fn availability(&self) -> LiveSummaryProviderAvailability;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableLiveSummaryProviderFactory;

#[derive(Debug, Default)]
pub struct UnavailableLiveSummaryProvider;

impl LiveSummaryProviderFactory for UnavailableLiveSummaryProviderFactory {
    type Provider = UnavailableLiveSummaryProvider;

    fn create(&self) -> Self::Provider {
        UnavailableLiveSummaryProvider
    }

    fn availability(&self) -> LiveSummaryProviderAvailability {
        LiveSummaryProviderAvailability {
            available: false,
            provider: "unavailable".to_string(),
            model: None,
            error: Some(LiveSummaryFrontendError::provider_unavailable()),
        }
    }
}

impl LiveSummaryProvider for UnavailableLiveSummaryProvider {
    fn provider_id(&self) -> &str {
        "unavailable"
    }

    fn model_id(&self) -> Option<&str> {
        None
    }

    fn start(
        &mut self,
        _request: &super::LiveSummaryRequest,
    ) -> Result<(), LiveSummaryProviderError> {
        Err(LiveSummaryProviderError {
            code: PROVIDER_UNAVAILABLE_CODE,
            retryable: false,
        })
    }

    fn cancel(&mut self, _request_id: &str) {}
}

#[derive(Debug, Error)]
pub enum LiveSummaryCoordinatorError {
    #[error("invalid meeting identifier")]
    InvalidMeetingId,
    #[error("invalid template identifier")]
    InvalidTemplateId,
    #[error("meeting does not exist")]
    MeetingNotFound,
    #[error("live summary session has not been started")]
    SessionNotStarted,
    #[error("only canonical stable final, correction or retraction events are accepted")]
    StableEventRequired,
    #[error("canonical transcript event timestamp is invalid")]
    InvalidCreatedAt,
    #[error("canonical transcript event numeric field is outside the storage range")]
    NumericFieldOutOfRange,
    #[error(transparent)]
    Actor(#[from] LiveSummaryActorError),
    #[error(transparent)]
    SummaryStore(#[from] LiveSummaryStoreError),
    #[error(transparent)]
    TranscriptStore(#[from] TranscriptEventStoreError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl LiveSummaryCoordinatorError {
    pub fn frontend_error(&self) -> LiveSummaryFrontendError {
        let (code, message, retryable) = match self {
            Self::InvalidMeetingId => ("live_summary_invalid_meeting_id", "会议标识无效。", false),
            Self::InvalidTemplateId => (
                "live_summary_invalid_template_id",
                "总结模板标识无效。",
                false,
            ),
            Self::MeetingNotFound => (
                "live_summary_meeting_not_found",
                "没有找到对应会议。",
                false,
            ),
            Self::SessionNotStarted => (
                "live_summary_session_not_started",
                "请先启动实时总结会话。",
                false,
            ),
            Self::StableEventRequired => (
                "live_summary_stable_event_required",
                "实时总结只接收稳定的最终、纠正或撤回事件。",
                false,
            ),
            Self::Actor(LiveSummaryActorError::AlreadyFinalized) => (
                "live_summary_already_finalized",
                "该会议的实时总结已经完成最终核对。",
                false,
            ),
            Self::Actor(_) | Self::InvalidCreatedAt | Self::NumericFieldOutOfRange => (
                "live_summary_invalid_transcript_event",
                "转写事件不符合实时总结的数据要求。",
                false,
            ),
            Self::SummaryStore(_) | Self::TranscriptStore(_) | Self::Database(_) => (
                "live_summary_storage_failed",
                "实时总结状态暂时无法保存，录音与字幕不受影响。",
                true,
            ),
        };
        LiveSummaryFrontendError {
            code: code.to_string(),
            message: message.to_string(),
            retryable,
        }
    }
}

struct LiveSummarySession<P: LiveSummaryProvider> {
    template_id: String,
    actor: LiveSummaryActor<P>,
    latest_revision: Option<LiveSummaryRevision>,
    public_items: Vec<LiveSummaryItem>,
    dispatch_state: LiveSummaryDispatchState,
    last_error: Option<LiveSummaryFrontendError>,
    deferred_changes: VecDeque<SummarySourceChange>,
    recovered: bool,
}

impl<P: LiveSummaryProvider> LiveSummarySession<P> {
    fn apply_dispatch(&mut self, dispatch: DispatchOutcome) {
        match dispatch {
            DispatchOutcome::Started { .. } | DispatchOutcome::WaitingForInFlight => {
                self.dispatch_state = LiveSummaryDispatchState::InFlight;
                self.last_error = None;
            }
            DispatchOutcome::Idle => {
                self.dispatch_state = LiveSummaryDispatchState::Idle;
            }
            DispatchOutcome::Deferred { error } => {
                self.dispatch_state = LiveSummaryDispatchState::Deferred;
                self.last_error = Some(LiveSummaryFrontendError::from_provider(&error));
            }
        }
    }

    fn snapshot(
        &self,
        meeting_id: &str,
        provider: LiveSummaryProviderAvailability,
    ) -> LiveSummaryPublicSnapshot {
        let (summary_revision, generation) = self.actor.counters();
        let finalized = self.actor.is_finalized();
        let lifecycle = if finalized {
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
        let scope = LiveSummaryScope::meeting(meeting_id);
        LiveSummaryPublicSnapshot {
            schema_version: LIVE_SUMMARY_SCHEMA_VERSION,
            scope,
            meeting_id: Some(meeting_id.to_string()),
            session_scope_id: None,
            template_id: Some(self.template_id.clone()),
            lifecycle,
            dispatch_state: self.dispatch_state.clone(),
            provider,
            transcript_cursor: self.actor.transcript_cursor(),
            summary_revision,
            generation,
            finalized,
            recovered: self.recovered,
            updated_at: self
                .latest_revision
                .as_ref()
                .map(|revision| revision.created_at.clone()),
            last_complete_revision: self.latest_revision.clone(),
            items: self.public_items.clone(),
            error: self.last_error.clone(),
        }
    }

    fn defer_change(&mut self, change: SummarySourceChange) -> Result<(), LiveSummaryActorError> {
        if let Some(position) = self
            .deferred_changes
            .iter()
            .position(|queued| queued.evidence.utterance_id == change.evidence.utterance_id)
        {
            let queued = &self.deferred_changes[position];
            let revision_order = change
                .evidence
                .source_revision
                .cmp(&queued.evidence.source_revision);
            let should_replace = match revision_order {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => {
                    if change.evidence.source_event_id == queued.evidence.source_event_id
                        && change.kind == queued.kind
                    {
                        false
                    } else {
                        match change
                            .kind
                            .replay_priority()
                            .cmp(&queued.kind.replay_priority())
                        {
                            std::cmp::Ordering::Greater => true,
                            std::cmp::Ordering::Less => false,
                            std::cmp::Ordering::Equal => {
                                return Err(LiveSummaryActorError::TranscriptRevisionConflict);
                            }
                        }
                    }
                }
            };
            if should_replace {
                self.deferred_changes[position] = change;
            }
        } else {
            self.deferred_changes.push_back(change);
        }
        Ok(())
    }

    fn drain_deferred(&mut self) -> Result<(), LiveSummaryActorError> {
        while let Some(change) = self.deferred_changes.pop_front() {
            match self.actor.submit_change(change)? {
                SummarySubmitOutcome::Accepted { dispatch, .. } => {
                    self.public_items = self.actor.current_items().to_vec();
                    self.apply_dispatch(dispatch);
                }
                SummarySubmitOutcome::Duplicate { .. } | SummarySubmitOutcome::Stale { .. } => {}
                SummarySubmitOutcome::RetryRequired { event, .. } => {
                    self.deferred_changes.push_front(event);
                    break;
                }
                SummarySubmitOutcome::IgnoredPartial { .. }
                | SummarySubmitOutcome::IgnoredUnsupported { .. } => {}
            }
        }
        Ok(())
    }
}

pub struct LiveSummaryCoordinator<F: LiveSummaryProviderFactory> {
    factory: F,
    queue_capacity: usize,
    sessions: Mutex<HashMap<String, LiveSummarySession<F::Provider>>>,
}

impl<F: LiveSummaryProviderFactory> LiveSummaryCoordinator<F> {
    pub fn new(factory: F, queue_capacity: usize) -> Self {
        Self {
            factory,
            queue_capacity: queue_capacity.max(1),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn session_from_recovery(
        &self,
        meeting_id: &str,
        template_id: String,
        recovery: crate::database::repositories::live_summary::LiveSummaryRecoverySnapshot,
        error: Option<LiveSummaryFrontendError>,
        dispatch_immediately: bool,
    ) -> Result<LiveSummarySession<F::Provider>, LiveSummaryCoordinatorError> {
        let recovered = recovery.latest_revision.is_some();
        let latest_revision = recovery.latest_revision.clone();
        let config = LiveSummaryActorConfig::new(meeting_id, self.queue_capacity);
        let provider = self.factory.create();
        let mut actor = match recovery.latest_revision {
            Some(last_complete_revision) => LiveSummaryActor::recover(
                config,
                provider,
                LiveSummaryRecoveryState {
                    last_complete_revision,
                    current_transcript_cursor: recovery.current_transcript_cursor,
                    transcript_heads: recovery.transcript_heads,
                },
            )?,
            None => LiveSummaryActor::hydrate_transcript_heads(
                config,
                provider,
                recovery.current_transcript_cursor,
                recovery.transcript_heads,
            )?,
        };
        let initial_items = actor.current_items().to_vec();
        let initial_dispatch = if dispatch_immediately {
            actor.retry_deferred_dispatch()
        } else {
            DispatchOutcome::Idle
        };
        let availability = self.factory.availability();
        let mut session = LiveSummarySession {
            template_id,
            actor,
            latest_revision,
            public_items: initial_items,
            dispatch_state: LiveSummaryDispatchState::Idle,
            last_error: error.or_else(|| {
                (!availability.available).then(LiveSummaryFrontendError::provider_unavailable)
            }),
            deferred_changes: VecDeque::new(),
            recovered,
        };
        session.apply_dispatch(initial_dispatch);
        Ok(session)
    }

    pub async fn start_session(
        &self,
        pool: &SqlitePool,
        request: StartLiveSummarySession,
    ) -> Result<LiveSummaryPublicSnapshot, LiveSummaryCoordinatorError> {
        validate_meeting_id(&request.meeting_id)?;
        let template_id = request
            .template_id
            .unwrap_or_else(|| DEFAULT_TEMPLATE_ID.to_string());
        crate::summary::templates::validate_template_id(&template_id)
            .map_err(|_| LiveSummaryCoordinatorError::InvalidTemplateId)?;

        if let Some(session) = self.sessions.lock().await.get(&request.meeting_id) {
            return Ok(session.snapshot(&request.meeting_id, self.factory.availability()));
        }

        let meeting_exists: i64 =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM meetings WHERE id = ?)")
                .bind(&request.meeting_id)
                .fetch_one(pool)
                .await?;
        if meeting_exists != 1 {
            return Err(LiveSummaryCoordinatorError::MeetingNotFound);
        }

        let recovery =
            LiveSummaryRepository::load_recovery_snapshot(pool, &request.meeting_id).await?;
        let session =
            self.session_from_recovery(&request.meeting_id, template_id, recovery, None, true)?;

        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .entry(request.meeting_id.clone())
            .or_insert(session);
        Ok(session.snapshot(&request.meeting_id, self.factory.availability()))
    }

    pub async fn submit_event(
        &self,
        pool: &SqlitePool,
        event: TranscriptEvent,
    ) -> Result<LiveSummaryPublicSnapshot, LiveSummaryCoordinatorError> {
        let meeting_id = event
            .meeting_id
            .as_deref()
            .ok_or(LiveSummaryCoordinatorError::InvalidMeetingId)?
            .to_string();
        validate_meeting_id(&meeting_id)?;
        let change = SummarySourceChange::from_transcript(event.clone())?
            .ok_or(LiveSummaryCoordinatorError::StableEventRequired)?;
        change.validate()?;

        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&meeting_id)
            .ok_or(LiveSummaryCoordinatorError::SessionNotStarted)?;
        if session.actor.is_finalized() {
            return Err(LiveSummaryActorError::AlreadyFinalized.into());
        }

        let stored = stored_event_from_canonical(&event)?;
        TranscriptEventsRepository::upsert_revision(pool, &stored).await?;
        match session.actor.submit_change(change)? {
            SummarySubmitOutcome::Accepted { dispatch, .. } => {
                session.public_items = session.actor.current_items().to_vec();
                session.apply_dispatch(dispatch);
            }
            SummarySubmitOutcome::Duplicate { .. } | SummarySubmitOutcome::Stale { .. } => {}
            SummarySubmitOutcome::RetryRequired { event, .. } => {
                session.defer_change(event)?;
                session.dispatch_state = LiveSummaryDispatchState::Deferred;
                session.last_error = Some(LiveSummaryFrontendError {
                    code: "live_summary_backpressure".to_string(),
                    message: "实时总结正在追赶会议进度，稳定转写已安全保存。".to_string(),
                    retryable: true,
                });
            }
            SummarySubmitOutcome::IgnoredPartial { .. }
            | SummarySubmitOutcome::IgnoredUnsupported { .. } => {
                return Err(LiveSummaryCoordinatorError::StableEventRequired);
            }
        }
        Ok(session.snapshot(&meeting_id, self.factory.availability()))
    }

    pub async fn request_final_reconcile(
        &self,
        meeting_id: &str,
    ) -> Result<LiveSummaryPublicSnapshot, LiveSummaryCoordinatorError> {
        validate_meeting_id(meeting_id)?;
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(meeting_id)
            .ok_or(LiveSummaryCoordinatorError::SessionNotStarted)?;
        let dispatch = session.actor.request_final_reconcile();
        session.apply_dispatch(dispatch);
        Ok(session.snapshot(meeting_id, self.factory.availability()))
    }

    pub async fn handle_provider_response(
        &self,
        pool: &SqlitePool,
        meeting_id: &str,
        response: LiveSummaryProviderResponse,
    ) -> Result<LiveSummaryPublicSnapshot, LiveSummaryCoordinatorError> {
        validate_meeting_id(meeting_id)?;
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(meeting_id)
            .ok_or(LiveSummaryCoordinatorError::SessionNotStarted)?;
        let response_error = match session.actor.prepare_response(response) {
            SummaryResponsePreparation::Ready(prepared) => {
                let request_id = prepared.request_id().to_string();
                let revision = prepared.revision().clone();
                if let Err(error) = LiveSummaryRepository::persist_revision(pool, &revision).await {
                    let _ = session.actor.discard_prepared_response(&request_id);
                    let template_id = session.template_id.clone();
                    let coordinator_error = LiveSummaryCoordinatorError::SummaryStore(error);
                    let frontend_error = coordinator_error.frontend_error();
                    let recovery =
                        LiveSummaryRepository::load_recovery_snapshot(pool, meeting_id).await?;
                    *session = self.session_from_recovery(
                        meeting_id,
                        template_id,
                        recovery,
                        Some(frontend_error.clone()),
                        false,
                    )?;
                    session.last_error = Some(frontend_error);
                    return Err(coordinator_error);
                }

                match session.actor.commit_prepared_response(prepared) {
                    Ok(SummaryResponseOutcome::Committed { next_dispatch, .. }) => {
                        session.public_items = revision.items.clone();
                        session.latest_revision = Some(revision);
                        session.apply_dispatch(next_dispatch);
                        None
                    }
                    Ok(_) => unreachable!("committing a prepared response always commits"),
                    Err(error) => {
                        let template_id = session.template_id.clone();
                        let recovery =
                            LiveSummaryRepository::load_recovery_snapshot(pool, meeting_id).await?;
                        *session = self.session_from_recovery(
                            meeting_id,
                            template_id,
                            recovery,
                            Some(error.frontend_error()),
                            false,
                        )?;
                        return Err(error.into());
                    }
                }
            }
            SummaryResponsePreparation::Resolved(SummaryResponseOutcome::Committed { .. }) => {
                unreachable!("successful provider responses are prepared before persistence")
            }
            SummaryResponsePreparation::Resolved(SummaryResponseOutcome::IgnoredStale {
                next_dispatch,
                ..
            }) => {
                session.apply_dispatch(next_dispatch);
                None
            }
            SummaryResponsePreparation::Resolved(SummaryResponseOutcome::Rejected {
                error,
                next_dispatch,
            }) => {
                session.apply_dispatch(next_dispatch);
                Some(error.frontend_error())
            }
            SummaryResponsePreparation::Resolved(SummaryResponseOutcome::ProviderFailed {
                error,
                next_dispatch,
            }) => {
                session.apply_dispatch(next_dispatch);
                Some(LiveSummaryFrontendError::from_provider(&error))
            }
        };
        session.drain_deferred()?;
        if let Some(error) = response_error {
            session.last_error = Some(error);
        }
        Ok(session.snapshot(meeting_id, self.factory.availability()))
    }

    pub async fn get_current(
        &self,
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<LiveSummaryPublicSnapshot, LiveSummaryCoordinatorError> {
        validate_meeting_id(meeting_id)?;
        if let Some(session) = self.sessions.lock().await.get(meeting_id) {
            return Ok(session.snapshot(meeting_id, self.factory.availability()));
        }

        let meeting_exists: i64 =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM meetings WHERE id = ?)")
                .bind(meeting_id)
                .fetch_one(pool)
                .await?;
        if meeting_exists != 1 {
            return Err(LiveSummaryCoordinatorError::MeetingNotFound);
        }
        let recovery = LiveSummaryRepository::load_recovery_snapshot(pool, meeting_id).await?;
        let provider = self.factory.availability();
        let latest = recovery.latest_revision;
        let finalized = latest
            .as_ref()
            .is_some_and(|value| value.revision_type == LiveSummaryRevisionType::Final);
        let (summary_revision, generation, updated_at, items) = latest
            .as_ref()
            .map(|value| {
                (
                    value.revision,
                    value.generation,
                    Some(value.created_at.clone()),
                    value.items.clone(),
                )
            })
            .unwrap_or((0, 0, None, Vec::new()));
        Ok(LiveSummaryPublicSnapshot {
            schema_version: LIVE_SUMMARY_SCHEMA_VERSION,
            scope: LiveSummaryScope::meeting(meeting_id),
            meeting_id: Some(meeting_id.to_string()),
            session_scope_id: None,
            template_id: None,
            lifecycle: if finalized {
                LiveSummaryLifecycle::Finalized
            } else if provider.available {
                LiveSummaryLifecycle::Active
            } else {
                LiveSummaryLifecycle::Unavailable
            },
            dispatch_state: LiveSummaryDispatchState::Idle,
            error: provider.error.clone(),
            provider,
            transcript_cursor: recovery.current_transcript_cursor,
            summary_revision,
            generation,
            finalized,
            recovered: latest.is_some(),
            updated_at,
            last_complete_revision: latest,
            items,
        })
    }

    #[cfg(test)]
    async fn in_flight_request(&self, meeting_id: &str) -> Option<super::LiveSummaryRequest> {
        self.sessions
            .lock()
            .await
            .get(meeting_id)
            .and_then(|session| session.actor.in_flight_request().cloned())
    }
}

impl LiveSummaryActorError {
    fn frontend_error(&self) -> LiveSummaryFrontendError {
        LiveSummaryCoordinatorError::Actor(self.clone()).frontend_error()
    }
}

fn validate_meeting_id(meeting_id: &str) -> Result<(), LiveSummaryCoordinatorError> {
    if meeting_id.trim().is_empty() || meeting_id.len() > 256 || meeting_id.contains('\0') {
        Err(LiveSummaryCoordinatorError::InvalidMeetingId)
    } else {
        Ok(())
    }
}

fn stored_event_from_canonical(
    event: &TranscriptEvent,
) -> Result<StoredTranscriptEvent, LiveSummaryCoordinatorError> {
    let meeting_id = event
        .meeting_id
        .clone()
        .ok_or(LiveSummaryCoordinatorError::InvalidMeetingId)?;
    let created_at = DateTime::parse_from_rfc3339(&event.created_at)
        .map_err(|_| LiveSummaryCoordinatorError::InvalidCreatedAt)?
        .with_timezone(&Utc);
    let revision = i64::try_from(event.revision)
        .map_err(|_| LiveSummaryCoordinatorError::NumericFieldOutOfRange)?;
    let sequence_id = event
        .sequence_id
        .map(i64::try_from)
        .transpose()
        .map_err(|_| LiveSummaryCoordinatorError::NumericFieldOutOfRange)?;
    let start_ms = i64::try_from(event.start_ms)
        .map_err(|_| LiveSummaryCoordinatorError::NumericFieldOutOfRange)?;
    let end_ms = i64::try_from(event.end_ms)
        .map_err(|_| LiveSummaryCoordinatorError::NumericFieldOutOfRange)?;
    let speaker = event.speaker.as_ref();
    let asr = event.asr.as_ref();
    let diarization = event.diarization.as_ref();

    Ok(StoredTranscriptEvent {
        event_id: event.event_id.clone(),
        meeting_id,
        schema_version: i64::from(event.schema_version),
        session_id: event.session_id.clone(),
        utterance_id: event.utterance_id.clone(),
        revision,
        event_kind: event.event_kind.as_str().to_string(),
        is_stable: event.is_stable,
        text: event.text.clone(),
        timestamp: created_at.format("%H:%M:%S").to_string(),
        sequence_id,
        start_ms: Some(start_ms),
        end_ms: Some(end_ms),
        audio_start_time: Some(event.start_ms as f64 / 1_000.0),
        audio_end_time: Some(event.end_ms as f64 / 1_000.0),
        duration: Some(event.end_ms.saturating_sub(event.start_ms) as f64 / 1_000.0),
        audio_source: Some(event.audio_source.legacy_label().to_string()),
        speaker_id: speaker.map(|value| value.speaker_id.clone()),
        speaker_local_label: speaker.and_then(|value| value.local_label.clone()),
        speaker_display_name: speaker.and_then(|value| value.display_name.clone()),
        speaker_confidence: speaker.and_then(|value| value.confidence.map(f64::from)),
        speaker_status: speaker.map(|value| value.status.as_str().to_string()),
        language: event.language.clone(),
        asr_provider: asr.map(|value| value.provider.clone()),
        asr_model: asr.and_then(|value| value.model.clone()),
        asr_confidence: asr.and_then(|value| value.confidence.map(f64::from)),
        asr_latency_ms: asr
            .and_then(|value| value.latency_ms)
            .map(i64::try_from)
            .transpose()
            .map_err(|_| LiveSummaryCoordinatorError::NumericFieldOutOfRange)?,
        diarization_provider: diarization.map(|value| value.provider.clone()),
        diarization_model: diarization.and_then(|value| value.model.clone()),
        diarization_model_revision: diarization.and_then(|value| value.model_revision.clone()),
        diarization_revision: diarization
            .map(|value| i64::try_from(value.revision))
            .transpose()
            .map_err(|_| LiveSummaryCoordinatorError::NumericFieldOutOfRange)?,
        diarization_window_id: diarization.and_then(|value| value.window_id.clone()),
        diarization_window_start_frame: diarization
            .and_then(|value| value.window_start_frame)
            .map(i64::try_from)
            .transpose()
            .map_err(|_| LiveSummaryCoordinatorError::NumericFieldOutOfRange)?,
        diarization_window_end_frame: diarization
            .and_then(|value| value.window_end_frame)
            .map(i64::try_from)
            .transpose()
            .map_err(|_| LiveSummaryCoordinatorError::NumericFieldOutOfRange)?,
        diarization_status: diarization.map(|value| value.status.as_str().to_string()),
        diarization_latency_ms: diarization
            .and_then(|value| value.latency_ms)
            .map(i64::try_from)
            .transpose()
            .map_err(|_| LiveSummaryCoordinatorError::NumericFieldOutOfRange)?,
        replaces_event_id: event.replaces_event_id.clone(),
        provider_event_id: event.provider_event_id.clone(),
        trace_id: event.trace_id.clone(),
        created_at,
    })
}

#[derive(Clone)]
pub struct LiveSummaryRuntimeState(
    pub Arc<LiveSummaryCoordinator<OpenAiCompatibleLiveSummaryProviderFactory>>,
    pub(crate) OpenAiCompatibleLiveSummaryProviderFactory,
    pub(crate) Arc<Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<LiveSummaryProviderEnvelope>>>>,
    pub(crate) Arc<RecordingLiveSummaryRegistry<OpenAiCompatibleLiveSummaryProviderFactory>>,
    pub(crate) Arc<Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<RecordingSummaryIngress>>>>,
    pub(crate) tokio::sync::mpsc::UnboundedSender<RecordingSummaryIngress>,
);

impl Default for LiveSummaryRuntimeState {
    fn default() -> Self {
        let (factory, receiver) = OpenAiCompatibleLiveSummaryProviderFactory::production();
        let (recording_sender, recording_receiver) = new_recording_summary_ingress();
        install_recording_summary_ingress(recording_sender.clone());
        Self(
            Arc::new(LiveSummaryCoordinator::new(
                factory.clone(),
                DEFAULT_QUEUE_CAPACITY,
            )),
            factory.clone(),
            Arc::new(Mutex::new(Some(receiver))),
            Arc::new(RecordingLiveSummaryRegistry::new(
                factory,
                DEFAULT_QUEUE_CAPACITY,
            )),
            Arc::new(Mutex::new(Some(recording_receiver))),
            recording_sender,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::transcription::event::{
        AudioSource, TranscriptEventKind, TRANSCRIPT_EVENT_SCHEMA_VERSION,
    };
    use crate::summary::live::openai_compatible::{
        injected_test_configuration, LiveSummaryHttpError, LiveSummaryHttpRequest,
        LiveSummaryHttpResponse, LiveSummaryHttpTransport,
        OpenAiCompatibleLiveSummaryProviderFactory,
    };
    use crate::summary::live::{DeterministicFakeProvider, LiveSummaryProviderResponse};
    use async_trait::async_trait;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::Mutex as StdMutex;

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

    struct StaticHttpTransport(StdMutex<Option<LiveSummaryHttpResponse>>);

    #[async_trait]
    impl LiveSummaryHttpTransport for StaticHttpTransport {
        async fn send(
            &self,
            _request: LiveSummaryHttpRequest,
        ) -> Result<LiveSummaryHttpResponse, LiveSummaryHttpError> {
            self.0
                .lock()
                .expect("static transport lock")
                .take()
                .ok_or(LiveSummaryHttpError::Transport)
        }
    }

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("create database");
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .expect("foreign keys");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations");
        sqlx::query("INSERT INTO meetings (id, title, created_at, updated_at) VALUES (?, ?, ?, ?)")
            .bind("meeting-runtime")
            .bind("Synthetic runtime meeting")
            .bind("2026-09-02T00:00:00.000Z")
            .bind("2026-09-02T00:00:00.000Z")
            .execute(&pool)
            .await
            .expect("meeting");
        pool
    }

    fn stable_event(event_id: &str, revision: u64, text: &str) -> TranscriptEvent {
        TranscriptEvent {
            schema_version: TRANSCRIPT_EVENT_SCHEMA_VERSION,
            event_id: event_id.to_string(),
            meeting_id: Some("meeting-runtime".to_string()),
            session_id: Some("session-runtime".to_string()),
            utterance_id: "utterance-runtime".to_string(),
            revision,
            event_kind: if revision == 0 {
                TranscriptEventKind::Final
            } else {
                TranscriptEventKind::Correction
            },
            is_stable: true,
            start_ms: 100,
            end_ms: 1_000,
            text: text.to_string(),
            language: Some("zh".to_string()),
            audio_source: AudioSource::System,
            speaker: None,
            asr: None,
            diarization: None,
            replaces_event_id: None,
            provider_event_id: None,
            created_at: "2026-09-02T00:00:01.000Z".to_string(),
            trace_id: None,
            sequence_id: Some(1),
        }
    }

    fn same_revision_event(
        event_id: &str,
        utterance_id: &str,
        event_kind: TranscriptEventKind,
        text: &str,
    ) -> TranscriptEvent {
        let mut event = stable_event(event_id, 0, text);
        event.utterance_id = utterance_id.to_string();
        event.event_kind = event_kind;
        event
    }

    #[tokio::test]
    async fn stable_event_dispatches_persists_and_final_reconcile_recovers() {
        let pool = test_pool().await;
        let coordinator = LiveSummaryCoordinator::new(FakeFactory, 8);
        coordinator
            .start_session(
                &pool,
                StartLiveSummarySession {
                    meeting_id: "meeting-runtime".to_string(),
                    template_id: Some("daily_standup".to_string()),
                },
            )
            .await
            .expect("start");
        let submitted = coordinator
            .submit_event(&pool, stable_event("event-runtime-0", 0, "确认发布范围"))
            .await
            .expect("submit");
        assert_eq!(submitted.lifecycle, LiveSummaryLifecycle::Generating);

        let request = coordinator
            .in_flight_request("meeting-runtime")
            .await
            .expect("request");
        let response = DeterministicFakeProvider::response_for(&request, 1_788_307_202_000);
        let committed = coordinator
            .handle_provider_response(&pool, "meeting-runtime", response)
            .await
            .expect("commit");
        assert_eq!(committed.summary_revision, 1);
        assert_eq!(committed.items.len(), 1);
        assert!(committed.last_complete_revision.is_some());

        let final_requested = coordinator
            .request_final_reconcile("meeting-runtime")
            .await
            .expect("final request");
        assert_eq!(final_requested.lifecycle, LiveSummaryLifecycle::Generating);
        let final_request = coordinator
            .in_flight_request("meeting-runtime")
            .await
            .expect("final provider request");
        assert!(final_request.final_reconcile);
        let final_response: LiveSummaryProviderResponse =
            DeterministicFakeProvider::response_for(&final_request, 1_788_307_203_000);
        let finalized = coordinator
            .handle_provider_response(&pool, "meeting-runtime", final_response)
            .await
            .expect("final commit");
        assert!(finalized.finalized);
        assert_eq!(finalized.lifecycle, LiveSummaryLifecycle::Finalized);

        let recovered_coordinator = LiveSummaryCoordinator::new(FakeFactory, 8);
        let recovered = recovered_coordinator
            .start_session(
                &pool,
                StartLiveSummarySession {
                    meeting_id: "meeting-runtime".to_string(),
                    template_id: Some("daily_standup".to_string()),
                },
            )
            .await
            .expect("recover");
        assert!(recovered.recovered);
        assert!(recovered.finalized);
        assert_eq!(recovered.summary_revision, 2);
        assert_eq!(recovered.items, finalized.items);
        assert!(recovered_coordinator
            .in_flight_request("meeting-runtime")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn failed_revision_write_rebuilds_actor_from_durable_state_before_retry() {
        let pool = test_pool().await;
        let coordinator = LiveSummaryCoordinator::new(FakeFactory, 8);
        coordinator
            .start_session(
                &pool,
                StartLiveSummarySession {
                    meeting_id: "meeting-runtime".to_string(),
                    template_id: None,
                },
            )
            .await
            .expect("start");
        coordinator
            .submit_event(&pool, stable_event("event-runtime-0", 0, "必须先写库"))
            .await
            .expect("submit");
        let request = coordinator
            .in_flight_request("meeting-runtime")
            .await
            .expect("request");

        sqlx::query(
            r#"
            CREATE TRIGGER fail_live_summary_revision_insert
            BEFORE INSERT ON live_summary_revisions
            BEGIN
                SELECT RAISE(ABORT, 'synthetic live summary write failure');
            END
            "#,
        )
        .execute(&pool)
        .await
        .expect("install failure trigger");

        let error = coordinator
            .handle_provider_response(
                &pool,
                "meeting-runtime",
                DeterministicFakeProvider::response_for(&request, 1_788_307_206_000),
            )
            .await
            .expect_err("write failure must not publish the prepared result");
        assert!(matches!(
            error,
            LiveSummaryCoordinatorError::SummaryStore(_)
        ));

        let rebuilt = coordinator
            .get_current(&pool, "meeting-runtime")
            .await
            .expect("rebuilt in-memory state");
        assert_eq!(rebuilt.summary_revision, 0);
        assert!(rebuilt.items.is_empty());
        assert_eq!(
            rebuilt.error.as_ref().map(|value| value.code.as_str()),
            Some("live_summary_storage_failed")
        );
        assert!(
            LiveSummaryRepository::load_latest_revision(&pool, "meeting-runtime")
                .await
                .expect("load durable head")
                .is_none()
        );

        sqlx::query("DROP TRIGGER fail_live_summary_revision_insert")
            .execute(&pool)
            .await
            .expect("remove failure trigger");
        assert!(coordinator
            .in_flight_request("meeting-runtime")
            .await
            .is_none());
        coordinator
            .request_final_reconcile("meeting-runtime")
            .await
            .expect("explicit retry after storage recovers");
        let retry = coordinator
            .in_flight_request("meeting-runtime")
            .await
            .expect("final reconcile dispatches from durable transcript heads");
        let committed = coordinator
            .handle_provider_response(
                &pool,
                "meeting-runtime",
                DeterministicFakeProvider::response_for(&retry, 1_788_307_207_000),
            )
            .await
            .expect("retry commits");
        assert_eq!(committed.summary_revision, 1);
        assert_eq!(committed.items.len(), 1);
    }

    #[tokio::test]
    async fn injected_openai_transport_response_commits_through_coordinator_without_cloud() {
        let pool = test_pool().await;
        let source_text = "确认上线窗口";
        let expected_item = LiveSummaryItem {
            item_id: "decision-release-window".to_string(),
            kind: crate::summary::live::SummaryItemKind::Decision,
            title: "上线窗口".to_string(),
            body: "团队确认了上线窗口。".to_string(),
            owner: None,
            due_at: None,
            status: crate::summary::live::SummaryItemStatus::Active,
            evidence: vec![crate::summary::live::SummaryEvidence {
                scope: LiveSummaryScope::meeting("meeting-runtime"),
                utterance_id: "utterance-runtime".to_string(),
                source_revision: 0,
                source_event_id: "event-runtime-provider".to_string(),
                source_text_hash: crate::summary::live::source_text_hash(source_text),
            }],
        };
        let response_body = serde_json::to_vec(&serde_json::json!({
            "choices": [{"message": {"content": serde_json::to_string(&serde_json::json!({
                "items": [expected_item.clone()]
            })).expect("structured content")}}]
        }))
        .expect("provider response body");
        let transport = Arc::new(StaticHttpTransport(StdMutex::new(Some(
            LiveSummaryHttpResponse {
                status: 200,
                body: response_body,
            },
        ))));
        let (factory, mut responses) = OpenAiCompatibleLiveSummaryProviderFactory::new(transport);
        factory.configure(Ok(injected_test_configuration()));
        let coordinator = LiveSummaryCoordinator::new(factory, 8);
        coordinator
            .start_session(
                &pool,
                StartLiveSummarySession {
                    meeting_id: "meeting-runtime".to_string(),
                    template_id: Some("standard_meeting".to_string()),
                },
            )
            .await
            .expect("start configured session");
        coordinator
            .submit_event(
                &pool,
                stable_event("event-runtime-provider", 0, source_text),
            )
            .await
            .expect("submit trusted canonical event");

        let envelope = tokio::time::timeout(std::time::Duration::from_secs(1), responses.recv())
            .await
            .expect("provider timeout")
            .expect("provider envelope");
        assert_eq!(envelope.scope, LiveSummaryScope::meeting("meeting-runtime"));
        let envelope_scope = envelope.scope.clone();
        let snapshot = coordinator
            .handle_provider_response(
                &pool,
                envelope_scope.meeting_id().expect("meeting scope"),
                envelope.response,
            )
            .await
            .expect("validate, persist and commit provider output");
        assert_eq!(snapshot.summary_revision, 1);
        assert_eq!(snapshot.items, vec![expected_item]);
        assert!(
            LiveSummaryRepository::load_latest_revision(&pool, "meeting-runtime")
                .await
                .expect("load persisted revision")
                .is_some()
        );
    }

    #[tokio::test]
    async fn production_factory_reports_unavailable_without_fake_output() {
        let pool = test_pool().await;
        let coordinator = LiveSummaryCoordinator::new(UnavailableLiveSummaryProviderFactory, 8);
        coordinator
            .start_session(
                &pool,
                StartLiveSummarySession {
                    meeting_id: "meeting-runtime".to_string(),
                    template_id: None,
                },
            )
            .await
            .expect("start");
        let snapshot = coordinator
            .submit_event(&pool, stable_event("event-runtime-0", 0, "保留字幕"))
            .await
            .expect("store transcript while summary is unavailable");
        assert_eq!(snapshot.lifecycle, LiveSummaryLifecycle::Unavailable);
        assert!(snapshot.items.is_empty());
        assert_eq!(
            snapshot.error.as_ref().map(|error| error.code.as_str()),
            Some(PROVIDER_UNAVAILABLE_CODE)
        );
        assert!(
            LiveSummaryRepository::load_latest_revision(&pool, "meeting-runtime")
                .await
                .expect("load")
                .is_none()
        );
    }

    #[tokio::test]
    async fn public_snapshot_contains_template_id_but_no_prompt_or_provider_body() {
        let pool = test_pool().await;
        let coordinator = LiveSummaryCoordinator::new(UnavailableLiveSummaryProviderFactory, 8);
        let snapshot = coordinator
            .start_session(
                &pool,
                StartLiveSummarySession {
                    meeting_id: "meeting-runtime".to_string(),
                    template_id: Some("custom_safe_1".to_string()),
                },
            )
            .await
            .expect("start");
        let json = serde_json::to_string(&snapshot).expect("serialize");
        assert!(json.contains("custom_safe_1"));
        assert!(!json.contains("apiKey"));
        assert!(!json.contains("prompt"));
        assert!(!json.contains("rawBody"));
    }

    #[tokio::test]
    async fn deferred_coalescing_cannot_replace_retraction_with_late_final() {
        let pool = test_pool().await;
        let coordinator = LiveSummaryCoordinator::new(FakeFactory, 1);
        coordinator
            .start_session(
                &pool,
                StartLiveSummarySession {
                    meeting_id: "meeting-runtime".to_string(),
                    template_id: None,
                },
            )
            .await
            .expect("start");

        coordinator
            .submit_event(
                &pool,
                same_revision_event(
                    "event-in-flight",
                    "utterance-in-flight",
                    TranscriptEventKind::Final,
                    "正在生成",
                ),
            )
            .await
            .expect("start first request");
        let first_request = coordinator
            .in_flight_request("meeting-runtime")
            .await
            .expect("first request");
        coordinator
            .submit_event(
                &pool,
                same_revision_event(
                    "event-dirty",
                    "utterance-dirty",
                    TranscriptEventKind::Final,
                    "占满 actor 队列",
                ),
            )
            .await
            .expect("fill actor queue");

        for event in [
            same_revision_event(
                "event-priority-final",
                "utterance-priority",
                TranscriptEventKind::Final,
                "初始文本",
            ),
            same_revision_event(
                "event-priority-correction",
                "utterance-priority",
                TranscriptEventKind::Correction,
                "纠正文本",
            ),
            same_revision_event(
                "event-priority-retraction",
                "utterance-priority",
                TranscriptEventKind::Retraction,
                "",
            ),
            same_revision_event(
                "event-priority-final-late",
                "utterance-priority",
                TranscriptEventKind::Final,
                "不得回滚",
            ),
        ] {
            coordinator
                .submit_event(&pool, event)
                .await
                .expect("persist and coalesce deferred event");
        }

        coordinator
            .handle_provider_response(
                &pool,
                "meeting-runtime",
                DeterministicFakeProvider::response_for(&first_request, 1_788_307_204_000),
            )
            .await
            .expect("advance to queued request");
        let second_request = coordinator
            .in_flight_request("meeting-runtime")
            .await
            .expect("second request");
        coordinator
            .handle_provider_response(
                &pool,
                "meeting-runtime",
                DeterministicFakeProvider::response_for(&second_request, 1_788_307_205_000),
            )
            .await
            .expect("commit and dispatch deferred retraction");

        let retraction_request = coordinator
            .in_flight_request("meeting-runtime")
            .await
            .expect("retraction request");
        assert_eq!(retraction_request.changes.len(), 1);
        assert_eq!(
            retraction_request.changes[0].evidence.source_event_id,
            "event-priority-retraction"
        );
        assert!(!retraction_request
            .sources
            .iter()
            .any(|source| source.evidence.utterance_id == "utterance-priority"));
    }

    #[tokio::test]
    async fn partial_event_is_rejected_before_storage() {
        let pool = test_pool().await;
        let coordinator = LiveSummaryCoordinator::new(FakeFactory, 8);
        coordinator
            .start_session(
                &pool,
                StartLiveSummarySession {
                    meeting_id: "meeting-runtime".to_string(),
                    template_id: None,
                },
            )
            .await
            .expect("start");
        let mut partial = stable_event("partial-event", 0, "半句话");
        partial.event_kind = TranscriptEventKind::Partial;
        partial.is_stable = false;
        assert!(matches!(
            coordinator.submit_event(&pool, partial).await,
            Err(LiveSummaryCoordinatorError::StableEventRequired)
        ));
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM utterance_revisions WHERE meeting_id = ?")
                .bind("meeting-runtime")
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(count, 0);
    }
}
