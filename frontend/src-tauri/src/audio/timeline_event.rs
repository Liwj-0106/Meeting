//! Durable, low-volume events for the audio capture timeline.
//!
//! This module records topology changes, health transitions, clock rebases,
//! gaps, and capture faults. It intentionally has no per-window event kind:
//! callers should emit an event only when observable state changes or a fault
//! needs to be retained for recovery and diagnostics.

use chrono::{SecondsFormat, Utc};
use once_cell::sync::Lazy;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tempfile::Builder;
use thiserror::Error;
use uuid::Uuid;

pub const AUDIO_TIMELINE_SCHEMA_VERSION: u16 = 1;
pub const AUDIO_TIMELINE_FILE_NAME: &str = "audio-events.ndjson";
const MAX_PENDING_AUDIO_TIMELINE_EVENTS: usize = 256;

macro_rules! forward_compatible_string_enum {
    ($name:ident, $default:ident, { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub enum $name {
            $($variant,)+
            Unknown(String),
        }

        impl $name {
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $value,)+
                    Self::Unknown(value) => value,
                }
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::$default
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Ok(match value.as_str() {
                    $($value => Self::$variant,)+
                    _ => Self::Unknown(value),
                })
            }
        }
    };
}

forward_compatible_string_enum!(AudioTrack, Mixed, {
    Microphone => "microphone",
    System => "system",
    Mixed => "mixed",
    Imported => "imported",
});

forward_compatible_string_enum!(AudioTimelineEventKind, StateChanged, {
    SessionStarted => "session_started",
    SessionStopped => "session_stopped",
    TrackConfigured => "track_configured",
    StateChanged => "state_changed",
    DeviceChanged => "device_changed",
    DeviceLost => "device_lost",
    DeviceRecovered => "device_recovered",
    StreamInterrupted => "stream_interrupted",
    StreamRestartScheduled => "stream_restart_scheduled",
    StreamRestarted => "stream_restarted",
    ClockRebased => "clock_rebased",
    GapDetected => "gap_detected",
    PermissionDenied => "permission_denied",
    FatalError => "fatal_error",
});

forward_compatible_string_enum!(AudioTrackState, Unconfigured, {
    Unconfigured => "unconfigured",
    Ready => "ready",
    Starting => "starting",
    Healthy => "healthy",
    Degraded => "degraded",
    Interrupted => "interrupted",
    Recovering => "recovering",
    Stopped => "stopped",
    Failed => "failed",
});

forward_compatible_string_enum!(AudioEventSeverity, Info, {
    Trace => "trace",
    Info => "info",
    Warning => "warning",
    Error => "error",
    Fatal => "fatal",
});

/// A versioned event in the meeting's audio timeline.
///
/// Frame positions are expressed in the session's canonical output sample
/// rate. `generation` increases when a source stream is recreated, allowing a
/// consumer to distinguish a genuine device restart from ordinary continuity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioTimelineEvent {
    pub schema_version: u16,
    pub event_id: String,
    pub session_id: String,
    pub sequence: u64,
    pub track: AudioTrack,
    pub kind: AudioTimelineEventKind,
    pub state: AudioTrackState,
    pub severity: AudioEventSeverity,
    pub code: String,
    pub recoverable: bool,
    pub attempt: u32,
    pub at_frame: u64,
    pub end_frame: Option<u64>,
    pub generation: u64,
    pub gap_frames: u64,
    pub device_label: Option<String>,
    pub detail: Option<String>,
    pub created_at: String,
}

/// Event fields supplied by an audio producer. Session identity, sequence,
/// event identity, and creation time are assigned by the one active recorder.
///
/// Keeping producers away from sequence allocation is important: device
/// supervision and the audio pipeline can report events concurrently, while
/// `audio-events.ndjson` still needs one strictly monotonic global order.
#[derive(Debug, Clone)]
pub struct AudioTimelineEventDraft {
    pub track: AudioTrack,
    pub kind: AudioTimelineEventKind,
    pub state: AudioTrackState,
    pub severity: AudioEventSeverity,
    pub code: String,
    pub recoverable: bool,
    pub attempt: Option<u32>,
    pub at_frame: u64,
    pub end_frame: Option<u64>,
    pub generation: Option<u64>,
    pub gap_frames: u64,
    pub device_label: Option<String>,
    pub detail: Option<String>,
}

