use log::{debug as log_debug, error as log_error, info as log_info, warn as log_warn};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Runtime};
use tauri_plugin_store::StoreExt;
use url::{Host, Url};

use crate::{
    audio::transcription::streaming::{
        DeepgramConnection, DeepgramControl, DeepgramError, DeepgramNetworkErrorKind,
        DeepgramOptions, OpenAiRealtimeConnection, OpenAiRealtimeDelay, OpenAiRealtimeError,
        OpenAiRealtimeNetworkErrorKind, OpenAiRealtimeOptions, OpenAiRealtimeServerEvent,
        OpenAiRealtimeTurnDetection,
    },
    database::{
        models::{
            MeetingModel, StreamingLatencyMode, StreamingProviderConfig, TranscriptConfigView,
            TranscriptProvider, TranscriptStreamingConfig,
        },
        repositories::{
            meeting::MeetingsRepository,
            recording_session_binding::RecordingSessionBindingRepository,
            setting::{
                normalize_summary_api_key, normalize_transcript_api_key,
                validate_and_normalize_streaming_config, SettingsRepository,
                SummaryApiKeyConfigured, SummaryApiKeyProvider, TranscriptApiKey,
            },
            transcript::{TranscriptsRepository, TrustedRecordingSaveBinding},
        },
    },
    state::AppState,
    summary::{CustomOpenAIConfig, CustomOpenAIConfigView},
};

// Hardcoded server URL
const APP_SERVER_URL: &str = "http://localhost:5167";

