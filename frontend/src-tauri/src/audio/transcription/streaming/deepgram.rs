//! Deepgram Nova live-transcription transport.
//!
//! This module owns only one WebSocket connection. Reconnect backoff, replay,
//! provider fallback, persistence, and Tauri events belong to the streaming
//! supervisor. Keeping those responsibilities separate makes URL generation
//! and provider message decoding testable without network access.

use super::pcm::{PcmError, PersistentPcmResampler, ProviderSampleRate};
use super::protocol::{
    ProviderTranscriptEvent, ProviderTranscriptKind, ProviderTranscriptWord, StreamingAudioFrame,
    StreamingProtocolError, STREAMING_ASR_SCHEMA_VERSION,
};
use crate::audio::transcription::AudioSource;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use std::fmt;
use std::io;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest,
    http::{header::AUTHORIZATION, HeaderValue},
    Error as WebSocketError, Message,
};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use url::Url;

pub const DEEPGRAM_LIVE_ENDPOINT: &str = "wss://api.deepgram.com/v1/listen";
pub const DEEPGRAM_SAMPLE_RATE: u32 = 16_000;
pub const DEEPGRAM_CHANNELS: u16 = 1;

const MAX_ENDPOINT_CHARS: usize = 2_048;
const MAX_MODEL_CHARS: usize = 128;
const MAX_LANGUAGE_CHARS: usize = 35;
const MAX_KEYTERMS: usize = 100;
const MAX_KEYTERM_CHARS: usize = 80;
const MAX_KEYTERM_TOTAL_CHARS: usize = 2_000;
const MIN_ENDPOINTING_MS: u32 = 10;
const MAX_ENDPOINTING_MS: u32 = 5_000;
const MIN_UTTERANCE_END_MS: u32 = 1_000;
const MAX_UTTERANCE_END_MS: u32 = 10_000;

const SECRET_QUERY_MARKERS: &[&str] = &[
    "api_key",
    "apikey",
    "key",
    "token",
    "secret",
    "auth",
    "signature",
    "credential",
    "password",
];

const MANAGED_QUERY_PARAMETERS: &[&str] = &[
    "model",
    "language",
    "encoding",
    "sample_rate",
    "channels",
    "interim_results",
    "endpointing",
    "utterance_end_ms",
    "diarize",
    "diarize_model",
    "keyterm",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepgramOptions {
    pub endpoint_override: Option<String>,
    pub model: String,
    /// A concrete Deepgram language tag, `multi`, or Meetily's `auto` alias.
    /// Streaming language detection is not supported by Deepgram, so `auto`
    /// is deliberately translated to Nova multilingual (`language=multi`).
    pub language: String,
    pub interim_results: bool,
    pub endpointing_ms: u32,
    pub utterance_end_ms: Option<u32>,
    pub diarize: bool,
    pub keyterms: Vec<String>,
}

impl Default for DeepgramOptions {
    fn default() -> Self {
        Self {
            endpoint_override: None,
            model: "nova-3".to_string(),
            language: "auto".to_string(),
            interim_results: true,
            endpointing_ms: 300,
            utterance_end_ms: Some(1_000),
            diarize: true,
            keyterms: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DeepgramConfigError {
    #[error("Deepgram endpoint is not a valid absolute URL")]
    InvalidEndpoint,
    #[error("Deepgram endpoint must use wss://")]
    InsecureEndpoint,
    #[error("Deepgram endpoint must include a host")]
    MissingEndpointHost,
    #[error("Deepgram endpoint must not contain user information")]
    EndpointContainsUserInfo,
    #[error("Deepgram endpoint must not contain a fragment")]
    EndpointContainsFragment,
    #[error("Deepgram endpoint query must not contain credentials")]
    EndpointContainsCredential,
    #[error("Deepgram endpoint override must not set managed parameter '{0}'")]
    EndpointOverridesManagedParameter(String),
    #[error("Deepgram model is empty or contains unsupported characters")]
    InvalidModel,
    #[error("Deepgram language is empty or is not a short language tag")]
    InvalidLanguage,
    #[error("Deepgram endpointing must be between 10 and 5000 milliseconds")]
    InvalidEndpointing,
    #[error("Deepgram utterance end must be between 1000 and 10000 milliseconds")]
    InvalidUtteranceEnd,
    #[error("Deepgram utterance end requires interim results")]
    UtteranceEndRequiresInterimResults,
    #[error("Deepgram keyterms exceed the supported count or length limits")]
    InvalidKeyterms,
}

/// Construct a credential-free Deepgram URL. Provider-owned parameters are
/// always appended through `url::Url`, so model names, languages, and keyterms
/// cannot escape into headers or alter URL structure.
pub fn build_deepgram_url(options: &DeepgramOptions) -> Result<Url, DeepgramConfigError> {
    validate_options(options)?;

    let endpoint = options
        .endpoint_override
        .as_deref()
        .unwrap_or(DEEPGRAM_LIVE_ENDPOINT);
    let mut url = Url::parse(endpoint).map_err(|_| DeepgramConfigError::InvalidEndpoint)?;
    validate_endpoint(&url)?;

    let retained_query: Vec<(String, String)> = url
        .query_pairs()
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    for (name, _) in &retained_query {
        let normalized = name.to_ascii_lowercase();
        if SECRET_QUERY_MARKERS
            .iter()
            .any(|marker| normalized.contains(marker))
        {
            return Err(DeepgramConfigError::EndpointContainsCredential);
        }
        if MANAGED_QUERY_PARAMETERS.contains(&normalized.as_str()) {
            return Err(DeepgramConfigError::EndpointOverridesManagedParameter(
                normalized,
            ));
        }
    }

    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        for (name, value) in retained_query {
            query.append_pair(&name, &value);
        }
        query
            .append_pair("model", options.model.trim())
            .append_pair("language", normalized_language(&options.language))
            .append_pair("encoding", "linear16")
            .append_pair("sample_rate", "16000")
            .append_pair("channels", "1")
            .append_pair(
                "interim_results",
                if options.interim_results {
                    "true"
                } else {
                    "false"
                },
            )
            .append_pair("endpointing", &options.endpointing_ms.to_string());
        if let Some(utterance_end_ms) = options.utterance_end_ms {
            query.append_pair("utterance_end_ms", &utterance_end_ms.to_string());
        }
        // Deepgram deprecated `diarize=true` for streaming. `latest` selects
        // the supported streaming diarization model without enabling v2.
        if options.diarize {
            query.append_pair("diarize_model", "latest");
        }
        for keyterm in normalized_keyterms(&options.keyterms)? {
            query.append_pair("keyterm", &keyterm);
        }
    }
    Ok(url)
}

fn validate_options(options: &DeepgramOptions) -> Result<(), DeepgramConfigError> {
    let model = options.model.trim();
    if model.is_empty()
        || model.chars().count() > MAX_MODEL_CHARS
        || model.chars().any(|character| {
            !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':'))
        })
    {
        return Err(DeepgramConfigError::InvalidModel);
    }

    let language = options.language.trim();
    if language.is_empty()
        || language.chars().count() > MAX_LANGUAGE_CHARS
        || !language
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(DeepgramConfigError::InvalidLanguage);
    }
    if !(MIN_ENDPOINTING_MS..=MAX_ENDPOINTING_MS).contains(&options.endpointing_ms) {
        return Err(DeepgramConfigError::InvalidEndpointing);
    }
    if let Some(value) = options.utterance_end_ms {
        if !(MIN_UTTERANCE_END_MS..=MAX_UTTERANCE_END_MS).contains(&value) {
            return Err(DeepgramConfigError::InvalidUtteranceEnd);
        }
        if !options.interim_results {
            return Err(DeepgramConfigError::UtteranceEndRequiresInterimResults);
        }
    }
    normalized_keyterms(&options.keyterms)?;
    Ok(())
}

