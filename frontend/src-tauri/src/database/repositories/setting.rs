use crate::database::models::{
    Setting, StreamingProviderConfig, TranscriptApiKeyConfigured, TranscriptConfigView,
    TranscriptProvider, TranscriptSetting, TranscriptStreamingConfig,
    TRANSCRIPT_STREAMING_CONFIG_SCHEMA_VERSION,
};
use crate::summary::CustomOpenAIConfig;
use sqlx::SqlitePool;
use std::collections::HashSet;
use url::Url;

pub struct SettingsRepository;

const MAX_SUMMARY_API_KEY_BYTES: usize = 8_192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SummaryApiKeyProvider {
    OpenAi,
    Claude,
    Groq,
    OpenRouter,
    CustomOpenAi,
}

impl SummaryApiKeyProvider {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "openai" => Ok(Self::OpenAi),
            "claude" => Ok(Self::Claude),
            "groq" => Ok(Self::Groq),
            "openrouter" => Ok(Self::OpenRouter),
            "custom-openai" => Ok(Self::CustomOpenAi),
            "ollama" | "builtin-ai" => Err("本地总结提供商不使用 API 密钥。".to_string()),
            _ => Err("不支持的总结服务提供商。".to_string()),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Claude => "claude",
            Self::Groq => "groq",
            Self::OpenRouter => "openrouter",
            Self::CustomOpenAi => "custom-openai",
        }
    }

    fn column(self) -> Option<&'static str> {
        match self {
            Self::OpenAi => Some("openaiApiKey"),
            Self::Claude => Some("anthropicApiKey"),
            Self::Groq => Some("groqApiKey"),
            Self::OpenRouter => Some("openRouterApiKey"),
            Self::CustomOpenAi => None,
        }
    }
}

/// Rust-only summary credential. It cannot be serialized or displayed, and
/// its hand-written `Debug` output never contains credential bytes.
pub(crate) struct SummaryApiKey(String);

impl SummaryApiKey {
    pub(crate) fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SummaryApiKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SummaryApiKey")
            .field("configured", &true)
            .finish()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryApiKeyConfigured {
    pub openai: bool,
    pub claude: bool,
    pub groq: bool,
    pub openrouter: bool,
    pub custom_openai: bool,
}

impl SummaryApiKeyConfigured {
    pub fn for_provider(&self, provider: &str) -> bool {
        match provider {
            "openai" => self.openai,
            "claude" => self.claude,
            "groq" => self.groq,
            "openrouter" => self.openrouter,
            "custom-openai" => self.custom_openai,
            _ => false,
        }
    }
}

pub(crate) fn normalize_summary_api_key(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("API 密钥不能为空。".to_string());
    }
    if value.len() > MAX_SUMMARY_API_KEY_BYTES {
        return Err(format!(
            "API 密钥长度不能超过 {MAX_SUMMARY_API_KEY_BYTES} 字节。"
        ));
    }
    if !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err("API 密钥只能包含不带空格的 ASCII 可打印字符。".to_string());
    }
    Ok(value.to_string())
}

/// Owned secret used only by Rust transcription workers.
///
/// There is intentionally no `Debug`, `Display`, or `Serialize`
/// implementation, which makes accidental logging and IPC serialization a
/// compile-time error.
pub(crate) struct TranscriptApiKey(String);

impl TranscriptApiKey {
    pub(crate) fn expose_secret(&self) -> &str {
        &self.0
    }
}

/// Private settings used to start a transcription provider. This type must
/// remain Rust-only because it can contain a credential.
pub(crate) struct TranscriptRuntimeConfig {
    pub(crate) provider: TranscriptProvider,
    pub(crate) model: String,
    pub(crate) streaming_config: TranscriptStreamingConfig,
    pub(crate) api_key: Option<TranscriptApiKey>,
}

fn protocol_error(message: impl Into<String>) -> sqlx::Error {
    sqlx::Error::Protocol(message.into())
}

fn transcript_api_key_column(provider: TranscriptProvider) -> Result<&'static str, String> {
    match provider {
        TranscriptProvider::Deepgram => Ok("deepgramApiKey"),
        TranscriptProvider::OpenAi => Ok("openaiApiKey"),
        TranscriptProvider::LocalWhisper | TranscriptProvider::Parakeet => Err(format!(
            "Provider '{}' does not accept an API key",
            provider.as_str()
        )),
    }
}

