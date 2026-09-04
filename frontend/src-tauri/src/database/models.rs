use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct MeetingModel {
    pub id: String,
    pub title: String,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
    pub folder_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type)]
#[sqlx(transparent)]
pub struct DateTimeUtc(pub DateTime<Utc>);

impl From<NaiveDateTime> for DateTimeUtc {
    fn from(naive: NaiveDateTime) -> Self {
        DateTimeUtc(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
    }
}

// Renamed from TranscriptSegment to Transcript to match the table name
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Transcript {
    pub id: String,
    pub meeting_id: String,
    pub transcript: String,
    pub timestamp: String,
    pub summary: Option<String>,
    pub action_items: Option<String>,
    pub key_points: Option<String>,
    // Recording-relative timestamps for audio-transcript synchronization
    pub audio_start_time: Option<f64>,
    pub audio_end_time: Option<f64>,
    pub duration: Option<f64>,
    /// Deprecated compatibility field. Older migrations used this column for
    /// `mic` / `system`, so it must not be interpreted as a human speaker.
    pub speaker: Option<String>,
    /// Stable logical identity shared by all partial/final/correction events.
    pub utterance_id: Option<String>,
    /// Latest materialized revision for `utterance_id`.
    pub revision: i64,
    pub latest_event_id: Option<String>,
    pub schema_version: i64,
    pub session_id: Option<String>,
    pub event_kind: String,
    pub is_stable: bool,
    pub sequence_id: Option<i64>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    /// Capture source (`mic`, `system`, `mixed`, `import`, ...).
    pub audio_source: Option<String>,
    /// Diarized human speaker identity, intentionally separate from source.
    pub speaker_id: Option<String>,
    pub speaker_local_label: Option<String>,
    pub speaker_display_name: Option<String>,
    pub speaker_confidence: Option<f64>,
    pub speaker_status: Option<String>,
    pub language: Option<String>,
    pub asr_provider: Option<String>,
    pub asr_model: Option<String>,
    pub asr_confidence: Option<f64>,
    pub asr_latency_ms: Option<i64>,
    pub diarization_provider: Option<String>,
    pub diarization_model: Option<String>,
    pub diarization_model_revision: Option<String>,
    pub diarization_revision: Option<i64>,
    pub diarization_window_id: Option<String>,
    pub diarization_window_start_frame: Option<i64>,
    pub diarization_window_end_frame: Option<i64>,
    pub diarization_status: Option<String>,
    pub diarization_latency_ms: Option<i64>,
    pub replaces_event_id: Option<String>,
    pub provider_event_id: Option<String>,
    pub trace_id: Option<String>,
    pub event_created_at: Option<DateTime<Utc>>,
    pub event_updated_at: Option<DateTime<Utc>>,
}

/// A normalized transcript event ready to be persisted.
///
/// Streaming providers should reuse `utterance_id`, start `revision` at zero,
/// and increment it whenever text, timing, stability, language, or speaker
/// attribution changes.
/// `event_id` identifies one delivered event; database idempotency is enforced
/// by `(meeting_id, utterance_id, revision)`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredTranscriptEvent {
    pub event_id: String,
    pub meeting_id: String,
    pub schema_version: i64,
    pub session_id: Option<String>,
    pub utterance_id: String,
    pub revision: i64,
    pub event_kind: String,
    pub is_stable: bool,
    pub text: String,
    pub timestamp: String,
    pub sequence_id: Option<i64>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub audio_start_time: Option<f64>,
    pub audio_end_time: Option<f64>,
    pub duration: Option<f64>,
    pub audio_source: Option<String>,
    pub speaker_id: Option<String>,
    pub speaker_local_label: Option<String>,
    pub speaker_display_name: Option<String>,
    pub speaker_confidence: Option<f64>,
    pub speaker_status: Option<String>,
    pub language: Option<String>,
    pub asr_provider: Option<String>,
    pub asr_model: Option<String>,
    pub asr_confidence: Option<f64>,
    pub asr_latency_ms: Option<i64>,
    pub diarization_provider: Option<String>,
    pub diarization_model: Option<String>,
    pub diarization_model_revision: Option<String>,
    pub diarization_revision: Option<i64>,
    pub diarization_window_id: Option<String>,
    pub diarization_window_start_frame: Option<i64>,
    pub diarization_window_end_frame: Option<i64>,
    pub diarization_status: Option<String>,
    pub diarization_latency_ms: Option<i64>,
    pub replaces_event_id: Option<String>,
    pub provider_event_id: Option<String>,
    pub trace_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// One immutable row from `utterance_revisions`.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize, PartialEq)]
pub struct UtteranceRevision {
    pub event_id: String,
    pub meeting_id: String,
    pub schema_version: i64,
    pub session_id: Option<String>,
    pub utterance_id: String,
    pub revision: i64,
    pub event_kind: String,
    pub is_stable: bool,
    pub transcript: String,
    pub timestamp: String,
    pub sequence_id: Option<i64>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub audio_start_time: Option<f64>,
    pub audio_end_time: Option<f64>,
    pub duration: Option<f64>,
    pub audio_source: Option<String>,
    pub speaker_id: Option<String>,
    pub speaker_local_label: Option<String>,
    pub speaker_display_name: Option<String>,
    pub speaker_confidence: Option<f64>,
    pub speaker_status: Option<String>,
    pub language: Option<String>,
    pub asr_provider: Option<String>,
    pub asr_model: Option<String>,
    pub asr_confidence: Option<f64>,
    pub asr_latency_ms: Option<i64>,
    pub diarization_provider: Option<String>,
    pub diarization_model: Option<String>,
    pub diarization_model_revision: Option<String>,
    pub diarization_revision: Option<i64>,
    pub diarization_window_id: Option<String>,
    pub diarization_window_start_frame: Option<i64>,
    pub diarization_window_end_frame: Option<i64>,
    pub diarization_status: Option<String>,
    pub diarization_latency_ms: Option<i64>,
    pub replaces_event_id: Option<String>,
    pub provider_event_id: Option<String>,
    pub trace_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct SummaryProcess {
    pub meeting_id: String,
    pub status: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub error: Option<String>,
    pub result: Option<String>, // JSON
    pub start_time: Option<chrono::DateTime<chrono::Utc>>,
    pub end_time: Option<chrono::DateTime<chrono::Utc>>,
    pub chunk_count: i64,
    pub processing_time: f64,
    pub metadata: Option<String>,      // JSON
    pub result_backup: Option<String>, // Backup of result before regeneration
    pub result_backup_timestamp: Option<chrono::DateTime<chrono::Utc>>, // When backup was created
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct TranscriptChunk {
    pub meeting_id: String,
    pub meeting_name: Option<String>,
    pub transcript_text: String,
    pub model: String,
    pub model_name: String,
    pub chunk_size: Option<i64>,
    pub overlap: Option<i64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Raw database row for summary-provider settings.
///
/// This type intentionally implements neither `Debug`, `Display` nor
/// `Serialize`: it contains credentials and must never cross IPC.
#[derive(Clone, FromRow)]
pub(crate) struct Setting {
    pub(crate) id: String,
    pub(crate) provider: String,
    pub(crate) model: String,
    #[sqlx(rename = "whisperModel")]
    pub(crate) whisper_model: String,
    #[sqlx(rename = "groqApiKey")]
    pub(crate) groq_api_key: Option<String>,
    #[sqlx(rename = "openaiApiKey")]
    pub(crate) openai_api_key: Option<String>,
    #[sqlx(rename = "anthropicApiKey")]
    pub(crate) anthropic_api_key: Option<String>,
    #[sqlx(rename = "ollamaApiKey")]
    pub(crate) ollama_api_key: Option<String>,
    #[sqlx(rename = "openRouterApiKey")]
    pub(crate) open_router_api_key: Option<String>,
    #[sqlx(rename = "ollamaEndpoint")]
    pub(crate) ollama_endpoint: Option<String>,
    /// Custom OpenAI-compatible endpoint configuration stored as JSON
    #[sqlx(rename = "customOpenAIConfig")]
    pub(crate) custom_openai_config: Option<String>,
}

/// Raw database row for transcription settings.
///
/// This type intentionally implements neither `Debug` nor `Serialize`: it
/// contains credentials and must never cross the Rust/WebView boundary.
#[derive(FromRow)]
pub struct TranscriptSetting {
    pub id: String,
    pub provider: String,
    pub model: String,
    #[sqlx(rename = "whisperApiKey")]
    pub whisper_api_key: Option<String>,
    #[sqlx(rename = "deepgramApiKey")]
    pub deepgram_api_key: Option<String>,
    #[sqlx(rename = "elevenLabsApiKey")]
    pub eleven_labs_api_key: Option<String>,
    #[sqlx(rename = "groqApiKey")]
    pub groq_api_key: Option<String>,
    #[sqlx(rename = "openaiApiKey")]
    pub openai_api_key: Option<String>,
    #[sqlx(rename = "streamingConfig")]
    pub streaming_config: Option<String>,
}

pub const TRANSCRIPT_STREAMING_CONFIG_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_DEEPGRAM_TRANSCRIPT_MODEL: &str = "nova-3";
pub const DEFAULT_OPENAI_TRANSCRIPT_MODEL: &str = "gpt-live-transcribe";

/// Providers exposed by the current transcription settings contract.
///
/// Parsing is deliberately explicit. Unknown values must be rejected instead
/// of silently becoming a local Whisper session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TranscriptProvider {
    #[serde(rename = "localWhisper")]
    LocalWhisper,
    #[serde(rename = "parakeet")]
    Parakeet,
    #[serde(rename = "deepgram")]
    Deepgram,
    #[serde(rename = "openai")]
    OpenAi,
}

impl TranscriptProvider {
    pub fn parse_request(value: &str) -> Result<Self, String> {
        match value {
            "localWhisper" => Ok(Self::LocalWhisper),
            "parakeet" => Ok(Self::Parakeet),
            "deepgram" => Ok(Self::Deepgram),
            "openai" => Ok(Self::OpenAi),
            _ => Err(format!("Unsupported transcription provider '{value}'")),
        }
    }

    /// Parse values previously written by released clients.
    ///
    /// Retired cloud providers are migrated to the reliable local fallback;
    /// arbitrary values still fail closed.
    pub fn parse_stored(value: &str) -> Result<Self, String> {
        match value {
            "localWhisper" | "local-whisper" | "whisper" => Ok(Self::LocalWhisper),
            "parakeet" => Ok(Self::Parakeet),
            "deepgram" => Ok(Self::Deepgram),
            "openai" => Ok(Self::OpenAi),
            "elevenLabs" | "elevenlabs" | "groq" => Ok(Self::Parakeet),
            _ => Err(format!(
                "Stored transcription provider '{value}' is not recognized"
            )),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalWhisper => "localWhisper",
            Self::Parakeet => "parakeet",
            Self::Deepgram => "deepgram",
            Self::OpenAi => "openai",
        }
    }

    pub const fn requires_api_key(self) -> bool {
        matches!(self, Self::Deepgram | Self::OpenAi)
    }

    pub const fn default_model(self) -> &'static str {
        match self {
            Self::LocalWhisper => crate::config::DEFAULT_WHISPER_MODEL,
            Self::Parakeet => crate::config::DEFAULT_PARAKEET_MODEL,
            Self::Deepgram => DEFAULT_DEEPGRAM_TRANSCRIPT_MODEL,
            Self::OpenAi => DEFAULT_OPENAI_TRANSCRIPT_MODEL,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamingLatencyMode {
    Minimal,
    Low,
    Balanced,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamingProviderConfig {
    pub endpoint_override: Option<String>,
    pub model: String,
    pub language: String,
    pub diarization: bool,
    pub latency_mode: StreamingLatencyMode,
    pub keywords: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamingProviderConfigs {
    pub deepgram: StreamingProviderConfig,
    pub openai: StreamingProviderConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TranscriptFallbackProvider {
    #[serde(rename = "parakeet")]
    Parakeet,
    #[serde(rename = "localWhisper")]
    LocalWhisper,
}

impl TranscriptFallbackProvider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Parakeet => "parakeet",
            Self::LocalWhisper => "localWhisper",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptFallbackConfig {
    pub enabled: bool,
    pub provider: TranscriptFallbackProvider,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptStreamingConfig {
    #[serde(default = "default_transcript_streaming_schema_version")]
    pub schema_version: u32,
    pub providers: StreamingProviderConfigs,
    pub fallback: TranscriptFallbackConfig,
}

const fn default_transcript_streaming_schema_version() -> u32 {
    TRANSCRIPT_STREAMING_CONFIG_SCHEMA_VERSION
}

impl Default for TranscriptStreamingConfig {
    fn default() -> Self {
        Self {
            schema_version: TRANSCRIPT_STREAMING_CONFIG_SCHEMA_VERSION,
            providers: StreamingProviderConfigs {
                deepgram: StreamingProviderConfig {
                    endpoint_override: None,
                    model: DEFAULT_DEEPGRAM_TRANSCRIPT_MODEL.to_string(),
                    language: "auto".to_string(),
                    diarization: true,
                    latency_mode: StreamingLatencyMode::Balanced,
                    keywords: Vec::new(),
                },
                openai: StreamingProviderConfig {
                    endpoint_override: None,
                    model: DEFAULT_OPENAI_TRANSCRIPT_MODEL.to_string(),
                    language: "auto".to_string(),
                    diarization: false,
                    latency_mode: StreamingLatencyMode::Low,
                    keywords: Vec::new(),
                },
            },
            fallback: TranscriptFallbackConfig {
                enabled: true,
                provider: TranscriptFallbackProvider::Parakeet,
                model: crate::config::DEFAULT_PARAKEET_MODEL.to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptApiKeyConfigured {
    pub deepgram: bool,
    pub openai: bool,
}

/// Safe public view returned to the WebView. It carries key-presence flags but
/// never the credential itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptConfigView {
    pub provider: String,
    pub model: String,
    pub streaming_config: TranscriptStreamingConfig,
    pub has_api_key: bool,
    pub api_key_configured: TranscriptApiKeyConfigured,
}

impl Default for TranscriptConfigView {
    fn default() -> Self {
        Self {
            provider: TranscriptProvider::Parakeet.as_str().to_string(),
            model: crate::config::DEFAULT_PARAKEET_MODEL.to_string(),
            streaming_config: TranscriptStreamingConfig::default(),
            has_api_key: false,
            api_key_configured: TranscriptApiKeyConfigured::default(),
        }
    }
}