#[derive(Debug, Serialize, Deserialize)]
pub struct ApiResponse<T> {
    pub success: bool,
    pub data: Option<T>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Meeting {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TranscriptSearchResult {
    pub id: String,
    pub title: String,
    #[serde(rename = "matchContext")]
    pub match_context: String,
    pub timestamp: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProfileRequest {
    pub email: String,
    pub license_key: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SaveProfileRequest {
    pub id: String,
    pub email: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateProfileRequest {
    pub email: String,
    pub license_key: String,
    pub company: String,
    pub position: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
    pub whisper_model: String,
    pub has_api_key: bool,
    pub api_key_configured: SummaryApiKeyConfigured,
    pub ollama_endpoint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryApiKeyMutationResult {
    pub provider: String,
    pub has_api_key: bool,
    pub api_key_configured: SummaryApiKeyConfigured,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryConnectionTestResult {
    pub ok: bool,
    pub code: &'static str,
    pub message: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
}

impl SummaryConnectionTestResult {
    fn success(http_status: u16) -> Self {
        Self {
            ok: true,
            code: "ok",
            message: "连接成功，服务返回了兼容的响应。",
            http_status: Some(http_status),
        }
    }

    fn failure(code: &'static str, message: &'static str, http_status: Option<u16>) -> Self {
        Self {
            ok: false,
            code,
            message,
            http_status,
        }
    }
}

/// Backwards-compatible Rust name for the safe public settings projection.
/// This alias contains key-presence flags only, never the credential.
pub type TranscriptConfig = TranscriptConfigView;

const TRANSCRIPT_CONNECTION_TEST_TIMEOUT: Duration = Duration::from_secs(8);
const TRANSCRIPT_CONNECTION_READY_TIMEOUT: Duration = Duration::from_secs(3);

/// Stable, secret-free result returned by the streaming transcription
/// connection probe. Expected validation and network failures are represented
/// as `ok: false` instead of crossing IPC as opaque command errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptConnectionTestResult {
    pub ok: bool,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

impl TranscriptConnectionTestResult {
    fn success(latency_ms: u64) -> Self {
        Self {
            ok: true,
            code: "connected".to_string(),
            message: "连接成功，在线转写服务已接受安全握手。".to_string(),
            latency_ms: Some(latency_ms),
        }
    }

    fn failure(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            code: code.into(),
            message: message.into(),
            latency_ms: None,
        }
    }
}

enum TranscriptConnectionCredential {
    Temporary(String),
    Stored(TranscriptApiKey),
}

impl TranscriptConnectionCredential {
    fn expose_secret(&self) -> &str {
        match self {
            Self::Temporary(value) => value,
            Self::Stored(value) => value.expose_secret(),
        }
    }
}

enum TranscriptConnectionOptions {
    Deepgram(DeepgramOptions),
    OpenAi(OpenAiRealtimeOptions),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeleteMeetingRequest {
    pub meeting_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MeetingDetails {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub transcripts: Vec<MeetingTranscript>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MeetingTranscript {
    pub id: String,
    pub text: String,
    pub timestamp: String,
    // Recording-relative timestamps for audio-transcript synchronization
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_start_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_end_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    pub schema_version: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utterance_id: Option<String>,
    pub revision: i64,
    pub event_kind: String,
    pub is_stable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speaker_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speaker_local_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speaker_display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speaker_confidence: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speaker_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_confidence: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_latency_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diarization_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diarization_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diarization_model_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diarization_revision: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diarization_window_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diarization_window_start_frame: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diarization_window_end_frame: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diarization_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diarization_latency_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaces_event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

/// Meeting metadata without transcripts (for pagination)
#[derive(Debug, Serialize, Deserialize)]
pub struct MeetingMetadata {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_path: Option<String>,
}

/// Paginated transcripts response with total count
#[derive(Debug, Serialize, Deserialize)]
pub struct PaginatedTranscriptsResponse {
    pub transcripts: Vec<MeetingTranscript>,
    pub total_count: i64,
    pub has_more: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SaveMeetingTitleRequest {
    pub meeting_id: String,
    pub title: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SaveMeetingSummaryRequest {
    pub meeting_id: String,
    pub summary: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SaveTranscriptRequest {
    pub meeting_title: String,
    pub transcripts: Vec<TranscriptSegment>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub id: String,
    pub text: String,
    pub timestamp: String,
    // Legacy ordering and provider fields. They stay optional so meetings
    // recorded before the revisioned event protocol remain importable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_start_time: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_partial: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    // NEW: Recording-relative timestamps for playback synchronization
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_start_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_end_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    // Revisioned transcript event identity. These fields let online streaming
    // providers replace partial hypotheses without appending duplicate text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utterance_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_stable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ms: Option<i64>,
    // Audio source and speaker identity deliberately use separate fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_local_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_confidence: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    // Flattened ASR metadata is retained alongside the live event's nested
    // metadata so SQLite import does not depend on provider-specific JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_confidence: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_latency_ms: Option<i64>,
    // Diarization provenance is independent from recognition provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_model_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_revision: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_window_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_window_start_frame: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_window_end_frame: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_latency_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: Option<String>,
    pub email: String,
    pub license_key: String,
    pub company: Option<String>,
    pub position: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub is_licensed: bool,
}

// Helper function to get auth token from store (optional)
#[allow(dead_code)]
async fn get_auth_token<R: Runtime>(app: &AppHandle<R>) -> Option<String> {
    let store_path = crate::storage::store_path(app, "store.json").ok()?;
    let store = match app.store(store_path) {
        Ok(store) => store,
        Err(_) => return None,
    };

    match store.get("authToken") {
        Some(token) => {
            if let Some(token_str) = token.as_str() {
                let truncated = token_str.chars().take(20).collect::<String>();
                log_info!("Found auth token: {}", truncated);
                Some(token_str.to_string())
            } else {
                log_warn!("Auth token is not a string");
                None
            }
        }
        None => {
            log_warn!("No auth token found in store");
            None
        }
    }
}

// Helper function to get server address - now hardcoded
async fn get_server_address<R: Runtime>(_app: &AppHandle<R>) -> Result<String, String> {
    log_info!("Using hardcoded server URL: {}", APP_SERVER_URL);
    Ok(APP_SERVER_URL.to_string())
}

// Generic API call function with optional authentication
async fn make_api_request<R: Runtime, T: for<'de> Deserialize<'de>>(
    app: &AppHandle<R>,
    endpoint: &str,
    method: &str,
    body: Option<&str>,
    additional_headers: Option<HashMap<String, String>>,
    auth_token: Option<String>, // Pass auth token from frontend
) -> Result<T, String> {
    let client = reqwest::Client::new();
    let server_url = get_server_address(app).await?;

    let url = format!("{}{}", server_url, endpoint);
    log_info!("Making {} request to: {}", method, url);

    let mut request = match method.to_uppercase().as_str() {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        "PUT" => client.put(&url),
        "DELETE" => client.delete(&url),
        _ => return Err(format!("Unsupported HTTP method: {}", method)),
    };

    // Add authorization header if auth token is provided
    if let Some(token) = auth_token {
        log_info!("Adding authorization header");
        request = request.header("Authorization", format!("Bearer {}", token));
    } else {
        log_warn!("No auth token provided, making unauthenticated request");
    }

    request = request.header("Content-Type", "application/json");

    // Add additional headers if provided
    if let Some(headers) = additional_headers {
        for (key, value) in headers {
            request = request.header(&key, &value);
        }
    }

    // Add body if provided
    if let Some(body_str) = body {
        request = request.body(body_str.to_string());
    }

    let response = request.send().await.map_err(|e| {
        let error_msg = format!("Request failed: {}", e);
        log_error!("{}", error_msg);
        error_msg
    })?;

    let status = response.status();
    log_info!("Response status: {}", status);

    if !status.is_success() {
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        let error_msg = format!("HTTP {}: {}", status, error_text);
        log_error!("{}", error_msg);
        return Err(error_msg);
    }

    let response_text = response.text().await.map_err(|e| {
        let error_msg = format!("Failed to read response: {}", e);
        log_error!("{}", error_msg);
        error_msg
    })?;

    // Safely truncate response for logging, respecting UTF-8 character boundaries
    let truncated = response_text.chars().take(200).collect::<String>();
    log_info!("Response body: {}", truncated);

    serde_json::from_str(&response_text).map_err(|e| {
        let error_msg = format!("Failed to parse JSON: {}", e);
        log_error!("{}", error_msg);
        error_msg
    })
}

// API Commands for Tauri

#[tauri::command]
pub async fn api_get_meetings<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    auth_token: Option<String>,
) -> Result<Vec<Meeting>, String> {
    log_info!(
        "api_get_meetings called with auth_token(native) : {}",
        auth_token.is_some()
    );
    let pool = state.db_manager.pool();
    let meetings: Result<Vec<MeetingModel>, sqlx::Error> =
        MeetingsRepository::get_meetings(pool).await;

    match meetings {
        Ok(meeting_models) => {
            log_info!("Successfully got {} meetings", meeting_models.len());

            let result: Vec<Meeting> = meeting_models
                .into_iter()
                .map(|m| Meeting {
                    id: m.id,
                    title: m.title,
                })
                .collect();
            Ok(result)
        }
        Err(e) => {
            log_error!("Error getting meetings: {}", e);
            Err(e.to_string())
        }
    }
}

#[tauri::command]
pub async fn api_search_transcripts<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    query: String,
    auth_token: Option<String>,
) -> Result<Vec<TranscriptSearchResult>, String> {
    log_info!(
        "api_search_transcripts called with query: '{}', auth_token: {}",
        query,
        auth_token.is_some()
    );

    let pool = state.db_manager.pool();

    match TranscriptsRepository::search_transcripts(pool, &query).await {
        Ok(results) => {
            log_info!(
                "Search completed successfully with {} results.",
                results.len()
            );
            Ok(results)
        }
        Err(e) => {
            log_error!(
                "Error searching transcripts (query_characters={}): {}",
                query.chars().count(),
                e
            );
            Err(format!("Failed to search transcripts: {}", e))
        }
    }
}

#[tauri::command]
pub async fn api_get_profile<R: Runtime>(
    app: AppHandle<R>,
    email: String,
    license_key: String,
    auth_token: Option<String>,
) -> Result<Profile, String> {
    log_info!(
        "api_get_profile called for email: {}, auth_token: {}",
        email,
        auth_token.is_some()
    );

    let profile_request = ProfileRequest { email, license_key };
    let body = serde_json::to_string(&profile_request).map_err(|e| e.to_string())?;

    make_api_request::<R, Profile>(&app, "/get-profile", "POST", Some(&body), None, auth_token)
        .await
}

#[tauri::command]
pub async fn api_save_profile<R: Runtime>(
    app: AppHandle<R>,
    id: String,
    email: String,
    auth_token: Option<String>,
) -> Result<serde_json::Value, String> {
    log_info!(
        "api_save_profile called for email: {}, auth_token: {}",
        email,
        auth_token.is_some()
    );

    let save_request = SaveProfileRequest { id, email };
    let body = serde_json::to_string(&save_request).map_err(|e| e.to_string())?;

    make_api_request::<R, serde_json::Value>(
        &app,
        "/save-profile",
        "POST",
        Some(&body),
        None,
        auth_token,
    )
    .await
}

#[tauri::command]
pub async fn api_update_profile<R: Runtime>(
    app: AppHandle<R>,
    email: String,
    license_key: String,
    company: String,
    position: String,
    auth_token: Option<String>,
) -> Result<serde_json::Value, String> {
    log_info!(
        "api_update_profile called for email: {}, auth_token: {}",
        email,
        auth_token.is_some()
    );

    let update_request = UpdateProfileRequest {
        email,
        license_key,
        company,
        position,
    };
    let body = serde_json::to_string(&update_request).map_err(|e| e.to_string())?;

    make_api_request::<R, serde_json::Value>(
        &app,
        "/update-profile",
        "POST",
        Some(&body),
        None,
        auth_token,
    )
    .await
}

#[tauri::command]
pub async fn api_get_model_config<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    _auth_token: Option<String>,
) -> Result<Option<ModelConfig>, String> {
    log_info!("api_get_model_config called (native)");
    let pool = state.db_manager.pool();

    match SettingsRepository::get_model_config(pool).await {
        Ok(Some(config)) => {
            log_info!(
                "✅ Found model config in database: provider={}, model={}, whisperModel={}, ollamaEndpoint={:?}",
                &config.provider,
                &config.model,
                &config.whisper_model,
                &config.ollama_endpoint
            );
            let configured = SettingsRepository::get_summary_api_key_configured(pool)
                .await
                .map_err(|error| {
                    log_error!("Failed to read summary key presence flags: {}", error);
                    "无法读取总结模型配置，请稍后重试。".to_string()
                })?;
            let has_api_key = configured.for_provider(&config.provider);
            Ok(Some(ModelConfig {
                provider: config.provider,
                model: config.model,
                whisper_model: config.whisper_model,
                has_api_key,
                api_key_configured: configured,
                ollama_endpoint: config.ollama_endpoint,
            }))
        }
        Ok(None) => {
            log_warn!("⚠️ No model config found in database - database may be empty or settings table not initialized");
            Ok(None)
        }
        Err(e) => {
            log_error!("❌ Failed to get model config from database: {}", e);
            Err("无法读取总结模型配置，请稍后重试。".to_string())
        }
    }
}

#[tauri::command]
pub async fn api_save_model_config<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    provider: String,
    model: String,
    whisper_model: String,
    ollama_endpoint: Option<String>,
    _auth_token: Option<String>,
) -> Result<serde_json::Value, String> {
    log_info!(
        "💾 api_save_model_config called (native): provider='{}', model='{}', whisperModel='{}', ollamaEndpoint={:?}",
        &provider,
        &model,
        &whisper_model,
        &ollama_endpoint
    );
    let pool = state.db_manager.pool();

    if let Err(e) = SettingsRepository::save_model_config(
        pool,
        &provider,
        &model,
        &whisper_model,
        ollama_endpoint.as_deref(),
    )
    .await
    {
        log_error!("❌ Failed to save model config to database: {}", e);
        return Err("无法保存总结模型配置，请稍后重试。".to_string());
    }

    // Trigger graceful shutdown of built-in AI sidecar if it's running
    // This ensures that if the user switched models/providers, the old one is cleaned up
    // The shutdown happens in the background, so it won't block the UI
    if let Err(e) = crate::summary::summary_engine::client::shutdown_sidecar_gracefully().await {
        log_warn!("Failed to initiate graceful sidecar shutdown: {}", e);
    }

    log_info!("✅ Successfully saved model configuration to database");
    Ok(
        serde_json::json!({ "status": "success", "message": "Model configuration saved successfully" }),
    )
}

#[tauri::command]
pub async fn api_set_summary_api_key<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    provider: String,
    api_key: String,
    _auth_token: Option<String>,
) -> Result<SummaryApiKeyMutationResult, String> {
    let provider = SummaryApiKeyProvider::parse(&provider)?;
    let normalized = normalize_summary_api_key(&api_key)?;
    SettingsRepository::set_summary_api_key(state.db_manager.pool(), provider, &normalized)
        .await
        .map_err(|error| {
            log_error!("Failed to replace summary API key: {}", error);
            "无法保存 API 密钥，请检查配置后重试。".to_string()
        })?;
    summary_key_mutation_result(state.db_manager.pool(), provider).await
}

#[tauri::command]
pub async fn api_delete_summary_api_key<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    provider: String,
    _auth_token: Option<String>,
) -> Result<SummaryApiKeyMutationResult, String> {
    let provider = SummaryApiKeyProvider::parse(&provider)?;
    SettingsRepository::delete_summary_api_key(state.db_manager.pool(), provider)
        .await
        .map_err(|error| {
            log_error!("Failed to delete summary API key: {}", error);
            "无法删除 API 密钥，请稍后重试。".to_string()
        })?;
    summary_key_mutation_result(state.db_manager.pool(), provider).await
}

async fn summary_key_mutation_result(
    pool: &sqlx::SqlitePool,
    provider: SummaryApiKeyProvider,
) -> Result<SummaryApiKeyMutationResult, String> {
    let configured = SettingsRepository::get_summary_api_key_configured(pool)
        .await
        .map_err(|error| {
            log_error!("Failed to read summary key presence flags: {}", error);
            "无法读取 API 密钥状态，请稍后重试。".to_string()
        })?;
    Ok(SummaryApiKeyMutationResult {
        provider: provider.as_str().to_string(),
        has_api_key: configured.for_provider(provider.as_str()),
        api_key_configured: configured,
    })
}

#[tauri::command]
pub async fn api_get_transcript_config<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    _auth_token: Option<String>,
) -> Result<Option<TranscriptConfig>, String> {
    log_info!("api_get_transcript_config called (native)");
    let pool = state.db_manager.pool();

    match SettingsRepository::get_transcript_config_view(pool).await {
        Ok(config) => {
            log_info!(
                "Found safe transcript config view: provider={}, model={}, has_api_key={}",
                &config.provider,
                &config.model,
                config.has_api_key
            );
            Ok(Some(config))
        }
        Err(e) => {
            log_error!("Failed to get transcript config: {}", e);
            Err(e.to_string())
        }
    }
}

#[tauri::command]
pub async fn api_save_transcript_config<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    provider: String,
    model: String,
    streaming_config: Option<TranscriptStreamingConfig>,
    _auth_token: Option<String>,
) -> Result<serde_json::Value, String> {
    log_info!(
        "api_save_transcript_config called (native) for provider '{}'",
        &provider
    );
    let pool = state.db_manager.pool();

    let parsed_provider = TranscriptProvider::parse_request(&provider)?;
    let canonical_model = if model.trim().is_empty() {
        parsed_provider.default_model().to_string()
    } else {
        model.trim().to_string()
    };
    let streaming_config_json = match streaming_config {
        Some(mut config) => {
            match parsed_provider {
                TranscriptProvider::Deepgram => {
                    config.providers.deepgram.model = canonical_model.clone()
                }
                TranscriptProvider::OpenAi => {
                    config.providers.openai.model = canonical_model.clone()
                }
                TranscriptProvider::LocalWhisper | TranscriptProvider::Parakeet => {}
            }
            let config = validate_and_normalize_streaming_config(config)?;
            Some(
                serde_json::to_string(&config)
                    .map_err(|_| "Failed to encode streaming transcription config".to_string())?,
            )
        }
        None => None,
    };

    SettingsRepository::save_transcript_config_with_streaming(
        pool,
        parsed_provider.as_str(),
        &canonical_model,
        streaming_config_json.as_deref(),
    )
    .await
    .map_err(|error| {
        log_error!("Failed to save transcript config: {}", error);
        error.to_string()
    })?;

    log_info!("Successfully saved transcript configuration.");
    Ok(
        serde_json::json!({ "status": "success", "message": "Transcript configuration saved successfully" }),
    )
}

#[tauri::command]
pub async fn api_set_transcript_api_key<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    provider: String,
    api_key: String,
    _auth_token: Option<String>,
) -> Result<serde_json::Value, String> {
    let provider = TranscriptProvider::parse_request(&provider)?;
    if !provider.requires_api_key() {
        return Err(format!(
            "Provider '{}' does not accept an API key",
            provider.as_str()
        ));
    }
    let api_key = normalize_transcript_api_key(&api_key)?;
    SettingsRepository::set_transcript_api_key(state.db_manager.pool(), provider, &api_key)
        .await
        .map_err(|error| {
            log_error!(
                "Failed to save transcript API key for provider '{}': {}",
                provider.as_str(),
                error
            );
            error.to_string()
        })?;
    log_info!(
        "Saved transcript API key for provider '{}' without exposing it to the WebView.",
        provider.as_str()
    );
    Ok(serde_json::json!({ "hasApiKey": true }))
}

#[tauri::command]
pub async fn api_delete_transcript_api_key<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    provider: String,
    _auth_token: Option<String>,
) -> Result<serde_json::Value, String> {
    let provider = TranscriptProvider::parse_request(&provider)?;
    if !provider.requires_api_key() {
        return Err(format!(
            "Provider '{}' does not accept an API key",
            provider.as_str()
        ));
    }
    SettingsRepository::delete_transcript_api_key(state.db_manager.pool(), provider)
        .await
        .map_err(|error| {
            log_error!(
                "Failed to delete transcript API key for provider '{}': {}",
                provider.as_str(),
                error
            );
            error.to_string()
        })?;
    log_info!(
        "Deleted transcript API key for provider '{}'.",
        provider.as_str()
    );
    Ok(serde_json::json!({ "hasApiKey": false }))
}

fn deepgram_connection_failure(error: DeepgramError) -> TranscriptConnectionTestResult {
    let (code, message) = match error {
        DeepgramError::Configuration(_) => (
            "invalid_config",
            "Deepgram 连接配置无效，请检查模型、语言和 WSS 地址。",
        ),
        DeepgramError::MissingCredential | DeepgramError::InvalidCredential => {
            ("invalid_api_key", "Deepgram API 密钥为空或格式无效。")
        }
        DeepgramError::Transport { kind, .. } => match kind {
            DeepgramNetworkErrorKind::Authentication => (
                "authentication_failed",
                "Deepgram 身份验证失败，请检查 API 密钥。",
            ),
            DeepgramNetworkErrorKind::RateLimited => {
                ("rate_limited", "Deepgram 当前触发限流，请稍后再试。")
            }
            DeepgramNetworkErrorKind::ServiceUnavailable => (
                "service_unavailable",
                "Deepgram 服务暂时不可用，请稍后再试。",
            ),
            DeepgramNetworkErrorKind::Timeout => {
                ("timeout", "连接 Deepgram 超时，请检查网络或代理设置。")
            }
            DeepgramNetworkErrorKind::Tls => (
                "tls_error",
                "Deepgram TLS 安全连接失败，请检查系统时间、代理或证书。",
            ),
            DeepgramNetworkErrorKind::Protocol | DeepgramNetworkErrorKind::Rejected => (
                "connection_rejected",
                "Deepgram 拒绝了 WebSocket 连接，请检查服务地址和配置。",
            ),
            DeepgramNetworkErrorKind::Disconnected => {
                ("disconnected", "Deepgram 在握手期间断开了连接。")
            }
            DeepgramNetworkErrorKind::Other => (
                "connection_failed",
                "无法连接 Deepgram，请检查网络和服务配置。",
            ),
        },
        DeepgramError::Parse(_)
        | DeepgramError::Protocol(_)
        | DeepgramError::Pcm(_)
        | DeepgramError::MissingConnectionId
        | DeepgramError::UtteranceIndexOverflow
        | DeepgramError::InvalidPcm16Payload
        | DeepgramError::UnexpectedBinaryMessage => (
            "provider_protocol_error",
            "Deepgram 返回了无法处理的协议消息。",
        ),
    };
    TranscriptConnectionTestResult::failure(code, message)
}

fn openai_connection_failure(error: OpenAiRealtimeError) -> TranscriptConnectionTestResult {
    let (code, message) = match error {
        OpenAiRealtimeError::Configuration(_) => (
            "invalid_config",
            "OpenAI Realtime 连接配置无效，请检查模型、语言和 WSS 地址。",
        ),
        OpenAiRealtimeError::MissingCredential | OpenAiRealtimeError::InvalidCredential => {
            ("invalid_api_key", "OpenAI API 密钥为空或格式无效。")
        }
        OpenAiRealtimeError::Transport { kind, .. } => match kind {
            OpenAiRealtimeNetworkErrorKind::Authentication => (
                "authentication_failed",
                "OpenAI 身份验证失败，请检查 API 密钥。",
            ),
            OpenAiRealtimeNetworkErrorKind::RateLimited => {
                ("rate_limited", "OpenAI 当前触发限流，请稍后再试。")
            }
            OpenAiRealtimeNetworkErrorKind::ServiceUnavailable => (
                "service_unavailable",
                "OpenAI Realtime 服务暂时不可用，请稍后再试。",
            ),
            OpenAiRealtimeNetworkErrorKind::Timeout => (
                "timeout",
                "连接 OpenAI Realtime 超时，请检查网络或代理设置。",
            ),
            OpenAiRealtimeNetworkErrorKind::Tls => (
                "tls_error",
                "OpenAI TLS 安全连接失败，请检查系统时间、代理或证书。",
            ),
            OpenAiRealtimeNetworkErrorKind::Protocol | OpenAiRealtimeNetworkErrorKind::Rejected => {
                (
                    "connection_rejected",
                    "OpenAI 拒绝了 WebSocket 连接，请检查服务地址和配置。",
                )
            }
            OpenAiRealtimeNetworkErrorKind::Disconnected => {
                ("disconnected", "OpenAI Realtime 在握手期间断开了连接。")
            }
            OpenAiRealtimeNetworkErrorKind::Other => (
                "connection_failed",
                "无法连接 OpenAI Realtime，请检查网络和服务配置。",
            ),
        },
        OpenAiRealtimeError::Parse(_)
        | OpenAiRealtimeError::Protocol(_)
        | OpenAiRealtimeError::Pcm(_)
        | OpenAiRealtimeError::Encode
        | OpenAiRealtimeError::InvalidPcm16Payload
        | OpenAiRealtimeError::EmptyAudioPayload
        | OpenAiRealtimeError::UnexpectedBinaryMessage => (
            "provider_protocol_error",
            "OpenAI Realtime 返回了无法处理的协议消息。",
        ),
    };
    TranscriptConnectionTestResult::failure(code, message)
}

fn latency_endpointing(mode: StreamingLatencyMode) -> (u32, Option<u32>) {
    match mode {
        StreamingLatencyMode::Minimal => (100, Some(1_000)),
        StreamingLatencyMode::Low => (200, Some(1_000)),
        StreamingLatencyMode::Balanced => (300, Some(1_200)),
        StreamingLatencyMode::High => (700, Some(2_000)),
    }
}

fn openai_delay(mode: StreamingLatencyMode) -> OpenAiRealtimeDelay {
    match mode {
        StreamingLatencyMode::Minimal => OpenAiRealtimeDelay::Minimal,
        StreamingLatencyMode::Low => OpenAiRealtimeDelay::Low,
        StreamingLatencyMode::Balanced => OpenAiRealtimeDelay::Medium,
        StreamingLatencyMode::High => OpenAiRealtimeDelay::High,
    }
}

fn prepare_transcript_connection_options(
    requested_provider: TranscriptProvider,
    config_override: TranscriptConfigView,
) -> Result<TranscriptConnectionOptions, TranscriptConnectionTestResult> {
    if config_override.provider != requested_provider.as_str() {
        return Err(TranscriptConnectionTestResult::failure(
            "provider_mismatch",
            "测试服务与当前配置中的转写服务不一致，请重新选择后再试。",
        ));
    }

    let config = validate_and_normalize_streaming_config(config_override.streaming_config)
        .map_err(|_| {
            TranscriptConnectionTestResult::failure(
                "invalid_config",
                "在线转写配置无效，请检查模型、语言、关键词和 WSS 地址。",
            )
        })?;

    let require_matching_model =
        |provider: &StreamingProviderConfig| -> Result<(), TranscriptConnectionTestResult> {
            if config_override.model.trim() != provider.model {
                return Err(TranscriptConnectionTestResult::failure(
                    "model_mismatch",
                    "当前模型与服务配置不一致，请重新选择模型后再试。",
                ));
            }
            Ok(())
        };

    match requested_provider {
        TranscriptProvider::Deepgram => {
            let provider = config.providers.deepgram;
            require_matching_model(&provider)?;
            let (endpointing_ms, utterance_end_ms) = latency_endpointing(provider.latency_mode);
            Ok(TranscriptConnectionOptions::Deepgram(DeepgramOptions {
                endpoint_override: provider.endpoint_override,
                model: provider.model,
                language: provider.language,
                interim_results: true,
                endpointing_ms,
                utterance_end_ms,
                diarize: provider.diarization,
                keyterms: provider.keywords,
            }))
        }
        TranscriptProvider::OpenAi => {
            let provider = config.providers.openai;
            require_matching_model(&provider)?;
            let languages = if provider.language == "auto" {
                Vec::new()
            } else {
                vec![provider.language]
            };
            Ok(TranscriptConnectionOptions::OpenAi(OpenAiRealtimeOptions {
                endpoint_override: provider.endpoint_override,
                model: provider.model,
                prompt: None,
                languages,
                keywords: provider.keywords,
                delay: Some(openai_delay(provider.latency_mode)),
                turn_detection: OpenAiRealtimeTurnDetection::Disabled,
            }))
        }
        TranscriptProvider::LocalWhisper | TranscriptProvider::Parakeet => {
            Err(TranscriptConnectionTestResult::failure(
                "unsupported_provider",
                "本地转写引擎不需要测试在线连接。",
            ))
        }
    }
}

async fn test_deepgram_connection(
    options: &DeepgramOptions,
    api_key: &str,
) -> Result<(), TranscriptConnectionTestResult> {
    let mut connection = DeepgramConnection::connect(options, api_key, "settings-test", 0)
        .await
        .map_err(deepgram_connection_failure)?;
    if let Err(error) = connection.send_control(DeepgramControl::KeepAlive).await {
        let _ = connection.close_websocket().await;
        return Err(deepgram_connection_failure(error));
    }
    connection
        .close_websocket()
        .await
        .map_err(deepgram_connection_failure)
}

async fn test_openai_connection(
    options: &OpenAiRealtimeOptions,
    api_key: &str,
) -> Result<(), TranscriptConnectionTestResult> {
    let mut connection = OpenAiRealtimeConnection::connect(options, api_key)
        .await
        .map_err(openai_connection_failure)?;

    let ready = tokio::time::timeout(TRANSCRIPT_CONNECTION_READY_TIMEOUT, async {
        loop {
            match connection.recv().await {
                Ok(Some(OpenAiRealtimeServerEvent::SessionUpdated(_)))
                | Ok(Some(OpenAiRealtimeServerEvent::TranscriptionSessionUpdated(_))) => {
                    return Ok(())
                }
                Ok(Some(OpenAiRealtimeServerEvent::Error(_))) => {
                    return Err(TranscriptConnectionTestResult::failure(
                        "provider_rejected",
                        "OpenAI Realtime 拒绝了会话配置，请检查密钥、模型和服务权限。",
                    ))
                }
                Ok(Some(_)) => continue,
                Ok(None) => {
                    return Err(TranscriptConnectionTestResult::failure(
                        "disconnected",
                        "OpenAI Realtime 在确认会话前断开了连接。",
                    ))
                }
                Err(error) => return Err(openai_connection_failure(error)),
            }
        }
    })
    .await;
    let result = match ready {
        Ok(result) => result,
        Err(_) => Err(TranscriptConnectionTestResult::failure(
            "ready_timeout",
            "OpenAI Realtime 已连接，但未在限定时间内确认会话。",
        )),
    };

    let close_result = connection
        .close_websocket()
        .await
        .map_err(openai_connection_failure);
    result.and(close_result)
}

/// Probe one online streaming ASR provider without uploading audio. A
/// temporary key and a stored key are mutually exclusive, and the selected
/// secret remains Rust-only for the lifetime of this command.
#[tauri::command]
pub async fn api_test_transcript_connection<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    provider: String,
    api_key: Option<String>,
    use_stored_key: bool,
    config_override: TranscriptConfigView,
) -> Result<TranscriptConnectionTestResult, String> {
    let provider = match TranscriptProvider::parse_request(&provider) {
        Ok(provider) => provider,
        Err(_) => {
            return Ok(TranscriptConnectionTestResult::failure(
                "unsupported_provider",
                "不支持该在线转写服务。",
            ));
        }
    };
    if !provider.requires_api_key() {
        return Ok(TranscriptConnectionTestResult::failure(
            "unsupported_provider",
            "本地转写引擎不需要测试在线连接。",
        ));
    }

    let options = match prepare_transcript_connection_options(provider, config_override) {
        Ok(options) => options,
        Err(result) => return Ok(result),
    };

    let temporary_key = api_key.filter(|value| !value.trim().is_empty());
    if use_stored_key && temporary_key.is_some() {
        return Ok(TranscriptConnectionTestResult::failure(
            "ambiguous_credential",
            "临时密钥与已保存密钥只能选择一种。",
        ));
    }

    let credential = if use_stored_key {
        match SettingsRepository::get_transcript_api_key_for_provider(
            state.db_manager.pool(),
            provider,
        )
        .await
        {
            Ok(Some(value)) => TranscriptConnectionCredential::Stored(value),
            Ok(None) => {
                return Ok(TranscriptConnectionTestResult::failure(
                    "missing_api_key",
                    "尚未保存该服务的 API 密钥。",
                ));
            }
            Err(_) => {
                log_error!(
                    "Failed to load the stored transcript credential for provider '{}'.",
                    provider.as_str()
                );
                return Ok(TranscriptConnectionTestResult::failure(
                    "credential_load_failed",
                    "无法读取已保存的 API 密钥，请稍后重试。",
                ));
            }
        }
    } else {
        let Some(value) = temporary_key else {
            return Ok(TranscriptConnectionTestResult::failure(
                "missing_api_key",
                "请输入用于本次测试的 API 密钥。",
            ));
        };
        match normalize_transcript_api_key(&value) {
            Ok(value) => TranscriptConnectionCredential::Temporary(value),
            Err(_) => {
                return Ok(TranscriptConnectionTestResult::failure(
                    "invalid_api_key",
                    "API 密钥为空、过长或包含无效字符。",
                ));
            }
        }
    };

    let started = Instant::now();
    let connection_test = async {
        match &options {
            TranscriptConnectionOptions::Deepgram(options) => {
                test_deepgram_connection(options, credential.expose_secret()).await
            }
            TranscriptConnectionOptions::OpenAi(options) => {
                test_openai_connection(options, credential.expose_secret()).await
            }
        }
    };

    let result =
        match tokio::time::timeout(TRANSCRIPT_CONNECTION_TEST_TIMEOUT, connection_test).await {
            Ok(Ok(())) => TranscriptConnectionTestResult::success(
                started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            ),
            Ok(Err(result)) => result,
            Err(_) => TranscriptConnectionTestResult::failure(
                "timeout",
                "连接测试超时，请检查网络、代理和服务地址。",
            ),
        };

    log_info!(
        "Transcript connection test finished for provider '{}' with code '{}'.",
        provider.as_str(),
        result.code
    );
    Ok(result)
}

#[tauri::command]
pub async fn api_delete_meeting<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
    auth_token: Option<String>,
) -> Result<serde_json::Value, String> {
    log_info!(
        "api_delete_meeting called for meeting_id(native): {}, auth_token: {}",
        meeting_id,
        auth_token.is_some()
    );

    let pool = state.db_manager.pool();

    match MeetingsRepository::delete_meeting(pool, &meeting_id).await {
        Ok(true) => {
            log_info!("Successfully deleted meeting {}", meeting_id);
            Ok(serde_json::json!({
                "status": "success",
                "message": "Meeting deleted successfully"
            }))
        }
        Ok(false) => {
            log_warn!("Meeting not found or already deleted: {}", meeting_id);
            Err(format!(
                "Meeting not found or could not be deleted: {}",
                meeting_id
            ))
        }
        Err(e) => {
            log_error!("Error deleting meeting {}: {}", meeting_id, e);
            Err(format!("Failed to delete meeting: {}", e))
        }
    }
}

#[tauri::command]
pub async fn api_get_meeting<R: Runtime>(
    _app: AppHandle<R>,
    meeting_id: String,
    state: tauri::State<'_, AppState>,
    auth_token: Option<String>,
) -> Result<MeetingDetails, String> {
    log_info!(
        "api_get_meeting called(native) for meeting_id: {}, auth_token: {}",
        meeting_id,
        auth_token.is_some()
    );

    let pool = state.db_manager.pool();

    match MeetingsRepository::get_meeting(pool, &meeting_id).await {
        Ok(Some(meeting)) => {
            log_info!("Successfully retrieved meeting {}", meeting_id);
            Ok(meeting)
        }
        Ok(None) => {
            log_warn!("Meeting not found: {}", meeting_id);
            Err(format!("Meeting not found: {}", meeting_id))
        }
        Err(e) => {
            log_error!("Error retrieving meeting {}: {}", meeting_id, e);
            Err(format!("Failed to retrieve meeting: {}", e))
        }
    }
}

/// Get meeting metadata without transcripts (for pagination)
#[tauri::command]
pub async fn api_get_meeting_metadata<R: Runtime>(
    _app: AppHandle<R>,
    meeting_id: String,
    state: tauri::State<'_, AppState>,
) -> Result<MeetingMetadata, String> {
    log_info!(
        "api_get_meeting_metadata called for meeting_id: {}",
        meeting_id
    );

    let pool = state.db_manager.pool();

    match MeetingsRepository::get_meeting_metadata(pool, &meeting_id).await {
        Ok(Some(meeting)) => {
            log_info!("Successfully retrieved meeting metadata {}", meeting_id);
            Ok(MeetingMetadata {
                id: meeting.id,
                title: meeting.title,
                created_at: meeting.created_at.0.to_rfc3339(),
                updated_at: meeting.updated_at.0.to_rfc3339(),
                folder_path: meeting.folder_path,
            })
        }
        Ok(None) => {
            log_warn!("Meeting not found: {}", meeting_id);
            Err(format!("Meeting not found: {}", meeting_id))
        }
        Err(e) => {
            log_error!("Error retrieving meeting metadata {}: {}", meeting_id, e);
            Err(format!("Failed to retrieve meeting metadata: {}", e))
        }
    }
}

/// Get paginated transcripts for a meeting
#[tauri::command]
pub async fn api_get_meeting_transcripts<R: Runtime>(
    _app: AppHandle<R>,
    meeting_id: String,
    limit: i64,
    offset: i64,
    state: tauri::State<'_, AppState>,
) -> Result<PaginatedTranscriptsResponse, String> {
    log_info!(
        "api_get_meeting_transcripts called for meeting_id: {}, limit: {}, offset: {}",
        meeting_id,
        limit,
        offset
    );

    let pool = state.db_manager.pool();

    match MeetingsRepository::get_meeting_transcripts_paginated(pool, &meeting_id, limit, offset)
        .await
    {
        Ok((transcripts, total_count)) => {
            log_info!(
                "Successfully retrieved {} transcripts for meeting {} (total: {})",
                transcripts.len(),
                meeting_id,
                total_count
            );

            // Convert Transcript to MeetingTranscript
            let meeting_transcripts = transcripts
                .into_iter()
                .map(|t| MeetingTranscript {
                    id: t.id,
                    text: t.transcript,
                    timestamp: t.timestamp,
                    audio_start_time: t.audio_start_time,
                    audio_end_time: t.audio_end_time,
                    duration: t.duration,
                    schema_version: t.schema_version,
                    event_id: t.latest_event_id,
                    session_id: t.session_id,
                    utterance_id: t.utterance_id,
                    revision: t.revision,
                    event_kind: t.event_kind,
                    is_stable: t.is_stable,
                    sequence_id: t.sequence_id,
                    start_ms: t.start_ms,
                    end_ms: t.end_ms,
                    audio_source: t.audio_source,
                    speaker_id: t.speaker_id,
                    speaker_local_label: t.speaker_local_label,
                    speaker_display_name: t.speaker_display_name,
                    speaker_confidence: t.speaker_confidence,
                    speaker_status: t.speaker_status,
                    language: t.language,
                    asr_provider: t.asr_provider,
                    asr_model: t.asr_model,
                    asr_confidence: t.asr_confidence,
                    asr_latency_ms: t.asr_latency_ms,
                    diarization_provider: t.diarization_provider,
                    diarization_model: t.diarization_model,
                    diarization_model_revision: t.diarization_model_revision,
                    diarization_revision: t.diarization_revision,
                    diarization_window_id: t.diarization_window_id,
                    diarization_window_start_frame: t.diarization_window_start_frame,
                    diarization_window_end_frame: t.diarization_window_end_frame,
                    diarization_status: t.diarization_status,
                    diarization_latency_ms: t.diarization_latency_ms,
                    replaces_event_id: t.replaces_event_id,
                    provider_event_id: t.provider_event_id,
                    created_at: t.event_created_at.map(|created_at| created_at.to_rfc3339()),
                    trace_id: t.trace_id,
                })
                .collect::<Vec<_>>();

            let has_more = (offset + meeting_transcripts.len() as i64) < total_count;

            Ok(PaginatedTranscriptsResponse {
                transcripts: meeting_transcripts,
                total_count,
                has_more,
            })
        }
        Err(e) => {
            log_error!(
                "Error retrieving transcripts for meeting {}: {}",
                meeting_id,
                e
            );
            Err(format!("Failed to retrieve transcripts: {}", e))
        }
    }
}

#[tauri::command]
pub async fn api_save_meeting_title<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
    title: String,
    auth_token: Option<String>,
) -> Result<serde_json::Value, String> {
    log_info!(
        "api_save_meeting_title called for meeting_id: {}, auth_token: {}",
        meeting_id,
        auth_token.is_some()
    );
    let pool = state.db_manager.pool();
    match MeetingsRepository::update_meeting_title(pool, &meeting_id, &title).await {
        Ok(true) => {
            log_info!("Successfully saved meeting title");
            Ok(serde_json::json!({"message": "Meeting title saved successfully"}))
        }
        Ok(false) => {
            log_error!("No meeting found with id {}", meeting_id);
            Err(format!("No meeting found with id {}", meeting_id))
        }
        Err(e) => {
            log_error!("Failed to update meeting {}", e);
            Err(format!("Failed to update meeting: {}", e))
        }
    }
}