fn secret_from_stored(value: Option<String>) -> Option<TranscriptApiKey> {
    value.and_then(|value| {
        if value.trim().is_empty() {
            None
        } else {
            Some(TranscriptApiKey(value))
        }
    })
}

fn key_is_configured(value: &Option<String>) -> bool {
    value
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
}

fn validate_model(value: &str, default: &str) -> Result<String, String> {
    let value = value.trim();
    let value = if value.is_empty() { default } else { value };
    let length = value.chars().count();
    if length > 128 {
        return Err("Transcription model name must be at most 128 characters".to_string());
    }
    if value.chars().any(|character| {
        character.is_control() || character.is_whitespace() || matches!(character, '?' | '#' | '&')
    }) {
        return Err("Transcription model name contains unsupported characters".to_string());
    }
    Ok(value.to_string())
}

fn normalize_language(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok("auto".to_string());
    }
    if value.chars().count() > 35
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err("Streaming language must be 'auto' or a short language tag".to_string());
    }
    Ok(value.to_string())
}

fn normalize_endpoint_override(value: Option<String>) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }

    let endpoint = Url::parse(value)
        .map_err(|_| "Endpoint override must be a valid absolute WebSocket URL".to_string())?;
    if endpoint.scheme() != "wss" {
        return Err("Endpoint override must use wss://".to_string());
    }
    if endpoint.host_str().is_none() {
        return Err("Endpoint override must include a host".to_string());
    }
    if !endpoint.username().is_empty() || endpoint.password().is_some() {
        return Err("Endpoint override must not include user information".to_string());
    }
    if endpoint.fragment().is_some() {
        return Err("Endpoint override must not include a URL fragment".to_string());
    }

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
    for (name, _) in endpoint.query_pairs() {
        let name = name.to_ascii_lowercase();
        if SECRET_QUERY_MARKERS
            .iter()
            .any(|marker| name.contains(marker))
        {
            return Err(
                "Endpoint override must not put credentials in the query string".to_string(),
            );
        }
    }

    Ok(Some(endpoint.to_string()))
}

fn normalize_keywords(values: Vec<String>) -> Result<Vec<String>, String> {
    const MAX_KEYWORDS: usize = 100;
    const MAX_KEYWORD_CHARS: usize = 80;
    const MAX_TOTAL_CHARS: usize = 2_000;

    if values.len() > MAX_KEYWORDS {
        return Err(format!(
            "At most {MAX_KEYWORDS} streaming keywords are allowed"
        ));
    }

    let mut normalized = Vec::with_capacity(values.len());
    let mut seen = HashSet::with_capacity(values.len());
    let mut total_chars = 0usize;
    for value in values {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let length = value.chars().count();
        if length > MAX_KEYWORD_CHARS || value.chars().any(char::is_control) {
            return Err(format!(
                "Each streaming keyword must be at most {MAX_KEYWORD_CHARS} characters"
            ));
        }
        total_chars += length;
        if total_chars > MAX_TOTAL_CHARS {
            return Err(format!(
                "Streaming keywords must total at most {MAX_TOTAL_CHARS} characters"
            ));
        }
        if seen.insert(value.to_string()) {
            normalized.push(value.to_string());
        }
    }

    Ok(normalized)
}

fn normalize_provider_config(
    mut config: StreamingProviderConfig,
    default_model: &str,
    supports_diarization: bool,
) -> Result<StreamingProviderConfig, String> {
    config.endpoint_override = normalize_endpoint_override(config.endpoint_override)?;
    config.model = validate_model(&config.model, default_model)?;
    config.language = normalize_language(&config.language)?;
    config.keywords = normalize_keywords(config.keywords)?;
    if config.diarization && !supports_diarization {
        return Err("OpenAI streaming transcription does not support diarization".to_string());
    }
    Ok(config)
}

