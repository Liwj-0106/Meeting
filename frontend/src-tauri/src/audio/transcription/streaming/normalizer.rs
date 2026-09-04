//! Convert provider interim/final messages into Meetily's canonical revisioned
//! `TranscriptUpdate` stream.

use super::protocol::{ProviderTranscriptEvent, ProviderTranscriptKind, StreamingProtocolError};
use crate::audio::transcription::{
    SpeakerMetadata, SpeakerStatus, TranscriptChunkInput, TranscriptUpdate,
};
use std::collections::{HashMap, HashSet};
use thiserror::Error;

const CANONICAL_SAMPLE_RATE: f64 = 48_000.0;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamingTranscriptContext {
    pub meeting_id: Option<String>,
    pub session_id: Option<String>,
    pub default_language: Option<String>,
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NormalizeOutcome {
    Emitted(TranscriptUpdate),
    Duplicate,
    IgnoredAfterFinal,
}

#[derive(Debug, Clone)]
struct UtteranceRevisionState {
    sequence_id: u64,
    utterance_id: String,
    last_revision: u64,
    last_event_id: String,
    finalized: bool,
}

/// Stateful normalizer. A provider utterance receives one Meetily sequence and
/// utterance id for its entire lifetime; every subsequent interim/final result
/// is a new revision replacing the prior event.
#[derive(Debug, Clone)]
pub struct StreamingTranscriptNormalizer {
    context: StreamingTranscriptContext,
    next_sequence: Option<u64>,
    utterances: HashMap<String, UtteranceRevisionState>,
    seen_provider_events: HashSet<String>,
    last_stable_frame: Option<u64>,
}

impl StreamingTranscriptNormalizer {
    pub fn new(context: StreamingTranscriptContext, first_sequence: u64) -> Self {
        Self {
            context,
            next_sequence: Some(first_sequence),
            utterances: HashMap::new(),
            seen_provider_events: HashSet::new(),
            last_stable_frame: None,
        }
    }

    pub fn last_stable_frame(&self) -> Option<u64> {
        self.last_stable_frame
    }

    pub fn normalize(
        &mut self,
        provider_event: ProviderTranscriptEvent,
    ) -> Result<NormalizeOutcome, NormalizerError> {
        provider_event.validate()?;
        let dedupe_key = provider_event_dedupe_key(&provider_event);
        if self.seen_provider_events.contains(&dedupe_key) {
            return Ok(NormalizeOutcome::Duplicate);
        }

        let utterance_key = format!(
            "{}:{}:{}",
            provider_event.provider.as_str(),
            provider_event.connection_id,
            provider_event.utterance_key
        );
        if self
            .utterances
            .get(&utterance_key)
            .map(|state| state.finalized)
            .unwrap_or(false)
        {
            return Ok(NormalizeOutcome::IgnoredAfterFinal);
        }

        let (start_frame, end_frame) = provider_event.time.absolute_frame_range()?;
        let (sequence_id, utterance_id, revision, replaces_event_id) =
            if let Some(state) = self.utterances.get(&utterance_key) {
                (
                    state.sequence_id,
                    state.utterance_id.clone(),
                    state
                        .last_revision
                        .checked_add(1)
                        .ok_or(NormalizerError::RevisionOverflow)?,
                    Some(state.last_event_id.clone()),
                )
            } else {
                let sequence_id = self
                    .next_sequence
                    .ok_or(NormalizerError::SequenceOverflow)?;
                let session_key = self.context.session_id.as_deref().unwrap_or("streaming");
                (
                    sequence_id,
                    format!("{}:streaming:{}", session_key, sequence_id),
                    0,
                    None,
                )
            };

        let provider_name = provider_event.provider.as_str().to_string();
        let mut input = TranscriptChunkInput::new(
            provider_event.text.clone(),
            sequence_id,
            start_frame as f64 / CANONICAL_SAMPLE_RATE,
            end_frame as f64 / CANONICAL_SAMPLE_RATE,
            matches!(provider_event.kind, ProviderTranscriptKind::Partial),
            provider_event.confidence,
            provider_event.audio_source.clone(),
            provider_name.clone(),
        );
        input.model = provider_event.model.clone();
        input.meeting_id = self.context.meeting_id.clone();
        input.session_id = self.context.session_id.clone();
        input.language = provider_event
            .language
            .clone()
            .or_else(|| self.context.default_language.clone());
        input.trace_id = provider_event
            .trace_id
            .clone()
            .or_else(|| self.context.trace_id.clone());

        let mut update = TranscriptUpdate::from_legacy_chunk(input);
        update.utterance_id = Some(utterance_id.clone());
        update.revision = revision;
        update.replaces_event_id = replaces_event_id;
        update.provider_event_id = provider_event.provider_event_id.clone();
        update.asr_latency_ms = provider_event.latency_ms;
        if let Some(asr) = update.asr.as_mut() {
            asr.latency_ms = provider_event.latency_ms;
        }
        if let Some(speaker_id) = provider_event.speaker_id.clone() {
            let speaker = SpeakerMetadata {
                speaker_id: speaker_id.clone(),
                local_label: None,
                display_name: None,
                confidence: provider_event.speaker_confidence,
                status: SpeakerStatus::Provisional,
            };
            update.speaker = Some(speaker);
            update.speaker_id = Some(speaker_id);
            update.speaker_confidence = provider_event.speaker_confidence;
            update.speaker_status = Some(SpeakerStatus::Provisional);
        }

        let event_id = update
            .event_id
            .clone()
            .ok_or(NormalizerError::MissingGeneratedEventId)?;
        let is_final = provider_event.kind.is_final();
        match self.utterances.get_mut(&utterance_key) {
            Some(state) => {
                state.last_revision = revision;
                state.last_event_id = event_id;
                state.finalized = is_final;
            }
            None => {
                self.next_sequence = sequence_id.checked_add(1);
                self.utterances.insert(
                    utterance_key,
                    UtteranceRevisionState {
                        sequence_id,
                        utterance_id,
                        last_revision: revision,
                        last_event_id: event_id,
                        finalized: is_final,
                    },
                );
            }
        }
        if is_final {
            self.last_stable_frame = Some(
                self.last_stable_frame
                    .map_or(end_frame, |current| current.max(end_frame)),
            );
        }
        self.seen_provider_events.insert(dedupe_key);
        Ok(NormalizeOutcome::Emitted(update))
    }
}

fn provider_event_dedupe_key(event: &ProviderTranscriptEvent) -> String {
    if let Some(event_id) = event.provider_event_id.as_deref() {
        return format!(
            "{}:{}:id:{}",
            event.provider.as_str(),
            event.connection_id,
            event_id
        );
    }

    // Providers are allowed to omit event IDs. The complete immutable payload
    // then forms a deterministic in-process idempotency key.
    format!(
        "{}:{}:{}:{:?}:{:?}:{:?}:{}",
        event.provider.as_str(),
        event.connection_id,
        event.utterance_key,
        event.kind,
        event.time,
        event.confidence,
        event.text
    )
}

#[derive(Debug, Error, PartialEq)]
pub enum NormalizerError {
    #[error(transparent)]
    InvalidProviderEvent(#[from] StreamingProtocolError),
    #[error("streaming transcript sequence space is exhausted")]
    SequenceOverflow,
    #[error("streaming transcript revision space is exhausted")]
    RevisionOverflow,
    #[error("canonical transcript builder did not allocate an event id")]
    MissingGeneratedEventId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::transcription::{AudioSource, TranscriptEventKind};

    fn event(id: &str, text: &str, is_final: bool) -> ProviderTranscriptEvent {
        let mut event = ProviderTranscriptEvent::deepgram(
            "connection-a",
            Some(id.to_string()),
            "utterance-a",
            text,
            is_final,
            480_000,
            1.25,
            0.75,
        )
        .unwrap();
        event.confidence = Some(0.96);
        event.speaker_confidence = Some(0.87);
        event.language = Some("en".to_string());
        event.speaker_id = Some("speaker-0".to_string());
        event.model = Some("nova-3".to_string());
        event.latency_ms = Some(140);
        event.audio_source = AudioSource::Mixed;
        event
    }

    fn emitted(outcome: NormalizeOutcome) -> TranscriptUpdate {
        match outcome {
            NormalizeOutcome::Emitted(update) => update,
            other => panic!("expected emitted update, got {other:?}"),
        }
    }

    #[test]
    fn partial_revisions_and_final_keep_one_sequence_and_replacement_chain() {
        let context = StreamingTranscriptContext {
            meeting_id: Some("meeting-1".to_string()),
            session_id: Some("session-1".to_string()),
            default_language: None,
            trace_id: Some("trace-1".to_string()),
        };
        let mut normalizer = StreamingTranscriptNormalizer::new(context, 41);

        let first = emitted(
            normalizer
                .normalize(event("event-1", "hel", false))
                .unwrap(),
        );
        let second = emitted(
            normalizer
                .normalize(event("event-2", "hello", false))
                .unwrap(),
        );
        let final_update = emitted(
            normalizer
                .normalize(event("event-3", "hello world", true))
                .unwrap(),
        );

        assert_eq!(
            (
                first.sequence_id,
                second.sequence_id,
                final_update.sequence_id
            ),
            (41, 41, 41)
        );
        assert_eq!(
            (first.revision, second.revision, final_update.revision),
            (0, 1, 2)
        );
        assert_eq!(first.utterance_id, second.utterance_id);
        assert_eq!(second.utterance_id, final_update.utterance_id);
        assert_eq!(second.replaces_event_id, first.event_id);
        assert_eq!(final_update.replaces_event_id, second.event_id);
        assert_eq!(first.event_kind, Some(TranscriptEventKind::Partial));
        assert_eq!(final_update.event_kind, Some(TranscriptEventKind::Final));
        assert_eq!(final_update.is_stable, Some(true));
        assert_eq!(
            (final_update.start_ms, final_update.end_ms),
            (Some(11_250), Some(12_000))
        );
        assert_eq!(normalizer.last_stable_frame(), Some(576_000));
        assert_eq!(final_update.asr_provider.as_deref(), Some("deepgram"));
        assert_eq!(final_update.asr_model.as_deref(), Some("nova-3"));
        assert_eq!(final_update.speaker_id.as_deref(), Some("speaker-0"));
        assert_eq!(final_update.speaker_confidence, Some(0.87));
    }

    #[test]
    fn provider_event_id_is_idempotent_without_advancing_revision() {
        let mut normalizer = StreamingTranscriptNormalizer::new(Default::default(), 0);
        let provider_event = event("same-event", "hello", false);
        let first = emitted(normalizer.normalize(provider_event.clone()).unwrap());
        assert_eq!(
            normalizer.normalize(provider_event).unwrap(),
            NormalizeOutcome::Duplicate
        );

        let second = emitted(
            normalizer
                .normalize(event("next-event", "hello again", false))
                .unwrap(),
        );
        assert_eq!(first.revision, 0);
        assert_eq!(second.revision, 1);
    }

    #[test]
    fn finalized_utterance_is_not_reopened_and_new_key_gets_next_sequence() {
        let mut normalizer = StreamingTranscriptNormalizer::new(Default::default(), 7);
        let first = emitted(
            normalizer
                .normalize(event("final-1", "done", true))
                .unwrap(),
        );
        assert_eq!(
            normalizer
                .normalize(event("late-1", "late correction", false))
                .unwrap(),
            NormalizeOutcome::IgnoredAfterFinal
        );

        let mut next = event("partial-b", "next", false);
        next.utterance_key = "utterance-b".to_string();
        let next = emitted(normalizer.normalize(next).unwrap());
        assert_eq!(first.sequence_id, 7);
        assert_eq!(next.sequence_id, 8);
    }
}