async fn load_durable_recording_binding_for_validated_folder(
    pool: &sqlx::SqlitePool,
    canonical_folder: &std::path::Path,
) -> Result<Option<TrustedRecordingSaveBinding>, String> {
    let folder_hash = RecordingSessionBindingRepository::recording_folder_hash(canonical_folder)
        .map_err(|_| "无法验证录音恢复关联。".to_string())?;
    let binding = RecordingSessionBindingRepository::load_by_folder_hash(pool, &folder_hash)
        .await
        .map_err(|_| "无法读取录音恢复关联。".to_string())?;
    Ok(binding.map(|binding| TrustedRecordingSaveBinding {
        source_session_id: binding.source_session_id,
        recording_folder_hash: binding.recording_folder_hash,
    }))
}

fn summary_binding_matches_durable(
    summary_session_id: &str,
    summary_folder_hash: &str,
    durable: &TrustedRecordingSaveBinding,
) -> bool {
    summary_session_id == durable.source_session_id.as_str()
        && summary_folder_hash == durable.recording_folder_hash.as_str()
}

#[tauri::command]
pub async fn api_save_transcript<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    live_summary_runtime: tauri::State<'_, crate::summary::live::LiveSummaryRuntimeState>,
    live_translation_runtime: tauri::State<
        '_,
        crate::audio::transcription::translation_runtime::LiveTranslationRuntimeState,
    >,
    meeting_title: String,
    transcripts: Vec<serde_json::Value>,
    folder_path: Option<String>,
    auth_token: Option<String>,
    summary_binding_handle: Option<String>,
) -> Result<serde_json::Value, String> {
    log_info!(
        "api_save_transcript called: transcripts={}, folder_present={}, auth_token_present={}",
        transcripts.len(),
        folder_path.is_some(),
        auth_token.is_some()
    );

    crate::summary::live::commands::flush_recording_summary_ingress(
        &app,
        live_summary_runtime.inner(),
    )
    .await;

    enum SaveSummaryBinding {
        NotRequested,
        Rejected(crate::summary::live::LiveSummaryFrontendError),
        Trusted {
            handle: String,
            scope_id: String,
            source_session_id: String,
            recording_folder_hash: String,
            segments: Vec<TranscriptSegment>,
        },
    }

    enum BindingRequest {
        NotRequested,
        Rejected(crate::summary::live::LiveSummaryFrontendError),
        Handle { handle: String },
    }

    let binding_request = match (summary_binding_handle, folder_path.as_deref()) {
        (None, Some(folder)) => {
            match crate::summary::live::commands::recover_trusted_recording_summary(
                &app,
                state.inner(),
                live_summary_runtime.inner(),
                std::path::Path::new(folder),
            )
            .await
            {
                Ok(Some(ticket)) => BindingRequest::Handle {
                    handle: ticket.handle,
                },
                Ok(None) => BindingRequest::NotRequested,
                Err(error) => BindingRequest::Rejected(error),
            }
        }
        (None, None) => BindingRequest::NotRequested,
        (Some(handle), Some(_)) => BindingRequest::Handle { handle },
        (Some(_), None) => BindingRequest::Rejected(
            crate::summary::live::RecordingSummaryRegistryError::MissingFolderCorrelation
                .frontend_error(),
        ),
    };

    let summary_binding = match (binding_request, folder_path.as_deref()) {
        (BindingRequest::NotRequested, _) => SaveSummaryBinding::NotRequested,
        (BindingRequest::Rejected(error), _) => SaveSummaryBinding::Rejected(error),
        (BindingRequest::Handle { handle }, Some(folder)) => match live_summary_runtime
            .3
            .prepare_binding(&handle, std::path::Path::new(folder))
            .await
        {
            Ok(prepared) => {
                let converted = prepared
                    .trusted_segments
                    .iter()
                    .map(trusted_transcript_for_save)
                    .collect::<Result<Vec<_>, _>>();
                match converted {
                    Ok(segments) => SaveSummaryBinding::Trusted {
                        handle,
                        scope_id: prepared.scope_id,
                        source_session_id: prepared.source_session_id,
                        recording_folder_hash: prepared.recording_folder_hash,
                        segments,
                    },
                    Err(_) => {
                        log_error!("Trusted transcript conversion failed validation");
                        live_summary_runtime.3.abort_binding(&handle).await;
                        SaveSummaryBinding::Rejected(
                            crate::summary::live::LiveSummaryFrontendError {
                                code: "live_summary_trusted_history_invalid".to_string(),
                                message: "可信转写历史无法安全绑定；已改用兼容保存流程。"
                                    .to_string(),
                                retryable: false,
                            },
                        )
                    }
                }
            }
            Err(error) => SaveSummaryBinding::Rejected(error.frontend_error()),
        },
        (BindingRequest::Handle { .. }, None) => SaveSummaryBinding::Rejected(
            crate::summary::live::RecordingSummaryRegistryError::MissingFolderCorrelation
                .frontend_error(),
        ),
    };

    let pool = state.db_manager.pool();
    let validated_recording_folder = folder_path.as_deref().and_then(|folder| {
        match crate::audio::recording_saver::validate_recording_folder(std::path::Path::new(folder))
        {
            Ok(folder) => Some(folder),
            Err(_) => {
                log_debug!("Save folder is not eligible for native recording recovery binding");
                None
            }
        }
    });
    let durable_recording_binding = match validated_recording_folder.as_deref() {
        Some(folder) => load_durable_recording_binding_for_validated_folder(pool, folder).await?,
        None => None,
    };

    if let SaveSummaryBinding::Trusted {
        handle,
        source_session_id,
        recording_folder_hash,
        ..
    } = &summary_binding
    {
        let matches = durable_recording_binding.as_ref().is_some_and(|durable| {
            summary_binding_matches_durable(source_session_id, recording_folder_hash, durable)
        });
        if !matches {
            live_summary_runtime.3.abort_binding(handle).await;
            log_error!("Summary ticket did not match the durable recording start anchor");
            return Err("录音恢复关联与实时总结会话不一致，已拒绝保存。".to_string());
        }
    }

    // A valid one-time handle switches evidence to the Rust registry history.
    // Otherwise preserve the legacy/import/recovery behavior exactly: parse
    // and save the renderer transcript, but never initialize a summary actor
    // or evidence binding from it.
    let transcripts_to_save: Vec<TranscriptSegment> =
        if let SaveSummaryBinding::Trusted { segments, .. } = &summary_binding {
            segments.clone()
        } else {
            transcripts
                .into_iter()
                .map(serde_json::from_value)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| {
                    log_error!("Failed to parse transcript segments: {}", e);
                    format!(
                        "Invalid transcript data format: {}. Please check the data structure.",
                        e
                    )
                })?
        };
    // Translation text is never accepted from the renderer. The session ID is
    // taken from the Rust-prepared live or durable recovery binding and is
    // committed with the meeting/transcript transaction below.
    let trusted_translation_session_id = match &summary_binding {
        SaveSummaryBinding::Trusted {
            source_session_id, ..
        } => Some(source_session_id.clone()),
        SaveSummaryBinding::NotRequested | SaveSummaryBinding::Rejected(_) => None,
    };

    // Meeting speech is private. Keep diagnostics to non-content timing and
    // count metadata; never write transcript text or the raw renderer payload
    // to application logs.
    if let Some(first_seg) = transcripts_to_save.first() {
        log_debug!("First parsed segment metadata: audio_start_time={:?}, audio_end_time={:?}, duration={:?}",
                   first_seg.audio_start_time,
                   first_seg.audio_end_time,
                   first_seg.duration);
    }

    // Meeting creation, all transcript revisions, and the durable
    // session-to-meeting transition commit or roll back together.
    match TranscriptsRepository::save_transcript_with_recording_binding(
        pool,
        &meeting_title,
        &transcripts_to_save,
        folder_path,
        durable_recording_binding.as_ref(),
    )
    .await
    {
        Ok(save_outcome) => {
            let meeting_id = save_outcome.meeting_id;
            log_info!(
                "Successfully saved transcript meeting: reused_existing={}",
                save_outcome.reused_existing_meeting
            );
            let binding_status = match &summary_binding {
                SaveSummaryBinding::NotRequested => serde_json::json!({
                    "status": "not_requested"
                }),
                SaveSummaryBinding::Rejected(error) => serde_json::json!({
                    "status": "rejected",
                    "error": error
                }),
                SaveSummaryBinding::Trusted {
                    handle, scope_id, ..
                } => match crate::summary::live::commands::bind_recording_summary_after_save(
                    &app,
                    state.inner(),
                    live_summary_runtime.inner(),
                    handle,
                    &meeting_id,
                )
                .await
                {
                    Ok(_) => serde_json::json!({
                        "status": "bound",
                        "scopeId": scope_id
                    }),
                    Err(error) => {
                        live_summary_runtime.3.abort_binding(&handle).await;
                        serde_json::json!({
                            "status": "pending_retry",
                            "scopeId": scope_id,
                            "error": error
                        })
                    }
                },
            };
            let translation_binding_status = match trusted_translation_session_id.as_deref() {
                Some(source_session_id) => match live_translation_runtime
                    .bind_recording_after_save(pool, source_session_id, &meeting_id)
                    .await
                {
                    Ok(report) => serde_json::to_value(report)
                        .unwrap_or_else(|_| serde_json::json!({ "status": "bound" })),
                    Err(error) => {
                        log_warn!(
                            "Meeting saved, but trusted live translation binding failed: {}",
                            error
                        );
                        serde_json::json!({
                            "status": "pending_retry",
                            "code": "translation_binding_failed",
                            "message": "会议已保存，实时译文暂未绑定。"
                        })
                    }
                },
                None => serde_json::json!({
                    "status": "not_bound_untrusted_source"
                }),
            };
            Ok(serde_json::json!({
                "status": "success",
                "message": "Transcript saved successfully",
                "meeting_id": meeting_id,
                "summary_binding": binding_status,
                "translation_binding": translation_binding_status
            }))
        }
        Err(e) => {
            if let SaveSummaryBinding::Trusted { handle, .. } = &summary_binding {
                live_summary_runtime.3.abort_binding(handle).await;
            }
            log_error!("Error saving meeting transcript: {}", e);
            Err(format!("Failed to save transcript: {}", e))
        }
    }
}