pub fn validate_and_normalize_streaming_config(
    mut config: TranscriptStreamingConfig,
) -> Result<TranscriptStreamingConfig, String> {
    if config.schema_version != TRANSCRIPT_STREAMING_CONFIG_SCHEMA_VERSION {
        return Err(format!(
            "Unsupported streaming configuration schema version {}",
            config.schema_version
        ));
    }

    config.providers.deepgram = normalize_provider_config(
        config.providers.deepgram,
        crate::database::models::DEFAULT_DEEPGRAM_TRANSCRIPT_MODEL,
        true,
    )?;
    config.providers.openai = normalize_provider_config(
        config.providers.openai,
        crate::database::models::DEFAULT_OPENAI_TRANSCRIPT_MODEL,
        false,
    )?;
    config.fallback.model = match config.fallback.provider {
        crate::database::models::TranscriptFallbackProvider::Parakeet => validate_model(
            &config.fallback.model,
            crate::config::DEFAULT_PARAKEET_MODEL,
        )?,
        crate::database::models::TranscriptFallbackProvider::LocalWhisper => {
            validate_model(&config.fallback.model, crate::config::DEFAULT_WHISPER_MODEL)?
        }
    };

    Ok(config)
}

pub fn normalize_transcript_api_key(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("API key must not be empty".to_string());
    }
    if value.chars().count() > 8_192 || value.chars().any(char::is_control) {
        return Err("API key is invalid or too long".to_string());
    }
    Ok(value.to_string())
}

fn transcript_view_from_row(row: &TranscriptSetting) -> Result<TranscriptConfigView, String> {
    let is_retired_provider = matches!(row.provider.as_str(), "elevenLabs" | "elevenlabs" | "groq");
    let provider = TranscriptProvider::parse_stored(&row.provider)?;
    let model = if is_retired_provider {
        provider.default_model().to_string()
    } else {
        validate_model(&row.model, provider.default_model())?
    };

    let mut streaming_config = match row.streaming_config.as_deref().map(str::trim) {
        Some(value) if !value.is_empty() => serde_json::from_str(value)
            .map_err(|_| "Stored streaming transcription config is invalid".to_string())?,
        _ => TranscriptStreamingConfig::default(),
    };
    streaming_config = validate_and_normalize_streaming_config(streaming_config)?;

    // `model` was the only model field in older databases. Keep it as the
    // active provider's source of truth while retaining independent inactive
    // provider selections in the new JSON document.
    match provider {
        TranscriptProvider::Deepgram => streaming_config.providers.deepgram.model = model.clone(),
        TranscriptProvider::OpenAi => streaming_config.providers.openai.model = model.clone(),
        TranscriptProvider::LocalWhisper | TranscriptProvider::Parakeet => {}
    }

    let api_key_configured = TranscriptApiKeyConfigured {
        deepgram: key_is_configured(&row.deepgram_api_key),
        openai: key_is_configured(&row.openai_api_key),
    };
    let has_api_key = match provider {
        TranscriptProvider::Deepgram => api_key_configured.deepgram,
        TranscriptProvider::OpenAi => api_key_configured.openai,
        TranscriptProvider::LocalWhisper | TranscriptProvider::Parakeet => false,
    };

    Ok(TranscriptConfigView {
        provider: provider.as_str().to_string(),
        model,
        streaming_config,
        has_api_key,
        api_key_configured,
    })
}

// Current transcript providers: localWhisper, parakeet, deepgram, openai.
// Summary providers: openai, claude, ollama, groq, added openrouter

impl SettingsRepository {
    pub(crate) async fn get_model_config(
        pool: &SqlitePool,
    ) -> std::result::Result<Option<Setting>, sqlx::Error> {
        let setting = sqlx::query_as::<_, Setting>("SELECT * FROM settings LIMIT 1")
            .fetch_optional(pool)
            .await?;
        Ok(setting)
    }

    pub async fn save_model_config(
        pool: &SqlitePool,
        provider: &str,
        model: &str,
        whisper_model: &str,
        ollama_endpoint: Option<&str>,
    ) -> std::result::Result<(), sqlx::Error> {
        // Using id '1' for backward compatibility
        sqlx::query(
            r#"
            INSERT INTO settings (id, provider, model, whisperModel, ollamaEndpoint)
            VALUES ('1', $1, $2, $3, $4)
            ON CONFLICT(id) DO UPDATE SET
                provider = excluded.provider,
                model = excluded.model,
                whisperModel = excluded.whisperModel,
                ollamaEndpoint = excluded.ollamaEndpoint
            "#,
        )
        .bind(provider)
        .bind(model)
        .bind(whisper_model)
        .bind(ollama_endpoint)
        .execute(pool)
        .await?;

        Ok(())
    }

