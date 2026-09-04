//! OpenAI Realtime transcription WebSocket transport.
//!
//! This module owns exactly one provider connection. Reconnect, replay,
//! fallback, transcript normalization, persistence, and Tauri events remain
//! responsibilities of the streaming supervisor. The wire codec intentionally
//! keeps OpenAI transcript events separate from [`ProviderTranscriptEvent`]:
//! Realtime deltas do not provide word timestamps, confidence, or speaker
//! identity, so this transport must not invent those fields.

use super::pcm::{PcmError, PersistentPcmResampler, ProviderSampleRate};
use super::protocol::{StreamingAudioFrame, StreamingProtocolError};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
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

pub const OPENAI_REALTIME_ENDPOINT: &str = "wss://api.openai.com/v1/realtime";
pub const OPENAI_REALTIME_SAMPLE_RATE: u32 = 24_000;
pub const OPENAI_REALTIME_CHANNELS: u16 = 1;

const MAX_ENDPOINT_CHARS: usize = 2_048;
const MAX_MODEL_CHARS: usize = 128;
const MAX_PROMPT_CHARS: usize = 16_000;
const MAX_LANGUAGES: usize = 20;
const MAX_LANGUAGE_CHARS: usize = 35;
const MAX_KEYWORDS: usize = 100;
const MAX_KEYWORD_CHARS: usize = 100;
const MAX_KEYWORD_TOTAL_CHARS: usize = 4_000;
const MAX_ACCUMULATED_TRANSCRIPT_BYTES: usize = 1_048_576;

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
const MANAGED_QUERY_PARAMETERS: &[&str] = &["intent", "model"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiRealtimeDelay {
    Minimal,
    Low,
    Medium,
    High,
    ExtraHigh,
}

impl OpenAiRealtimeDelay {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::ExtraHigh => "xhigh",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum OpenAiRealtimeTurnDetection {
    Disabled,
    ServerVad {
        threshold: f32,
        prefix_padding_ms: u32,
        silence_duration_ms: u32,
    },
}

impl Default for OpenAiRealtimeTurnDetection {
    fn default() -> Self {
        Self::Disabled
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenAiRealtimeOptions {
    pub endpoint_override: Option<String>,
    pub model: String,
    pub prompt: Option<String>,
    /// Expected input languages. An empty list, or the sole value `auto`,
    /// omits the provider hint and lets the model infer the language.
    pub languages: Vec<String>,
    pub keywords: Vec<String>,
    pub delay: Option<OpenAiRealtimeDelay>,
    pub turn_detection: OpenAiRealtimeTurnDetection,
}

impl Default for OpenAiRealtimeOptions {
    fn default() -> Self {
        Self {
            endpoint_override: None,
            model: "gpt-live-transcribe".to_string(),
            prompt: None,
            languages: Vec::new(),
            keywords: Vec::new(),
            delay: Some(OpenAiRealtimeDelay::Low),
            turn_detection: OpenAiRealtimeTurnDetection::Disabled,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OpenAiRealtimeConfigError {
    #[error("OpenAI Realtime endpoint is not a valid absolute URL")]
    InvalidEndpoint,
    #[error("OpenAI Realtime endpoint must use wss://")]
    InsecureEndpoint,
    #[error("OpenAI Realtime endpoint must include a host")]
    MissingEndpointHost,
    #[error("OpenAI Realtime endpoint must not contain user information")]
    EndpointContainsUserInfo,
    #[error("OpenAI Realtime endpoint must not contain a fragment")]
    EndpointContainsFragment,
    #[error("OpenAI Realtime endpoint query must not contain credentials")]
    EndpointContainsCredential,
    #[error("OpenAI Realtime endpoint override must not set managed parameter '{0}'")]
    EndpointOverridesManagedParameter(String),
    #[error("OpenAI Realtime transcription model is invalid")]
    InvalidModel,
    #[error("OpenAI Realtime transcription prompt is too long or contains NUL")]
    InvalidPrompt,
    #[error("OpenAI Realtime language hints are invalid")]
    InvalidLanguages,
    #[error("OpenAI Realtime keyword hints are invalid")]
    InvalidKeywords,
    #[error("OpenAI Realtime server VAD settings are invalid")]
    InvalidTurnDetection,
}

/// Build a credential-free transcription URL. `intent=transcription` is owned
/// by this connector and cannot be changed by an endpoint override. The model
/// is configured in `session.update`, never in the URL.
pub fn build_openai_realtime_url(
    options: &OpenAiRealtimeOptions,
) -> Result<Url, OpenAiRealtimeConfigError> {
    validate_options(options)?;
    let endpoint = options
        .endpoint_override
        .as_deref()
        .unwrap_or(OPENAI_REALTIME_ENDPOINT);
    let mut url = Url::parse(endpoint).map_err(|_| OpenAiRealtimeConfigError::InvalidEndpoint)?;
    validate_endpoint(&url)?;

    let retained_query: Vec<(String, String)> = url
        .query_pairs()
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    for (name, value) in &retained_query {
        let normalized_name = name.to_ascii_lowercase();
        let normalized_value = value.to_ascii_lowercase();
        if SECRET_QUERY_MARKERS
            .iter()
            .any(|marker| normalized_name.contains(marker))
            || normalized_value.contains("bearer ")
            || normalized_value.starts_with("sk-")
        {
            return Err(OpenAiRealtimeConfigError::EndpointContainsCredential);
        }
        if MANAGED_QUERY_PARAMETERS.contains(&normalized_name.as_str()) {
            return Err(
                OpenAiRealtimeConfigError::EndpointOverridesManagedParameter(normalized_name),
            );
        }
    }

    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        for (name, value) in retained_query {
            query.append_pair(&name, &value);
        }
        query.append_pair("intent", "transcription");
    }
    Ok(url)
}

fn validate_endpoint(url: &Url) -> Result<(), OpenAiRealtimeConfigError> {
    if url.as_str().chars().count() > MAX_ENDPOINT_CHARS {
        return Err(OpenAiRealtimeConfigError::InvalidEndpoint);
    }
    if url.scheme() != "wss" {
        return Err(OpenAiRealtimeConfigError::InsecureEndpoint);
    }
    if url.host_str().is_none() {
        return Err(OpenAiRealtimeConfigError::MissingEndpointHost);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(OpenAiRealtimeConfigError::EndpointContainsUserInfo);
    }
    if url.fragment().is_some() {
        return Err(OpenAiRealtimeConfigError::EndpointContainsFragment);
    }
    Ok(())
}

fn validate_options(options: &OpenAiRealtimeOptions) -> Result<(), OpenAiRealtimeConfigError> {
    let model = options.model.trim();
    if model.is_empty()
        || model.chars().count() > MAX_MODEL_CHARS
        || model.chars().any(|character| {
            !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':'))
        })
    {
        return Err(OpenAiRealtimeConfigError::InvalidModel);
    }
    if options.prompt.as_ref().is_some_and(|prompt| {
        prompt.chars().count() > MAX_PROMPT_CHARS || prompt.chars().any(|value| value == '\0')
    }) {
        return Err(OpenAiRealtimeConfigError::InvalidPrompt);
    }
    normalized_languages(&options.languages)?;
    normalized_keywords(&options.keywords)?;
    if let OpenAiRealtimeTurnDetection::ServerVad {
        threshold,
        prefix_padding_ms,
        silence_duration_ms,
    } = options.turn_detection
    {
        if !threshold.is_finite()
            || !(0.0..=1.0).contains(&threshold)
            || prefix_padding_ms > 5_000
            || !(100..=10_000).contains(&silence_duration_ms)
        {
            return Err(OpenAiRealtimeConfigError::InvalidTurnDetection);
        }
    }
    Ok(())
}

fn normalized_languages(values: &[String]) -> Result<Vec<String>, OpenAiRealtimeConfigError> {
    if values.len() > MAX_LANGUAGES {
        return Err(OpenAiRealtimeConfigError::InvalidLanguages);
    }
    if values.len() == 1 && values[0].trim().eq_ignore_ascii_case("auto") {
        return Ok(Vec::new());
    }
    let mut normalized = Vec::with_capacity(values.len());
    for value in values {
        let value = value.trim().to_ascii_lowercase();
        if value.is_empty()
            || value == "auto"
            || value.chars().count() > MAX_LANGUAGE_CHARS
            || !value
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
        {
            return Err(OpenAiRealtimeConfigError::InvalidLanguages);
        }
        if !normalized.contains(&value) {
            normalized.push(value);
        }
    }
    Ok(normalized)
}

fn normalized_keywords(values: &[String]) -> Result<Vec<String>, OpenAiRealtimeConfigError> {
    if values.len() > MAX_KEYWORDS {
        return Err(OpenAiRealtimeConfigError::InvalidKeywords);
    }
    let mut normalized = Vec::with_capacity(values.len());
    let mut total_chars = 0usize;
    for value in values {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let length = value.chars().count();
        if length > MAX_KEYWORD_CHARS
            || value
                .chars()
                .any(|character| matches!(character, '<' | '>' | '\r' | '\n' | '\0'))
        {
            return Err(OpenAiRealtimeConfigError::InvalidKeywords);
        }
        total_chars = total_chars
            .checked_add(length)
            .ok_or(OpenAiRealtimeConfigError::InvalidKeywords)?;
        if total_chars > MAX_KEYWORD_TOTAL_CHARS {
            return Err(OpenAiRealtimeConfigError::InvalidKeywords);
        }
        if !normalized.iter().any(|existing| existing == value) {
            normalized.push(value.to_string());
        }
    }
    Ok(normalized)
}

/// Encode the current GA transcription-session shape documented by OpenAI.
pub fn encode_openai_session_update(
    options: &OpenAiRealtimeOptions,
) -> Result<String, OpenAiRealtimeError> {
    validate_options(options)?;
    let languages = normalized_languages(&options.languages)?;
    let keywords = normalized_keywords(&options.keywords)?;

    let mut transcription = Map::new();
    transcription.insert("model".to_string(), json!(options.model.trim()));
    if let Some(prompt) = options
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        transcription.insert("prompt".to_string(), json!(prompt));
    }
    if !keywords.is_empty() {
        transcription.insert("keywords".to_string(), json!(keywords));
    }
    if !languages.is_empty() {
        transcription.insert("languages".to_string(), json!(languages));
    }
    if let Some(delay) = options.delay {
        transcription.insert("delay".to_string(), json!(delay.as_str()));
    }

    let turn_detection = match options.turn_detection {
        OpenAiRealtimeTurnDetection::Disabled => Value::Null,
        OpenAiRealtimeTurnDetection::ServerVad {
            threshold,
            prefix_padding_ms,
            silence_duration_ms,
        } => json!({
            "type": "server_vad",
            "threshold": threshold,
            "prefix_padding_ms": prefix_padding_ms,
            "silence_duration_ms": silence_duration_ms,
        }),
    };
    let event = json!({
        "type": "session.update",
        "session": {
            "type": "transcription",
            "audio": {
                "input": {
                    "format": {
                        "type": "audio/pcm",
                        "rate": OPENAI_REALTIME_SAMPLE_RATE,
                    },
                    "transcription": Value::Object(transcription),
                    "turn_detection": turn_detection,
                }
            }
        }
    });
    serde_json::to_string(&event).map_err(|_| OpenAiRealtimeError::Encode)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiRealtimeControl {
    Commit,
    Clear,
}

pub fn encode_openai_control(control: OpenAiRealtimeControl) -> &'static str {
    match control {
        OpenAiRealtimeControl::Commit => r#"{"type":"input_audio_buffer.commit"}"#,
        OpenAiRealtimeControl::Clear => r#"{"type":"input_audio_buffer.clear"}"#,
    }
}

pub fn encode_openai_audio_append(pcm16_le: &[u8]) -> Result<String, OpenAiRealtimeError> {
    if pcm16_le.len() % 2 != 0 {
        return Err(OpenAiRealtimeError::InvalidPcm16Payload);
    }
    if pcm16_le.is_empty() {
        return Err(OpenAiRealtimeError::EmptyAudioPayload);
    }
    let event = json!({
        "type": "input_audio_buffer.append",
        "audio": BASE64_STANDARD.encode(pcm16_le),
    });
    serde_json::to_string(&event).map_err(|_| OpenAiRealtimeError::Encode)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiSessionLifecycle {
    pub event_id: Option<String>,
    pub session_id: Option<String>,
    pub object: Option<String>,
    pub session_type: Option<String>,
    pub transcription_model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiSpeechStarted {
    pub event_id: Option<String>,
    pub item_id: String,
    pub audio_start_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiSpeechStopped {
    pub event_id: Option<String>,
    pub item_id: String,
    pub audio_end_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiAudioBufferCommitted {
    pub event_id: Option<String>,
    pub item_id: String,
    pub previous_item_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiAudioBufferCleared {
    pub event_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiTranscriptDelta {
    pub event_id: Option<String>,
    pub item_id: String,
    pub content_index: u32,
    pub delta: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiTranscriptCompleted {
    pub event_id: Option<String>,
    pub item_id: String,
    pub content_index: u32,
    pub transcript: String,
    /// Present for committed-turn models that report detected language.
    /// `gpt-live-transcribe` currently does not return this field.
    pub languages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiProviderError {
    pub error_type: Option<String>,
    pub code: Option<String>,
    pub message: Option<String>,
    pub param: Option<String>,
    pub related_event_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiTranscriptFailed {
    pub event_id: Option<String>,
    pub item_id: String,
    pub content_index: u32,
    pub error: OpenAiProviderError,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenAiRateLimit {
    pub name: String,
    pub limit: Option<f64>,
    pub remaining: Option<f64>,
    pub reset_seconds: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenAiRateLimitsUpdated {
    pub event_id: Option<String>,
    pub rate_limits: Vec<OpenAiRateLimit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiErrorEvent {
    pub event_id: Option<String>,
    pub error: OpenAiProviderError,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OpenAiRealtimeMessage {
    SessionCreated(OpenAiSessionLifecycle),
    SessionUpdated(OpenAiSessionLifecycle),
    /// Accepted for compatibility with pre-GA transcription sessions.
    TranscriptionSessionCreated(OpenAiSessionLifecycle),
    /// Accepted for compatibility with pre-GA transcription sessions.
    TranscriptionSessionUpdated(OpenAiSessionLifecycle),
    SpeechStarted(OpenAiSpeechStarted),
    SpeechStopped(OpenAiSpeechStopped),
    AudioBufferCommitted(OpenAiAudioBufferCommitted),
    AudioBufferCleared(OpenAiAudioBufferCleared),
    TranscriptDelta(OpenAiTranscriptDelta),
    TranscriptCompleted(OpenAiTranscriptCompleted),
    TranscriptFailed(OpenAiTranscriptFailed),
    Error(OpenAiErrorEvent),
    RateLimitsUpdated(OpenAiRateLimitsUpdated),
    Unknown {
        message_type: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OpenAiRealtimeParseError {
    #[error("OpenAI Realtime sent malformed JSON")]
    InvalidJson,
    #[error("OpenAI Realtime message is missing its type")]
    MissingMessageType,
    #[error("OpenAI Realtime message type is invalid")]
    InvalidMessageType,
    #[error("OpenAI Realtime {message_type} message contains an invalid '{field}' field")]
    InvalidField {
        message_type: &'static str,
        field: &'static str,
    },
    #[error("OpenAI Realtime accumulated transcript exceeds the local safety limit")]
    TranscriptTooLarge,
}

#[derive(Debug, Deserialize)]
struct WireEnvelope {
    #[serde(rename = "type")]
    message_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireSessionEvent {
    event_id: Option<String>,
    session: Value,
}

#[derive(Debug, Deserialize)]
struct WireSpeechStarted {
    event_id: Option<String>,
    item_id: Option<String>,
    audio_start_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct WireSpeechStopped {
    event_id: Option<String>,
    item_id: Option<String>,
    audio_end_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct WireCommitted {
    event_id: Option<String>,
    item_id: Option<String>,
    previous_item_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireCleared {
    event_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireTranscriptDelta {
    event_id: Option<String>,
    item_id: Option<String>,
    content_index: Option<u32>,
    delta: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireLanguage {
    code: String,
}

#[derive(Debug, Deserialize)]
struct WireTranscriptCompleted {
    event_id: Option<String>,
    item_id: Option<String>,
    content_index: Option<u32>,
    transcript: Option<String>,
    #[serde(default)]
    languages: Vec<WireLanguage>,
}

#[derive(Debug, Deserialize, Default)]
struct WireProviderError {
    #[serde(rename = "type")]
    error_type: Option<String>,
    code: Option<String>,
    message: Option<String>,
    param: Option<String>,
    event_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireTranscriptFailed {
    event_id: Option<String>,
    item_id: Option<String>,
    content_index: Option<u32>,
    error: WireProviderError,
}

#[derive(Debug, Deserialize)]
struct WireErrorEvent {
    event_id: Option<String>,
    error: WireProviderError,
}

#[derive(Debug, Deserialize)]
struct WireRateLimit {
    name: String,
    limit: Option<f64>,
    remaining: Option<f64>,
    reset_seconds: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct WireRateLimitsUpdated {
    event_id: Option<String>,
    #[serde(default)]
    rate_limits: Vec<WireRateLimit>,
}

/// Parse one OpenAI text frame without retaining socket or credential state.
pub fn parse_openai_realtime_message(
    input: &str,
) -> Result<OpenAiRealtimeMessage, OpenAiRealtimeParseError> {
    let value: Value =
        serde_json::from_str(input).map_err(|_| OpenAiRealtimeParseError::InvalidJson)?;
    let envelope: WireEnvelope =
        serde_json::from_value(value.clone()).map_err(|_| OpenAiRealtimeParseError::InvalidJson)?;
    let message_type = envelope
        .message_type
        .ok_or(OpenAiRealtimeParseError::MissingMessageType)?;
    if message_type.is_empty()
        || message_type.chars().count() > 128
        || message_type.chars().any(char::is_control)
    {
        return Err(OpenAiRealtimeParseError::InvalidMessageType);
    }

    match message_type.as_str() {
        "session.created" => parse_session_lifecycle(value, "session.created")
            .map(OpenAiRealtimeMessage::SessionCreated),
        "session.updated" => parse_session_lifecycle(value, "session.updated")
            .map(OpenAiRealtimeMessage::SessionUpdated),
        "transcription_session.created" => {
            parse_session_lifecycle(value, "transcription_session.created")
                .map(OpenAiRealtimeMessage::TranscriptionSessionCreated)
        }
        "transcription_session.updated" => {
            parse_session_lifecycle(value, "transcription_session.updated")
                .map(OpenAiRealtimeMessage::TranscriptionSessionUpdated)
        }
        "input_audio_buffer.speech_started" => {
            let wire: WireSpeechStarted = parse_wire(value, "input_audio_buffer.speech_started")?;
            Ok(OpenAiRealtimeMessage::SpeechStarted(OpenAiSpeechStarted {
                event_id: valid_optional_string(
                    wire.event_id,
                    "input_audio_buffer.speech_started",
                    "event_id",
                )?,
                item_id: required_string(
                    wire.item_id,
                    "input_audio_buffer.speech_started",
                    "item_id",
                )?,
                audio_start_ms: wire.audio_start_ms.ok_or(
                    OpenAiRealtimeParseError::InvalidField {
                        message_type: "input_audio_buffer.speech_started",
                        field: "audio_start_ms",
                    },
                )?,
            }))
        }
        "input_audio_buffer.speech_stopped" => {
            let wire: WireSpeechStopped = parse_wire(value, "input_audio_buffer.speech_stopped")?;
            Ok(OpenAiRealtimeMessage::SpeechStopped(OpenAiSpeechStopped {
                event_id: valid_optional_string(
                    wire.event_id,
                    "input_audio_buffer.speech_stopped",
                    "event_id",
                )?,
                item_id: required_string(
                    wire.item_id,
                    "input_audio_buffer.speech_stopped",
                    "item_id",
                )?,
                audio_end_ms: wire
                    .audio_end_ms
                    .ok_or(OpenAiRealtimeParseError::InvalidField {
                        message_type: "input_audio_buffer.speech_stopped",
                        field: "audio_end_ms",
                    })?,
            }))
        }
        "input_audio_buffer.committed" => {
            let wire: WireCommitted = parse_wire(value, "input_audio_buffer.committed")?;
            Ok(OpenAiRealtimeMessage::AudioBufferCommitted(
                OpenAiAudioBufferCommitted {
                    event_id: valid_optional_string(
                        wire.event_id,
                        "input_audio_buffer.committed",
                        "event_id",
                    )?,
                    item_id: required_string(
                        wire.item_id,
                        "input_audio_buffer.committed",
                        "item_id",
                    )?,
                    previous_item_id: valid_optional_string(
                        wire.previous_item_id,
                        "input_audio_buffer.committed",
                        "previous_item_id",
                    )?,
                },
            ))
        }
        "input_audio_buffer.cleared" => {
            let wire: WireCleared = parse_wire(value, "input_audio_buffer.cleared")?;
            Ok(OpenAiRealtimeMessage::AudioBufferCleared(
                OpenAiAudioBufferCleared {
                    event_id: valid_optional_string(
                        wire.event_id,
                        "input_audio_buffer.cleared",
                        "event_id",
                    )?,
                },
            ))
        }
        "conversation.item.input_audio_transcription.delta" => {
            parse_transcript_delta(value).map(OpenAiRealtimeMessage::TranscriptDelta)
        }
        "conversation.item.input_audio_transcription.completed" => {
            parse_transcript_completed(value).map(OpenAiRealtimeMessage::TranscriptCompleted)
        }
        "conversation.item.input_audio_transcription.failed" => {
            parse_transcript_failed(value).map(OpenAiRealtimeMessage::TranscriptFailed)
        }
        "error" => {
            let wire: WireErrorEvent = parse_wire(value, "error")?;
            Ok(OpenAiRealtimeMessage::Error(OpenAiErrorEvent {
                event_id: valid_optional_string(wire.event_id, "error", "event_id")?,
                error: provider_error(wire.error),
            }))
        }
        "rate_limits.updated" => {
            parse_rate_limits(value).map(OpenAiRealtimeMessage::RateLimitsUpdated)
        }
        _ => Ok(OpenAiRealtimeMessage::Unknown { message_type }),
    }
}

fn parse_wire<T: for<'de> Deserialize<'de>>(
    value: Value,
    message_type: &'static str,
) -> Result<T, OpenAiRealtimeParseError> {
    serde_json::from_value(value).map_err(|_| OpenAiRealtimeParseError::InvalidField {
        message_type,
        field: "message",
    })
}

fn parse_session_lifecycle(
    value: Value,
    message_type: &'static str,
) -> Result<OpenAiSessionLifecycle, OpenAiRealtimeParseError> {
    let wire: WireSessionEvent = parse_wire(value, message_type)?;
    let session = wire
        .session
        .as_object()
        .ok_or(OpenAiRealtimeParseError::InvalidField {
            message_type,
            field: "session",
        })?;
    let mut session_type = optional_object_string(session, "type", message_type)?;
    if session_type.is_none() && message_type.starts_with("transcription_session.") {
        session_type = Some("transcription".to_string());
    }
    let transcription_model = optional_pointer_string(
        &wire.session,
        "/audio/input/transcription/model",
        message_type,
        "session.audio.input.transcription.model",
    )?
    .or(optional_pointer_string(
        &wire.session,
        "/input_audio_transcription/model",
        message_type,
        "session.input_audio_transcription.model",
    )?);

    Ok(OpenAiSessionLifecycle {
        event_id: valid_optional_string(wire.event_id, message_type, "event_id")?,
        session_id: optional_object_string(session, "id", message_type)?,
        object: optional_object_string(session, "object", message_type)?,
        session_type,
        transcription_model,
    })
}

fn optional_object_string(
    object: &serde_json::Map<String, Value>,
    key: &'static str,
    message_type: &'static str,
) -> Result<Option<String>, OpenAiRealtimeParseError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => valid_optional_string(Some(value.clone()), message_type, key),
        Some(_) => Err(OpenAiRealtimeParseError::InvalidField {
            message_type,
            field: key,
        }),
    }
}

fn optional_pointer_string(
    value: &Value,
    pointer: &str,
    message_type: &'static str,
    field: &'static str,
) -> Result<Option<String>, OpenAiRealtimeParseError> {
    match value.pointer(pointer) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            valid_optional_string(Some(value.clone()), message_type, field)
        }
        Some(_) => Err(OpenAiRealtimeParseError::InvalidField {
            message_type,
            field,
        }),
    }
}

fn required_string(
    value: Option<String>,
    message_type: &'static str,
    field: &'static str,
) -> Result<String, OpenAiRealtimeParseError> {
    valid_optional_string(value, message_type, field)?.ok_or(
        OpenAiRealtimeParseError::InvalidField {
            message_type,
            field,
        },
    )
}

fn valid_optional_string(
    value: Option<String>,
    message_type: &'static str,
    field: &'static str,
) -> Result<Option<String>, OpenAiRealtimeParseError> {
    value
        .map(|value| {
            if value.trim().is_empty()
                || value.chars().count() > 4_096
                || value.chars().any(char::is_control)
            {
                Err(OpenAiRealtimeParseError::InvalidField {
                    message_type,
                    field,
                })
            } else {
                Ok(value)
            }
        })
        .transpose()
}

fn parse_transcript_delta(value: Value) -> Result<OpenAiTranscriptDelta, OpenAiRealtimeParseError> {
    const MESSAGE_TYPE: &str = "conversation.item.input_audio_transcription.delta";
    let wire: WireTranscriptDelta = parse_wire(value, MESSAGE_TYPE)?;
    let delta = wire.delta.ok_or(OpenAiRealtimeParseError::InvalidField {
        message_type: MESSAGE_TYPE,
        field: "delta",
    })?;
    if delta.len() > MAX_ACCUMULATED_TRANSCRIPT_BYTES {
        return Err(OpenAiRealtimeParseError::TranscriptTooLarge);
    }
    Ok(OpenAiTranscriptDelta {
        event_id: valid_optional_string(wire.event_id, MESSAGE_TYPE, "event_id")?,
        item_id: required_string(wire.item_id, MESSAGE_TYPE, "item_id")?,
        content_index: wire
            .content_index
            .ok_or(OpenAiRealtimeParseError::InvalidField {
                message_type: MESSAGE_TYPE,
                field: "content_index",
            })?,
        delta,
    })
}

fn parse_transcript_completed(
    value: Value,
) -> Result<OpenAiTranscriptCompleted, OpenAiRealtimeParseError> {
    const MESSAGE_TYPE: &str = "conversation.item.input_audio_transcription.completed";
    let wire: WireTranscriptCompleted = parse_wire(value, MESSAGE_TYPE)?;
    let transcript = wire
        .transcript
        .ok_or(OpenAiRealtimeParseError::InvalidField {
            message_type: MESSAGE_TYPE,
            field: "transcript",
        })?;
    if transcript.len() > MAX_ACCUMULATED_TRANSCRIPT_BYTES {
        return Err(OpenAiRealtimeParseError::TranscriptTooLarge);
    }
    let mut languages = Vec::with_capacity(wire.languages.len());
    for language in wire.languages {
        let language = required_string(Some(language.code), MESSAGE_TYPE, "languages.code")?;
        if !languages.contains(&language) {
            languages.push(language);
        }
    }
    Ok(OpenAiTranscriptCompleted {
        event_id: valid_optional_string(wire.event_id, MESSAGE_TYPE, "event_id")?,
        item_id: required_string(wire.item_id, MESSAGE_TYPE, "item_id")?,
        content_index: wire
            .content_index
            .ok_or(OpenAiRealtimeParseError::InvalidField {
                message_type: MESSAGE_TYPE,
                field: "content_index",
            })?,
        transcript,
        languages,
    })
}

fn parse_transcript_failed(
    value: Value,
) -> Result<OpenAiTranscriptFailed, OpenAiRealtimeParseError> {
    const MESSAGE_TYPE: &str = "conversation.item.input_audio_transcription.failed";
    let wire: WireTranscriptFailed = parse_wire(value, MESSAGE_TYPE)?;
    Ok(OpenAiTranscriptFailed {
        event_id: valid_optional_string(wire.event_id, MESSAGE_TYPE, "event_id")?,
        item_id: required_string(wire.item_id, MESSAGE_TYPE, "item_id")?,
        content_index: wire
            .content_index
            .ok_or(OpenAiRealtimeParseError::InvalidField {
                message_type: MESSAGE_TYPE,
                field: "content_index",
            })?,
        error: provider_error(wire.error),
    })
}

fn provider_error(error: WireProviderError) -> OpenAiProviderError {
    OpenAiProviderError {
        error_type: error.error_type,
        code: error.code,
        message: error.message,
        param: error.param,
        related_event_id: error.event_id,
    }
}

fn parse_rate_limits(value: Value) -> Result<OpenAiRateLimitsUpdated, OpenAiRealtimeParseError> {
    const MESSAGE_TYPE: &str = "rate_limits.updated";
    let wire: WireRateLimitsUpdated = parse_wire(value, MESSAGE_TYPE)?;
    let mut rate_limits = Vec::with_capacity(wire.rate_limits.len());
    for rate_limit in wire.rate_limits {
        if rate_limit.name.trim().is_empty()
            || rate_limit.name.chars().count() > 128
            || rate_limit.name.chars().any(char::is_control)
        {
            return Err(OpenAiRealtimeParseError::InvalidField {
                message_type: MESSAGE_TYPE,
                field: "rate_limits.name",
            });
        }
        for (field, number) in [
            ("rate_limits.limit", rate_limit.limit),
            ("rate_limits.remaining", rate_limit.remaining),
            ("rate_limits.reset_seconds", rate_limit.reset_seconds),
        ] {
            if number.is_some_and(|number| !number.is_finite() || number < 0.0) {
                return Err(OpenAiRealtimeParseError::InvalidField {
                    message_type: MESSAGE_TYPE,
                    field,
                });
            }
        }
        rate_limits.push(OpenAiRateLimit {
            name: rate_limit.name,
            limit: rate_limit.limit,
            remaining: rate_limit.remaining,
            reset_seconds: rate_limit.reset_seconds,
        });
    }
    Ok(OpenAiRateLimitsUpdated {
        event_id: valid_optional_string(wire.event_id, MESSAGE_TYPE, "event_id")?,
        rate_limits,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiTranscriptDeltaUpdate {
    pub event: OpenAiTranscriptDelta,
    pub accumulated_transcript: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiTranscriptCompletedUpdate {
    pub event: OpenAiTranscriptCompleted,
    pub streamed_transcript: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiTranscriptFailedUpdate {
    pub event: OpenAiTranscriptFailed,
    pub streamed_transcript: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OpenAiRealtimeServerEvent {
    SessionCreated(OpenAiSessionLifecycle),
    SessionUpdated(OpenAiSessionLifecycle),
    TranscriptionSessionCreated(OpenAiSessionLifecycle),
    TranscriptionSessionUpdated(OpenAiSessionLifecycle),
    SpeechStarted(OpenAiSpeechStarted),
    SpeechStopped(OpenAiSpeechStopped),
    AudioBufferCommitted(OpenAiAudioBufferCommitted),
    AudioBufferCleared(OpenAiAudioBufferCleared),
    TranscriptDelta(OpenAiTranscriptDeltaUpdate),
    TranscriptCompleted(OpenAiTranscriptCompletedUpdate),
    TranscriptFailed(OpenAiTranscriptFailedUpdate),
    Error(OpenAiErrorEvent),
    RateLimitsUpdated(OpenAiRateLimitsUpdated),
    Unknown { message_type: String },
}

/// Correlates deltas by `(item_id, content_index)`. Completion ordering across
/// items is intentionally unconstrained, matching the provider contract.
#[derive(Debug, Clone, Default)]
pub struct OpenAiRealtimeEventDecoder {
    partials: HashMap<(String, u32), String>,
}

impl OpenAiRealtimeEventDecoder {
    pub fn decode(
        &mut self,
        message: OpenAiRealtimeMessage,
    ) -> Result<OpenAiRealtimeServerEvent, OpenAiRealtimeParseError> {
        Ok(match message {
            OpenAiRealtimeMessage::TranscriptDelta(event) => {
                let key = (event.item_id.clone(), event.content_index);
                let transcript = self.partials.entry(key).or_default();
                let next_len = transcript
                    .len()
                    .checked_add(event.delta.len())
                    .ok_or(OpenAiRealtimeParseError::TranscriptTooLarge)?;
                if next_len > MAX_ACCUMULATED_TRANSCRIPT_BYTES {
                    return Err(OpenAiRealtimeParseError::TranscriptTooLarge);
                }
                transcript.push_str(&event.delta);
                OpenAiRealtimeServerEvent::TranscriptDelta(OpenAiTranscriptDeltaUpdate {
                    event,
                    accumulated_transcript: transcript.clone(),
                })
            }
            OpenAiRealtimeMessage::TranscriptCompleted(event) => {
                let key = (event.item_id.clone(), event.content_index);
                let streamed_transcript = self.partials.remove(&key);
                OpenAiRealtimeServerEvent::TranscriptCompleted(OpenAiTranscriptCompletedUpdate {
                    event,
                    streamed_transcript,
                })
            }
            OpenAiRealtimeMessage::TranscriptFailed(event) => {
                let key = (event.item_id.clone(), event.content_index);
                let streamed_transcript = self.partials.remove(&key);
                OpenAiRealtimeServerEvent::TranscriptFailed(OpenAiTranscriptFailedUpdate {
                    event,
                    streamed_transcript,
                })
            }
            OpenAiRealtimeMessage::SessionCreated(event) => {
                OpenAiRealtimeServerEvent::SessionCreated(event)
            }
            OpenAiRealtimeMessage::SessionUpdated(event) => {
                OpenAiRealtimeServerEvent::SessionUpdated(event)
            }
            OpenAiRealtimeMessage::TranscriptionSessionCreated(event) => {
                OpenAiRealtimeServerEvent::TranscriptionSessionCreated(event)
            }
            OpenAiRealtimeMessage::TranscriptionSessionUpdated(event) => {
                OpenAiRealtimeServerEvent::TranscriptionSessionUpdated(event)
            }
            OpenAiRealtimeMessage::SpeechStarted(event) => {
                OpenAiRealtimeServerEvent::SpeechStarted(event)
            }
            OpenAiRealtimeMessage::SpeechStopped(event) => {
                OpenAiRealtimeServerEvent::SpeechStopped(event)
            }
            OpenAiRealtimeMessage::AudioBufferCommitted(event) => {
                OpenAiRealtimeServerEvent::AudioBufferCommitted(event)
            }
            OpenAiRealtimeMessage::AudioBufferCleared(event) => {
                OpenAiRealtimeServerEvent::AudioBufferCleared(event)
            }
            OpenAiRealtimeMessage::Error(event) => OpenAiRealtimeServerEvent::Error(event),
            OpenAiRealtimeMessage::RateLimitsUpdated(event) => {
                OpenAiRealtimeServerEvent::RateLimitsUpdated(event)
            }
            OpenAiRealtimeMessage::Unknown { message_type } => {
                OpenAiRealtimeServerEvent::Unknown { message_type }
            }
        })
    }

    pub fn decode_text(
        &mut self,
        input: &str,
    ) -> Result<OpenAiRealtimeServerEvent, OpenAiRealtimeParseError> {
        self.decode(parse_openai_realtime_message(input)?)
    }

    pub fn clear(&mut self) {
        self.partials.clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiRealtimeFailureStage {
    Connect,
    SendSession,
    SendAudio,
    SendControl,
    Receive,
    Close,
}

impl fmt::Display for OpenAiRealtimeFailureStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Connect => "connect",
            Self::SendSession => "send_session",
            Self::SendAudio => "send_audio",
            Self::SendControl => "send_control",
            Self::Receive => "receive",
            Self::Close => "close",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiRealtimeNetworkErrorKind {
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

impl fmt::Display for OpenAiRealtimeNetworkErrorKind {
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
pub enum OpenAiRealtimeError {
    #[error(transparent)]
    Configuration(#[from] OpenAiRealtimeConfigError),
    #[error(transparent)]
    Parse(#[from] OpenAiRealtimeParseError),
    #[error(transparent)]
    Protocol(#[from] StreamingProtocolError),
    #[error(transparent)]
    Pcm(#[from] PcmError),
    #[error("OpenAI Realtime JSON encoding failed")]
    Encode,
    #[error("OpenAI API key is missing")]
    MissingCredential,
    #[error("OpenAI API key cannot be used as an authorization header")]
    InvalidCredential,
    #[error("OpenAI Realtime PCM16 payload must contain complete 16-bit samples")]
    InvalidPcm16Payload,
    #[error("OpenAI Realtime audio payload cannot be empty")]
    EmptyAudioPayload,
    #[error("OpenAI Realtime sent a binary server message instead of JSON")]
    UnexpectedBinaryMessage,
    #[error("OpenAI Realtime WebSocket {stage} failed ({kind})")]
    Transport {
        stage: OpenAiRealtimeFailureStage,
        kind: OpenAiRealtimeNetworkErrorKind,
        #[source]
        source: WebSocketError,
    },
}

type OpenAiRealtimeSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One authenticated OpenAI Realtime WebSocket. The API key is consumed only
/// while constructing the handshake and is never retained by this value.
pub struct OpenAiRealtimeConnection {
    socket: OpenAiRealtimeSocket,
    decoder: OpenAiRealtimeEventDecoder,
    resampler: PersistentPcmResampler,
}

impl OpenAiRealtimeConnection {
    /// Establish a transcription WebSocket and immediately send the current
    /// GA `session.update` event. The first call to `recv` may yield either the
    /// provider's initial `session.created` or its `session.updated` response.
    pub async fn connect(
        options: &OpenAiRealtimeOptions,
        api_key: &str,
    ) -> Result<Self, OpenAiRealtimeError> {
        let url = build_openai_realtime_url(options)?;
        let request = authenticated_request(&url, api_key)?;
        let (socket, _) = connect_async(request)
            .await
            .map_err(|source| transport_error(OpenAiRealtimeFailureStage::Connect, source))?;
        let mut connection = Self {
            socket,
            decoder: OpenAiRealtimeEventDecoder::default(),
            resampler: PersistentPcmResampler::new(ProviderSampleRate::Hz24000),
        };
        connection.send_session_update(options).await?;
        Ok(connection)
    }

    pub async fn send_session_update(
        &mut self,
        options: &OpenAiRealtimeOptions,
    ) -> Result<(), OpenAiRealtimeError> {
        let event = encode_openai_session_update(options)?;
        self.socket
            .send(Message::text(event))
            .await
            .map_err(|source| transport_error(OpenAiRealtimeFailureStage::SendSession, source))
    }

    /// Validate one canonical 48 kHz mono frame, continuously downsample it to
    /// 24 kHz signed PCM16 little endian, Base64-encode it, and append it to
    /// the provider input buffer.
    pub async fn send_audio(
        &mut self,
        frame: &StreamingAudioFrame,
    ) -> Result<usize, OpenAiRealtimeError> {
        frame.validate()?;
        let bytes = self.resampler.process_pcm16_le(&frame.samples)?;
        self.send_pcm16_le(&bytes).await
    }

    /// Send already-normalized 24 kHz mono PCM16 little-endian bytes.
    pub async fn send_pcm16_le(&mut self, bytes: &[u8]) -> Result<usize, OpenAiRealtimeError> {
        if bytes.len() % 2 != 0 {
            return Err(OpenAiRealtimeError::InvalidPcm16Payload);
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        let event = encode_openai_audio_append(bytes)?;
        self.socket
            .send(Message::text(event))
            .await
            .map_err(|source| transport_error(OpenAiRealtimeFailureStage::SendAudio, source))?;
        Ok(bytes.len())
    }

    /// Commit or clear the provider buffer. Commit first flushes the
    /// resampler's incomplete 48 -> 24 kHz group. Clear also resets that local
    /// group so audio from before the clear cannot be averaged with later
    /// samples.
    pub async fn send_control(
        &mut self,
        control: OpenAiRealtimeControl,
    ) -> Result<(), OpenAiRealtimeError> {
        match control {
            OpenAiRealtimeControl::Commit => {
                let trailing = self.resampler.finish_pcm16_le()?;
                self.send_pcm16_le(&trailing).await?;
            }
            OpenAiRealtimeControl::Clear => self.resampler.reset(),
        }
        self.socket
            .send(Message::text(encode_openai_control(control)))
            .await
            .map_err(|source| transport_error(OpenAiRealtimeFailureStage::SendControl, source))
    }

    /// Send an application-scheduled WebSocket keepalive without attaching
    /// audio, session data, credentials, or any other business payload.
    pub async fn send_ping(&mut self) -> Result<(), OpenAiRealtimeError> {
        self.socket
            .send(empty_keepalive_ping())
            .await
            .map_err(|source| transport_error(OpenAiRealtimeFailureStage::SendControl, source))
    }

    /// Receive one decoded provider event. Ping/Pong frames are handled
    /// internally; `None` means the remote WebSocket closed normally.
    pub async fn recv(&mut self) -> Result<Option<OpenAiRealtimeServerEvent>, OpenAiRealtimeError> {
        loop {
            let Some(message) = self.socket.next().await else {
                return Ok(None);
            };
            let message = message
                .map_err(|source| transport_error(OpenAiRealtimeFailureStage::Receive, source))?;
            if message.is_text() {
                let text = message.to_text().map_err(|source| {
                    transport_error(OpenAiRealtimeFailureStage::Receive, source)
                })?;
                let parsed = parse_openai_realtime_message(text)?;
                return self.decoder.decode(parsed).map(Some).map_err(Into::into);
            }
            if message.is_ping() {
                self.socket
                    .send(Message::Pong(message.into_data()))
                    .await
                    .map_err(|source| {
                        transport_error(OpenAiRealtimeFailureStage::SendControl, source)
                    })?;
                continue;
            }
            if message.is_pong() {
                continue;
            }
            if message.is_close() {
                return Ok(None);
            }
            if message.is_binary() {
                return Err(OpenAiRealtimeError::UnexpectedBinaryMessage);
            }
        }
    }

    pub async fn close_websocket(&mut self) -> Result<(), OpenAiRealtimeError> {
        self.socket
            .close(None)
            .await
            .map_err(|source| transport_error(OpenAiRealtimeFailureStage::Close, source))
    }
}

fn empty_keepalive_ping() -> Message {
    Message::Ping(Vec::new())
}

fn authenticated_request(
    url: &Url,
    api_key: &str,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, OpenAiRealtimeError> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Err(OpenAiRealtimeError::MissingCredential);
    }
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|_| OpenAiRealtimeConfigError::InvalidEndpoint)?;
    let mut authorization = HeaderValue::from_str(&format!("Bearer {api_key}"))
        .map_err(|_| OpenAiRealtimeError::InvalidCredential)?;
    authorization.set_sensitive(true);
    request.headers_mut().insert(AUTHORIZATION, authorization);
    Ok(request)
}

fn transport_error(
    stage: OpenAiRealtimeFailureStage,
    source: WebSocketError,
) -> OpenAiRealtimeError {
    let kind = classify_network_error(&source);
    OpenAiRealtimeError::Transport {
        stage,
        kind,
        source,
    }
}

fn classify_network_error(error: &WebSocketError) -> OpenAiRealtimeNetworkErrorKind {
    match error {
        WebSocketError::Http(response) => match response.status().as_u16() {
            401 | 403 => OpenAiRealtimeNetworkErrorKind::Authentication,
            429 => OpenAiRealtimeNetworkErrorKind::RateLimited,
            500..=599 => OpenAiRealtimeNetworkErrorKind::ServiceUnavailable,
            _ => OpenAiRealtimeNetworkErrorKind::Rejected,
        },
        WebSocketError::Io(error) if error.kind() == io::ErrorKind::TimedOut => {
            OpenAiRealtimeNetworkErrorKind::Timeout
        }
        WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed => {
            OpenAiRealtimeNetworkErrorKind::Disconnected
        }
        WebSocketError::Tls(_) => OpenAiRealtimeNetworkErrorKind::Tls,
        WebSocketError::Protocol(_) | WebSocketError::Utf8 => {
            OpenAiRealtimeNetworkErrorKind::Protocol
        }
        _ => OpenAiRealtimeNetworkErrorKind::Other,
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

    fn decode_append_payload(event: &str) -> Vec<u8> {
        let value: Value = serde_json::from_str(event).unwrap();
        BASE64_STANDARD
            .decode(value["audio"].as_str().unwrap())
            .unwrap()
    }

    #[test]
    fn keepalive_ping_contains_no_payload() {
        let message = empty_keepalive_ping();
        assert!(message.is_ping());
        assert!(message.into_data().is_empty());
    }

    #[test]
    fn default_url_is_wss_transcription_only_and_credential_free() {
        let url = build_openai_realtime_url(&OpenAiRealtimeOptions::default()).unwrap();
        assert_eq!(url.scheme(), "wss");
        assert_eq!(url.host_str(), Some("api.openai.com"));
        assert_eq!(url.path(), "/v1/realtime");
        assert!(url.username().is_empty());
        assert!(url.password().is_none());
        let query = query_map(&url);
        assert_eq!(query["intent"], ["transcription"]);
        assert!(!query.contains_key("model"));
        assert!(!url.as_str().to_ascii_lowercase().contains("api_key"));
    }

    #[test]
    fn endpoint_override_retains_safe_query_but_rejects_credentials_and_managed_fields() {
        let options = OpenAiRealtimeOptions {
            endpoint_override: Some(
                "wss://gateway.example.test/realtime?api-version=2026-08-01".to_string(),
            ),
            ..OpenAiRealtimeOptions::default()
        };
        let query = query_map(&build_openai_realtime_url(&options).unwrap());
        assert_eq!(query["api-version"], ["2026-08-01"]);
        assert_eq!(query["intent"], ["transcription"]);

        for (endpoint, expected) in [
            (
                "wss://user:pass@api.openai.com/v1/realtime",
                OpenAiRealtimeConfigError::EndpointContainsUserInfo,
            ),
            (
                "wss://api.openai.com/v1/realtime?api_key=secret",
                OpenAiRealtimeConfigError::EndpointContainsCredential,
            ),
            (
                "wss://api.openai.com/v1/realtime?intent=realtime",
                OpenAiRealtimeConfigError::EndpointOverridesManagedParameter("intent".to_string()),
            ),
            (
                "wss://api.openai.com/v1/realtime?model=other",
                OpenAiRealtimeConfigError::EndpointOverridesManagedParameter("model".to_string()),
            ),
        ] {
            let options = OpenAiRealtimeOptions {
                endpoint_override: Some(endpoint.to_string()),
                ..OpenAiRealtimeOptions::default()
            };
            assert_eq!(build_openai_realtime_url(&options), Err(expected));
        }
    }

    #[test]
    fn session_update_uses_current_transcription_shape_and_exact_24khz_pcm() {
        let options = OpenAiRealtimeOptions {
            prompt: Some("A bilingual engineering meeting".to_string()),
            languages: vec!["JA".to_string(), "zh-cn".to_string()],
            keywords: vec!["Meetily".to_string(), "ASR".to_string()],
            delay: Some(OpenAiRealtimeDelay::Medium),
            turn_detection: OpenAiRealtimeTurnDetection::ServerVad {
                threshold: 0.45,
                prefix_padding_ms: 300,
                silence_duration_ms: 500,
            },
            ..OpenAiRealtimeOptions::default()
        };
        let value: Value =
            serde_json::from_str(&encode_openai_session_update(&options).unwrap()).unwrap();
        assert_eq!(value["type"], "session.update");
        assert_eq!(value["session"]["type"], "transcription");
        assert_eq!(
            value["session"]["audio"]["input"]["format"],
            json!({"type": "audio/pcm", "rate": 24_000})
        );
        let transcription = &value["session"]["audio"]["input"]["transcription"];
        assert_eq!(transcription["model"], "gpt-live-transcribe");
        assert_eq!(transcription["languages"], json!(["ja", "zh-cn"]));
        assert_eq!(transcription["keywords"], json!(["Meetily", "ASR"]));
        assert_eq!(transcription["delay"], "medium");
        assert_eq!(
            value["session"]["audio"]["input"]["turn_detection"]["type"],
            "server_vad"
        );
    }

    #[test]
    fn audio_append_is_base64_pcm16_and_resampling_is_continuous_across_frames() {
        let input = [1.0, 1.0, -1.0, -1.0, 0.5, 0.5];
        let mut one = PersistentPcmResampler::new(ProviderSampleRate::Hz24000);
        let expected = one.process_pcm16_le(&input).unwrap();

        let mut split = PersistentPcmResampler::new(ProviderSampleRate::Hz24000);
        let mut actual = split.process_pcm16_le(&input[..1]).unwrap();
        actual.extend(split.process_pcm16_le(&input[1..4]).unwrap());
        actual.extend(split.process_pcm16_le(&input[4..]).unwrap());
        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 6);

        let encoded = encode_openai_audio_append(&actual).unwrap();
        let value: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["type"], "input_audio_buffer.append");
        assert_eq!(decode_append_payload(&encoded), actual);
        assert!(matches!(
            encode_openai_audio_append(&[0]),
            Err(OpenAiRealtimeError::InvalidPcm16Payload)
        ));
    }

    #[test]
    fn authorization_is_bearer_header_only_sensitive_and_never_in_debug_output() {
        let secret = "test-secret-that-must-not-leak";
        let url = build_openai_realtime_url(&OpenAiRealtimeOptions::default()).unwrap();
        let request = authenticated_request(&url, secret).unwrap();
        assert_eq!(request.headers()[AUTHORIZATION], format!("Bearer {secret}"));
        assert!(request.headers()[AUTHORIZATION].is_sensitive());
        assert!(!request.uri().to_string().contains(secret));
        assert!(!format!("{request:?}").contains(secret));
    }

    #[test]
    fn parses_current_and_legacy_session_lifecycle_without_retaining_client_secret() {
        let current = parse_openai_realtime_message(
            r#"{"type":"session.created","event_id":"evt-1","session":{"id":"sess-1","object":"realtime.session","type":"transcription","audio":{"input":{"transcription":{"model":"gpt-live-transcribe"}}},"client_secret":{"value":"do-not-retain"}}}"#,
        )
        .unwrap();
        let OpenAiRealtimeMessage::SessionCreated(current) = current else {
            panic!("expected current session");
        };
        assert_eq!(current.session_type.as_deref(), Some("transcription"));
        assert_eq!(
            current.transcription_model.as_deref(),
            Some("gpt-live-transcribe")
        );
        assert!(!format!("{current:?}").contains("do-not-retain"));

        let legacy = parse_openai_realtime_message(
            r#"{"type":"transcription_session.updated","event_id":"evt-2","session":{"id":"sess-2","object":"realtime.transcription_session","input_audio_transcription":{"model":"gpt-transcribe"}}}"#,
        )
        .unwrap();
        let OpenAiRealtimeMessage::TranscriptionSessionUpdated(legacy) = legacy else {
            panic!("expected legacy transcription session");
        };
        assert_eq!(legacy.session_type.as_deref(), Some("transcription"));
        assert_eq!(
            legacy.transcription_model.as_deref(),
            Some("gpt-transcribe")
        );
    }

    #[test]
    fn delta_chains_are_correlated_per_item_and_completion_order_is_independent() {
        let mut decoder = OpenAiRealtimeEventDecoder::default();
        let OpenAiRealtimeServerEvent::TranscriptDelta(first_a) = decoder
            .decode_text(
                r#"{"type":"conversation.item.input_audio_transcription.delta","event_id":"d-1","item_id":"item-a","content_index":0,"delta":"Hel"}"#,
            )
            .unwrap()
        else {
            panic!("expected first delta");
        };
        assert_eq!(first_a.accumulated_transcript, "Hel");

        decoder
            .decode_text(
                r#"{"type":"conversation.item.input_audio_transcription.delta","event_id":"d-2","item_id":"item-b","content_index":0,"delta":"World"}"#,
            )
            .unwrap();
        let OpenAiRealtimeServerEvent::TranscriptDelta(second_a) = decoder
            .decode_text(
                r#"{"type":"conversation.item.input_audio_transcription.delta","event_id":"d-3","item_id":"item-a","content_index":0,"delta":"lo"}"#,
            )
            .unwrap()
        else {
            panic!("expected second delta");
        };
        assert_eq!(second_a.accumulated_transcript, "Hello");

        let OpenAiRealtimeServerEvent::TranscriptCompleted(completed_b) = decoder
            .decode_text(
                r#"{"type":"conversation.item.input_audio_transcription.completed","event_id":"c-1","item_id":"item-b","content_index":0,"transcript":"World","languages":[]}"#,
            )
            .unwrap()
        else {
            panic!("expected item-b completion");
        };
        assert_eq!(completed_b.streamed_transcript.as_deref(), Some("World"));

        let OpenAiRealtimeServerEvent::TranscriptCompleted(completed_a) = decoder
            .decode_text(
                r#"{"type":"conversation.item.input_audio_transcription.completed","event_id":"c-2","item_id":"item-a","content_index":0,"transcript":"Hello","languages":[{"code":"en"}]}"#,
            )
            .unwrap()
        else {
            panic!("expected item-a completion");
        };
        assert_eq!(completed_a.streamed_transcript.as_deref(), Some("Hello"));
        assert_eq!(completed_a.event.languages, ["en"]);
    }

    #[test]
    fn preserves_speech_commit_failure_error_and_rate_limit_identifiers() {
        let committed = parse_openai_realtime_message(
            r#"{"type":"input_audio_buffer.committed","event_id":"e-1","previous_item_id":"item-0","item_id":"item-1"}"#,
        )
        .unwrap();
        assert!(matches!(
            committed,
            OpenAiRealtimeMessage::AudioBufferCommitted(OpenAiAudioBufferCommitted {
                item_id,
                previous_item_id: Some(previous),
                ..
            }) if item_id == "item-1" && previous == "item-0"
        ));

        let started = parse_openai_realtime_message(
            r#"{"type":"input_audio_buffer.speech_started","event_id":"e-2","audio_start_ms":120,"item_id":"item-1"}"#,
        )
        .unwrap();
        assert!(matches!(
            started,
            OpenAiRealtimeMessage::SpeechStarted(OpenAiSpeechStarted {
                audio_start_ms: 120,
                ..
            })
        ));

        let failed = parse_openai_realtime_message(
            r#"{"type":"conversation.item.input_audio_transcription.failed","event_id":"e-3","item_id":"item-1","content_index":0,"error":{"type":"transcription_error","code":"audio_unintelligible","message":"Could not transcribe"}}"#,
        )
        .unwrap();
        assert!(matches!(
            failed,
            OpenAiRealtimeMessage::TranscriptFailed(OpenAiTranscriptFailed {
                item_id,
                content_index: 0,
                ..
            }) if item_id == "item-1"
        ));

        let limits = parse_openai_realtime_message(
            r#"{"type":"rate_limits.updated","event_id":"e-4","rate_limits":[{"name":"tokens","limit":1000,"remaining":750,"reset_seconds":1.5}]}"#,
        )
        .unwrap();
        let OpenAiRealtimeMessage::RateLimitsUpdated(limits) = limits else {
            panic!("expected rate limit event");
        };
        assert_eq!(limits.rate_limits[0].remaining, Some(750.0));
    }

    #[test]
    fn controls_match_official_client_event_names() {
        assert_eq!(
            encode_openai_control(OpenAiRealtimeControl::Commit),
            r#"{"type":"input_audio_buffer.commit"}"#
        );
        assert_eq!(
            encode_openai_control(OpenAiRealtimeControl::Clear),
            r#"{"type":"input_audio_buffer.clear"}"#
        );
    }
}
