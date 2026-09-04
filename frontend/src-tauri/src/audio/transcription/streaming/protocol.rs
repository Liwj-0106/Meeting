//! Provider-neutral protocol for a future live ASR transport.
//!
//! The types in this file deliberately contain no sockets, credentials, or
//! Tauri handles. A provider actor can therefore be tested independently and
//! can use the same commands regardless of whether its transport is a WebSocket
//! or another bidirectional stream.

use crate::audio::transcription::AudioSource;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use thiserror::Error;

pub const STREAMING_ASR_SCHEMA_VERSION: u16 = 1;
pub const STREAMING_AUDIO_SAMPLE_RATE: u32 = 48_000;
pub const STREAMING_AUDIO_CHANNELS: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StreamingAsrProvider {
    Deepgram,
    OpenAiRealtime,
    Custom(String),
}

impl StreamingAsrProvider {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Deepgram => "deepgram",
            Self::OpenAiRealtime => "openai_realtime",
            Self::Custom(value) => value,
        }
    }
}

impl fmt::Display for StreamingAsrProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for StreamingAsrProvider {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for StreamingAsrProvider {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "deepgram" => Self::Deepgram,
            "openai_realtime" => Self::OpenAiRealtime,
            _ => Self::Custom(value),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamingAudioEncoding {
    PcmF32,
    Pcm16Le,
}

/// Capabilities are explicit so an unsupported provider option fails during
/// configuration instead of being silently ignored by a transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamingAsrCapabilities {
    pub provider: StreamingAsrProvider,
    pub accepted_sample_rates: Vec<u32>,
    pub accepted_encodings: Vec<StreamingAudioEncoding>,
    pub interim_results: bool,
    pub provider_timestamps: bool,
    pub word_timestamps: bool,
    pub diarization: bool,
    pub endpointing: bool,
    pub reconnect_replay: bool,
}

impl StreamingAsrCapabilities {
    pub fn deepgram() -> Self {
        Self {
            provider: StreamingAsrProvider::Deepgram,
            accepted_sample_rates: vec![16_000, 24_000],
            accepted_encodings: vec![StreamingAudioEncoding::Pcm16Le],
            interim_results: true,
            provider_timestamps: true,
            word_timestamps: true,
            diarization: true,
            endpointing: true,
            reconnect_replay: true,
        }
    }

    pub fn openai_realtime() -> Self {
        Self {
            provider: StreamingAsrProvider::OpenAiRealtime,
            accepted_sample_rates: vec![24_000],
            accepted_encodings: vec![StreamingAudioEncoding::Pcm16Le],
            interim_results: true,
            // Current Realtime transcription delta/completed events identify
            // items but do not carry precise audio timestamps. Integration may
            // retain Meetily's local canonical frame range, but must not label
            // that range as provider timing.
            provider_timestamps: false,
            word_timestamps: false,
            diarization: false,
            endpointing: true,
            reconnect_replay: true,
        }
    }

    pub fn accepts(&self, sample_rate: u32, encoding: StreamingAudioEncoding) -> bool {
        self.accepted_sample_rates.contains(&sample_rate)
            && self.accepted_encodings.contains(&encoding)
    }
}

/// One canonical, clock-aligned audio window. Frame positions use Meetily's
/// session-wide 48 kHz mono clock, regardless of the provider's wire format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamingAudioFrame {
    pub schema_version: u16,
    pub sequence: u64,
    pub origin_frame: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub source: AudioSource,
    pub samples: Vec<f32>,
}

impl StreamingAudioFrame {
    pub fn new(sequence: u64, origin_frame: u64, samples: Vec<f32>, source: AudioSource) -> Self {
        Self {
            schema_version: STREAMING_ASR_SCHEMA_VERSION,
            sequence,
            origin_frame,
            sample_rate: STREAMING_AUDIO_SAMPLE_RATE,
            channels: STREAMING_AUDIO_CHANNELS,
            source,
            samples,
        }
    }

    pub fn validate(&self) -> Result<(), StreamingProtocolError> {
        if self.sample_rate != STREAMING_AUDIO_SAMPLE_RATE {
            return Err(StreamingProtocolError::InvalidSampleRate {
                expected: STREAMING_AUDIO_SAMPLE_RATE,
                actual: self.sample_rate,
            });
        }
        if self.channels != STREAMING_AUDIO_CHANNELS {
            return Err(StreamingProtocolError::InvalidChannelCount {
                expected: STREAMING_AUDIO_CHANNELS,
                actual: self.channels,
            });
        }
        if self.samples.is_empty() {
            return Err(StreamingProtocolError::EmptyAudioFrame);
        }
        self.end_frame()?;
        Ok(())
    }