fn validate_endpoint(url: &Url) -> Result<(), DeepgramConfigError> {
    if url.as_str().chars().count() > MAX_ENDPOINT_CHARS {
        return Err(DeepgramConfigError::InvalidEndpoint);
    }
    if url.scheme() != "wss" {
        return Err(DeepgramConfigError::InsecureEndpoint);
    }
    if url.host_str().is_none() {
        return Err(DeepgramConfigError::MissingEndpointHost);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(DeepgramConfigError::EndpointContainsUserInfo);
    }
    if url.fragment().is_some() {
        return Err(DeepgramConfigError::EndpointContainsFragment);
    }
    Ok(())
}

fn normalized_language(language: &str) -> &str {
    if language.trim().eq_ignore_ascii_case("auto") {
        "multi"
    } else {
        language.trim()
    }
}

fn normalized_keyterms(values: &[String]) -> Result<Vec<String>, DeepgramConfigError> {
    if values.len() > MAX_KEYTERMS {
        return Err(DeepgramConfigError::InvalidKeyterms);
    }
    let mut normalized = Vec::with_capacity(values.len());
    let mut total_chars = 0usize;
    for value in values {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let length = value.chars().count();
        if length > MAX_KEYTERM_CHARS || value.chars().any(char::is_control) {
            return Err(DeepgramConfigError::InvalidKeyterms);
        }
        total_chars = total_chars
            .checked_add(length)
            .ok_or(DeepgramConfigError::InvalidKeyterms)?;
        if total_chars > MAX_KEYTERM_TOTAL_CHARS {
            return Err(DeepgramConfigError::InvalidKeyterms);
        }
        if !normalized.iter().any(|existing| existing == value) {
            normalized.push(value.to_string());
        }
    }
    Ok(normalized)
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeepgramAlternative {
    pub transcript: String,
    pub confidence: Option<f32>,
    pub languages: Vec<String>,
    pub words: Vec<ProviderTranscriptWord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepgramModelInfo {
    pub id: Option<String>,
    pub name: Option<String>,
    pub version: Option<String>,
    pub architecture: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeepgramResultMetadata {
    pub request_id: Option<String>,
    pub model_uuid: Option<String>,
    pub model_info: Vec<DeepgramModelInfo>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeepgramResults {
    pub start_seconds: Option<f64>,
    pub duration_seconds: Option<f64>,
    pub is_final: bool,
    pub speech_final: bool,
    pub from_finalize: bool,
    pub channel_index: Vec<u32>,
    pub alternatives: Vec<DeepgramAlternative>,
    pub metadata: DeepgramResultMetadata,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeepgramUtteranceEnd {
    pub channel: Vec<u32>,
    pub last_word_end_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeepgramSpeechStarted {
    pub channel: Vec<u32>,
    pub timestamp_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeepgramMetadata {
    pub request_id: Option<String>,
    pub transaction_key: Option<String>,
    pub created: Option<String>,
    pub sha256: Option<String>,
    pub duration_seconds: Option<f64>,
    pub channels: Option<u32>,
    pub models: Vec<String>,
    pub model_info: Vec<DeepgramModelInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepgramProviderError {
    pub code: Option<String>,
    pub message: Option<String>,
    pub description: Option<String>,
    pub variant: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DeepgramMessage {
    Results(DeepgramResults),
    UtteranceEnd(DeepgramUtteranceEnd),
    SpeechStarted(DeepgramSpeechStarted),
    Metadata(DeepgramMetadata),
    ProviderError(DeepgramProviderError),
    Unknown { message_type: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DeepgramParseError {
    #[error("Deepgram sent malformed JSON")]
    InvalidJson,
    #[error("Deepgram message is missing its type")]
    MissingMessageType,
    #[error("Deepgram {message_type} message contains an invalid '{field}' field")]
    InvalidField {
        message_type: &'static str,
        field: &'static str,
    },
}

#[derive(Debug, Deserialize)]
struct WireEnvelope {
    #[serde(rename = "type")]
    message_type: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct WireChannel {
    #[serde(default)]
    alternatives: Vec<WireAlternative>,
}

#[derive(Debug, Deserialize)]
struct WireAlternative {
    #[serde(default)]
    transcript: String,
    confidence: Option<f32>,
    #[serde(default)]
    languages: Vec<String>,
    #[serde(default)]
    words: Vec<WireWord>,
}

#[derive(Debug, Deserialize)]
struct WireWord {
    #[serde(default)]
    word: String,
    punctuated_word: Option<String>,
    start: Option<f64>,
    end: Option<f64>,
    confidence: Option<f32>,
    language: Option<String>,
    speaker: Option<Value>,
    speaker_confidence: Option<f32>,
}

#[derive(Debug, Deserialize, Default)]
struct WireResultMetadata {
    request_id: Option<String>,
    model_uuid: Option<String>,
    #[serde(default)]
    model_info: Value,
}

#[derive(Debug, Deserialize)]
struct WireResults {
    start: Option<f64>,
    duration: Option<f64>,
    #[serde(default)]
    is_final: bool,
    #[serde(default)]
    speech_final: bool,
    #[serde(default)]
    from_finalize: bool,
    #[serde(default)]
    channel_index: Vec<u32>,
    #[serde(default)]
    channel: WireChannel,
    #[serde(default)]
    metadata: WireResultMetadata,
}

#[derive(Debug, Deserialize)]
struct WireUtteranceEnd {
    #[serde(default)]
    channel: Vec<u32>,
    last_word_end: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct WireSpeechStarted {
    #[serde(default)]
    channel: Vec<u32>,
    timestamp: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct WireMetadata {
    request_id: Option<String>,
    transaction_key: Option<String>,
    created: Option<String>,
    sha256: Option<String>,
    duration: Option<f64>,
    channels: Option<u32>,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    model_info: Value,
}

#[derive(Debug, Deserialize)]
struct WireProviderError {
    #[serde(alias = "err_code")]
    code: Option<String>,
    #[serde(alias = "err_msg")]
    message: Option<String>,
    description: Option<String>,
    variant: Option<String>,
}

/// Parse one Deepgram text frame without any socket or connection state.
pub fn parse_deepgram_message(input: &str) -> Result<DeepgramMessage, DeepgramParseError> {
    let value: Value = serde_json::from_str(input).map_err(|_| DeepgramParseError::InvalidJson)?;
    let envelope: WireEnvelope =
        serde_json::from_value(value.clone()).map_err(|_| DeepgramParseError::InvalidJson)?;
    let message_type = envelope
        .message_type
        .ok_or(DeepgramParseError::MissingMessageType)?;

    match message_type.as_str() {
        "Results" => parse_results(value).map(DeepgramMessage::Results),
        "UtteranceEnd" => {
            let message: WireUtteranceEnd = parse_wire(value)?;
            Ok(DeepgramMessage::UtteranceEnd(DeepgramUtteranceEnd {
                channel: message.channel,
                last_word_end_ms: optional_seconds_to_ms(
                    message.last_word_end,
                    "UtteranceEnd",
                    "last_word_end",
                )?,
            }))
        }
        "SpeechStarted" => {
            let message: WireSpeechStarted = parse_wire(value)?;
            Ok(DeepgramMessage::SpeechStarted(DeepgramSpeechStarted {
                channel: message.channel,
                timestamp_ms: optional_seconds_to_ms(
                    message.timestamp,
                    "SpeechStarted",
                    "timestamp",
                )?,
            }))
        }
        "Metadata" => {
            let message: WireMetadata = parse_wire(value)?;
            validate_optional_nonnegative(message.duration, "Metadata", "duration")?;
            Ok(DeepgramMessage::Metadata(DeepgramMetadata {
                request_id: message.request_id,
                transaction_key: message.transaction_key,
                created: message.created,
                sha256: message.sha256,
                duration_seconds: message.duration,
                channels: message.channels,
                models: message.models,
                model_info: parse_model_info(&message.model_info),
            }))
        }
        "Error" => {
            let message: WireProviderError = parse_wire(value)?;
            Ok(DeepgramMessage::ProviderError(DeepgramProviderError {
                code: message.code,
                message: message.message,
                description: message.description,
                variant: message.variant,
            }))
        }
        _ => Ok(DeepgramMessage::Unknown { message_type }),
    }
}

fn parse_wire<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, DeepgramParseError> {
    serde_json::from_value(value).map_err(|_| DeepgramParseError::InvalidJson)
}

fn parse_results(value: Value) -> Result<DeepgramResults, DeepgramParseError> {
    let message: WireResults = parse_wire(value)?;
    validate_optional_nonnegative(message.start, "Results", "start")?;
    validate_optional_nonnegative(message.duration, "Results", "duration")?;

    let mut alternatives = Vec::with_capacity(message.channel.alternatives.len());
    for alternative in message.channel.alternatives {
        validate_confidence(alternative.confidence, "confidence")?;
        let mut words = Vec::with_capacity(alternative.words.len());
        for word in alternative.words {
            if word.word.trim().is_empty() {
                return Err(DeepgramParseError::InvalidField {
                    message_type: "Results",
                    field: "word",
                });
            }
            validate_optional_nonnegative(word.start, "Results", "word.start")?;
            validate_optional_nonnegative(word.end, "Results", "word.end")?;
            if matches!((word.start, word.end), (Some(start), Some(end)) if end < start) {
                return Err(DeepgramParseError::InvalidField {
                    message_type: "Results",
                    field: "word.end",
                });
            }
            validate_confidence(word.confidence, "word.confidence")?;
            validate_confidence(word.speaker_confidence, "word.speaker_confidence")?;
            words.push(ProviderTranscriptWord {
                text: word.word,
                punctuated_text: word.punctuated_word,
                start_ms: optional_seconds_to_ms(word.start, "Results", "word.start")?,
                end_ms: optional_seconds_to_ms(word.end, "Results", "word.end")?,
                confidence: word.confidence,
                language: word.language,
                speaker_id: parse_speaker_id(word.speaker)?,
                speaker_confidence: word.speaker_confidence,
            });
        }
        alternatives.push(DeepgramAlternative {
            transcript: alternative.transcript,
            confidence: alternative.confidence,
            languages: alternative.languages,
            words,
        });
    }

    Ok(DeepgramResults {
        start_seconds: message.start,
        duration_seconds: message.duration,
        is_final: message.is_final,
        speech_final: message.speech_final,
        from_finalize: message.from_finalize,
        channel_index: message.channel_index,
        alternatives,
        metadata: DeepgramResultMetadata {
            request_id: message.metadata.request_id,
            model_uuid: message.metadata.model_uuid,
            model_info: parse_model_info(&message.metadata.model_info),
        },
    })
}

fn validate_optional_nonnegative(
    value: Option<f64>,
    message_type: &'static str,
    field: &'static str,
) -> Result<(), DeepgramParseError> {
    if value.is_some_and(|value| !value.is_finite() || value < 0.0) {
        return Err(DeepgramParseError::InvalidField {
            message_type,
            field,
        });
    }
    Ok(())
}

fn validate_confidence(value: Option<f32>, field: &'static str) -> Result<(), DeepgramParseError> {
    if value.is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value)) {
        return Err(DeepgramParseError::InvalidField {
            message_type: "Results",
            field,
        });
    }
    Ok(())
}

fn optional_seconds_to_ms(
    value: Option<f64>,
    message_type: &'static str,
    field: &'static str,
) -> Result<Option<f64>, DeepgramParseError> {
    validate_optional_nonnegative(value, message_type, field)?;
    value
        .map(|value| {
            let milliseconds = value * 1_000.0;
            if milliseconds.is_finite() {
                Ok(milliseconds)
            } else {
                Err(DeepgramParseError::InvalidField {
                    message_type,
                    field,
                })
            }
        })
        .transpose()
}

fn parse_speaker_id(value: Option<Value>) -> Result<Option<String>, DeepgramParseError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let normalized = match value {
        Value::Number(number) => number.as_u64().map(|value| value.to_string()),
        Value::String(value)
            if !value.trim().is_empty()
                && value.chars().count() <= 64
                && !value.chars().any(char::is_control) =>
        {
            Some(value)
        }
        _ => None,
    };
    normalized
        .map(Some)
        .ok_or(DeepgramParseError::InvalidField {
            message_type: "Results",
            field: "word.speaker",
        })
}

fn parse_model_info(value: &Value) -> Vec<DeepgramModelInfo> {
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    if object.contains_key("name") || object.contains_key("version") || object.contains_key("arch")
    {
        return vec![model_info_from_object(None, object)];
    }

    let mut infos: Vec<_> = object
        .iter()
        .filter_map(|(id, value)| {
            value
                .as_object()
                .map(|object| model_info_from_object(Some(id.clone()), object))
        })
        .collect();
    infos.sort_by(|left, right| left.id.cmp(&right.id));
    infos
}

fn model_info_from_object(
    id: Option<String>,
    object: &serde_json::Map<String, Value>,
) -> DeepgramModelInfo {
    DeepgramModelInfo {
        id,
        name: object
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string),
        version: object
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string),
        architecture: object
            .get("arch")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeepgramTranscript {
    pub event: ProviderTranscriptEvent,
    pub speech_final: bool,
    pub from_finalize: bool,
    pub channel_index: Vec<u32>,
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DeepgramServerEvent {
    Transcript(DeepgramTranscript),
    EmptyResult(DeepgramResults),
    UtteranceEnd(DeepgramUtteranceEnd),
    SpeechStarted(DeepgramSpeechStarted),
    Metadata(DeepgramMetadata),
    ProviderError(DeepgramProviderError),
    Unknown { message_type: String },
}

/// Stateful conversion from provider messages to stable utterance revisions.
/// It is intentionally independent of the socket and can be used with a fake
/// message source in tests.
#[derive(Debug, Clone)]
pub struct DeepgramEventDecoder {
    connection_id: String,
    origin_frame: u64,
    source: AudioSource,
    utterance_index: u64,
    utterance_active: bool,
}

impl DeepgramEventDecoder {
    pub fn new(
        connection_id: impl Into<String>,
        origin_frame: u64,
        source: AudioSource,
    ) -> Result<Self, DeepgramError> {
        let connection_id = connection_id.into();
        if connection_id.trim().is_empty() {
            return Err(DeepgramError::MissingConnectionId);
        }
        Ok(Self {
            connection_id,
            origin_frame,
            source,
            utterance_index: 0,
            utterance_active: false,
        })
    }

    pub fn decode(
        &mut self,
        message: DeepgramMessage,
    ) -> Result<DeepgramServerEvent, DeepgramError> {
        match message {
            DeepgramMessage::Results(results) => self.decode_results(results),
            DeepgramMessage::UtteranceEnd(event) => {
                self.finish_active_utterance()?;
                Ok(DeepgramServerEvent::UtteranceEnd(event))
            }
            DeepgramMessage::SpeechStarted(event) => Ok(DeepgramServerEvent::SpeechStarted(event)),
            DeepgramMessage::Metadata(event) => Ok(DeepgramServerEvent::Metadata(event)),
            DeepgramMessage::ProviderError(event) => Ok(DeepgramServerEvent::ProviderError(event)),
            DeepgramMessage::Unknown { message_type } => {
                Ok(DeepgramServerEvent::Unknown { message_type })
            }
        }
    }

    fn decode_results(
        &mut self,
        results: DeepgramResults,
    ) -> Result<DeepgramServerEvent, DeepgramError> {
        let Some(alternative) = results.alternatives.first() else {
            if results.speech_final {
                self.finish_active_utterance()?;
            }
            return Ok(DeepgramServerEvent::EmptyResult(results));
        };
        if alternative.transcript.trim().is_empty() {
            if results.speech_final {
                self.finish_active_utterance()?;
            }
            return Ok(DeepgramServerEvent::EmptyResult(results));
        }
        let start = results
            .start_seconds
            .ok_or(DeepgramParseError::InvalidField {
                message_type: "Results",
                field: "start",
            })?;
        let duration = results
            .duration_seconds
            .ok_or(DeepgramParseError::InvalidField {
                message_type: "Results",
                field: "duration",
            })?;

        self.utterance_active = true;
        let utterance_key = format!("utterance-{}", self.utterance_index);
        let model = results
            .metadata
            .model_info
            .iter()
            .find_map(|info| info.name.clone());
        let language = single_language(alternative);
        let speaker_id = single_speaker(&alternative.words);
        let mut event = ProviderTranscriptEvent::deepgram(
            self.connection_id.clone(),
            None,
            utterance_key,
            alternative.transcript.clone(),
            results.is_final,
            self.origin_frame,
            start,
            duration,
        )?;
        event.schema_version = STREAMING_ASR_SCHEMA_VERSION;
        event.kind = if results.is_final {
            ProviderTranscriptKind::Final
        } else {
            ProviderTranscriptKind::Partial
        };
        event.confidence = alternative.confidence;
        event.language = language;
        event.speaker_id = speaker_id;
        // Deepgram streaming exposes diarization confidence per word, not as
        // one utterance-level score. Keep the aggregate absent instead of
        // relabeling transcript confidence as speaker confidence.
        event.speaker_confidence = None;
        event.model = model;
        event.audio_source = self.source.clone();
        event.words = alternative.words.clone();
        event.validate()?;

        let decoded = DeepgramServerEvent::Transcript(DeepgramTranscript {
            event,
            speech_final: results.speech_final,
            from_finalize: results.from_finalize,
            channel_index: results.channel_index,
            request_id: results.metadata.request_id,
        });
        // `is_final` closes the revision chain for this stable segment.
        // `speech_final` remains available on the wrapper as the higher-level
        // endpoint signal consumed by the meeting supervisor.
        if results.is_final || results.speech_final {
            self.finish_active_utterance()?;
        }
        Ok(decoded)
    }

    fn finish_active_utterance(&mut self) -> Result<(), DeepgramError> {
        if self.utterance_active {
            self.utterance_index = self
                .utterance_index
                .checked_add(1)
                .ok_or(DeepgramError::UtteranceIndexOverflow)?;
            self.utterance_active = false;
        }
        Ok(())
    }
}

fn single_language(alternative: &DeepgramAlternative) -> Option<String> {
    if alternative.languages.len() == 1 {
        return alternative.languages.first().cloned();
    }
    let first = alternative.words.first()?.language.as_ref()?;
    if alternative
        .words
        .iter()
        .all(|word| word.language.as_ref() == Some(first))
    {
        Some(first.clone())
    } else {
        None
    }
}

fn single_speaker(words: &[ProviderTranscriptWord]) -> Option<String> {
    let first = words.first()?.speaker_id.as_ref()?;
    if words
        .iter()
        .all(|word| word.speaker_id.as_ref() == Some(first))
    {
        Some(first.clone())
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeepgramControl {
    KeepAlive,
    Finalize,
    CloseStream,
}

pub fn encode_deepgram_control(control: DeepgramControl) -> &'static str {
    match control {
        DeepgramControl::KeepAlive => r#"{"type":"KeepAlive"}"#,
        DeepgramControl::Finalize => r#"{"type":"Finalize"}"#,
        DeepgramControl::CloseStream => r#"{"type":"CloseStream"}"#,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeepgramFailureStage {
    Connect,
    SendAudio,
    SendControl,
    Receive,
    Close,
}

impl fmt::Display for DeepgramFailureStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Connect => "connect",
            Self::SendAudio => "send_audio",
            Self::SendControl => "send_control",
            Self::Receive => "receive",
            Self::Close => "close",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeepgramNetworkErrorKind {
    Authentication,
    RateLimited,
    ServiceUnavailable,
    Timeout,
    Disconnected,
    Tls,
    Protocol,
    Rejected,
    Other,
}

impl fmt::Display for DeepgramNetworkErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Authentication => "authentication",
            Self::RateLimited => "rate_limited",
            Self::ServiceUnavailable => "service_unavailable",
            Self::Timeout => "timeout",
            Self::Disconnected => "disconnected",
            Self::Tls => "tls",
            Self::Protocol => "protocol",
            Self::Rejected => "rejected",
            Self::Other => "other",
        })
    }
}

#[derive(Debug, Error)]
pub enum DeepgramError {
    #[error(transparent)]
    Configuration(#[from] DeepgramConfigError),
    #[error(transparent)]
    Parse(#[from] DeepgramParseError),
    #[error(transparent)]
    Protocol(#[from] StreamingProtocolError),
    #[error(transparent)]
    Pcm(#[from] PcmError),
    #[error("Deepgram API key is missing")]
    MissingCredential,
    #[error("Deepgram API key cannot be used as an authorization header")]
    InvalidCredential,
    #[error("Deepgram connection id is missing")]
    MissingConnectionId,
    #[error("Deepgram utterance index overflow")]
    UtteranceIndexOverflow,
    #[error("Deepgram PCM16 payload must contain complete 16-bit samples")]
    InvalidPcm16Payload,
    #[error("Deepgram sent a binary server message instead of JSON")]
    UnexpectedBinaryMessage,
    #[error("Deepgram WebSocket {stage} failed ({kind})")]
    Transport {
        stage: DeepgramFailureStage,
        kind: DeepgramNetworkErrorKind,
        #[source]
        source: WebSocketError,
    },
}

type DeepgramSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One authenticated Deepgram WebSocket. The API key is used only to build the
/// handshake request and is never stored on the connection or included in an
/// error value.
pub struct DeepgramConnection {
    socket: DeepgramSocket,
    decoder: DeepgramEventDecoder,
    resampler: PersistentPcmResampler,
}

impl DeepgramConnection {
    pub async fn connect(
        options: &DeepgramOptions,
        api_key: &str,
        connection_id: impl Into<String>,
        origin_frame: u64,
    ) -> Result<Self, DeepgramError> {
        Self::connect_with_source(
            options,
            api_key,
            connection_id,
            origin_frame,
            AudioSource::Mixed,
        )
        .await
    }

    /// Connect while preserving the capture topology that produced the
    /// canonical stream. The compatibility `connect` entry point remains
    /// mixed-source, while live recording supervisors can pass mic-only or
    /// system-only explicitly.
    pub async fn connect_with_source(
        options: &DeepgramOptions,
        api_key: &str,
        connection_id: impl Into<String>,
        origin_frame: u64,
        audio_source: AudioSource,
    ) -> Result<Self, DeepgramError> {
        let url = build_deepgram_url(options)?;
        let decoder = DeepgramEventDecoder::new(connection_id, origin_frame, audio_source)?;
        let request = authenticated_request(&url, api_key)?;
        let (socket, _) = connect_async(request)
            .await
            .map_err(|source| transport_error(DeepgramFailureStage::Connect, source))?;
        Ok(Self {
            socket,
            decoder,
            resampler: PersistentPcmResampler::new(ProviderSampleRate::Hz16000),
        })
    }

    /// Validate a canonical 48 kHz mono frame, continuously downsample it to
    /// 16 kHz, encode signed little-endian PCM16, and send a binary frame.
    pub async fn send_audio(
        &mut self,
        frame: &StreamingAudioFrame,
    ) -> Result<usize, DeepgramError> {
        frame.validate()?;
        let bytes = self.resampler.process_pcm16_le(&frame.samples)?;
        self.send_pcm16_le(&bytes).await
    }

    /// Send already-normalized 16 kHz mono PCM16 little-endian bytes.
    pub async fn send_pcm16_le(&mut self, bytes: &[u8]) -> Result<usize, DeepgramError> {
        if bytes.len() % 2 != 0 {
            return Err(DeepgramError::InvalidPcm16Payload);
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        self.socket
            .send(Message::binary(bytes.to_vec()))
            .await
            .map_err(|source| transport_error(DeepgramFailureStage::SendAudio, source))?;
        Ok(bytes.len())
    }

    /// Send Deepgram JSON protocol controls as text frames. Finalization and
    /// close-stream first flush the resampler's incomplete sample group.
    pub async fn send_control(&mut self, control: DeepgramControl) -> Result<(), DeepgramError> {
        if matches!(
            control,
            DeepgramControl::Finalize | DeepgramControl::CloseStream
        ) {
            let trailing = self.resampler.finish_pcm16_le()?;
            self.send_pcm16_le(&trailing).await?;
        }
        self.socket
            .send(Message::text(encode_deepgram_control(control)))
            .await
            .map_err(|source| transport_error(DeepgramFailureStage::SendControl, source))
    }

    /// Receive and decode the next provider event. Ping/Pong frames are handled
    /// internally; `None` means the remote WebSocket closed normally.
    pub async fn recv(&mut self) -> Result<Option<DeepgramServerEvent>, DeepgramError> {
        loop {
            let Some(message) = self.socket.next().await else {
                return Ok(None);
            };
            let message =
                message.map_err(|source| transport_error(DeepgramFailureStage::Receive, source))?;
            if message.is_text() {
                let text = message
                    .to_text()
                    .map_err(|source| transport_error(DeepgramFailureStage::Receive, source))?;
                let parsed = parse_deepgram_message(text)?;
                return self.decoder.decode(parsed).map(Some);
            }
            if message.is_ping() {
                self.socket
                    .send(Message::Pong(message.into_data()))
                    .await
                    .map_err(|source| transport_error(DeepgramFailureStage::SendControl, source))?;
                continue;
            }
            if message.is_pong() {
                continue;
            }
            if message.is_close() {
                return Ok(None);
            }
            if message.is_binary() {
                return Err(DeepgramError::UnexpectedBinaryMessage);
            }
        }
    }

    pub async fn close_websocket(&mut self) -> Result<(), DeepgramError> {
        self.socket
            .close(None)
            .await
            .map_err(|source| transport_error(DeepgramFailureStage::Close, source))
    }
}

fn authenticated_request(
    url: &Url,
    api_key: &str,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, DeepgramError> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Err(DeepgramError::MissingCredential);
    }
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|_| DeepgramConfigError::InvalidEndpoint)?;
    let authorization = HeaderValue::from_str(&format!("Token {api_key}"))
        .map_err(|_| DeepgramError::InvalidCredential)?;
    request.headers_mut().insert(AUTHORIZATION, authorization);
    Ok(request)
}

fn transport_error(stage: DeepgramFailureStage, source: WebSocketError) -> DeepgramError {
    let kind = classify_network_error(&source);
    DeepgramError::Transport {
        stage,
        kind,
        source,
    }
}

fn classify_network_error(error: &WebSocketError) -> DeepgramNetworkErrorKind {
    match error {
        WebSocketError::Http(response) => match response.status().as_u16() {
            401 | 403 => DeepgramNetworkErrorKind::Authentication,
            429 => DeepgramNetworkErrorKind::RateLimited,
            500..=599 => DeepgramNetworkErrorKind::ServiceUnavailable,
            _ => DeepgramNetworkErrorKind::Rejected,
        },
        WebSocketError::Io(error) if error.kind() == io::ErrorKind::TimedOut => {
            DeepgramNetworkErrorKind::Timeout
        }
        WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed => {
            DeepgramNetworkErrorKind::Disconnected
        }
        WebSocketError::Tls(_) => DeepgramNetworkErrorKind::Tls,
        WebSocketError::Protocol(_) | WebSocketError::Utf8 => DeepgramNetworkErrorKind::Protocol,
        _ => DeepgramNetworkErrorKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn query_map(url: &Url) -> HashMap<String, Vec<String>> {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for (name, value) in url.query_pairs() {
            map.entry(name.into_owned())
                .or_default()
                .push(value.into_owned());
        }
        map
    }

    #[test]
    fn default_url_is_credential_free_linear16_mono_and_multilingual() {
        let url = build_deepgram_url(&DeepgramOptions::default()).unwrap();
        assert_eq!(url.scheme(), "wss");
        assert_eq!(url.host_str(), Some("api.deepgram.com"));
        assert_eq!(url.path(), "/v1/listen");
        assert!(url.username().is_empty());
        assert!(url.password().is_none());

        let query = query_map(&url);
        assert_eq!(query["model"], ["nova-3"]);
        assert_eq!(query["language"], ["multi"]);
        assert_eq!(query["encoding"], ["linear16"]);
        assert_eq!(query["sample_rate"], ["16000"]);
        assert_eq!(query["channels"], ["1"]);
        assert_eq!(query["interim_results"], ["true"]);
        assert_eq!(query["endpointing"], ["300"]);
        assert_eq!(query["utterance_end_ms"], ["1000"]);
        assert_eq!(query["diarize_model"], ["latest"]);
        assert!(!query.contains_key("detect_language"));
        assert!(!query.contains_key("diarize"));
        assert!(!url.as_str().to_ascii_lowercase().contains("token"));
    }

    #[test]
    fn custom_endpoint_retains_safe_query_and_percent_encodes_keyterms() {
        let options = DeepgramOptions {
            endpoint_override: Some("wss://api.deepgram.com/v1/listen?version=1".to_string()),
            language: "ja-JP".to_string(),
            keyterms: vec!["Meetily 会議".to_string(), "Meetily 会議".to_string()],
            ..DeepgramOptions::default()
        };
        let query = query_map(&build_deepgram_url(&options).unwrap());
        assert_eq!(query["version"], ["1"]);
        assert_eq!(query["language"], ["ja-JP"]);
        assert_eq!(query["keyterm"], ["Meetily 会議"]);
    }

    #[test]
    fn endpoint_rejects_credentials_in_authority_or_query_and_managed_duplicates() {
        for (endpoint, expected) in [
            (
                "wss://user:pass@api.deepgram.com/v1/listen",
                DeepgramConfigError::EndpointContainsUserInfo,
            ),
            (
                "wss://api.deepgram.com/v1/listen?api_key=secret",
                DeepgramConfigError::EndpointContainsCredential,
            ),
            (
                "wss://api.deepgram.com/v1/listen?model=other",
                DeepgramConfigError::EndpointOverridesManagedParameter("model".to_string()),
            ),
        ] {
            let options = DeepgramOptions {
                endpoint_override: Some(endpoint.to_string()),
                ..DeepgramOptions::default()
            };
            assert_eq!(build_deepgram_url(&options), Err(expected));
        }

        let options = DeepgramOptions {
            endpoint_override: Some("ws://api.deepgram.com/v1/listen".to_string()),
            ..DeepgramOptions::default()
        };
        assert_eq!(
            build_deepgram_url(&options),
            Err(DeepgramConfigError::InsecureEndpoint)
        );
    }

    #[test]
    fn utterance_end_requires_interim_results_and_documented_range() {
        let options = DeepgramOptions {
            interim_results: false,
            ..DeepgramOptions::default()
        };
        assert_eq!(
            build_deepgram_url(&options),
            Err(DeepgramConfigError::UtteranceEndRequiresInterimResults)
        );
        let options = DeepgramOptions {
            utterance_end_ms: Some(999),
            ..DeepgramOptions::default()
        };
        assert_eq!(
            build_deepgram_url(&options),
            Err(DeepgramConfigError::InvalidUtteranceEnd)
        );
    }

    #[test]
    fn parses_results_without_dropping_word_metadata() {
        let message = parse_deepgram_message(
            r#"{
              "type":"Results",
              "channel_index":[0,1],
              "duration":1.25,
              "start":2.5,
              "is_final":true,
              "speech_final":true,
              "from_finalize":false,
              "channel":{"alternatives":[{
                "transcript":"こんにちは 世界",
                "confidence":0.97,
                "languages":["ja"],
                "words":[
                  {"word":"こんにちは","punctuated_word":"こんにちは、","start":2.5,"end":3.0,"confidence":0.98,"language":"ja","speaker":0,"speaker_confidence":0.91},
                  {"word":"世界","start":3.1,"end":3.75,"confidence":0.96,"language":"ja","speaker":0}
                ]
              }]},
              "metadata":{"request_id":"request-1","model_uuid":"model-1","model_info":{"name":"nova-3","version":"2026-01","arch":"nova-3"}}
            }"#,
        )
        .unwrap();
        let DeepgramMessage::Results(results) = message else {
            panic!("expected results");
        };
        assert_eq!(results.start_seconds, Some(2.5));
        assert_eq!(results.duration_seconds, Some(1.25));
        assert!(results.is_final);
        assert!(results.speech_final);
        let alternative = &results.alternatives[0];
        assert_eq!(alternative.confidence, Some(0.97));
        assert_eq!(alternative.words[0].start_ms, Some(2_500.0));
        assert_eq!(alternative.words[0].end_ms, Some(3_000.0));
        assert_eq!(alternative.words[0].confidence, Some(0.98));
        assert_eq!(alternative.words[0].speaker_id.as_deref(), Some("0"));
        assert_eq!(alternative.words[0].speaker_confidence, Some(0.91));
        assert_eq!(alternative.words[1].speaker_confidence, None);
        assert_eq!(results.metadata.request_id.as_deref(), Some("request-1"));
        assert_eq!(
            results.metadata.model_info[0].name.as_deref(),
            Some("nova-3")
        );
    }

    #[test]
    fn decoder_keeps_partial_and_final_on_one_utterance_then_advances() {
        use crate::audio::transcription::streaming::normalizer::{
            NormalizeOutcome, StreamingTranscriptNormalizer,
        };

        fn results(text: &str, is_final: bool, speech_final: bool) -> DeepgramResults {
            DeepgramResults {
                start_seconds: Some(0.0),
                duration_seconds: Some(1.0),
                is_final,
                speech_final,
                from_finalize: false,
                channel_index: vec![0, 1],
                alternatives: vec![DeepgramAlternative {
                    transcript: text.to_string(),
                    confidence: Some(0.9),
                    languages: vec!["en".to_string()],
                    words: vec![ProviderTranscriptWord {
                        text: "hello".to_string(),
                        punctuated_text: None,
                        start_ms: Some(0.0),
                        end_ms: Some(1_000.0),
                        confidence: Some(0.9),
                        language: Some("en".to_string()),
                        speaker_id: Some("2".to_string()),
                        speaker_confidence: None,
                    }],
                }],
                metadata: DeepgramResultMetadata::default(),
            }
        }

        let mut decoder =
            DeepgramEventDecoder::new("connection-1", 48_000, AudioSource::Mixed).unwrap();
        let DeepgramServerEvent::Transcript(partial) = decoder
            .decode(DeepgramMessage::Results(results("hel", false, false)))
            .unwrap()
        else {
            panic!("expected partial transcript");
        };
        let DeepgramServerEvent::Transcript(final_event) = decoder
            .decode(DeepgramMessage::Results(results("hello", true, false)))
            .unwrap()
        else {
            panic!("expected final transcript");
        };
        let DeepgramServerEvent::Transcript(next) = decoder
            .decode(DeepgramMessage::Results(results("next", true, true)))
            .unwrap()
        else {
            panic!("expected next transcript");
        };

        assert_eq!(partial.event.utterance_key, "utterance-0");
        assert_eq!(final_event.event.utterance_key, "utterance-0");
        assert_eq!(next.event.utterance_key, "utterance-1");
        assert_eq!(final_event.event.speaker_id.as_deref(), Some("2"));
        assert_eq!(final_event.event.language.as_deref(), Some("en"));
        assert_eq!(
            final_event.event.time.absolute_frame_range().unwrap(),
            (48_000, 96_000)
        );

        let mut normalizer = StreamingTranscriptNormalizer::new(Default::default(), 0);
        assert!(matches!(
            normalizer.normalize(partial.event).unwrap(),
            NormalizeOutcome::Emitted(_)
        ));
        assert!(matches!(
            normalizer.normalize(final_event.event).unwrap(),
            NormalizeOutcome::Emitted(_)
        ));
        let normalized_next = normalizer.normalize(next.event).unwrap();
        assert!(matches!(normalized_next, NormalizeOutcome::Emitted(_)));
    }

    #[test]
    fn parses_metadata_utterance_end_and_structured_provider_error() {
        let metadata = parse_deepgram_message(
            r#"{"type":"Metadata","request_id":"r-1","duration":3.2,"channels":1,"models":["m-1"],"model_info":{"m-1":{"name":"nova-3","version":"v","arch":"nova-3"}}}"#,
        )
        .unwrap();
        let DeepgramMessage::Metadata(metadata) = metadata else {
            panic!("expected metadata");
        };
        assert_eq!(metadata.duration_seconds, Some(3.2));
        assert_eq!(metadata.model_info[0].id.as_deref(), Some("m-1"));

        let utterance = parse_deepgram_message(
            r#"{"type":"UtteranceEnd","channel":[0,1],"last_word_end":4.25}"#,
        )
        .unwrap();
        assert!(matches!(
            utterance,
            DeepgramMessage::UtteranceEnd(DeepgramUtteranceEnd {
                last_word_end_ms: Some(4250.0),
                ..
            })
        ));

        let provider_error = parse_deepgram_message(
            r#"{"type":"Error","err_code":"NET-0001","err_msg":"timeout","variant":"Network"}"#,
        )
        .unwrap();
        assert!(matches!(
            provider_error,
            DeepgramMessage::ProviderError(DeepgramProviderError {
                code: Some(code),
                ..
            }) if code == "NET-0001"
        ));
    }

    #[test]
    fn controls_are_text_json_and_credentials_are_header_only() {
        assert_eq!(
            encode_deepgram_control(DeepgramControl::KeepAlive),
            r#"{"type":"KeepAlive"}"#
        );
        assert_eq!(
            encode_deepgram_control(DeepgramControl::Finalize),
            r#"{"type":"Finalize"}"#
        );
        assert_eq!(
            encode_deepgram_control(DeepgramControl::CloseStream),
            r#"{"type":"CloseStream"}"#
        );

        let url = build_deepgram_url(&DeepgramOptions::default()).unwrap();
        let request = authenticated_request(&url, "test-secret").unwrap();
        assert_eq!(request.headers()[AUTHORIZATION], "Token test-secret");
        assert!(!request.uri().to_string().contains("test-secret"));
    }
}