impl AudioTimelineEventDraft {
    pub fn new(
        track: AudioTrack,
        kind: AudioTimelineEventKind,
        state: AudioTrackState,
        at_frame: u64,
    ) -> Self {
        Self {
            track,
            kind,
            state,
            severity: AudioEventSeverity::Info,
            code: "audio_state_changed".to_string(),
            recoverable: true,
            attempt: None,
            at_frame,
            end_frame: None,
            generation: None,
            gap_frames: 0,
            device_label: None,
            detail: None,
        }
    }
}

impl AudioTimelineEvent {
    pub fn new(
        session_id: impl Into<String>,
        sequence: u64,
        track: AudioTrack,
        kind: AudioTimelineEventKind,
        state: AudioTrackState,
        at_frame: u64,
    ) -> Self {
        Self {
            schema_version: AUDIO_TIMELINE_SCHEMA_VERSION,
            event_id: Uuid::new_v4().to_string(),
            session_id: session_id.into(),
            sequence,
            track,
            kind,
            state,
            severity: AudioEventSeverity::Info,
            code: "audio_state_changed".to_string(),
            recoverable: true,
            attempt: 0,
            at_frame,
            end_frame: None,
            generation: 0,
            gap_frames: 0,
            device_label: None,
            detail: None,
            created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        }
    }
}

/// Latest health retained independently for every capture/source track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioTrackHealthSnapshot {
    pub session_id: String,
    pub track: AudioTrack,
    pub state: AudioTrackState,
    pub severity: AudioEventSeverity,
    pub code: String,
    pub recoverable: bool,
    pub attempt: u32,
    pub at_frame: u64,
    pub end_frame: Option<u64>,
    pub generation: u64,
    pub gap_frames: u64,
    pub device_label: Option<String>,
    pub detail: Option<String>,
    pub last_event_id: String,
    pub last_sequence: u64,
    pub updated_at: String,
}