    pub(crate) async fn set_summary_api_key(
        pool: &SqlitePool,
        provider: SummaryApiKeyProvider,
        api_key: &str,
    ) -> std::result::Result<(), sqlx::Error> {
        let api_key = normalize_summary_api_key(api_key).map_err(protocol_error)?;

        if provider == SummaryApiKeyProvider::CustomOpenAi {
            let Some(mut config) = Self::get_custom_openai_config(pool).await? else {
                return Err(protocol_error("请先保存自定义 OpenAI 的服务地址和模型。"));
            };
            config.api_key = Some(api_key);
            return Self::save_custom_openai_config(pool, &config).await;
        }

        let api_key_column = provider
            .column()
            .expect("custom provider handled before selecting a column");
        let query = format!(
            r#"
            INSERT INTO settings (id, provider, model, whisperModel, "{api_key_column}")
            VALUES ('1', 'openai', 'gpt-4o-2024-11-20', 'large-v3', $1)
            ON CONFLICT(id) DO UPDATE SET
                "{api_key_column}" = $1
            "#
        );
        sqlx::query(&query).bind(api_key).execute(pool).await?;
        Ok(())
    }

    pub(crate) async fn get_summary_api_key(
        pool: &SqlitePool,
        provider: SummaryApiKeyProvider,
    ) -> std::result::Result<Option<SummaryApiKey>, sqlx::Error> {
        let value = if provider == SummaryApiKeyProvider::CustomOpenAi {
            Self::get_custom_openai_config(pool)
                .await?
                .and_then(|config| config.api_key)
        } else {
            let api_key_column = provider
                .column()
                .expect("custom provider handled before selecting a column");
            let query = format!("SELECT \"{api_key_column}\" FROM settings WHERE id = '1' LIMIT 1");
            sqlx::query_scalar::<_, Option<String>>(&query)
                .fetch_optional(pool)
                .await?
                .flatten()
        };

        Ok(value.and_then(|value| {
            let value = value.trim();
            (!value.is_empty()).then(|| SummaryApiKey(value.to_string()))
        }))
    }

    pub(crate) async fn get_summary_api_key_configured(
        pool: &SqlitePool,
    ) -> std::result::Result<SummaryApiKeyConfigured, sqlx::Error> {
        use sqlx::Row;

        let row = sqlx::query(
            r#"
            SELECT openaiApiKey, anthropicApiKey, groqApiKey, openRouterApiKey,
                   customOpenAIConfig
            FROM settings
            WHERE id = '1'
            LIMIT 1
            "#,
        )
        .fetch_optional(pool)
        .await?;

        let Some(row) = row else {
            return Ok(SummaryApiKeyConfigured::default());
        };
        let custom_openai = row
            .get::<Option<String>, _>("customOpenAIConfig")
            .map(|json| serde_json::from_str::<CustomOpenAIConfig>(&json))
            .transpose()
            .map_err(|_| protocol_error("自定义 OpenAI 配置无法解析。"))?
            .and_then(|config| config.api_key);

        Ok(SummaryApiKeyConfigured {
            openai: key_is_configured(&row.get("openaiApiKey")),
            claude: key_is_configured(&row.get("anthropicApiKey")),
            groq: key_is_configured(&row.get("groqApiKey")),
            openrouter: key_is_configured(&row.get("openRouterApiKey")),
            custom_openai: key_is_configured(&custom_openai),
        })
    }

    /// Load the credential-bearing row. Keep this method private to the
    /// repository so callers must choose either the safe public view or the
    /// explicitly private runtime configuration.
    async fn get_transcript_setting(
        pool: &SqlitePool,
    ) -> std::result::Result<Option<TranscriptSetting>, sqlx::Error> {
        sqlx::query_as::<_, TranscriptSetting>(
            r#"
            SELECT id, provider, model,
                   whisperApiKey, deepgramApiKey, elevenLabsApiKey,
                   groqApiKey, openaiApiKey, streamingConfig
            FROM transcript_settings
            WHERE id = '1'
            LIMIT 1
            "#,
        )
        .fetch_optional(pool)
        .await
    }

    /// Safe WebView-facing projection. API keys are reduced to presence flags
    /// before the credential-bearing database row leaves this function.
    pub async fn get_transcript_config_view(
        pool: &SqlitePool,
    ) -> std::result::Result<TranscriptConfigView, sqlx::Error> {
        let Some(row) = Self::get_transcript_setting(pool).await? else {
            return Ok(TranscriptConfigView::default());
        };

        transcript_view_from_row(&row).map_err(protocol_error)
    }