fn trusted_transcript_for_save(
    source: &crate::audio::recording_saver::TranscriptSegment,
) -> Result<TranscriptSegment, String> {
    let to_i64 = |value: u64, field: &str| {
        i64::try_from(value).map_err(|_| format!("trusted {field} exceeds SQLite range"))
    };
    Ok(TranscriptSegment {
        id: source.id.clone(),
        text: source.text.clone(),
        timestamp: source.display_time.clone(),
        sequence_id: Some(source.sequence_id),
        chunk_start_time: Some(source.chunk_start_time),
        is_partial: Some(source.is_partial),
        confidence: Some(source.confidence),
        source: Some(source.source.clone()),
        audio_start_time: Some(source.audio_start_time),
        audio_end_time: Some(source.audio_end_time),
        duration: Some(source.duration),
        event_id: Some(source.event_id.clone()),
        schema_version: Some(i64::from(source.schema_version)),
        session_id: source.session_id.clone(),
        utterance_id: Some(source.utterance_id.clone()),
        revision: Some(to_i64(source.revision, "revision")?),
        event_kind: Some(source.event_kind.as_str().to_string()),
        is_stable: Some(source.is_stable),
        start_ms: Some(to_i64(source.start_ms, "start_ms")?),
        end_ms: Some(to_i64(source.end_ms, "end_ms")?),
        audio_source: Some(source.audio_source.legacy_label().to_string()),
        speaker_id: source.speaker_id.clone(),
        speaker_local_label: source.speaker_local_label.clone(),
        speaker_display_name: source.speaker_display_name.clone(),
        speaker_confidence: source.speaker_confidence,
        speaker_status: source
            .speaker_status
            .as_ref()
            .map(|status| status.as_str().to_string()),
        language: source.language.clone(),
        asr_provider: source.asr_provider.clone(),
        asr_model: source.asr_model.clone(),
        asr_confidence: source.asr_confidence,
        asr_latency_ms: source
            .asr_latency_ms
            .map(|value| to_i64(value, "asr_latency_ms"))
            .transpose()?,
        diarization_provider: source.diarization_provider.clone(),
        diarization_model: source.diarization_model.clone(),
        diarization_model_revision: source.diarization_model_revision.clone(),
        diarization_revision: source
            .diarization_revision
            .map(|value| to_i64(value, "diarization_revision"))
            .transpose()?,
        diarization_window_id: source.diarization_window_id.clone(),
        diarization_window_start_frame: source
            .diarization_window_start_frame
            .map(|value| to_i64(value, "diarization_window_start_frame"))
            .transpose()?,
        diarization_window_end_frame: source
            .diarization_window_end_frame
            .map(|value| to_i64(value, "diarization_window_end_frame"))
            .transpose()?,
        diarization_status: source
            .diarization_status
            .as_ref()
            .map(|status| status.as_str().to_string()),
        diarization_latency_ms: source
            .diarization_latency_ms
            .map(|value| to_i64(value, "diarization_latency_ms"))
            .transpose()?,
        replaces_event_id: source.replaces_event_id.clone(),
        provider_event_id: source.provider_event_id.clone(),
        trace_id: source.trace_id.clone(),
        created_at: Some(source.created_at.clone()),
    })
}

