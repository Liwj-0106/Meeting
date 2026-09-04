//! Provider-neutral live-summary state machine.
//!
//! This is deliberately an in-process core, not recording integration. The
//! provider begins one request and returns a complete response later through
//! `handle_response`. That split makes late-response and generation behavior
//! deterministic without putting an LLM, network call, or database write on
//! the recording/audio path.

#[cfg(test)]
use super::models::SummaryItemKind;
use super::models::{
    sha256_hex, source_text_hash, validate_identifier, LiveSummaryContractError, LiveSummaryItem,
    LiveSummaryRevision, LiveSummaryRevisionType, LiveSummaryScope, SummaryEvidence,
    SummaryItemStatus,
};
use crate::audio::transcription::event::{TranscriptEvent, TranscriptEventKind, TranscriptUpdate};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummarySourceKind {
    Final,
    Correction,
    Retraction,
}

impl SummarySourceKind {
    pub(crate) fn replay_priority(self) -> u8 {
        match self {
            Self::Final => 1,
            Self::Correction => 2,
            Self::Retraction => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StableSummarySource {
    pub evidence: SummaryEvidence,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_id: Option<String>,
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummarySourceChange {
    pub kind: SummarySourceKind,
    pub evidence: SummaryEvidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_id: Option<String>,
    pub start_ms: u64,
    pub end_ms: u64,
}

impl SummarySourceChange {
    pub(crate) fn from_transcript(
        event: TranscriptEvent,
    ) -> Result<Option<Self>, LiveSummaryActorError> {
        let meeting_id = event
            .meeting_id
            .clone()
            .ok_or(LiveSummaryActorError::MissingMeetingId)?;
        Self::from_transcript_in_scope(event, &LiveSummaryScope::meeting(meeting_id), None)
    }

    pub(crate) fn from_transcript_in_scope(
        event: TranscriptEvent,
        scope: &LiveSummaryScope,
        expected_source_session_id: Option<&str>,
    ) -> Result<Option<Self>, LiveSummaryActorError> {
        let kind = match event.event_kind {
            TranscriptEventKind::Partial => return Ok(None),
            TranscriptEventKind::Final => SummarySourceKind::Final,
            TranscriptEventKind::Correction => SummarySourceKind::Correction,
            TranscriptEventKind::Retraction => SummarySourceKind::Retraction,
            TranscriptEventKind::SpeakerUpdate
            | TranscriptEventKind::LanguageUpdate
            | TranscriptEventKind::Unknown(_) => return Ok(None),
        };

        if !event.is_stable {
            return Err(LiveSummaryActorError::InvalidStableEvent(
                "final, correction and retraction events must be stable",
            ));
        }
        scope.validate()?;
        match scope {
            LiveSummaryScope::Meeting(meeting_id) => {
                if event.meeting_id.as_deref() != Some(meeting_id.as_str()) {
                    return Err(LiveSummaryActorError::ScopeMismatch);
                }
            }
            LiveSummaryScope::RecordingSession(_) => {
                let expected = expected_source_session_id
                    .ok_or(LiveSummaryActorError::MissingSourceSessionId)?;
                if event.session_id.as_deref() != Some(expected) {
                    return Err(LiveSummaryActorError::SourceSessionMismatch);
                }
                if event.meeting_id.is_some() {
                    return Err(LiveSummaryActorError::ScopeMismatch);
                }
            }
        }
        validate_identifier("source.utterance_id", &event.utterance_id)?;
        validate_identifier("source.event_id", &event.event_id)?;
        if kind != SummarySourceKind::Retraction && event.text.trim().is_empty() {
            return Err(LiveSummaryActorError::InvalidStableEvent(
                "final and correction text must not be empty",
            ));
        }

        let speaker_id = event
            .speaker
            .as_ref()
            .map(|speaker| speaker.speaker_id.clone());
        Ok(Some(Self {
            kind,
            evidence: SummaryEvidence {
                scope: scope.clone(),
                utterance_id: event.utterance_id,
                source_revision: event.revision,
                source_event_id: event.event_id,
                source_text_hash: source_text_hash(&event.text),
            },
            text: (kind != SummarySourceKind::Retraction).then_some(event.text),
            speaker_id,
            start_ms: event.start_ms,
            end_ms: event.end_ms,
        }))
    }

    pub(crate) fn validate(&self) -> Result<(), LiveSummaryActorError> {
        self.evidence.validate()?;
        if self.end_ms < self.start_ms {
            return Err(LiveSummaryActorError::InvalidStableEvent(
                "end_ms cannot be earlier than start_ms",
            ));
        }
        if let Some(speaker_id) = self.speaker_id.as_deref() {
            validate_identifier("source.speaker_id", speaker_id)?;
        }
        match (&self.kind, self.text.as_deref()) {
            (SummarySourceKind::Final | SummarySourceKind::Correction, Some(text))
                if !text.trim().is_empty()
                    && source_text_hash(text) == self.evidence.source_text_hash =>
            {
                Ok(())
            }
            (SummarySourceKind::Retraction, None)
                if self.evidence.source_text_hash == source_text_hash("") =>
            {
                Ok(())
            }
            _ => Err(LiveSummaryActorError::InvalidStableEvent(
                "source text and SHA-256 binding do not match the event kind",
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSummaryRequest {
    pub request_id: String,
    pub scope: LiveSummaryScope,
    pub generation: u64,
    pub transcript_cursor: u64,
    pub snapshot_hash: String,
    pub final_reconcile: bool,
    /// Complete authoritative stable transcript snapshot.
    pub sources: Vec<StableSummarySource>,
    /// Dirty events coalesced into this generation. This is provider context,
    /// not a durable raw-response field.
    pub changes: Vec<SummarySourceChange>,
    pub previous_items: Vec<LiveSummaryItem>,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSummaryProviderError {
    pub code: &'static str,
    pub retryable: bool,
}

pub trait LiveSummaryProvider {
    fn provider_id(&self) -> &str;
    fn model_id(&self) -> Option<&str>;
    fn start(&mut self, request: &LiveSummaryRequest) -> Result<(), LiveSummaryProviderError>;
    fn cancel(&mut self, request_id: &str);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveSummaryProviderResult {
    Success { items: Vec<LiveSummaryItem> },
    Failure(LiveSummaryProviderError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSummaryProviderResponse {
    pub request_id: String,
    pub generation: u64,
    pub snapshot_hash: String,
    pub completed_at_ms: u64,
    pub result: LiveSummaryProviderResult,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSummaryActorConfig {
    pub scope: LiveSummaryScope,
    /// Trusted backend ASR session identity. It is never serialized to the
    /// provider or WebView and is required only while the public scope is an
    /// opaque recording-session ID.
    source_session_id: Option<String>,
    /// Maximum accepted dirty events waiting behind the one in-flight request.
    pub queue_capacity: usize,
}

impl LiveSummaryActorConfig {
    pub fn new(meeting_id: impl Into<String>, queue_capacity: usize) -> Self {
        Self {
            scope: LiveSummaryScope::meeting(meeting_id),
            source_session_id: None,
            queue_capacity,
        }
    }

    pub fn for_recording_session(
        scope_id: impl Into<String>,
        source_session_id: impl Into<String>,
        queue_capacity: usize,
    ) -> Self {
        Self {
            scope: LiveSummaryScope::recording_session(scope_id),
            source_session_id: Some(source_session_id.into()),
            queue_capacity,
        }
    }

    fn validate(&self) -> Result<(), LiveSummaryActorError> {
        self.scope.validate()?;
        match (&self.scope, self.source_session_id.as_deref()) {
            (LiveSummaryScope::Meeting(_), None) => {}
            (LiveSummaryScope::RecordingSession(_), Some(value)) => {
                validate_identifier("config.source_session_id", value)?;
            }
            _ => return Err(LiveSummaryActorError::InvalidScopeConfiguration),
        }
        if self.queue_capacity == 0 {
            return Err(LiveSummaryActorError::InvalidQueueCapacity);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchOutcome {
    Started { generation: u64 },
    WaitingForInFlight,
    Idle,
    Deferred { error: LiveSummaryProviderError },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummarySubmitOutcome {
    Accepted {
        transcript_cursor: u64,
        coalesced_event_id: Option<String>,
        dispatch: DispatchOutcome,
    },
    Duplicate {
        event_id: String,
    },
    Stale {
        event_id: String,
        current_revision: u64,
    },
    IgnoredPartial {
        event_id: String,
    },
    IgnoredUnsupported {
        event_id: String,
        event_kind: String,
    },
    /// The actor did not mutate its source state. The caller owns this event
    /// and can retry it after a request completion drains the dirty queue.
    RetryRequired {
        event: SummarySourceChange,
        queue_capacity: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryResponseOutcome {
    Committed {
        revision: LiveSummaryRevision,
        next_dispatch: DispatchOutcome,
    },
    IgnoredStale {
        request_id: String,
        next_dispatch: DispatchOutcome,
    },
    Rejected {
        error: LiveSummaryActorError,
        next_dispatch: DispatchOutcome,
    },
    ProviderFailed {
        error: LiveSummaryProviderError,
        next_dispatch: DispatchOutcome,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedLiveSummaryRevision {
    request_id: String,
    revision: LiveSummaryRevision,
}

impl PreparedLiveSummaryRevision {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn revision(&self) -> &LiveSummaryRevision {
        &self.revision
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryResponsePreparation {
    Ready(PreparedLiveSummaryRevision),
    Resolved(SummaryResponseOutcome),
}

#[derive(Debug, Clone, PartialEq)]
pub struct LiveSummaryRecoveryState {
    pub last_complete_revision: LiveSummaryRevision,
    pub current_transcript_cursor: u64,
    /// Latest stable head for each utterance. Retraction heads should be
    /// included so a delayed pre-retraction event cannot resurrect text.
    pub transcript_heads: Vec<TranscriptEvent>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LiveSummaryActorError {
    #[error(transparent)]
    Contract(#[from] LiveSummaryContractError),
    #[error("live summary queue capacity must be at least one")]
    InvalidQueueCapacity,
    #[error("stable transcript event is missing meeting_id")]
    MissingMeetingId,
    #[error("stable transcript event belongs to another summary scope")]
    ScopeMismatch,
    #[error("recording summary scope is missing its trusted source session")]
    MissingSourceSessionId,
    #[error("stable transcript event belongs to another recording session")]
    SourceSessionMismatch,
    #[error("live summary scope configuration is invalid")]
    InvalidScopeConfiguration,
    #[error("invalid stable transcript event: {0}")]
    InvalidStableEvent(&'static str),
    #[error("same transcript revision has conflicting event identities")]
    TranscriptRevisionConflict,
    #[error("provider returned duplicate summary item IDs")]
    DuplicateProviderItemId,
    #[error("active summary items must contain evidence")]
    MissingEvidence,
    #[error("provider response evidence no longer matches the request snapshot")]
    StaleEvidence,
    #[error("prepared provider response no longer matches the in-flight request")]
    PreparedResponseMismatch,
    #[error("recovery cursor is earlier than the last complete summary revision")]
    RecoveryCursorRegressed,
    #[error("completed_at_ms is outside the supported timestamp range")]
    TimestampOutOfRange,
    #[error("a finalized live summary session cannot accept more transcript events")]
    AlreadyFinalized,
}

#[derive(Debug, Clone)]
struct UtteranceHead {
    revision: u64,
    event_id: String,
    kind: SummarySourceKind,
}

#[derive(Debug, Clone)]
struct InFlightRequest {
    request: LiveSummaryRequest,
}

pub struct LiveSummaryActor<P: LiveSummaryProvider> {
    config: LiveSummaryActorConfig,
    provider: P,
    sources: BTreeMap<String, StableSummarySource>,
    utterance_heads: HashMap<String, UtteranceHead>,
    dirty_events: VecDeque<SummarySourceChange>,
    seen_event_ids: HashSet<String>,
    in_flight: Option<InFlightRequest>,
    items: Vec<LiveSummaryItem>,
    transcript_cursor: u64,
    revision_counter: u64,
    generation_counter: u64,
    force_generation: bool,
    final_requested: bool,
    finalized: bool,
}

impl<P: LiveSummaryProvider> LiveSummaryActor<P> {
    pub fn new(config: LiveSummaryActorConfig, provider: P) -> Result<Self, LiveSummaryActorError> {
        config.validate()?;
        Ok(Self {
            config,
            provider,
            sources: BTreeMap::new(),
            utterance_heads: HashMap::new(),
            dirty_events: VecDeque::new(),
            seen_event_ids: HashSet::new(),
            in_flight: None,
            items: Vec::new(),
            transcript_cursor: 0,
            revision_counter: 0,
            generation_counter: 0,
            force_generation: false,
            final_requested: false,
            finalized: false,
        })
    }

    /// Restore authoritative stable transcript heads before any live-summary
    /// revision has been persisted for the meeting.
    pub fn hydrate_transcript_heads(
        config: LiveSummaryActorConfig,
        provider: P,
        current_transcript_cursor: u64,
        transcript_heads: Vec<TranscriptEvent>,
    ) -> Result<Self, LiveSummaryActorError> {
        let mut actor = Self::new(config, provider)?;
        for event in transcript_heads {
            let Some(change) = SummarySourceChange::from_transcript_in_scope(
                event,
                &actor.config.scope,
                actor.config.source_session_id.as_deref(),
            )?
            else {
                continue;
            };
            if change.evidence.scope != actor.config.scope {
                return Err(LiveSummaryActorError::ScopeMismatch);
            }
            actor.apply_recovery_head(change)?;
        }
        actor.transcript_cursor = current_transcript_cursor;
        actor.force_generation = !actor.sources.is_empty();
        Ok(actor)
    }

    pub fn recover(
        config: LiveSummaryActorConfig,
        provider: P,
        recovery: LiveSummaryRecoveryState,
    ) -> Result<Self, LiveSummaryActorError> {
        config.validate()?;
        recovery.last_complete_revision.validate()?;
        if recovery.last_complete_revision.scope != config.scope {
            return Err(LiveSummaryActorError::ScopeMismatch);
        }
        if recovery.current_transcript_cursor < recovery.last_complete_revision.transcript_cursor {
            return Err(LiveSummaryActorError::RecoveryCursorRegressed);
        }

        let last = recovery.last_complete_revision;
        let mut actor = Self {
            config,
            provider,
            sources: BTreeMap::new(),
            utterance_heads: HashMap::new(),
            dirty_events: VecDeque::new(),
            seen_event_ids: HashSet::new(),
            in_flight: None,
            items: last.items.clone(),
            transcript_cursor: recovery.current_transcript_cursor,
            revision_counter: last.revision,
            generation_counter: last.generation,
            force_generation: false,
            final_requested: false,
            finalized: false,
        };

        for event in recovery.transcript_heads {
            let Some(change) = SummarySourceChange::from_transcript_in_scope(
                event,
                &actor.config.scope,
                actor.config.source_session_id.as_deref(),
            )?
            else {
                continue;
            };
            if change.evidence.scope != actor.config.scope {
                return Err(LiveSummaryActorError::ScopeMismatch);
            }
            actor.apply_recovery_head(change)?;
        }
        actor.invalidate_items_against_sources();
        actor.force_generation = snapshot_hash(&actor.config.scope, &actor.sources)
            != last.snapshot_hash
            || actor.transcript_cursor > last.transcript_cursor;
        actor.finalized =
            last.revision_type == LiveSummaryRevisionType::Final && !actor.force_generation;
        Ok(actor)
    }

    pub fn provider(&self) -> &P {
        &self.provider
    }

    pub fn provider_mut(&mut self) -> &mut P {
        &mut self.provider
    }

    pub fn current_items(&self) -> &[LiveSummaryItem] {
        &self.items
    }

    pub fn transcript_cursor(&self) -> u64 {
        self.transcript_cursor
    }

    pub fn counters(&self) -> (u64, u64) {
        (self.revision_counter, self.generation_counter)
    }

    pub fn in_flight_request(&self) -> Option<&LiveSummaryRequest> {
        self.in_flight.as_ref().map(|value| &value.request)
    }

    pub fn dirty_len(&self) -> usize {
        self.dirty_events.len()
    }

    pub fn is_finalized(&self) -> bool {
        self.finalized
    }

    /// Convert an in-memory recording-session revision into a canonical
    /// meeting revision after the trusted transcript save created the meeting.
    /// Only scope fields and the scope-bound snapshot hash change; item text
    /// and exact event/revision/text hashes remain untouched.
    pub(crate) fn rebind_revision_to_meeting(
        &self,
        revision: &LiveSummaryRevision,
        meeting_id: &str,
    ) -> Result<LiveSummaryRevision, LiveSummaryActorError> {
        if revision.scope != self.config.scope
            || !matches!(&self.config.scope, LiveSummaryScope::RecordingSession(_))
        {
            return Err(LiveSummaryActorError::ScopeMismatch);
        }
        let target_scope = LiveSummaryScope::meeting(meeting_id);
        target_scope.validate()?;
        let mut rebound = revision.clone();
        rebound.summary_revision_id = Uuid::new_v4().to_string();
        rebound.scope = target_scope.clone();
        rebound.snapshot_hash = snapshot_hash(&target_scope, &self.sources);
        // The canonical meeting always performs its own final pass. A final
        // response produced against the temporary session scope is retained
        // as a live revision, not misrepresented as the meeting's final head.
        rebound.revision_type = LiveSummaryRevisionType::Live;
        for item in &mut rebound.items {
            for evidence in &mut item.evidence {
                if evidence.scope != self.config.scope {
                    return Err(LiveSummaryActorError::ScopeMismatch);
                }
                evidence.scope = target_scope.clone();
            }
        }
        rebound.validate()?;
        Ok(rebound)
    }

    pub fn submit_update(
        &mut self,
        update: TranscriptUpdate,
    ) -> Result<SummarySubmitOutcome, LiveSummaryActorError> {
        self.submit_event(update.normalize())
    }

    pub fn submit_event(
        &mut self,
        event: TranscriptEvent,
    ) -> Result<SummarySubmitOutcome, LiveSummaryActorError> {
        let event_id = event.event_id.clone();
        let event_kind = event.event_kind.clone();
        if event_kind == TranscriptEventKind::Partial {
            return Ok(SummarySubmitOutcome::IgnoredPartial { event_id });
        }
        if !matches!(
            event_kind,
            TranscriptEventKind::Final
                | TranscriptEventKind::Correction
                | TranscriptEventKind::Retraction
        ) {
            return Ok(SummarySubmitOutcome::IgnoredUnsupported {
                event_id,
                event_kind: event_kind.as_str().to_string(),
            });
        }

        let change = SummarySourceChange::from_transcript_in_scope(
            event,
            &self.config.scope,
            self.config.source_session_id.as_deref(),
        )?
        .expect("eligible event kinds always create a summary source change");
        self.submit_change(change)
    }

    /// Submit a previously backpressured source change without reconstructing
    /// a transcript event. `RetryRequired` guarantees the actor did not mark
    /// the returned event as seen or advance its transcript cursor.
    pub fn submit_change(
        &mut self,
        change: SummarySourceChange,
    ) -> Result<SummarySubmitOutcome, LiveSummaryActorError> {
        if self.finalized {
            return Err(LiveSummaryActorError::AlreadyFinalized);
        }
        change.validate()?;
        if change.evidence.scope != self.config.scope {
            return Err(LiveSummaryActorError::ScopeMismatch);
        }
        if self
            .seen_event_ids
            .contains(&change.evidence.source_event_id)
        {
            return Ok(SummarySubmitOutcome::Duplicate {
                event_id: change.evidence.source_event_id,
            });
        }

        if let Some(head) = self.utterance_heads.get(&change.evidence.utterance_id) {
            if change.evidence.source_revision < head.revision {
                return Ok(SummarySubmitOutcome::Stale {
                    event_id: change.evidence.source_event_id,
                    current_revision: head.revision,
                });
            }
            if change.evidence.source_revision == head.revision {
                if change.evidence.source_event_id == head.event_id && change.kind == head.kind {
                    return Ok(SummarySubmitOutcome::Duplicate {
                        event_id: change.evidence.source_event_id,
                    });
                }
                match change
                    .kind
                    .replay_priority()
                    .cmp(&head.kind.replay_priority())
                {
                    std::cmp::Ordering::Greater => {}
                    std::cmp::Ordering::Less => {
                        return Ok(SummarySubmitOutcome::Stale {
                            event_id: change.evidence.source_event_id,
                            current_revision: head.revision,
                        });
                    }
                    std::cmp::Ordering::Equal => {
                        return Err(LiveSummaryActorError::TranscriptRevisionConflict);
                    }
                }
            }
        }

        let coalesced_position = self
            .dirty_events
            .iter()
            .position(|queued| queued.evidence.utterance_id == change.evidence.utterance_id);
        if coalesced_position.is_none() && self.dirty_events.len() >= self.config.queue_capacity {
            return Ok(SummarySubmitOutcome::RetryRequired {
                event: change,
                queue_capacity: self.config.queue_capacity,
            });
        }

        self.apply_change(&change);
        self.seen_event_ids
            .insert(change.evidence.source_event_id.clone());
        self.transcript_cursor = self.transcript_cursor.saturating_add(1);
        self.finalized = false;

        let coalesced_event_id = if let Some(position) = coalesced_position {
            let old = std::mem::replace(&mut self.dirty_events[position], change);
            Some(old.evidence.source_event_id)
        } else {
            self.dirty_events.push_back(change);
            None
        };

        let dispatch = self.start_if_needed();
        Ok(SummarySubmitOutcome::Accepted {
            transcript_cursor: self.transcript_cursor,
            coalesced_event_id,
            dispatch,
        })
    }

    /// Request one final provider pass over the complete stable snapshot. If a
    /// live request is already running, the final pass waits behind it.
    pub fn request_final_reconcile(&mut self) -> DispatchOutcome {
        if self.finalized {
            return DispatchOutcome::Idle;
        }
        self.final_requested = true;
        self.force_generation = true;
        self.start_if_needed()
    }

    /// Cancel only the provider request. Transcript state and accepted dirty
    /// events remain authoritative, and a new generation starts immediately.
    pub fn cancel_in_flight(&mut self) -> DispatchOutcome {
        if let Some(in_flight) = self.in_flight.take() {
            self.provider.cancel(&in_flight.request.request_id);
            self.force_generation = true;
            if in_flight.request.final_reconcile {
                self.final_requested = true;
            }
        }
        self.start_if_needed()
    }

    pub fn retry_deferred_dispatch(&mut self) -> DispatchOutcome {
        self.start_if_needed()
    }

    pub fn handle_response(
        &mut self,
        response: LiveSummaryProviderResponse,
    ) -> SummaryResponseOutcome {
        match self.prepare_response(response) {
            SummaryResponsePreparation::Ready(prepared) => {
                let request_id = prepared.request_id.clone();
                match self.commit_prepared_response(prepared) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        let _ = self.discard_prepared_response(&request_id);
                        SummaryResponseOutcome::Rejected {
                            error,
                            next_dispatch: self.start_if_needed(),
                        }
                    }
                }
            }
            SummaryResponsePreparation::Resolved(outcome) => outcome,
        }
    }

    /// Validate a successful provider response and build the immutable
    /// revision without advancing actor state. The coordinator can persist
    /// this revision first, then call `commit_prepared_response`; a failed
    /// database transaction therefore never becomes the public actor head.
    pub fn prepare_response(
        &mut self,
        response: LiveSummaryProviderResponse,
    ) -> SummaryResponsePreparation {
        let Some(current) = self.in_flight.as_ref() else {
            return SummaryResponsePreparation::Resolved(SummaryResponseOutcome::IgnoredStale {
                request_id: response.request_id,
                next_dispatch: self.start_if_needed(),
            });
        };
        if response.request_id != current.request.request_id {
            return SummaryResponsePreparation::Resolved(SummaryResponseOutcome::IgnoredStale {
                request_id: response.request_id,
                next_dispatch: DispatchOutcome::WaitingForInFlight,
            });
        }

        let request = current.request.clone();
        let current_hash = snapshot_hash(&self.config.scope, &self.sources);
        if response.generation != request.generation
            || response.snapshot_hash != request.snapshot_hash
            || current_hash != request.snapshot_hash
        {
            let next_dispatch = self.reject_matching_response(request.final_reconcile);
            return SummaryResponsePreparation::Resolved(SummaryResponseOutcome::IgnoredStale {
                request_id: response.request_id,
                next_dispatch,
            });
        }

        let items = match response.result {
            LiveSummaryProviderResult::Success { items } => items,
            LiveSummaryProviderResult::Failure(error) => {
                self.pause_after_rejected_response(request.final_reconcile);
                let next_dispatch = DispatchOutcome::Deferred {
                    error: error.clone(),
                };
                return SummaryResponsePreparation::Resolved(
                    SummaryResponseOutcome::ProviderFailed {
                        error,
                        next_dispatch,
                    },
                );
            }
        };

        if let Err(error) = self.validate_provider_items(&items) {
            self.pause_after_rejected_response(request.final_reconcile);
            let next_dispatch = DispatchOutcome::Idle;
            return SummaryResponsePreparation::Resolved(SummaryResponseOutcome::Rejected {
                error,
                next_dispatch,
            });
        }

        let revision_type = if request.final_reconcile {
            LiveSummaryRevisionType::Final
        } else {
            LiveSummaryRevisionType::Live
        };
        let created_at = match timestamp_from_millis(response.completed_at_ms) {
            Ok(value) => value,
            Err(error) => {
                self.pause_after_rejected_response(request.final_reconcile);
                let next_dispatch = DispatchOutcome::Idle;
                return SummaryResponsePreparation::Resolved(SummaryResponseOutcome::Rejected {
                    error,
                    next_dispatch,
                });
            }
        };
        let revision = LiveSummaryRevision {
            summary_revision_id: Uuid::new_v4().to_string(),
            scope: self.config.scope.clone(),
            revision: self.revision_counter.saturating_add(1),
            generation: request.generation,
            transcript_cursor: request.transcript_cursor,
            snapshot_hash: request.snapshot_hash,
            revision_type,
            provider: request.provider,
            model: request.model,
            created_at,
            items,
        };
        SummaryResponsePreparation::Ready(PreparedLiveSummaryRevision {
            request_id: response.request_id,
            revision,
        })
    }

    pub fn commit_prepared_response(
        &mut self,
        prepared: PreparedLiveSummaryRevision,
    ) -> Result<SummaryResponseOutcome, LiveSummaryActorError> {
        let current = self
            .in_flight
            .as_ref()
            .ok_or(LiveSummaryActorError::PreparedResponseMismatch)?;
        let request = &current.request;
        let revision = &prepared.revision;
        let expected_type = if request.final_reconcile {
            LiveSummaryRevisionType::Final
        } else {
            LiveSummaryRevisionType::Live
        };
        if prepared.request_id != request.request_id
            || revision.scope != self.config.scope
            || revision.revision != self.revision_counter.saturating_add(1)
            || revision.generation != request.generation
            || revision.transcript_cursor != request.transcript_cursor
            || revision.snapshot_hash != request.snapshot_hash
            || revision.revision_type != expected_type
            || revision.provider != request.provider
            || revision.model != request.model
            || snapshot_hash(&self.config.scope, &self.sources) != request.snapshot_hash
        {
            return Err(LiveSummaryActorError::PreparedResponseMismatch);
        }
        revision.validate()?;

        self.in_flight
            .take()
            .expect("prepared response matched an in-flight request");
        let revision = prepared.revision;
        self.revision_counter = revision.revision;
        self.items = revision.items.clone();
        self.force_generation = false;
        if revision.revision_type == LiveSummaryRevisionType::Final {
            self.finalized = true;
        }
        let next_dispatch = if self.finalized {
            DispatchOutcome::Idle
        } else {
            self.start_if_needed()
        };
        Ok(SummaryResponseOutcome::Committed {
            revision,
            next_dispatch,
        })
    }

    /// Drop a validated response after persistence failed. Source state stays
    /// authoritative and a later retry can regenerate the same snapshot.
    pub fn discard_prepared_response(
        &mut self,
        request_id: &str,
    ) -> Result<(), LiveSummaryActorError> {
        let current = self
            .in_flight
            .as_ref()
            .ok_or(LiveSummaryActorError::PreparedResponseMismatch)?;
        if current.request.request_id != request_id {
            return Err(LiveSummaryActorError::PreparedResponseMismatch);
        }
        let request = self
            .in_flight
            .take()
            .expect("prepared response matched an in-flight request")
            .request;
        self.force_generation = true;
        if request.final_reconcile {
            self.final_requested = true;
        }
        Ok(())
    }

    fn reject_matching_response(&mut self, final_reconcile: bool) -> DispatchOutcome {
        self.in_flight
            .take()
            .expect("matching in-flight request was checked before rejection");
        self.force_generation = true;
        if final_reconcile {
            self.final_requested = true;
        }
        self.start_if_needed()
    }

    /// Stop automatic dispatch after a provider or output-contract failure.
    /// A later stable transcript event, explicit final reconcile, or future
    /// retry scheduler may start a new generation; the response channel must
    /// never become an unbounded cloud-call loop.
    fn pause_after_rejected_response(&mut self, final_reconcile: bool) {
        self.in_flight
            .take()
            .expect("matching in-flight request was checked before rejection");
        self.force_generation = true;
        if final_reconcile {
            self.final_requested = true;
        }
    }

    fn start_if_needed(&mut self) -> DispatchOutcome {
        if self.in_flight.is_some() {
            return DispatchOutcome::WaitingForInFlight;
        }
        if self.dirty_events.is_empty() && !self.force_generation && !self.final_requested {
            return DispatchOutcome::Idle;
        }

        self.generation_counter = self.generation_counter.saturating_add(1);
        let generation = self.generation_counter;
        let final_reconcile = self.final_requested;
        let provider = self.provider.provider_id().to_string();
        let model = self.provider.model_id().map(ToOwned::to_owned);
        let request = LiveSummaryRequest {
            request_id: Uuid::new_v4().to_string(),
            scope: self.config.scope.clone(),
            generation,
            transcript_cursor: self.transcript_cursor,
            snapshot_hash: snapshot_hash(&self.config.scope, &self.sources),
            final_reconcile,
            sources: ordered_source_snapshot(&self.sources),
            changes: self.dirty_events.iter().cloned().collect(),
            previous_items: self.items.clone(),
            provider,
            model,
        };

        match self.provider.start(&request) {
            Ok(()) => {
                self.dirty_events.clear();
                self.force_generation = false;
                if final_reconcile {
                    self.final_requested = false;
                }
                self.in_flight = Some(InFlightRequest { request });
                DispatchOutcome::Started { generation }
            }
            Err(error) => {
                self.force_generation = true;
                DispatchOutcome::Deferred { error }
            }
        }
    }

    fn apply_change(&mut self, change: &SummarySourceChange) {
        let utterance_id = change.evidence.utterance_id.clone();
        self.utterance_heads.insert(
            utterance_id.clone(),
            UtteranceHead {
                revision: change.evidence.source_revision,
                event_id: change.evidence.source_event_id.clone(),
                kind: change.kind,
            },
        );

        match change.kind {
            SummarySourceKind::Final | SummarySourceKind::Correction => {
                self.sources.insert(
                    utterance_id.clone(),
                    StableSummarySource {
                        evidence: change.evidence.clone(),
                        text: change.text.clone().unwrap_or_default(),
                        speaker_id: change.speaker_id.clone(),
                        start_ms: change.start_ms,
                        end_ms: change.end_ms,
                    },
                );
                if change.kind == SummarySourceKind::Correction {
                    self.mark_items_for_utterance(&utterance_id, SummaryItemStatus::NeedsReview);
                }
            }
            SummarySourceKind::Retraction => {
                self.sources.remove(&utterance_id);
                self.mark_items_for_utterance(&utterance_id, SummaryItemStatus::Retracted);
            }
        }
    }

    fn apply_recovery_head(
        &mut self,
        change: SummarySourceChange,
    ) -> Result<(), LiveSummaryActorError> {
        if let Some(head) = self.utterance_heads.get(&change.evidence.utterance_id) {
            if change.evidence.source_revision < head.revision {
                return Ok(());
            }
            if change.evidence.source_revision == head.revision {
                if change.evidence.source_event_id == head.event_id && change.kind == head.kind {
                    return Ok(());
                }
                match change
                    .kind
                    .replay_priority()
                    .cmp(&head.kind.replay_priority())
                {
                    std::cmp::Ordering::Greater => {}
                    std::cmp::Ordering::Less => return Ok(()),
                    std::cmp::Ordering::Equal => {
                        return Err(LiveSummaryActorError::TranscriptRevisionConflict);
                    }
                }
            }
        }
        self.seen_event_ids
            .insert(change.evidence.source_event_id.clone());
        self.apply_change(&change);
        Ok(())
    }

    fn mark_items_for_utterance(&mut self, utterance_id: &str, status: SummaryItemStatus) {
        for item in &mut self.items {
            if item
                .evidence
                .iter()
                .any(|evidence| evidence.utterance_id == utterance_id)
            {
                item.status = match (item.status, status) {
                    (SummaryItemStatus::Retracted, _) | (_, SummaryItemStatus::Retracted) => {
                        SummaryItemStatus::Retracted
                    }
                    _ => SummaryItemStatus::NeedsReview,
                };
            }
        }
    }

    fn invalidate_items_against_sources(&mut self) {
        for item in &mut self.items {
            let mut missing = false;
            let mut changed = false;
            for evidence in &item.evidence {
                match self.sources.get(&evidence.utterance_id) {
                    None => missing = true,
                    Some(source) if source.evidence != *evidence => changed = true,
                    Some(_) => {}
                }
            }
            if missing {
                item.status = SummaryItemStatus::Retracted;
            } else if changed {
                item.status = SummaryItemStatus::NeedsReview;
            }
        }
    }

    fn validate_provider_items(
        &self,
        items: &[LiveSummaryItem],
    ) -> Result<(), LiveSummaryActorError> {
        let mut item_ids = HashSet::with_capacity(items.len());
        for item in items {
            item.validate(&self.config.scope)?;
            if !item_ids.insert(item.item_id.as_str()) {
                return Err(LiveSummaryActorError::DuplicateProviderItemId);
            }
            if item.status != SummaryItemStatus::Retracted && item.evidence.is_empty() {
                return Err(LiveSummaryActorError::MissingEvidence);
            }
            for evidence in &item.evidence {
                let Some(source) = self.sources.get(&evidence.utterance_id) else {
                    return Err(LiveSummaryActorError::StaleEvidence);
                };
                if source.evidence != *evidence {
                    return Err(LiveSummaryActorError::StaleEvidence);
                }
            }
        }
        Ok(())
    }
}

fn snapshot_hash(
    scope: &LiveSummaryScope,
    sources: &BTreeMap<String, StableSummarySource>,
) -> String {
    let mut bytes = Vec::new();
    append_field(
        &mut bytes,
        match scope {
            LiveSummaryScope::Meeting(_) => b"meeting".as_slice(),
            LiveSummaryScope::RecordingSession(_) => b"recording_session".as_slice(),
        },
    );
    append_field(&mut bytes, scope.id().as_bytes());
    for source in ordered_source_snapshot(sources) {
        append_field(&mut bytes, source.evidence.utterance_id.as_bytes());
        append_field(&mut bytes, &source.evidence.source_revision.to_be_bytes());
        append_field(&mut bytes, source.evidence.source_event_id.as_bytes());
        append_field(&mut bytes, source.evidence.source_text_hash.as_bytes());
        append_field(&mut bytes, source.text.as_bytes());
    }
    sha256_hex(&bytes)
}

fn ordered_source_snapshot(
    sources: &BTreeMap<String, StableSummarySource>,
) -> Vec<StableSummarySource> {
    let mut ordered = sources.values().cloned().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        (
            left.start_ms,
            left.end_ms,
            left.evidence.utterance_id.as_str(),
        )
            .cmp(&(
                right.start_ms,
                right.end_ms,
                right.evidence.utterance_id.as_str(),
            ))
    });
    ordered
}

fn append_field(target: &mut Vec<u8>, value: &[u8]) {
    target.extend_from_slice(&(value.len() as u64).to_be_bytes());
    target.extend_from_slice(value);
}

fn timestamp_from_millis(value: u64) -> Result<String, LiveSummaryActorError> {
    let millis = i64::try_from(value).map_err(|_| LiveSummaryActorError::TimestampOutOfRange)?;
    let timestamp = chrono::DateTime::<Utc>::from_timestamp_millis(millis)
        .ok_or(LiveSummaryActorError::TimestampOutOfRange)?;
    Ok(timestamp.to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// Deterministic fake used until a real provider adapter is wired. It records
/// requests but performs no I/O, model loading, or background work.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct DeterministicFakeProvider {
    provider_id: String,
    model_id: String,
    started: Vec<LiveSummaryRequest>,
    canceled: Vec<String>,
    fail_next_start: Option<LiveSummaryProviderError>,
}

#[cfg(test)]
impl Default for DeterministicFakeProvider {
    fn default() -> Self {
        Self {
            provider_id: "deterministic-fake".to_string(),
            model_id: "fixture-v1".to_string(),
            started: Vec::new(),
            canceled: Vec::new(),
            fail_next_start: None,
        }
    }
}

#[cfg(test)]
impl DeterministicFakeProvider {
    pub fn started_requests(&self) -> &[LiveSummaryRequest] {
        &self.started
    }

    pub fn canceled_request_ids(&self) -> &[String] {
        &self.canceled
    }

    pub fn fail_next_start(&mut self, error: LiveSummaryProviderError) {
        self.fail_next_start = Some(error);
    }

    pub fn response_for(
        request: &LiveSummaryRequest,
        completed_at_ms: u64,
    ) -> LiveSummaryProviderResponse {
        let items = if request.sources.is_empty() {
            Vec::new()
        } else {
            let mut stable_key = Vec::new();
            append_field(&mut stable_key, request.scope.id().as_bytes());
            append_field(&mut stable_key, SummaryItemKind::Topic.as_str().as_bytes());
            let identity = sha256_hex(&stable_key);
            let body = request
                .sources
                .iter()
                .map(|source| source.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            vec![LiveSummaryItem {
                item_id: format!("topic-{}", &identity[..24]),
                kind: SummaryItemKind::Topic,
                title: "Transcript snapshot".to_string(),
                body,
                owner: None,
                due_at: None,
                status: SummaryItemStatus::Active,
                evidence: request
                    .sources
                    .iter()
                    .map(|source| source.evidence.clone())
                    .collect(),
            }]
        };
        LiveSummaryProviderResponse {
            request_id: request.request_id.clone(),
            generation: request.generation,
            snapshot_hash: request.snapshot_hash.clone(),
            completed_at_ms,
            result: LiveSummaryProviderResult::Success { items },
        }
    }
}

#[cfg(test)]
impl LiveSummaryProvider for DeterministicFakeProvider {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn model_id(&self) -> Option<&str> {
        Some(&self.model_id)
    }

    fn start(&mut self, request: &LiveSummaryRequest) -> Result<(), LiveSummaryProviderError> {
        if let Some(error) = self.fail_next_start.take() {
            return Err(error);
        }
        self.started.push(request.clone());
        Ok(())
    }

    fn cancel(&mut self, request_id: &str) {
        self.canceled.push(request_id.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::transcription::event::{AudioSource, TRANSCRIPT_EVENT_SCHEMA_VERSION};

    fn transcript(
        utterance: &str,
        revision: u64,
        kind: TranscriptEventKind,
        text: &str,
    ) -> TranscriptEvent {
        TranscriptEvent {
            schema_version: TRANSCRIPT_EVENT_SCHEMA_VERSION,
            event_id: format!("event-{utterance}-{revision}-{}", kind.as_str()),
            meeting_id: Some("meeting-live".to_string()),
            session_id: Some("session-live".to_string()),
            utterance_id: utterance.to_string(),
            revision,
            is_stable: kind != TranscriptEventKind::Partial,
            event_kind: kind,
            start_ms: revision * 1_000,
            end_ms: revision * 1_000 + 900,
            text: text.to_string(),
            language: Some("zh".to_string()),
            audio_source: AudioSource::Mixed,
            speaker: None,
            asr: None,
            diarization: None,
            replaces_event_id: None,
            provider_event_id: None,
            created_at: "2026-09-02T00:00:00.000Z".to_string(),
            trace_id: None,
            sequence_id: Some(revision),
        }
    }

    fn actor(capacity: usize) -> LiveSummaryActor<DeterministicFakeProvider> {
        LiveSummaryActor::new(
            LiveSummaryActorConfig::new("meeting-live", capacity),
            DeterministicFakeProvider::default(),
        )
        .expect("create actor")
    }

    fn current_request(actor: &LiveSummaryActor<DeterministicFakeProvider>) -> LiveSummaryRequest {
        actor
            .in_flight_request()
            .expect("request should be in flight")
            .clone()
    }

    fn complete_current(
        actor: &mut LiveSummaryActor<DeterministicFakeProvider>,
        completed_at_ms: u64,
    ) -> SummaryResponseOutcome {
        let request = current_request(actor);
        actor.handle_response(DeterministicFakeProvider::response_for(
            &request,
            completed_at_ms,
        ))
    }

    #[test]
    fn partial_is_explicitly_ignored() {
        let mut actor = actor(2);
        let outcome = actor
            .submit_event(transcript(
                "utterance-1",
                0,
                TranscriptEventKind::Partial,
                "临时文本",
            ))
            .expect("handle partial");
        assert!(matches!(
            outcome,
            SummarySubmitOutcome::IgnoredPartial { .. }
        ));
        assert_eq!(actor.transcript_cursor(), 0);
        assert!(actor.provider().started_requests().is_empty());
    }

    #[test]
    fn dirty_events_coalesce_behind_exactly_one_in_flight_request() {
        let mut actor = actor(3);
        actor
            .submit_event(transcript(
                "utterance-1",
                1,
                TranscriptEventKind::Final,
                "第一段",
            ))
            .expect("submit first");
        let first = current_request(&actor);

        actor
            .submit_event(transcript(
                "utterance-2",
                1,
                TranscriptEventKind::Final,
                "第二段初稿",
            ))
            .expect("submit second");
        let coalesced = actor
            .submit_event(transcript(
                "utterance-2",
                2,
                TranscriptEventKind::Correction,
                "第二段修订",
            ))
            .expect("submit correction");
        assert!(matches!(
            coalesced,
            SummarySubmitOutcome::Accepted {
                coalesced_event_id: Some(_),
                dispatch: DispatchOutcome::WaitingForInFlight,
                ..
            }
        ));
        assert_eq!(actor.provider().started_requests().len(), 1);
        assert_eq!(actor.dirty_len(), 1);

        let stale = actor.handle_response(DeterministicFakeProvider::response_for(&first, 10));
        assert!(matches!(
            stale,
            SummaryResponseOutcome::IgnoredStale {
                next_dispatch: DispatchOutcome::Started { generation: 2 },
                ..
            }
        ));
        assert_eq!(actor.provider().started_requests().len(), 2);
        assert_eq!(actor.in_flight_request().map(|r| r.sources.len()), Some(2));
    }

    #[test]
    fn canceled_generation_and_late_response_cannot_overwrite_new_state() {
        let mut actor = actor(2);
        actor
            .submit_event(transcript(
                "utterance-1",
                1,
                TranscriptEventKind::Final,
                "内容",
            ))
            .expect("submit");
        let first = current_request(&actor);
        assert!(matches!(
            actor.cancel_in_flight(),
            DispatchOutcome::Started { generation: 2 }
        ));
        let second = current_request(&actor);
        assert_eq!(
            actor.provider().canceled_request_ids(),
            &[first.request_id.clone()]
        );

        let late = actor.handle_response(DeterministicFakeProvider::response_for(&first, 20));
        assert!(matches!(
            late,
            SummaryResponseOutcome::IgnoredStale {
                next_dispatch: DispatchOutcome::WaitingForInFlight,
                ..
            }
        ));
        assert_eq!(
            actor.in_flight_request().map(|r| r.request_id.as_str()),
            Some(second.request_id.as_str())
        );

        let accepted = actor.handle_response(DeterministicFakeProvider::response_for(&second, 21));
        assert!(matches!(
            accepted,
            SummaryResponseOutcome::Committed { ref revision, .. }
                if revision.generation == 2 && revision.revision == 1
        ));
    }

    #[test]
    fn correction_and_retraction_invalidate_old_evidence_until_replaced() {
        let mut actor = actor(2);
        actor
            .submit_event(transcript(
                "utterance-1",
                1,
                TranscriptEventKind::Final,
                "旧文本",
            ))
            .expect("submit initial");
        assert!(matches!(
            complete_current(&mut actor, 30),
            SummaryResponseOutcome::Committed { .. }
        ));
        let stable_item_id = actor.current_items()[0].item_id.clone();

        actor
            .submit_event(transcript(
                "utterance-1",
                2,
                TranscriptEventKind::Correction,
                "修订文本",
            ))
            .expect("submit correction");
        assert_eq!(
            actor.current_items()[0].status,
            SummaryItemStatus::NeedsReview
        );
        assert!(matches!(
            complete_current(&mut actor, 31),
            SummaryResponseOutcome::Committed { .. }
        ));
        assert_eq!(actor.current_items()[0].item_id, stable_item_id);
        assert_eq!(actor.current_items()[0].status, SummaryItemStatus::Active);

        actor
            .submit_event(transcript(
                "utterance-1",
                3,
                TranscriptEventKind::Retraction,
                "",
            ))
            .expect("submit retraction");
        assert_eq!(
            actor.current_items()[0].status,
            SummaryItemStatus::Retracted
        );
        assert!(matches!(
            complete_current(&mut actor, 32),
            SummaryResponseOutcome::Committed { ref revision, .. }
                if revision.items.is_empty()
        ));
    }

    #[test]
    fn same_revision_uses_stable_event_kind_priority_without_rollback() {
        let mut actor = actor(4);
        let final_outcome = actor
            .submit_event(transcript(
                "utterance-priority",
                1,
                TranscriptEventKind::Final,
                "初始文本",
            ))
            .expect("accept final");
        assert!(matches!(
            final_outcome,
            SummarySubmitOutcome::Accepted { .. }
        ));

        let correction_outcome = actor
            .submit_event(transcript(
                "utterance-priority",
                1,
                TranscriptEventKind::Correction,
                "同修订纠正",
            ))
            .expect("higher-priority correction replaces final");
        assert!(matches!(
            correction_outcome,
            SummarySubmitOutcome::Accepted { .. }
        ));

        let retraction_outcome = actor
            .submit_event(transcript(
                "utterance-priority",
                1,
                TranscriptEventKind::Retraction,
                "",
            ))
            .expect("higher-priority retraction replaces correction");
        assert!(matches!(
            retraction_outcome,
            SummarySubmitOutcome::Accepted { .. }
        ));

        let mut late_final_event = transcript(
            "utterance-priority",
            1,
            TranscriptEventKind::Final,
            "不得回滚",
        );
        late_final_event.event_id = "event-utterance-priority-1-final-late".to_string();
        let delayed_final = actor
            .submit_event(late_final_event)
            .expect("lower-priority final is stale");
        assert!(matches!(
            delayed_final,
            SummarySubmitOutcome::Stale {
                current_revision: 1,
                ..
            }
        ));
        assert_eq!(actor.transcript_cursor(), 3);
    }

    #[test]
    fn bounded_queue_returns_the_exact_event_for_retry_without_mutation() {
        let mut actor = actor(1);
        actor
            .submit_event(transcript("u1", 1, TranscriptEventKind::Final, "一"))
            .expect("submit first");
        let first = current_request(&actor);
        actor
            .submit_event(transcript("u2", 1, TranscriptEventKind::Final, "二"))
            .expect("queue second");

        let retry = actor
            .submit_event(transcript("u3", 1, TranscriptEventKind::Final, "三"))
            .expect("backpressure is an outcome");
        let retry_event = match retry {
            SummarySubmitOutcome::RetryRequired {
                event,
                queue_capacity,
            } => {
                assert_eq!(queue_capacity, 1);
                event
            }
            other => panic!("expected retry result, got {other:?}"),
        };
        assert_eq!(actor.transcript_cursor(), 2);

        assert!(matches!(
            actor.handle_response(DeterministicFakeProvider::response_for(&first, 40)),
            SummaryResponseOutcome::IgnoredStale { .. }
        ));
        let retried = actor
            .submit_change(retry_event)
            .expect("retry accepted after queue drain");
        assert!(matches!(retried, SummarySubmitOutcome::Accepted { .. }));
        assert_eq!(actor.transcript_cursor(), 3);
    }

    #[test]
    fn final_reconcile_creates_a_final_revision_from_the_stable_snapshot() {
        let mut actor = actor(2);
        actor
            .submit_event(transcript("u1", 1, TranscriptEventKind::Final, "结论"))
            .expect("submit");
        assert!(matches!(
            complete_current(&mut actor, 50),
            SummaryResponseOutcome::Committed { .. }
        ));
        assert!(matches!(
            actor.request_final_reconcile(),
            DispatchOutcome::Started { generation: 2 }
        ));
        assert!(actor
            .in_flight_request()
            .is_some_and(|request| request.final_reconcile));
        assert!(matches!(
            complete_current(&mut actor, 51),
            SummaryResponseOutcome::Committed { ref revision, .. }
                if revision.revision_type == LiveSummaryRevisionType::Final
        ));
    }

    #[test]
    fn prepared_response_does_not_advance_actor_before_durable_commit() {
        let mut actor = actor(2);
        actor
            .submit_event(transcript(
                "u1",
                1,
                TranscriptEventKind::Final,
                "先持久化再发布",
            ))
            .expect("submit");
        let request = current_request(&actor);

        let prepared =
            match actor.prepare_response(DeterministicFakeProvider::response_for(&request, 52)) {
                SummaryResponsePreparation::Ready(prepared) => prepared,
                other => panic!("expected prepared revision, got {other:?}"),
            };

        assert_eq!(actor.counters(), (0, 1));
        assert!(actor.current_items().is_empty());
        assert_eq!(
            actor
                .in_flight_request()
                .map(|value| value.request_id.as_str()),
            Some(request.request_id.as_str())
        );

        let committed = actor
            .commit_prepared_response(prepared)
            .expect("durable write would have completed before this call");
        assert!(matches!(
            committed,
            SummaryResponseOutcome::Committed { ref revision, .. }
                if revision.revision == 1 && revision.generation == 1
        ));
        assert_eq!(actor.counters(), (1, 1));
        assert_eq!(actor.current_items().len(), 1);
    }

    #[test]
    fn provider_evidence_must_match_the_exact_request_snapshot() {
        let mut actor = actor(2);
        actor
            .submit_event(transcript(
                "u1",
                1,
                TranscriptEventKind::Final,
                "有依据的文本",
            ))
            .expect("submit");
        let request = current_request(&actor);
        let mut response = DeterministicFakeProvider::response_for(&request, 55);
        let LiveSummaryProviderResult::Success { items } = &mut response.result else {
            unreachable!("fake provider always succeeds")
        };
        items[0].evidence[0].source_text_hash = source_text_hash("另一段文本");

        assert!(matches!(
            actor.handle_response(response),
            SummaryResponseOutcome::Rejected {
                error: LiveSummaryActorError::StaleEvidence,
                next_dispatch: DispatchOutcome::Idle
            }
        ));
        assert_eq!(actor.counters().0, 0);
        assert!(actor.current_items().is_empty());
    }

    #[test]
    fn provider_failure_pauses_instead_of_spinning_new_cloud_requests() {
        let mut actor = actor(2);
        actor
            .submit_event(transcript("u1", 1, TranscriptEventKind::Final, "等待重试"))
            .expect("submit");
        let request = current_request(&actor);
        let outcome = actor.handle_response(LiveSummaryProviderResponse {
            request_id: request.request_id,
            generation: request.generation,
            snapshot_hash: request.snapshot_hash,
            completed_at_ms: 56,
            result: LiveSummaryProviderResult::Failure(LiveSummaryProviderError {
                code: "synthetic_provider_failure",
                retryable: true,
            }),
        });
        assert!(matches!(
            outcome,
            SummaryResponseOutcome::ProviderFailed {
                next_dispatch: DispatchOutcome::Deferred { .. },
                ..
            }
        ));
        assert!(actor.in_flight_request().is_none());
        assert_eq!(actor.provider().started_requests().len(), 1);

        assert!(matches!(
            actor.retry_deferred_dispatch(),
            DispatchOutcome::Started { generation: 2 }
        ));
        assert_eq!(actor.provider().started_requests().len(), 2);
    }

    #[test]
    fn recovery_restores_revision_generation_and_transcript_counters() {
        let mut original = actor(2);
        let head = transcript("u1", 1, TranscriptEventKind::Final, "恢复文本");
        original.submit_event(head.clone()).expect("submit");
        let committed = match complete_current(&mut original, 60) {
            SummaryResponseOutcome::Committed { revision, .. } => revision,
            other => panic!("expected commit, got {other:?}"),
        };
        let mut checkpoint = committed;
        checkpoint.revision = 7;
        checkpoint.generation = 11;
        checkpoint.transcript_cursor = 42;

        let mut recovered = LiveSummaryActor::recover(
            LiveSummaryActorConfig::new("meeting-live", 2),
            DeterministicFakeProvider::default(),
            LiveSummaryRecoveryState {
                last_complete_revision: checkpoint,
                current_transcript_cursor: 42,
                transcript_heads: vec![head],
            },
        )
        .expect("recover actor");
        assert_eq!(recovered.counters(), (7, 11));
        assert_eq!(recovered.transcript_cursor(), 42);
        assert!(recovered.in_flight_request().is_none());

        recovered
            .submit_event(transcript("u2", 1, TranscriptEventKind::Final, "新增"))
            .expect("submit after recovery");
        assert_eq!(recovered.transcript_cursor(), 43);
        assert_eq!(
            recovered.in_flight_request().map(|r| r.generation),
            Some(12)
        );
        let next = match complete_current(&mut recovered, 61) {
            SummaryResponseOutcome::Committed { revision, .. } => revision,
            other => panic!("expected recovered commit, got {other:?}"),
        };
        assert_eq!(next.revision, 8);
        assert_eq!(next.generation, 12);
    }
}