    /// Rust-only configuration for provider workers. The returned type cannot
    /// be debug-printed or serialized.
    pub(crate) async fn get_transcript_runtime_config(
        pool: &SqlitePool,
    ) -> std::result::Result<TranscriptRuntimeConfig, sqlx::Error> {
        let Some(row) = Self::get_transcript_setting(pool).await? else {
            let view = TranscriptConfigView::default();
            return Ok(TranscriptRuntimeConfig {
                provider: TranscriptProvider::Parakeet,
                model: view.model,
                streaming_config: view.streaming_config,
                api_key: None,
            });
        };

        let view = transcript_view_from_row(&row).map_err(protocol_error)?;
        let provider = TranscriptProvider::parse_request(&view.provider).map_err(protocol_error)?;
        let api_key = match provider {
            TranscriptProvider::Deepgram => secret_from_stored(row.deepgram_api_key),
            TranscriptProvider::OpenAi => secret_from_stored(row.openai_api_key),
            TranscriptProvider::LocalWhisper | TranscriptProvider::Parakeet => None,
        };

        Ok(TranscriptRuntimeConfig {
            provider,
            model: view.model,
            streaming_config: view.streaming_config,
            api_key,
        })
    }

    /// Load one streaming-provider credential for a Rust-only operation such
    /// as a connection test. The secret-bearing return type cannot be logged
    /// or serialized, and local providers fail closed instead of selecting an
    /// unrelated credential column.
    pub(crate) async fn get_transcript_api_key_for_provider(
        pool: &SqlitePool,
        provider: TranscriptProvider,
    ) -> std::result::Result<Option<TranscriptApiKey>, sqlx::Error> {
        if !provider.requires_api_key() {
            return Err(protocol_error(format!(
                "Provider '{}' does not accept an API key",
                provider.as_str()
            )));
        }

        let Some(row) = Self::get_transcript_setting(pool).await? else {
            return Ok(None);
        };
        Ok(match provider {
            TranscriptProvider::Deepgram => secret_from_stored(row.deepgram_api_key),
            TranscriptProvider::OpenAi => secret_from_stored(row.openai_api_key),
            TranscriptProvider::LocalWhisper | TranscriptProvider::Parakeet => {
                unreachable!("local providers were rejected before reading transcript credentials")
            }
        })
    }

    /// Backwards-compatible helper for older local-model callers. Streaming
    /// settings are omitted, so an existing JSON value is retained.
    pub async fn save_transcript_config(
        pool: &SqlitePool,
        provider: &str,
        model: &str,
    ) -> std::result::Result<(), sqlx::Error> {
        Self::save_transcript_config_with_streaming(pool, provider, model, None).await
    }

    /// Atomically save the ordinary transcription selection and its
    /// non-secret streaming JSON. Existing key columns are never mentioned by
    /// this statement and therefore cannot be overwritten.
    pub async fn save_transcript_config_with_streaming(
        pool: &SqlitePool,
        provider: &str,
        model: &str,
        streaming_config_json: Option<&str>,
    ) -> std::result::Result<(), sqlx::Error> {
        let provider = TranscriptProvider::parse_request(provider).map_err(protocol_error)?;
        let model = validate_model(model, provider.default_model()).map_err(protocol_error)?;

        let mut transaction = pool.begin().await?;
        sqlx::query(
            r#"
            INSERT INTO transcript_settings (id, provider, model, streamingConfig)
            VALUES ('1', $1, $2, $3)
            ON CONFLICT(id) DO UPDATE SET
                provider = excluded.provider,
                model = excluded.model,
                streamingConfig = COALESCE(
                    excluded.streamingConfig,
                    transcript_settings.streamingConfig
                )
            "#,
        )
        .bind(provider.as_str())
        .bind(model)
        .bind(streaming_config_json)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;

        Ok(())
    }

    pub async fn set_transcript_api_key(
        pool: &SqlitePool,
        provider: TranscriptProvider,
        api_key: &str,
    ) -> std::result::Result<(), sqlx::Error> {
        let api_key_column = transcript_api_key_column(provider).map_err(protocol_error)?;
        let query = format!(
            r#"
            INSERT INTO transcript_settings (id, provider, model, "{api_key_column}")
            VALUES ('1', 'parakeet', $1, $2)
            ON CONFLICT(id) DO UPDATE SET
                "{api_key_column}" = $2
            "#
        );
        sqlx::query(&query)
            .bind(crate::config::DEFAULT_PARAKEET_MODEL)
            .bind(api_key)
            .execute(pool)
            .await?;

        Ok(())
    }