impl From<&AudioTimelineEvent> for AudioTrackHealthSnapshot {
    fn from(event: &AudioTimelineEvent) -> Self {
        Self {
            session_id: event.session_id.clone(),
            track: event.track.clone(),
            state: event.state.clone(),
            severity: event.severity.clone(),
            code: event.code.clone(),
            recoverable: event.recoverable,
            attempt: event.attempt,
            at_frame: event.at_frame,
            end_frame: event.end_frame,
            generation: event.generation,
            gap_frames: event.gap_frames,
            device_label: event.device_label.clone(),
            detail: event.detail.clone(),
            last_event_id: event.event_id.clone(),
            last_sequence: event.sequence,
            updated_at: event.created_at.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioTimelineApplyResult {
    Applied,
    Duplicate,
    RejectedNonMonotonic { last_sequence: u64 },
}

/// Deterministic reducer for an append-only audio timeline.
///
/// Event identifiers are globally idempotent within the meeting log. Accepted
/// sequences must be strictly increasing, including across different tracks.
#[derive(Debug, Clone, Default)]
pub struct AudioTimelineState {
    events: Vec<AudioTimelineEvent>,
    seen_event_ids: HashSet<String>,
    health_by_track: HashMap<AudioTrack, AudioTrackHealthSnapshot>,
    last_sequence: Option<u64>,
}

impl AudioTimelineState {
    pub fn apply(&mut self, event: AudioTimelineEvent) -> AudioTimelineApplyResult {
        if self.seen_event_ids.contains(&event.event_id) {
            return AudioTimelineApplyResult::Duplicate;
        }

        if let Some(last_sequence) = self.last_sequence {
            if event.sequence <= last_sequence {
                return AudioTimelineApplyResult::RejectedNonMonotonic { last_sequence };
            }
        }

        self.seen_event_ids.insert(event.event_id.clone());
        self.last_sequence = Some(event.sequence);
        self.health_by_track
            .insert(event.track.clone(), AudioTrackHealthSnapshot::from(&event));
        self.events.push(event);
        AudioTimelineApplyResult::Applied
    }

    pub fn replay(events: impl IntoIterator<Item = AudioTimelineEvent>) -> Self {
        let mut state = Self::default();
        for event in events {
            state.apply(event);
        }
        state
    }

    pub fn event_history(&self) -> &[AudioTimelineEvent] {
        &self.events
    }

    pub fn last_sequence(&self) -> Option<u64> {
        self.last_sequence
    }

    pub fn next_sequence(&self) -> Option<u64> {
        self.last_sequence
            .map_or(Some(0), |value| value.checked_add(1))
    }

    pub fn health_for(&self, track: &AudioTrack) -> Option<&AudioTrackHealthSnapshot> {
        self.health_by_track.get(track)
    }

    /// Stable ordering makes command responses and tests deterministic.
    pub fn health_snapshot(&self) -> Vec<AudioTrackHealthSnapshot> {
        let mut snapshot: Vec<_> = self.health_by_track.values().cloned().collect();
        snapshot.sort_by(|left, right| left.track.as_str().cmp(right.track.as_str()));
        snapshot
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioTimelineReadWarning {
    pub code: String,
    pub line: usize,
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioTimelineReadResult {
    pub events: Vec<AudioTimelineEvent>,
    pub warnings: Vec<AudioTimelineReadWarning>,
}

#[derive(Debug, Error)]
pub enum AudioTimelineError {
    #[error("audio timeline I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("audio timeline serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("audio timeline state lock was poisoned")]
    LockPoisoned,
    #[error("audio timeline sequence space was exhausted")]
    SequenceOverflow,
    #[error("audio timeline rejected its internally allocated sequence {sequence}")]
    SequenceRejected { sequence: u64 },
    #[error("failed to atomically replace {path}: {source}")]
    Persist {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug)]
struct AudioTimelineSessionState {
    log: AudioTimelineLog,
    next_sequence: Option<u64>,
    generation_by_track: HashMap<AudioTrack, u64>,
    attempt_by_track: HashMap<AudioTrack, u32>,
}

/// The sole writer for one live recording session.
///
/// The outer global slot is used only to discover the current writer. All
/// sequence allocation and persistence happens under this object's short
/// synchronous lock, so callers never need to keep a global lock across an
/// async operation.
#[derive(Debug)]
pub struct AudioTimelineSession {
    session_id: String,
    accepting_events: AtomicBool,
    state: Mutex<AudioTimelineSessionState>,
    pending_events: Mutex<VecDeque<AudioTimelineEvent>>,
}

impl AudioTimelineSession {
    pub fn open(
        meeting_folder: impl AsRef<Path>,
        session_id: impl Into<String>,
    ) -> Result<(Arc<Self>, Vec<AudioTimelineReadWarning>), AudioTimelineError> {
        let session_id = session_id.into();
        let (log, warnings) = AudioTimelineLog::open(meeting_folder)?;
        let next_sequence = log
            .last_sequence()?
            .map_or(Some(0), |sequence| sequence.checked_add(1));

        // A meeting folder may be reopened after a crash. Only health from the
        // same session seeds restart counters; a new session starts at
        // generation zero while continuing the file's global sequence.
        let mut generation_by_track = HashMap::new();
        let mut attempt_by_track = HashMap::new();
        for snapshot in log.health_snapshot()? {
            if snapshot.session_id == session_id {
                generation_by_track.insert(snapshot.track.clone(), snapshot.generation);
                attempt_by_track.insert(snapshot.track, snapshot.attempt);
            }
        }

        Ok((
            Arc::new(Self {
                session_id,
                accepting_events: AtomicBool::new(true),
                state: Mutex::new(AudioTimelineSessionState {
                    log,
                    next_sequence,
                    generation_by_track,
                    attempt_by_track,
                }),
                pending_events: Mutex::new(VecDeque::new()),
            }),
            warnings,
        ))
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn is_accepting_events(&self) -> bool {
        self.accepting_events.load(Ordering::Acquire)
    }

    /// Return the complete, ordered low-volume history for this live session.
    ///
    /// A meeting folder can contain events from an earlier crashed recording,
    /// so the active-session command must not expose the whole file. Holding
    /// the session state lock linearizes this snapshot with `record`: an event
    /// is either fully persisted and present here, or will arrive through the
    /// already-installed Tauri listener afterwards.
    pub fn event_snapshot(&self) -> Result<Vec<AudioTimelineEvent>, AudioTimelineError> {
        let state = self
            .state
            .lock()
            .map_err(|_| AudioTimelineError::LockPoisoned)?;
        Ok(state
            .log
            .event_history()?
            .into_iter()
            .filter(|event| event.session_id == self.session_id)
            .collect())
    }

    /// Persist one event and return the exact wire payload to emit to the UI.
    /// Returns `Ok(None)` after this session has been closed, which lets a late
    /// producer exit harmlessly instead of contaminating the next meeting.
    pub fn record(
        &self,
        draft: AudioTimelineEventDraft,
    ) -> Result<Option<AudioTimelineEvent>, AudioTimelineError> {
        if !self.is_accepting_events() {
            return Ok(None);
        }

        let mut state = self
            .state
            .lock()
            .map_err(|_| AudioTimelineError::LockPoisoned)?;
        if !self.is_accepting_events() {
            return Ok(None);
        }

        let sequence = state
            .next_sequence
            .ok_or(AudioTimelineError::SequenceOverflow)?;
        let current_generation = state
            .generation_by_track
            .get(&draft.track)
            .copied()
            .unwrap_or(0);
        let generation = draft.generation.unwrap_or_else(|| {
            if matches!(
                &draft.kind,
                AudioTimelineEventKind::DeviceRecovered | AudioTimelineEventKind::StreamRestarted
            ) {
                current_generation.saturating_add(1)
            } else {
                current_generation
            }
        });
        let current_attempt = state
            .attempt_by_track
            .get(&draft.track)
            .copied()
            .unwrap_or(0);
        let attempt = draft.attempt.unwrap_or_else(|| {
            if matches!(&draft.kind, AudioTimelineEventKind::DeviceLost) {
                current_attempt.saturating_add(1)
            } else {
                current_attempt
            }
        });

        let mut event = AudioTimelineEvent::new(
            self.session_id.clone(),
            sequence,
            draft.track.clone(),
            draft.kind.clone(),
            draft.state,
            draft.at_frame,
        );
        event.severity = draft.severity;
        event.code = draft.code;
        event.recoverable = draft.recoverable;
        event.attempt = attempt;
        event.end_frame = draft.end_frame;
        event.generation = generation;
        event.gap_frames = draft.gap_frames;
        event.device_label = draft.device_label;
        event.detail = draft.detail;

        match state.log.record(event.clone())? {
            AudioTimelineApplyResult::Applied => {
                state.next_sequence = sequence.checked_add(1);
                state
                    .generation_by_track
                    .insert(draft.track.clone(), generation);
                if matches!(&draft.kind, AudioTimelineEventKind::DeviceRecovered) {
                    state.attempt_by_track.insert(draft.track, 0);
                } else {
                    state.attempt_by_track.insert(draft.track, attempt);
                }
                Ok(Some(event))
            }
            AudioTimelineApplyResult::Duplicate
            | AudioTimelineApplyResult::RejectedNonMonotonic { .. } => {
                Err(AudioTimelineError::SequenceRejected { sequence })
            }
        }
    }

    /// Record an event from a producer that cannot emit Tauri events itself.
    /// The health supervisor drains this bounded queue and emits the exact
    /// already-persisted wire payload, so UI delivery never allocates a second
    /// sequence or opens a second timeline log.
    fn record_for_shared_producer(
        &self,
        draft: AudioTimelineEventDraft,
    ) -> Result<Option<AudioTimelineEvent>, AudioTimelineError> {
        let Some(event) = self.record(draft)? else {
            return Ok(None);
        };
        let mut pending = self
            .pending_events
            .lock()
            .map_err(|_| AudioTimelineError::LockPoisoned)?;
        if !self.is_accepting_events() {
            return Ok(Some(event));
        }
        if pending.len() == MAX_PENDING_AUDIO_TIMELINE_EVENTS {
            pending.pop_front();
        }
        pending.push_back(event.clone());
        Ok(Some(event))
    }

    pub(crate) fn drain_pending_events(
        &self,
        maximum: usize,
    ) -> Result<Vec<AudioTimelineEvent>, AudioTimelineError> {
        let mut pending = self
            .pending_events
            .lock()
            .map_err(|_| AudioTimelineError::LockPoisoned)?;
        let count = maximum.min(pending.len());
        Ok(pending.drain(..count).collect())
    }

    fn clear_pending_events(&self) {
        self.pending_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    pub fn close(&self) {
        self.accepting_events.store(false, Ordering::Release);
        // Synchronize with a producer that passed the fast-path check just
        // before closure. When this returns, no event can still be appended to
        // the old meeting folder.
        drop(
            self.state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        self.clear_pending_events();
    }
}

static ACTIVE_AUDIO_TIMELINE: Lazy<Mutex<Option<Arc<AudioTimelineSession>>>> =
    Lazy::new(|| Mutex::new(None));

/// Install the prepared session after startup boundary events have been
/// recorded. The audio pipeline and device supervisor then share this exact
/// writer instead of opening competing `AudioTimelineLog` instances.
pub(crate) fn activate_audio_timeline_session(
    session: Arc<AudioTimelineSession>,
) -> Result<(), AudioTimelineError> {
    session.clear_pending_events();
    let mut active = ACTIVE_AUDIO_TIMELINE
        .lock()
        .map_err(|_| AudioTimelineError::LockPoisoned)?;
    if let Some(previous) = active.replace(session) {
        previous.close();
    }
    Ok(())
}

pub(crate) fn active_audio_timeline_session(
) -> Result<Option<Arc<AudioTimelineSession>>, AudioTimelineError> {
    Ok(ACTIVE_AUDIO_TIMELINE
        .lock()
        .map_err(|_| AudioTimelineError::LockPoisoned)?
        .clone())
}

/// Snapshot the one currently active session without opening another timeline
/// log or allocating any event sequence. No active recording is represented by
/// an empty history, which keeps WebView reload recovery side-effect free.
pub(crate) fn active_audio_timeline_snapshot() -> Result<Vec<AudioTimelineEvent>, AudioTimelineError>
{
    match active_audio_timeline_session()? {
        Some(session) => session.event_snapshot(),
        None => Ok(Vec::new()),
    }
}

/// Shared low-frequency recording entry point for pipeline, synchronizer, and
/// device-supervision producers.
pub(crate) fn record_audio_timeline_event(
    draft: AudioTimelineEventDraft,
) -> Result<Option<AudioTimelineEvent>, AudioTimelineError> {
    match active_audio_timeline_session()? {
        Some(session) => session.record_for_shared_producer(draft),
        None => Ok(None),
    }
}

/// Remove exactly the named session. A stale stop callback cannot close a
/// newer meeting that has already occupied the global slot.
pub(crate) fn deactivate_audio_timeline_session(
    session_id: &str,
) -> Result<(), AudioTimelineError> {
    let mut active = ACTIVE_AUDIO_TIMELINE
        .lock()
        .map_err(|_| AudioTimelineError::LockPoisoned)?;
    if active
        .as_ref()
        .is_some_and(|session| session.session_id() == session_id)
    {
        if let Some(session) = active.take() {
            session.close();
        }
    }
    Ok(())
}

/// Thread-safe durable event log.
///
/// Persistence rewrites the low-volume event history to a temporary file and
/// atomically replaces `audio-events.ndjson`. The reducer is committed only
/// after persistence succeeds, so memory and disk cannot report different
/// accepted events after an ordinary write error.
#[derive(Debug)]
pub struct AudioTimelineLog {
    path: PathBuf,
    state: Mutex<AudioTimelineState>,
}

impl AudioTimelineLog {
    pub fn open(
        meeting_folder: impl AsRef<Path>,
    ) -> Result<(Self, Vec<AudioTimelineReadWarning>), AudioTimelineError> {
        let meeting_folder = meeting_folder.as_ref();
        fs::create_dir_all(meeting_folder)?;
        let result = read_audio_timeline(meeting_folder)?;
        let state = AudioTimelineState::replay(result.events);
        Ok((
            Self {
                path: meeting_folder.join(AUDIO_TIMELINE_FILE_NAME),
                state: Mutex::new(state),
            },
            result.warnings,
        ))
    }

    pub fn record(
        &self,
        event: AudioTimelineEvent,
    ) -> Result<AudioTimelineApplyResult, AudioTimelineError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AudioTimelineError::LockPoisoned)?;
        let mut candidate = state.clone();
        let result = candidate.apply(event);
        if result == AudioTimelineApplyResult::Applied {
            persist_audio_timeline(&self.path, &candidate)?;
            *state = candidate;
        }
        Ok(result)
    }

    pub fn event_history(&self) -> Result<Vec<AudioTimelineEvent>, AudioTimelineError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| AudioTimelineError::LockPoisoned)?
            .event_history()
            .to_vec())
    }

    pub fn health_snapshot(&self) -> Result<Vec<AudioTrackHealthSnapshot>, AudioTimelineError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| AudioTimelineError::LockPoisoned)?
            .health_snapshot())
    }

    pub fn last_sequence(&self) -> Result<Option<u64>, AudioTimelineError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| AudioTimelineError::LockPoisoned)?
            .last_sequence())
    }
}