#[cfg(test)]
mod recording_save_binding_tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_pool() -> sqlx::SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("create in-memory database");
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .expect("enable foreign keys");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");
        pool
    }

    #[tokio::test]
    async fn summary_disabled_save_uses_durable_folder_anchor_and_retry_is_idempotent() {
        let pool = test_pool().await;
        // This represents the canonical path already returned by the recordings
        // root validator; the resolver below never consults summary state.
        let canonical_folder = std::env::current_dir()
            .expect("current directory")
            .join("summary-disabled-recording-fixture");
        let folder_hash =
            RecordingSessionBindingRepository::recording_folder_hash(&canonical_folder)
                .expect("hash canonical folder");
        RecordingSessionBindingRepository::ensure_pending(
            &pool,
            "session-summary-disabled",
            &folder_hash,
        )
        .await
        .expect("persist recording start anchor");

        let durable = load_durable_recording_binding_for_validated_folder(&pool, &canonical_folder)
            .await
            .expect("load recording anchor without summary")
            .expect("recording anchor exists");
        let folder_path = canonical_folder
            .to_str()
            .expect("ASCII fixture path")
            .to_string();
        let transcripts = vec![TranscriptSegment {
            id: "summary-disabled-segment".to_string(),
            text: "synthetic fixture".to_string(),
            timestamp: "00:00:01".to_string(),
            session_id: Some("session-summary-disabled".to_string()),
            ..TranscriptSegment::default()
        }];
        let first = TranscriptsRepository::save_transcript_with_recording_binding(
            &pool,
            "Summary disabled",
            &transcripts,
            Some(folder_path.clone()),
            Some(&durable),
        )
        .await
        .expect("save without summary binding");
        let retry_binding =
            load_durable_recording_binding_for_validated_folder(&pool, &canonical_folder)
                .await
                .expect("reload bound recording anchor")
                .expect("bound anchor exists");
        let retry = TranscriptsRepository::save_transcript_with_recording_binding(
            &pool,
            "Summary still disabled",
            &transcripts,
            Some(folder_path),
            Some(&retry_binding),
        )
        .await
        .expect("retry without summary binding");

        assert!(!first.reused_existing_meeting);
        assert!(retry.reused_existing_meeting);
        assert_eq!(retry.meeting_id, first.meeting_id);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM meetings")
            .fetch_one(&pool)
            .await
            .expect("count meetings");
        assert_eq!(count, 1);
    }

    #[test]
    fn summary_ticket_must_match_both_durable_session_and_folder() {
        let durable = TrustedRecordingSaveBinding {
            source_session_id: "session-one".to_string(),
            recording_folder_hash:
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        };
        assert!(summary_binding_matches_durable(
            "session-one",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &durable,
        ));
        assert!(!summary_binding_matches_durable(
            "session-two",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &durable,
        ));
        assert!(!summary_binding_matches_durable(
            "session-one",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            &durable,
        ));
    }
}