    pub fn end_frame(&self) -> Result<u64, StreamingProtocolError> {
        let length = u64::try_from(self.samples.len())
            .map_err(|_| StreamingProtocolError::FrameIndexOverflow)?;
        self.origin_frame
            .checked_add(length)
            .ok_or(StreamingProtocolError::FrameIndexOverflow)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamingFlushReason {
    Endpoint,
    ProviderCommit,
    Reconnect,
    RecordingStopped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", content = "payload", rename_all = "snake_case")]
pub enum StreamingAsrCommand {
    Audio(StreamingAudioFrame),
    Commit {
        through_frame: u64,
    },
    Flush {
        reason: StreamingFlushReason,
    },
    Reconnect {
        attempt: u32,
        replay_from_frame: u64,
    },
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTranscriptKind {
    Partial,
    Final,
}

/// Provider word metadata retained before provider-neutral transcript
/// normalization. Every optional field stays optional: transports must not
/// invent timing, confidence, language, or speaker information when a provider
/// omits it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderTranscriptWord {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub punctuated_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_confidence: Option<f32>,
}

impl ProviderTranscriptWord {
    pub fn validate(&self) -> Result<(), StreamingProtocolError> {
        if self.text.trim().is_empty() {
            return Err(StreamingProtocolError::EmptyProviderWord);
        }
        if self
            .start_ms
            .is_some_and(|value| !value.is_finite() || value < 0.0)
            || self
                .end_ms
                .is_some_and(|value| !value.is_finite() || value < 0.0)
            || matches!((self.start_ms, self.end_ms), (Some(start), Some(end)) if end < start)
        {
            return Err(StreamingProtocolError::InvalidProviderWordTime);
        }
        if self.confidence.is_some_and(|value| !value.is_finite())
            || self
                .speaker_confidence
                .is_some_and(|value| !value.is_finite())
        {
            return Err(StreamingProtocolError::InvalidProviderWordConfidence);
        }
        Ok(())
    }
}

impl ProviderTranscriptKind {
    pub fn is_final(self) -> bool {
        matches!(self, Self::Final)
    }
}

/// Provider-relative time plus the canonical frame at which this provider
/// connection began. Deepgram timestamps start again after reconnect, so the
/// origin is required to recover a stable meeting-wide time range.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ProviderTimeRange {
    pub origin_frame: u64,
    pub provider_start_ms: f64,
    pub provider_end_ms: f64,
}

impl ProviderTimeRange {
    pub fn from_seconds(
        origin_frame: u64,
        start_seconds: f64,
        duration_seconds: f64,
    ) -> Result<Self, StreamingProtocolError> {
        if !start_seconds.is_finite() || start_seconds < 0.0 {
            return Err(StreamingProtocolError::InvalidProviderTime);
        }
        if !duration_seconds.is_finite() || duration_seconds < 0.0 {
            return Err(StreamingProtocolError::InvalidProviderTime);
        }
        let end_seconds = start_seconds + duration_seconds;
        if !end_seconds.is_finite() {
            return Err(StreamingProtocolError::InvalidProviderTime);
        }
        Ok(Self {
            origin_frame,
            provider_start_ms: start_seconds * 1_000.0,
            provider_end_ms: end_seconds * 1_000.0,
        })
    }

    pub fn absolute_frame_range(&self) -> Result<(u64, u64), StreamingProtocolError> {
        if !self.provider_start_ms.is_finite()
            || !self.provider_end_ms.is_finite()
            || self.provider_start_ms < 0.0
            || self.provider_end_ms < self.provider_start_ms
        {
            return Err(StreamingProtocolError::InvalidProviderTime);
        }

        let start_offset = milliseconds_to_frames(self.provider_start_ms)?;
        let end_offset = milliseconds_to_frames(self.provider_end_ms)?;
        let start = self
            .origin_frame
            .checked_add(start_offset)
            .ok_or(StreamingProtocolError::FrameIndexOverflow)?;
        let end = self
            .origin_frame
            .checked_add(end_offset)
            .ok_or(StreamingProtocolError::FrameIndexOverflow)?
            .max(start);
        Ok((start, end))
    }
}

/// A transport-neutral provider result. `utterance_key` must remain stable
/// across interim and final revisions within one provider connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderTranscriptEvent {
    pub schema_version: u16,
    pub provider: StreamingAsrProvider,
    pub connection_id: String,
    pub provider_event_id: Option<String>,
    pub utterance_key: String,
    pub kind: ProviderTranscriptKind,
    pub text: String,
    pub time: ProviderTimeRange,
    pub confidence: Option<f32>,
    pub language: Option<String>,
    pub speaker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_confidence: Option<f32>,
    pub model: Option<String>,
    pub latency_ms: Option<u64>,
    pub audio_source: AudioSource,
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<ProviderTranscriptWord>,
}

impl ProviderTranscriptEvent {
    #[allow(clippy::too_many_arguments)]
    pub fn deepgram(
        connection_id: impl Into<String>,
        provider_event_id: Option<String>,
        utterance_key: impl Into<String>,
        text: impl Into<String>,
        is_final: bool,
        origin_frame: u64,
        provider_start_seconds: f64,
        provider_duration_seconds: f64,
    ) -> Result<Self, StreamingProtocolError> {
        Ok(Self {
            schema_version: STREAMING_ASR_SCHEMA_VERSION,
            provider: StreamingAsrProvider::Deepgram,
            connection_id: connection_id.into(),
            provider_event_id,
            utterance_key: utterance_key.into(),
            kind: if is_final {
                ProviderTranscriptKind::Final
            } else {
                ProviderTranscriptKind::Partial
            },
            text: text.into(),
            time: ProviderTimeRange::from_seconds(
                origin_frame,
                provider_start_seconds,
                provider_duration_seconds,
            )?,
            confidence: None,
            language: None,
            speaker_id: None,
            speaker_confidence: None,
            model: None,
            latency_ms: None,
            audio_source: AudioSource::Mixed,
            trace_id: None,
            words: Vec::new(),
        })
    }