    pub async fn delete_transcript_api_key(
        pool: &SqlitePool,
        provider: TranscriptProvider,
    ) -> std::result::Result<(), sqlx::Error> {
        let api_key_column = transcript_api_key_column(provider).map_err(protocol_error)?;
        let query =
            format!("UPDATE transcript_settings SET \"{api_key_column}\" = NULL WHERE id = '1'");
        sqlx::query(&query).execute(pool).await?;
        Ok(())
    }

    pub(crate) async fn delete_summary_api_key(
        pool: &SqlitePool,
        provider: SummaryApiKeyProvider,
    ) -> std::result::Result<(), sqlx::Error> {
        if provider == SummaryApiKeyProvider::CustomOpenAi {
            if let Some(mut config) = Self::get_custom_openai_config(pool).await? {
                config.api_key = None;
                Self::save_custom_openai_config(pool, &config).await?;
            }
            return Ok(());
        }

        let api_key_column = provider
            .column()
            .expect("custom provider handled before selecting a column");
        let query = format!("UPDATE settings SET \"{api_key_column}\" = NULL WHERE id = '1'");
        sqlx::query(&query).execute(pool).await?;
        Ok(())
    }

    // ===== CUSTOM OPENAI CONFIG METHODS =====

    /// Gets the custom OpenAI configuration from JSON
    ///
    /// # Returns
    /// * `Ok(Some(CustomOpenAIConfig))` - Config exists and is valid JSON
    /// * `Ok(None)` - No config stored
    /// * `Err(sqlx::Error)` - Database error
    pub async fn get_custom_openai_config(
        pool: &SqlitePool,
    ) -> std::result::Result<Option<CustomOpenAIConfig>, sqlx::Error> {
        use sqlx::Row;

        let row = sqlx::query(
            r#"
            SELECT customOpenAIConfig
            FROM settings
            WHERE id = '1'
            LIMIT 1
            "#,
        )
        .fetch_optional(pool)
        .await?;

        match row {
            Some(record) => {
                let config_json: Option<String> = record.get("customOpenAIConfig");

                if let Some(json) = config_json {
                    // Parse JSON into CustomOpenAIConfig
                    let config: CustomOpenAIConfig = serde_json::from_str(&json).map_err(|e| {
                        sqlx::Error::Protocol(
                            format!("Invalid JSON in customOpenAIConfig: {}", e).into(),
                        )
                    })?;

                    Ok(Some(config))
                } else {
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }

    /// Saves the custom OpenAI configuration as JSON
    ///
    /// # Arguments
    /// * `pool` - Database connection pool
    /// * `config` - CustomOpenAIConfig to save (includes endpoint, apiKey, model, maxTokens, temperature, topP)
    ///
    /// # Returns
    /// * `Ok(())` - Config saved successfully
    /// * `Err(sqlx::Error)` - Database or JSON serialization error
    pub async fn save_custom_openai_config(
        pool: &SqlitePool,
        config: &CustomOpenAIConfig,
    ) -> std::result::Result<(), sqlx::Error> {
        // Serialize config to JSON
        let config_json = serde_json::to_string(config).map_err(|e| {
            sqlx::Error::Protocol(format!("Failed to serialize config to JSON: {}", e).into())
        })?;

        // Upsert into settings table
        sqlx::query(
            r#"
            INSERT INTO settings (id, provider, model, whisperModel, customOpenAIConfig)
            VALUES ('1', 'custom-openai', $1, 'large-v3', $2)
            ON CONFLICT(id) DO UPDATE SET
                customOpenAIConfig = excluded.customOpenAIConfig
            "#,
        )
        .bind(&config.model)
        .bind(config_json)
        .execute(pool)
        .await?;

        Ok(())
    }
}

#[cfg(test)]
mod transcript_config_tests {
    use super::*;
    use crate::database::models::{
        StreamingLatencyMode, TranscriptFallbackConfig, TranscriptFallbackProvider,
    };
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("create in-memory database");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");
        pool
    }

    fn streaming_config() -> TranscriptStreamingConfig {
        TranscriptStreamingConfig {
            schema_version: TRANSCRIPT_STREAMING_CONFIG_SCHEMA_VERSION,
            providers: crate::database::models::StreamingProviderConfigs {
                deepgram: StreamingProviderConfig {
                    endpoint_override: Some(
                        "wss://api.deepgram.com/v1/listen?version=1".to_string(),
                    ),
                    model: "nova-3".to_string(),
                    language: "zh-CN".to_string(),
                    diarization: true,
                    latency_mode: StreamingLatencyMode::Minimal,
                    keywords: vec!["Meetily".to_string(), "会议".to_string()],
                },
                openai: StreamingProviderConfig {
                    endpoint_override: None,
                    model: "gpt-live-transcribe".to_string(),
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

    #[tokio::test]
    async fn public_view_never_serializes_the_stored_api_key() {
        let pool = test_pool().await;
        let config = validate_and_normalize_streaming_config(streaming_config()).unwrap();
        let config_json = serde_json::to_string(&config).unwrap();

        SettingsRepository::save_transcript_config_with_streaming(
            &pool,
            "deepgram",
            "nova-3",
            Some(&config_json),
        )
        .await
        .unwrap();
        SettingsRepository::set_transcript_api_key(
            &pool,
            TranscriptProvider::Deepgram,
            "stage3-test-secret",
        )
        .await
        .unwrap();

        let view = SettingsRepository::get_transcript_config_view(&pool)
            .await
            .unwrap();
        let serialized = serde_json::to_string(&view).unwrap();
        assert!(view.has_api_key);
        assert!(view.api_key_configured.deepgram);
        assert!(!view.api_key_configured.openai);
        assert!(!serialized.contains("stage3-test-secret"));
        assert!(!serialized.contains("\"apiKey\":"));

        let runtime = SettingsRepository::get_transcript_runtime_config(&pool)
            .await
            .unwrap();
        assert_eq!(runtime.provider, TranscriptProvider::Deepgram);
        assert_eq!(runtime.model, "nova-3");
        assert_eq!(runtime.streaming_config.schema_version, 1);
        assert_eq!(
            runtime
                .api_key
                .as_ref()
                .map(TranscriptApiKey::expose_secret),
            Some("stage3-test-secret")
        );
    }

    #[tokio::test]
    async fn ordinary_config_save_preserves_provider_keys() {
        let pool = test_pool().await;
        SettingsRepository::set_transcript_api_key(
            &pool,
            TranscriptProvider::OpenAi,
            "stage3-openai-secret",
        )
        .await
        .unwrap();

        let config = serde_json::to_string(&streaming_config()).unwrap();
        SettingsRepository::save_transcript_config_with_streaming(
            &pool,
            "openai",
            "gpt-live-transcribe",
            Some(&config),
        )
        .await
        .unwrap();
        SettingsRepository::save_transcript_config(
            &pool,
            "parakeet",
            crate::config::DEFAULT_PARAKEET_MODEL,
        )
        .await
        .unwrap();
        SettingsRepository::save_transcript_config_with_streaming(
            &pool,
            "openai",
            "gpt-live-transcribe",
            Some(&config),
        )
        .await
        .unwrap();

        let runtime = SettingsRepository::get_transcript_runtime_config(&pool)
            .await
            .unwrap();
        assert_eq!(
            runtime
                .api_key
                .as_ref()
                .map(TranscriptApiKey::expose_secret),
            Some("stage3-openai-secret")
        );

        SettingsRepository::delete_transcript_api_key(&pool, TranscriptProvider::OpenAi)
            .await
            .unwrap();
        let view = SettingsRepository::get_transcript_config_view(&pool)
            .await
            .unwrap();
        assert!(!view.has_api_key);
        assert!(!view.api_key_configured.openai);
    }

    #[tokio::test]
    async fn stored_legacy_providers_are_explicitly_migrated_but_unknown_values_fail() {
        let pool = test_pool().await;
        sqlx::query(
            "INSERT INTO transcript_settings (id, provider, model) VALUES ('1', 'groq', 'old-model')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let view = SettingsRepository::get_transcript_config_view(&pool)
            .await
            .unwrap();
        assert_eq!(view.provider, "parakeet");
        assert_eq!(view.model, crate::config::DEFAULT_PARAKEET_MODEL);

        sqlx::query("UPDATE transcript_settings SET provider = 'mystery-provider' WHERE id = '1'")
            .execute(&pool)
            .await
            .unwrap();
        let error = SettingsRepository::get_transcript_config_view(&pool)
            .await
            .expect_err("unknown stored provider must fail closed");
        assert!(error.to_string().contains("not recognized"));
        assert!(TranscriptProvider::parse_request("mystery-provider").is_err());
    }

    #[test]
    fn streaming_validation_rejects_insecure_endpoints_and_unbounded_input() {
        let mut insecure = streaming_config();
        insecure.providers.deepgram.endpoint_override =
            Some("ws://api.deepgram.com/v1/listen".to_string());
        assert!(validate_and_normalize_streaming_config(insecure).is_err());

        let mut userinfo = streaming_config();
        userinfo.providers.deepgram.endpoint_override =
            Some("wss://user:password@api.deepgram.com/v1/listen".to_string());
        assert!(validate_and_normalize_streaming_config(userinfo).is_err());

        let mut secret_query = streaming_config();
        secret_query.providers.deepgram.endpoint_override =
            Some("wss://api.deepgram.com/v1/listen?api_key=secret".to_string());
        assert!(validate_and_normalize_streaming_config(secret_query).is_err());

        let mut too_many_keywords = streaming_config();
        too_many_keywords.providers.deepgram.keywords =
            (0..101).map(|index| format!("keyword-{index}")).collect();
        assert!(validate_and_normalize_streaming_config(too_many_keywords).is_err());

        let mut unsupported_diarization = streaming_config();
        unsupported_diarization.providers.openai.diarization = true;
        assert!(validate_and_normalize_streaming_config(unsupported_diarization).is_err());
    }

    #[test]
    fn api_key_validation_rejects_empty_and_control_characters() {
        assert!(normalize_transcript_api_key("  ").is_err());
        assert!(normalize_transcript_api_key("secret\nheader").is_err());
        assert_eq!(
            normalize_transcript_api_key("  synthetic-key  ").unwrap(),
            "synthetic-key"
        );
    }
}

#[cfg(test)]
mod summary_secret_tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("create in-memory database");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");
        pool
    }

    #[test]
    fn summary_key_normalization_is_bounded_and_header_safe() {
        assert!(normalize_summary_api_key("  ").is_err());
        assert!(normalize_summary_api_key("secret\nheader").is_err());
        assert!(normalize_summary_api_key("密钥").is_err());
        assert!(normalize_summary_api_key(&"x".repeat(MAX_SUMMARY_API_KEY_BYTES + 1)).is_err());
        assert_eq!(
            normalize_summary_api_key("  sk-stage5-fake-secret  ").unwrap(),
            "sk-stage5-fake-secret"
        );
    }

    #[tokio::test]
    async fn public_presence_and_debug_never_expose_stored_keys() {
        const SECRET: &str = "sk-stage5-fake-secret";
        let pool = test_pool().await;
        SettingsRepository::set_summary_api_key(&pool, SummaryApiKeyProvider::OpenAi, SECRET)
            .await
            .unwrap();

        let configured = SettingsRepository::get_summary_api_key_configured(&pool)
            .await
            .unwrap();
        assert!(configured.openai);
        let json = serde_json::to_string(&configured).unwrap();
        assert!(!json.contains(SECRET));

        let runtime = SettingsRepository::get_summary_api_key(&pool, SummaryApiKeyProvider::OpenAi)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(runtime.expose_secret(), SECRET);
        assert!(!format!("{runtime:?}").contains(SECRET));
    }

    #[tokio::test]
    async fn deleting_custom_key_preserves_public_endpoint_settings() {
        const SECRET: &str = "sk-stage5-fake-secret";
        let pool = test_pool().await;
        let config = CustomOpenAIConfig {
            endpoint: "https://example.invalid/v1".to_string(),
            api_key: Some(SECRET.to_string()),
            model: "test-model".to_string(),
            max_tokens: Some(128),
            temperature: Some(0.2),
            top_p: Some(0.9),
        };
        SettingsRepository::save_custom_openai_config(&pool, &config)
            .await
            .unwrap();

        SettingsRepository::delete_summary_api_key(&pool, SummaryApiKeyProvider::CustomOpenAi)
            .await
            .unwrap();
        let stored = SettingsRepository::get_custom_openai_config(&pool)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.endpoint, config.endpoint);
        assert_eq!(stored.model, config.model);
        assert_eq!(stored.max_tokens, config.max_tokens);
        assert!(stored.api_key.is_none());
    }
}