/// Opens the meeting's recording folder in the system file explorer
#[tauri::command]
pub async fn open_meeting_folder<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
) -> Result<(), String> {
    log_info!("open_meeting_folder called for meeting_id: {}", meeting_id);

    let pool = state.db_manager.pool();

    // Get meeting with folder_path
    let meeting: Option<MeetingModel> = sqlx::query_as(
        "SELECT id, title, created_at, updated_at, folder_path FROM meetings WHERE id = ?",
    )
    .bind(&meeting_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("Database error: {}", e))?;

    match meeting {
        Some(m) => {
            if let Some(folder_path) = m.folder_path {
                log_info!("Opening meeting folder: {}", folder_path);

                // Verify folder exists
                let path = std::path::Path::new(&folder_path);
                if !path.exists() {
                    log_warn!("Folder path does not exist: {}", folder_path);
                    return Err(format!("Recording folder not found: {}", folder_path));
                }

                // Open folder based on OS
                #[cfg(target_os = "macos")]
                {
                    std::process::Command::new("open")
                        .arg(&folder_path)
                        .spawn()
                        .map_err(|e| format!("Failed to open folder: {}", e))?;
                }

                #[cfg(target_os = "windows")]
                {
                    std::process::Command::new("explorer")
                        .arg(&folder_path)
                        .spawn()
                        .map_err(|e| format!("Failed to open folder: {}", e))?;
                }

                #[cfg(target_os = "linux")]
                {
                    std::process::Command::new("xdg-open")
                        .arg(&folder_path)
                        .spawn()
                        .map_err(|e| format!("Failed to open folder: {}", e))?;
                }

                log_info!("Successfully opened folder: {}", folder_path);
                Ok(())
            } else {
                log_warn!("Meeting {} has no folder_path set", meeting_id);
                Err("Recording folder path not available for this meeting".to_string())
            }
        }
        None => {
            log_warn!("Meeting not found: {}", meeting_id);
            Err("Meeting not found".to_string())
        }
    }
}