    pub fn validate(&self) -> Result<(), StreamingProtocolError> {
        if self.connection_id.trim().is_empty() {
            return Err(StreamingProtocolError::MissingConnectionId);
        }
        if self.utterance_key.trim().is_empty() {
            return Err(StreamingProtocolError::MissingUtteranceKey);
        }
        if self.text.trim().is_empty() {
            return Err(StreamingProtocolError::EmptyTranscript);
        }
        self.time.absolute_frame_range()?;
        if self
            .speaker_confidence
            .is_some_and(|value| !value.is_finite())
        {
            return Err(StreamingProtocolError::InvalidProviderSpeakerConfidence);
        }
        for word in &self.words {
            word.validate()?;
        }
        Ok(())
    }
}

fn milliseconds_to_frames(milliseconds: f64) -> Result<u64, StreamingProtocolError> {
    let frames = milliseconds * f64::from(STREAMING_AUDIO_SAMPLE_RATE) / 1_000.0;
    if !frames.is_finite() || frames < 0.0 || frames > u64::MAX as f64 {
        return Err(StreamingProtocolError::FrameIndexOverflow);
    }
    Ok(frames.round() as u64)
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StreamingProtocolError {
    #[error("streaming audio must use {expected} Hz, received {actual} Hz")]
    InvalidSampleRate { expected: u32, actual: u32 },
    #[error("streaming audio must use {expected} channel, received {actual}")]
    InvalidChannelCount { expected: u16, actual: u16 },
    #[error("streaming audio frame is empty")]
    EmptyAudioFrame,
    #[error("streaming frame index overflow")]
    FrameIndexOverflow,
    #[error("provider time is negative, non-finite, or reversed")]
    InvalidProviderTime,
    #[error("provider connection id is missing")]
    MissingConnectionId,
    #[error("provider utterance key is missing")]
    MissingUtteranceKey,
    #[error("provider transcript text is empty")]
    EmptyTranscript,
    #[error("provider word text is empty")]
    EmptyProviderWord,
    #[error("provider word time is negative, non-finite, or reversed")]
    InvalidProviderWordTime,
    #[error("provider word confidence is non-finite")]
    InvalidProviderWordConfidence,
    #[error("provider speaker confidence is non-finite")]
    InvalidProviderSpeakerConfidence,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepgram_time_keeps_connection_origin() {
        let event = ProviderTranscriptEvent::deepgram(
            "connection-1",
            Some("request-1".to_string()),
            "utterance-1",
            "hello",
            false,
            480_000,
            1.25,
            0.75,
        )
        .unwrap();

        assert_eq!(
            event.time.absolute_frame_range().unwrap(),
            (540_000, 576_000)
        );
        assert_eq!(event.time.provider_start_ms, 1_250.0);
        assert_eq!(event.time.provider_end_ms, 2_000.0);
    }

    #[test]
    fn protocol_rejects_invalid_audio_shape_and_time_overflow() {
        let mut frame = StreamingAudioFrame::new(0, 0, vec![0.0; 480], AudioSource::Mixed);
        frame.sample_rate = 44_100;
        assert!(matches!(
            frame.validate(),
            Err(StreamingProtocolError::InvalidSampleRate { .. })
        ));

        let time = ProviderTimeRange {
            origin_frame: u64::MAX,
            provider_start_ms: 1.0,
            provider_end_ms: 2.0,
        };
        assert_eq!(
            time.absolute_frame_range(),
            Err(StreamingProtocolError::FrameIndexOverflow)
        );
    }

    #[test]
    fn capabilities_reject_implicit_format_fallbacks() {
        let deepgram = StreamingAsrCapabilities::deepgram();
        assert!(deepgram.accepts(16_000, StreamingAudioEncoding::Pcm16Le));
        assert!(!deepgram.accepts(48_000, StreamingAudioEncoding::Pcm16Le));
        assert!(!deepgram.accepts(16_000, StreamingAudioEncoding::PcmF32));

        let openai = StreamingAsrCapabilities::openai_realtime();
        assert!(!openai.provider_timestamps);
        assert!(!openai.word_timestamps);
        assert!(!openai.diarization);
    }
}
