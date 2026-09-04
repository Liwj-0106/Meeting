//! Independent health protocol for online ASR. Audio capture health and cloud
//! transport health have different failure domains and must not overwrite one
//! another in the UI or recovery log.

use super::protocol::{StreamingAsrProvider, STREAMING_ASR_SCHEMA_VERSION};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AsrHealthEventKind {
    SessionStarting,
    ConnectionChanged,
    ReconnectScheduled,
    ReplayStarted,
    BufferOverflow,
    ProviderError,
    FallbackActivated,
    SessionStopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AsrHealthState {
    Idle,
    Connecting,
    Streaming,
    Backoff,
    Replaying,
    Degraded,
    Fallback,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AsrHealthSeverity {
    Info,
    Warning,
    Error,
    Fatal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AsrHealthEvent {
    pub schema_version: u16,
    pub event_id: String,
    pub session_id: String,
    pub sequence: u64,
    pub provider: StreamingAsrProvider,
    pub kind: AsrHealthEventKind,
    pub state: AsrHealthState,
    pub severity: AsrHealthSeverity,
    pub code: String,
    pub recoverable: bool,
    pub attempt: u32,
    pub at_frame: u64,
    pub replay_from_frame: Option<u64>,
    pub dropped_frames: u64,
    pub detail: Option<String>,
    pub created_at: String,
}

impl AsrHealthEvent {
    pub fn new(
        session_id: impl Into<String>,
        sequence: u64,
        provider: StreamingAsrProvider,
        kind: AsrHealthEventKind,
        state: AsrHealthState,
        at_frame: u64,
    ) -> Self {
        Self {
            schema_version: STREAMING_ASR_SCHEMA_VERSION,
            event_id: Uuid::new_v4().to_string(),
            session_id: session_id.into(),
            sequence,
            provider,
            kind,
            state,
            severity: AsrHealthSeverity::Info,
            code: "asr_state_changed".to_string(),
            recoverable: true,
            attempt: 0,
            at_frame,
            replay_from_frame: None,
            dropped_frames: 0,
            detail: None,
            created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AsrHealthSnapshot {
    pub session_id: String,
    pub provider: StreamingAsrProvider,
    pub state: AsrHealthState,
    pub severity: AsrHealthSeverity,
    pub code: String,
    pub recoverable: bool,
    pub attempt: u32,
    pub at_frame: u64,
    pub replay_from_frame: Option<u64>,
    pub dropped_frames: u64,
    pub detail: Option<String>,
    pub last_event_id: String,
    pub last_sequence: u64,
    pub updated_at: String,
}

impl From<&AsrHealthEvent> for AsrHealthSnapshot {
    fn from(event: &AsrHealthEvent) -> Self {
        Self {
            session_id: event.session_id.clone(),
            provider: event.provider.clone(),
            state: event.state,
            severity: event.severity,
            code: event.code.clone(),
            recoverable: event.recoverable,
            attempt: event.attempt,
            at_frame: event.at_frame,
            replay_from_frame: event.replay_from_frame,
            dropped_frames: event.dropped_frames,
            detail: event.detail.clone(),
            last_event_id: event.event_id.clone(),
            last_sequence: event.sequence,
            updated_at: event.created_at.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsrHealthApplyResult {
    Applied,
    Duplicate,
    RejectedNonMonotonic { last_sequence: u64 },
}

/// Deterministic/idempotent reducer shared by eventual persistence and UI
/// snapshot adapters. Event IDs are checked before sequence order so replaying
/// the last delivered event is harmless.
#[derive(Debug, Clone, Default)]
pub struct AsrHealthReducer {
    events: Vec<AsrHealthEvent>,
    seen_event_ids: HashSet<String>,
    latest_by_provider: HashMap<StreamingAsrProvider, AsrHealthSnapshot>,
    last_sequence: Option<u64>,
}

impl AsrHealthReducer {
    pub fn apply(&mut self, event: AsrHealthEvent) -> AsrHealthApplyResult {
        if self.seen_event_ids.contains(&event.event_id) {
            return AsrHealthApplyResult::Duplicate;
        }
        if let Some(last_sequence) = self.last_sequence {
            if event.sequence <= last_sequence {
                return AsrHealthApplyResult::RejectedNonMonotonic { last_sequence };
            }
        }

        self.seen_event_ids.insert(event.event_id.clone());
        self.last_sequence = Some(event.sequence);
        self.latest_by_provider
            .insert(event.provider.clone(), AsrHealthSnapshot::from(&event));
        self.events.push(event);
        AsrHealthApplyResult::Applied
    }

    pub fn replay(events: impl IntoIterator<Item = AsrHealthEvent>) -> Self {
        let mut reducer = Self::default();
        for event in events {
            reducer.apply(event);
        }
        reducer
    }

    pub fn event_history(&self) -> &[AsrHealthEvent] {
        &self.events
    }

    pub fn last_sequence(&self) -> Option<u64> {
        self.last_sequence
    }

    pub fn snapshot_for(&self, provider: &StreamingAsrProvider) -> Option<&AsrHealthSnapshot> {
        self.latest_by_provider.get(provider)
    }

    pub fn snapshot(&self) -> Vec<AsrHealthSnapshot> {
        let mut snapshot: Vec<_> = self.latest_by_provider.values().cloned().collect();
        snapshot.sort_by(|left, right| left.provider.as_str().cmp(right.provider.as_str()));
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: &str, sequence: u64, state: AsrHealthState) -> AsrHealthEvent {
        let mut event = AsrHealthEvent::new(
            "session-1",
            sequence,
            StreamingAsrProvider::Deepgram,
            AsrHealthEventKind::ConnectionChanged,
            state,
            sequence * 480,
        );
        event.event_id = id.to_string();
        event
    }

    #[test]
    fn reducer_is_idempotent_and_globally_monotonic() {
        let mut reducer = AsrHealthReducer::default();
        let first = event("event-1", 3, AsrHealthState::Connecting);
        assert_eq!(reducer.apply(first.clone()), AsrHealthApplyResult::Applied);
        assert_eq!(reducer.apply(first), AsrHealthApplyResult::Duplicate);
        assert_eq!(
            reducer.apply(event("event-2", 2, AsrHealthState::Streaming)),
            AsrHealthApplyResult::RejectedNonMonotonic { last_sequence: 3 }
        );
        assert_eq!(reducer.event_history().len(), 1);
    }

    #[test]
    fn latest_snapshot_is_independent_per_provider_and_stably_sorted() {
        let mut reducer = AsrHealthReducer::default();
        reducer.apply(event("deepgram-1", 0, AsrHealthState::Streaming));

        let mut openai = AsrHealthEvent::new(
            "session-1",
            1,
            StreamingAsrProvider::OpenAiRealtime,
            AsrHealthEventKind::FallbackActivated,
            AsrHealthState::Fallback,
            480,
        );
        openai.event_id = "openai-1".to_string();
        reducer.apply(openai);

        let snapshot = reducer.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].provider, StreamingAsrProvider::Deepgram);
        assert_eq!(snapshot[1].provider, StreamingAsrProvider::OpenAiRealtime);
        assert_eq!(
            reducer
                .snapshot_for(&StreamingAsrProvider::OpenAiRealtime)
                .unwrap()
                .state,
            AsrHealthState::Fallback
        );
    }
}