// Simple test command to check backend connectivity
#[tauri::command]
pub async fn test_backend_connection<R: Runtime>(
    app: AppHandle<R>,
    auth_token: Option<String>,
) -> Result<String, String> {
    log_debug!("Testing backend connection...");

    let client = reqwest::Client::new();
    let server_url = get_server_address(&app).await?;

    log_debug!("Testing connection to: {}", server_url);

    let mut request = client.get(&format!("{}/docs", server_url));

    if let Some(token) = auth_token {
        request = request.header("Authorization", format!("Bearer {}", token));
    }

    match request.send().await {
        Ok(response) => {
            let status = response.status();
            log_debug!("Backend responded with status: {}", status);
            Ok(format!("Backend is reachable. Status: {}", status))
        }
        Err(e) => {
            let error_msg = format!("Failed to connect to backend: {}", e);
            log_debug!("{}", error_msg);
            Err(error_msg)
        }
    }
}

#[tauri::command]
pub async fn debug_backend_connection<R: Runtime>(app: AppHandle<R>) -> Result<String, String> {
    log_debug!("=== DEBUG: Testing backend connection ===");

    // Test 1: Check server address from store
    let server_url = match get_server_address(&app).await {
        Ok(url) => {
            log_debug!("✓ Server URL from store: {}", url);
            url
        }
        Err(e) => {
            log_error!("✗ Failed to get server URL: {}", e);
            return Err(format!("Failed to get server URL: {}", e));
        }
    };

    // Test 2: Make a simple HTTP request to the backend
    let client = reqwest::Client::new();
    let test_url = format!("{}/docs", server_url); // Try the docs endpoint which should be public

    log_debug!("Testing connection to: {}", test_url);

    match client.get(&test_url).send().await {
        Ok(response) => {
            let status = response.status();
            log_debug!("✓ Backend responded with status: {}", status);
            Ok(format!(
                "Backend connection successful! Status: {}, URL: {}",
                status, server_url
            ))
        }
        Err(e) => {
            log_error!("✗ Backend connection failed: {}", e);
            Err(format!("Backend connection failed: {}", e))
        }
    }
}

#[tauri::command]
pub async fn open_external_url(url: String) -> Result<(), String> {
    use std::process::Command;

    let result = if cfg!(target_os = "windows") {
        Command::new("cmd").args(&["/C", "start", &url]).output()
    } else if cfg!(target_os = "macos") {
        Command::new("open").arg(&url).output()
    } else {
        // Linux and other Unix-like systems
        Command::new("xdg-open").arg(&url).output()
    };

    match result {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("Failed to open URL: {}", e)),
    }
}

// ===== CUSTOM OPENAI API COMMANDS =====

pub(crate) fn normalize_custom_openai_endpoint(value: &str) -> Result<String, String> {
    let endpoint =
        Url::parse(value.trim()).map_err(|_| "服务地址必须是有效的绝对 URL。".to_string())?;
    if endpoint.host_str().is_none() {
        return Err("服务地址必须包含主机名。".to_string());
    }
    if !endpoint.username().is_empty() || endpoint.password().is_some() {
        return Err("服务地址不能包含用户名或密码。".to_string());
    }
    if endpoint.fragment().is_some() {
        return Err("服务地址不能包含 URL 片段。".to_string());
    }
    if endpoint.query().is_some() {
        return Err("服务地址不能包含查询参数，请勿把密钥放入 URL。".to_string());
    }

    let loopback = match endpoint.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    match endpoint.scheme() {
        "https" => {}
        "http" if loopback => {}
        "http" => return Err("远程服务地址必须使用 HTTPS；HTTP 仅允许本机回环地址。".to_string()),
        _ => return Err("服务地址必须使用 HTTPS，或使用本机回环 HTTP。".to_string()),
    }

    Ok(endpoint.to_string().trim_end_matches('/').to_string())
}

pub(crate) fn normalize_custom_openai_model(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("模型名称不能为空。".to_string());
    }
    if value.chars().count() > 128 || value.chars().any(char::is_control) {
        return Err("模型名称不能超过 128 个字符，且不能包含控制字符。".to_string());
    }
    Ok(value.to_string())
}

pub(crate) fn validate_custom_openai_parameters(
    max_tokens: Option<i32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
) -> Result<(), String> {
    if temperature.is_some_and(|value| !(0.0..=2.0).contains(&value)) {
        return Err("Temperature 必须在 0.0 到 2.0 之间。".to_string());
    }
    if top_p.is_some_and(|value| !(0.0..=1.0).contains(&value)) {
        return Err("Top P 必须在 0.0 到 1.0 之间。".to_string());
    }
    if max_tokens.is_some_and(|value| !(1..=1_000_000).contains(&value)) {
        return Err("最大输出 Token 必须在 1 到 1000000 之间。".to_string());
    }
    Ok(())
}

fn summary_connection_http_failure(status: reqwest::StatusCode) -> SummaryConnectionTestResult {
    let status_code = Some(status.as_u16());
    match status.as_u16() {
        401 | 403 => SummaryConnectionTestResult::failure(
            "authentication_failed",
            "认证失败，请替换 API 密钥后重试。",
            status_code,
        ),
        429 => SummaryConnectionTestResult::failure(
            "rate_limited",
            "请求过于频繁，请稍后重试。",
            status_code,
        ),
        500..=599 => SummaryConnectionTestResult::failure(
            "provider_unavailable",
            "服务暂时不可用，请稍后重试。",
            status_code,
        ),
        _ => SummaryConnectionTestResult::failure(
            "request_rejected",
            "服务拒绝了连接测试请求，请检查地址和模型。",
            status_code,
        ),
    }
}

/// Saves the public custom endpoint settings. A missing or empty key keeps the
/// stored credential unchanged; replacement and deletion are explicit actions.
#[tauri::command]
pub async fn api_save_custom_openai_config<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    endpoint: String,
    api_key: Option<String>,
    model: String,
    max_tokens: Option<i32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
) -> Result<CustomOpenAIConfigView, String> {
    log_info!("Saving custom OpenAI public configuration.");
    let endpoint = normalize_custom_openai_endpoint(&endpoint)?;
    let model = normalize_custom_openai_model(&model)?;
    validate_custom_openai_parameters(max_tokens, temperature, top_p)?;

    let pool = state.db_manager.pool();
    let preserved_key = SettingsRepository::get_custom_openai_config(pool)
        .await
        .map_err(|error| {
            log_error!(
                "Failed to load custom OpenAI config before update: {}",
                error
            );
            "无法读取现有自定义模型配置，请稍后重试。".to_string()
        })?
        .and_then(|config| config.api_key);
    let replacement_key = match api_key.as_deref().map(str::trim) {
        Some(value) if !value.is_empty() => Some(normalize_summary_api_key(value)?),
        _ => preserved_key,
    };

    let config = CustomOpenAIConfig {
        endpoint,
        api_key: replacement_key,
        model,
        max_tokens,
        temperature,
        top_p,
    };

    match SettingsRepository::save_custom_openai_config(pool, &config).await {
        Ok(()) => {
            log_info!("Successfully saved custom OpenAI public configuration.");
            Ok(CustomOpenAIConfigView::from(&config))
        }
        Err(e) => {
            log_error!("❌ Failed to save custom OpenAI config: {}", e);
            Err("无法保存自定义模型配置，请稍后重试。".to_string())
        }
    }
}

/// Gets the custom OpenAI configuration
#[tauri::command]
pub async fn api_get_custom_openai_config<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
) -> Result<Option<CustomOpenAIConfigView>, String> {
    log_info!("api_get_custom_openai_config called");

    let pool = state.db_manager.pool();

    match SettingsRepository::get_custom_openai_config(pool).await {
        Ok(config) => {
            log_info!("Custom OpenAI public configuration lookup completed.");
            Ok(config.as_ref().map(CustomOpenAIConfigView::from))
        }
        Err(e) => {
            log_error!("❌ Failed to get custom OpenAI config: {}", e);
            Err("无法读取自定义模型配置，请稍后重试。".to_string())
        }
    }
}