/// Reads the valid prefix of a meeting's audio event log.
///
/// A malformed line terminates replay and becomes a warning. This deliberately
/// preserves every preceding valid event while preventing data after a damaged
/// tail from being interpreted without its missing causal predecessor.
pub fn read_audio_timeline(
    meeting_folder: impl AsRef<Path>,
) -> Result<AudioTimelineReadResult, AudioTimelineError> {
    let path = meeting_folder.as_ref().join(AUDIO_TIMELINE_FILE_NAME);
    if !path.exists() {
        return Ok(AudioTimelineReadResult::default());
    }

    let bytes = fs::read(path)?;
    let mut state = AudioTimelineState::default();
    let mut warnings = Vec::new();

    for (index, raw_line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        let line_number = index + 1;
        let line = trim_ascii_whitespace(raw_line);
        if line.is_empty() {
            continue;
        }

        let event = match serde_json::from_slice::<AudioTimelineEvent>(line) {
            Ok(event) => event,
            Err(_) => {
                warnings.push(AudioTimelineReadWarning {
                    code: "corrupt_ndjson_tail".to_string(),
                    line: line_number,
                    detail: "Stopped at a malformed audio event; earlier events were retained"
                        .to_string(),
                });
                break;
            }
        };

        match state.apply(event) {
            AudioTimelineApplyResult::Applied | AudioTimelineApplyResult::Duplicate => {}
            AudioTimelineApplyResult::RejectedNonMonotonic { last_sequence } => {
                warnings.push(AudioTimelineReadWarning {
                    code: "non_monotonic_sequence".to_string(),
                    line: line_number,
                    detail: format!(
                        "Skipped an audio event whose sequence did not exceed {last_sequence}"
                    ),
                });
            }
        }
    }

    Ok(AudioTimelineReadResult {
        events: state.event_history().to_vec(),
        warnings,
    })
}