/// Tests the connection to a custom OpenAI-compatible endpoint
/// Makes a minimal request to verify the endpoint is reachable and responds correctly
#[tauri::command]
pub async fn api_test_custom_openai_connection<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    endpoint: String,
    api_key: Option<String>,
    model: String,
    use_stored_key: Option<bool>,
) -> Result<SummaryConnectionTestResult, String> {
    log_info!("Testing a custom OpenAI-compatible connection.");
    let endpoint = normalize_custom_openai_endpoint(&endpoint)?;
    let model = normalize_custom_openai_model(&model)?;

    let url = format!("{endpoint}/chat/completions");

    // Create a minimal test request
    let test_request = serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "user",
                "content": "Hi"
            }
        ],
        "max_tokens": 5
    });

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            return Ok(SummaryConnectionTestResult::failure(
                "client_initialization_failed",
                "无法初始化连接测试，请重启应用后重试。",
                None,
            ))
        }
    };

    let mut request = client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&test_request);

    let temporary_key = match api_key.as_deref().map(str::trim) {
        Some(value) if !value.is_empty() => Some(normalize_summary_api_key(value)?),
        _ => None,
    };
    let stored_key = if temporary_key.is_none() && use_stored_key.unwrap_or(false) {
        SettingsRepository::get_summary_api_key(
            state.db_manager.pool(),
            SummaryApiKeyProvider::CustomOpenAi,
        )
        .await
        .map_err(|error| {
            log_error!(
                "Failed to read stored custom OpenAI key for connection test: {}",
                error
            );
            "无法读取已保存的 API 密钥，请稍后重试。".to_string()
        })?
    } else {
        None
    };
    let key = temporary_key
        .as_deref()
        .or_else(|| stored_key.as_ref().map(|key| key.expose_secret()));
    if let Some(key) = key {
        let mut authorization =
            reqwest::header::HeaderValue::from_bytes(format!("Bearer {key}").as_bytes())
                .map_err(|_| "API 密钥格式无效。".to_string())?;
        authorization.set_sensitive(true);
        request = request.header(reqwest::header::AUTHORIZATION, authorization);
    }

    match request.send().await {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                match response.json::<serde_json::Value>().await {
                    Ok(json) => {
                        let compatible = json
                            .get("choices")
                            .and_then(serde_json::Value::as_array)
                            .and_then(|choices| choices.first())
                            .and_then(|choice| choice.get("message"))
                            .and_then(|message| {
                                message
                                    .get("content")
                                    .or_else(|| message.get("reasoning_content"))
                            })
                            .is_some();
                        Ok(if compatible {
                            SummaryConnectionTestResult::success(status.as_u16())
                        } else {
                            SummaryConnectionTestResult::failure(
                                "incompatible_response",
                                "服务可访问，但响应格式与 OpenAI Chat Completions 不兼容。",
                                Some(status.as_u16()),
                            )
                        })
                    }
                    Err(_) => Ok(SummaryConnectionTestResult::failure(
                        "invalid_response",
                        "服务可访问，但返回了无法解析的响应。",
                        Some(status.as_u16()),
                    )),
                }
            } else {
                Ok(summary_connection_http_failure(status))
            }
        }
        Err(e) => {
            let result = if e.is_timeout() {
                SummaryConnectionTestResult::failure(
                    "timeout",
                    "连接测试超时，请检查服务地址和网络。",
                    None,
                )
            } else if e.is_connect() {
                SummaryConnectionTestResult::failure(
                    "connection_failed",
                    "无法连接到服务，请确认地址正确且服务正在运行。",
                    None,
                )
            } else {
                SummaryConnectionTestResult::failure(
                    "request_failed",
                    "连接测试失败，请稍后重试。",
                    None,
                )
            };
            Ok(result)
        }
    }
}

#[cfg(test)]
mod summary_security_tests {
    use super::*;

    #[test]
    fn model_config_serializes_presence_only() {
        let config = ModelConfig {
            provider: "openai".to_string(),
            model: "test-model".to_string(),
            whisper_model: "small".to_string(),
            has_api_key: true,
            api_key_configured: SummaryApiKeyConfigured {
                openai: true,
                ..SummaryApiKeyConfigured::default()
            },
            ollama_endpoint: None,
        };
        let value = serde_json::to_value(config).unwrap();
        assert!(value.get("apiKey").is_none());
        assert_eq!(value["hasApiKey"], true);
        assert_eq!(value["apiKeyConfigured"]["openai"], true);
    }

    #[test]
    fn custom_endpoint_requires_tls_except_for_loopback() {
        assert_eq!(
            normalize_custom_openai_endpoint("https://example.invalid/v1/").unwrap(),
            "https://example.invalid/v1"
        );
        assert!(normalize_custom_openai_endpoint("http://localhost:8000/v1").is_ok());
        assert!(normalize_custom_openai_endpoint("http://127.0.0.1:8000/v1").is_ok());
        assert!(normalize_custom_openai_endpoint("http://[::1]:8000/v1").is_ok());
        assert!(normalize_custom_openai_endpoint("http://example.invalid/v1").is_err());
    }

    #[test]
    fn custom_endpoint_rejects_credential_bearing_url_parts() {
        assert!(normalize_custom_openai_endpoint("https://user@example.invalid/v1").is_err());
        assert!(
            normalize_custom_openai_endpoint("https://example.invalid/v1?token=secret").is_err()
        );
        assert!(normalize_custom_openai_endpoint("https://example.invalid/v1#secret").is_err());
    }

    #[test]
    fn public_connection_result_never_contains_provider_body() {
        const SECRET: &str = "sk-stage5-fake-secret";
        let raw_body = format!("unauthorized: {SECRET}");
        let result = summary_connection_http_failure(reqwest::StatusCode::UNAUTHORIZED);
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains(SECRET));
        assert!(!serialized.contains(&raw_body));
        assert_eq!(result.code, "authentication_failed");
    }
}

#[cfg(test)]
mod transcript_connection_tests {
    use super::*;

    fn config_for(provider: TranscriptProvider) -> TranscriptConfigView {
        let mut config = TranscriptConfigView::default();
        config.provider = provider.as_str().to_string();
        config.model = match provider {
            TranscriptProvider::Deepgram => {
                config.streaming_config.providers.deepgram.model.clone()
            }
            TranscriptProvider::OpenAi => config.streaming_config.providers.openai.model.clone(),
            TranscriptProvider::LocalWhisper | TranscriptProvider::Parakeet => {
                provider.default_model().to_string()
            }
        };
        config
    }

    #[test]
    fn connection_result_uses_camel_case_and_omits_missing_latency() {
        let failed = TranscriptConnectionTestResult::failure("invalid_config", "配置无效");
        let value = serde_json::to_value(failed).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["code"], "invalid_config");
        assert_eq!(value["message"], "配置无效");
        assert!(value.get("latencyMs").is_none());

        let success = serde_json::to_value(TranscriptConnectionTestResult::success(42)).unwrap();
        assert_eq!(success["latencyMs"], 42);
    }

    #[test]
    fn prepares_deepgram_options_without_credentials() {
        let mut config = config_for(TranscriptProvider::Deepgram);
        config.streaming_config.providers.deepgram.latency_mode = StreamingLatencyMode::Minimal;
        config.streaming_config.providers.deepgram.keywords = vec!["Meetily".to_string()];

        let prepared =
            prepare_transcript_connection_options(TranscriptProvider::Deepgram, config).unwrap();
        let TranscriptConnectionOptions::Deepgram(options) = prepared else {
            panic!("expected Deepgram options");
        };
        assert_eq!(options.model, "nova-3");
        assert_eq!(options.endpointing_ms, 100);
        assert_eq!(options.keyterms, vec!["Meetily"]);
    }

    #[test]
    fn prepares_openai_options_without_audio_or_speaker_fabrication() {
        let mut config = config_for(TranscriptProvider::OpenAi);
        config.streaming_config.providers.openai.language = "ja".to_string();
        config.streaming_config.providers.openai.latency_mode = StreamingLatencyMode::Balanced;

        let prepared =
            prepare_transcript_connection_options(TranscriptProvider::OpenAi, config).unwrap();
        let TranscriptConnectionOptions::OpenAi(options) = prepared else {
            panic!("expected OpenAI options");
        };
        assert_eq!(options.languages, vec!["ja"]);
        assert_eq!(options.delay, Some(OpenAiRealtimeDelay::Medium));
        assert_eq!(
            options.turn_detection,
            OpenAiRealtimeTurnDetection::Disabled
        );
    }

    #[test]
    fn rejects_provider_and_model_mismatches_before_network_access() {
        let mismatch = match prepare_transcript_connection_options(
            TranscriptProvider::OpenAi,
            config_for(TranscriptProvider::Deepgram),
        ) {
            Err(result) => result,
            Ok(_) => panic!("provider mismatch must fail before network access"),
        };
        assert_eq!(mismatch.code, "provider_mismatch");

        let mut config = config_for(TranscriptProvider::Deepgram);
        config.model = "different-model".to_string();
        let mismatch =
            match prepare_transcript_connection_options(TranscriptProvider::Deepgram, config) {
                Err(result) => result,
                Ok(_) => panic!("model mismatch must fail before network access"),
            };
        assert_eq!(mismatch.code, "model_mismatch");
    }

    #[test]
    fn rejects_invalid_openai_diarization_before_network_access() {
        let mut config = config_for(TranscriptProvider::OpenAi);
        config.streaming_config.providers.openai.diarization = true;
        let result = match prepare_transcript_connection_options(TranscriptProvider::OpenAi, config)
        {
            Err(result) => result,
            Ok(_) => panic!("OpenAI diarization must fail before network access"),
        };
        assert_eq!(result.code, "invalid_config");
    }

    #[test]
    fn connector_configuration_errors_are_sanitized() {
        let result = deepgram_connection_failure(DeepgramError::Configuration(
            crate::audio::transcription::streaming::DeepgramConfigError::InvalidModel,
        ));
        assert_eq!(result.code, "invalid_config");
        assert!(!result.message.contains("sk-"));

        let result = openai_connection_failure(OpenAiRealtimeError::Configuration(
            crate::audio::transcription::streaming::OpenAiRealtimeConfigError::InvalidModel,
        ));
        assert_eq!(result.code, "invalid_config");
        assert!(!result.message.contains("Authorization"));
    }
}