fn persist_audio_timeline(
    path: &Path,
    state: &AudioTimelineState,
) -> Result<(), AudioTimelineError> {
    let folder = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "audio timeline path has no parent folder",
        )
    })?;
    fs::create_dir_all(folder)?;

    let mut temp = Builder::new()
        .prefix(".meetily-audio-events-")
        .tempfile_in(folder)?;
    for event in state.event_history() {
        serde_json::to_writer(&mut temp, event)?;
        temp.write_all(b"\n")?;
    }
    temp.flush()?;
    temp.as_file_mut().sync_all()?;
    temp.persist(path)
        .map_err(|error| AudioTimelineError::Persist {
            path: path.display().to_string(),
            source: error.error,
        })?;
    Ok(())
}

fn trim_ascii_whitespace(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn event(
        event_id: &str,
        sequence: u64,
        track: AudioTrack,
        kind: AudioTimelineEventKind,
        state: AudioTrackState,
    ) -> AudioTimelineEvent {
        let mut event = AudioTimelineEvent::new("session-1", sequence, track, kind, state, 480);
        event.event_id = event_id.to_string();
        event.created_at = format!("2026-09-01T00:00:{sequence:02}Z");
        event
    }

    #[test]
    fn transitions_update_per_track_health() {
        let mut state = AudioTimelineState::default();
        assert_eq!(
            state.apply(event(
                "mic-started",
                0,
                AudioTrack::Microphone,
                AudioTimelineEventKind::StateChanged,
                AudioTrackState::Healthy,
            )),
            AudioTimelineApplyResult::Applied
        );

        let mut lost = event(
            "mic-lost",
            1,
            AudioTrack::Microphone,
            AudioTimelineEventKind::DeviceLost,
            AudioTrackState::Interrupted,
        );
        lost.severity = AudioEventSeverity::Warning;
        lost.code = "device_disconnected".to_string();
        lost.recoverable = true;
        lost.attempt = 1;
        state.apply(lost);

        let mut recovered = event(
            "mic-recovered",
            2,
            AudioTrack::Microphone,
            AudioTimelineEventKind::DeviceRecovered,
            AudioTrackState::Healthy,
        );
        recovered.generation = 1;
        state.apply(recovered);

        let health = state.health_for(&AudioTrack::Microphone).unwrap();
        assert_eq!(health.state, AudioTrackState::Healthy);
        assert_eq!(health.generation, 1);
        assert_eq!(health.last_sequence, 2);
        assert_eq!(state.event_history().len(), 3);
    }

    #[test]
    fn duplicate_event_id_is_globally_idempotent_and_sequence_is_monotonic() {
        let mut state = AudioTimelineState::default();
        let first = event(
            "same-id",
            7,
            AudioTrack::Microphone,
            AudioTimelineEventKind::StateChanged,
            AudioTrackState::Healthy,
        );
        assert_eq!(
            state.apply(first.clone()),
            AudioTimelineApplyResult::Applied
        );

        let mut reused_on_another_track = first;
        reused_on_another_track.sequence = 8;
        reused_on_another_track.track = AudioTrack::System;
        assert_eq!(
            state.apply(reused_on_another_track),
            AudioTimelineApplyResult::Duplicate
        );

        assert_eq!(
            state.apply(event(
                "different-id",
                7,
                AudioTrack::System,
                AudioTimelineEventKind::StateChanged,
                AudioTrackState::Healthy,
            )),
            AudioTimelineApplyResult::RejectedNonMonotonic { last_sequence: 7 }
        );
        assert_eq!(state.event_history().len(), 1);
        assert!(state.health_for(&AudioTrack::System).is_none());
    }

    #[test]
    fn unknown_protocol_values_round_trip_without_loss() {
        let value = serde_json::json!({
            "schema_version": 99,
            "event_id": "future-1",
            "session_id": "session-future",
            "sequence": 1,
            "track": "browser_tab",
            "kind": "buffer_pressure_changed",
            "state": "throttled",
            "severity": "notice",
            "code": "future_code",
            "recoverable": true,
            "attempt": 3,
            "at_frame": 960,
            "end_frame": 1440,
            "generation": 2,
            "gap_frames": 32,
            "device_label": "Future Device",
            "detail": "future detail",
            "created_at": "2026-09-01T00:00:00Z"
        });

        let event: AudioTimelineEvent = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(event.track, AudioTrack::Unknown("browser_tab".to_string()));
        assert_eq!(
            event.kind,
            AudioTimelineEventKind::Unknown("buffer_pressure_changed".to_string())
        );
        assert_eq!(
            event.state,
            AudioTrackState::Unknown("throttled".to_string())
        );
        assert_eq!(
            event.severity,
            AudioEventSeverity::Unknown("notice".to_string())
        );
        assert_eq!(serde_json::to_value(event).unwrap(), value);
    }

    #[test]
    fn corrupt_tail_warns_and_preserves_valid_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let valid = event(
            "valid-1",
            0,
            AudioTrack::System,
            AudioTimelineEventKind::StateChanged,
            AudioTrackState::Healthy,
        );
        let contents = format!(
            "{}\n{{\"event_id\":\"truncated",
            serde_json::to_string(&valid).unwrap()
        );
        fs::write(directory.path().join(AUDIO_TIMELINE_FILE_NAME), contents).unwrap();

        let result = read_audio_timeline(directory.path()).unwrap();
        assert_eq!(result.events, vec![valid]);
        assert_eq!(result.warnings.len(), 1);
        assert_eq!(result.warnings[0].code, "corrupt_ndjson_tail");
        assert_eq!(result.warnings[0].line, 2);
    }

    #[test]
    fn log_persists_atomically_and_repairs_a_corrupt_tail_on_next_event() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AudioTimelineLog>();

        let directory = tempfile::tempdir().unwrap();
        let valid = event(
            "valid-1",
            0,
            AudioTrack::System,
            AudioTimelineEventKind::StateChanged,
            AudioTrackState::Healthy,
        );
        let contents = format!("{}\nnot-json", serde_json::to_string(&valid).unwrap());
        fs::write(directory.path().join(AUDIO_TIMELINE_FILE_NAME), contents).unwrap();

        let (log, warnings) = AudioTimelineLog::open(directory.path()).unwrap();
        assert_eq!(warnings.len(), 1);
        let log = Arc::new(log);
        assert_eq!(
            log.record(event(
                "recovered-2",
                1,
                AudioTrack::System,
                AudioTimelineEventKind::DeviceRecovered,
                AudioTrackState::Healthy,
            ))
            .unwrap(),
            AudioTimelineApplyResult::Applied
        );

        let repaired = read_audio_timeline(directory.path()).unwrap();
        assert_eq!(repaired.events.len(), 2);
        assert!(repaired.warnings.is_empty());
        assert_eq!(log.event_history().unwrap(), repaired.events);
    }

    #[test]
    fn live_session_serializes_concurrent_producers_and_rejects_late_events() {
        let directory = tempfile::tempdir().unwrap();
        let (session, warnings) =
            AudioTimelineSession::open(directory.path(), "live-session").unwrap();
        assert!(warnings.is_empty());

        let mut workers = Vec::new();
        for index in 0..12_u64 {
            let session = session.clone();
            workers.push(std::thread::spawn(move || {
                let track = if index % 2 == 0 {
                    AudioTrack::Microphone
                } else {
                    AudioTrack::System
                };
                let mut draft = AudioTimelineEventDraft::new(
                    track,
                    AudioTimelineEventKind::StateChanged,
                    AudioTrackState::Healthy,
                    index * 480,
                );
                draft.code = format!("producer_{index}");
                session.record(draft).unwrap().unwrap()
            }));
        }

        let mut returned_events: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        returned_events.sort_by_key(|event| event.sequence);
        assert_eq!(
            returned_events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            (0..12).collect::<Vec<_>>()
        );

        let persisted = read_audio_timeline(directory.path()).unwrap();
        assert!(persisted.warnings.is_empty());
        assert_eq!(persisted.events.len(), 12);
        assert_eq!(
            persisted
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            (0..12).collect::<Vec<_>>()
        );

        session.close();
        let late = AudioTimelineEventDraft::new(
            AudioTrack::Mixed,
            AudioTimelineEventKind::GapDetected,
            AudioTrackState::Degraded,
            999,
        );
        assert!(session.record(late).unwrap().is_none());
        assert_eq!(
            read_audio_timeline(directory.path()).unwrap().events.len(),
            12
        );
    }

    #[test]
    fn shared_producer_events_are_queued_once_and_close_discards_pending_delivery() {
        let directory = tempfile::tempdir().unwrap();
        let (session, _) = AudioTimelineSession::open(directory.path(), "queue-session").unwrap();

        let direct = AudioTimelineEventDraft::new(
            AudioTrack::Mixed,
            AudioTimelineEventKind::SessionStarted,
            AudioTrackState::Starting,
            0,
        );
        session.record(direct).unwrap().unwrap();
        assert!(session.drain_pending_events(10).unwrap().is_empty());

        let shared = AudioTimelineEventDraft::new(
            AudioTrack::Microphone,
            AudioTimelineEventKind::GapDetected,
            AudioTrackState::Degraded,
            480,
        );
        let persisted = session
            .record_for_shared_producer(shared.clone())
            .unwrap()
            .unwrap();
        assert_eq!(session.drain_pending_events(10).unwrap(), vec![persisted]);
        assert!(session.drain_pending_events(10).unwrap().is_empty());

        session.record_for_shared_producer(shared).unwrap().unwrap();
        session.close();
        assert!(session.drain_pending_events(10).unwrap().is_empty());
    }

    #[test]
    fn session_snapshot_is_complete_ordered_filtered_and_does_not_drain_delivery() {
        let directory = tempfile::tempdir().unwrap();
        let old = event(
            "old-session-event",
            0,
            AudioTrack::System,
            AudioTimelineEventKind::StateChanged,
            AudioTrackState::Healthy,
        );
        let old_contents = format!("{}\n", serde_json::to_string(&old).unwrap());
        fs::write(
            directory.path().join(AUDIO_TIMELINE_FILE_NAME),
            old_contents,
        )
        .unwrap();

        let (session, _) =
            AudioTimelineSession::open(directory.path(), "snapshot-session").unwrap();
        let direct = session
            .record(AudioTimelineEventDraft::new(
                AudioTrack::Mixed,
                AudioTimelineEventKind::SessionStarted,
                AudioTrackState::Starting,
                0,
            ))
            .unwrap()
            .unwrap();
        let queued = session
            .record_for_shared_producer(AudioTimelineEventDraft::new(
                AudioTrack::Microphone,
                AudioTimelineEventKind::GapDetected,
                AudioTrackState::Degraded,
                480,
            ))
            .unwrap()
            .unwrap();

        assert_eq!(
            session.event_snapshot().unwrap(),
            vec![direct, queued.clone()]
        );
        assert_eq!(session.drain_pending_events(10).unwrap(), vec![queued]);
    }
}
