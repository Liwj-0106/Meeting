//! OpenAI-compatible live-summary provider.
//!
//! Credentials, prompt text, request JSON and response bodies remain in this
//! Rust-only module. The only value sent onward is a validated structured
//! provider response paired with its meeting scope.

use super::{
    LiveSummaryFrontendError, LiveSummaryItem, LiveSummaryProvider,
    LiveSummaryProviderAvailability, LiveSummaryProviderError, LiveSummaryProviderFactory,
    LiveSummaryProviderResponse, LiveSummaryProviderResult, LiveSummaryRequest,
};
use crate::api::{
    normalize_custom_openai_endpoint, normalize_custom_openai_model,
    validate_custom_openai_parameters,
};
use crate::database::repositories::setting::{
    normalize_summary_api_key, SettingsRepository, SummaryApiKeyProvider,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;
use url::Url;

const OPENAI_ENDPOINT: &str = "https://api.openai.com/v1";
const GROQ_ENDPOINT: &str = "https://api.groq.com/openai/v1";
const OPENROUTER_ENDPOINT: &str = "https://openrouter.ai/api/v1";
const MAX_HTTP_REQUEST_BYTES: usize = 512 * 1024;
const MAX_HTTP_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_PROVIDER_ITEMS: usize = 128;
const MAX_SOURCE_TEXT_CHARS: usize = 120_000;
const MAX_TEMPLATE_CHARS: usize = 16_000;
const MAX_CUSTOM_PROMPT_CHARS: usize = 8_000;
const DEFAULT_MAX_TOKENS: i32 = 2_048;
const PROVIDER_UNAVAILABLE_CODE: &str = "live_summary_provider_unavailable";

#[derive(Clone)]
struct SecretText(Arc<str>);

impl SecretText {
    fn new(value: impl Into<Arc<str>>) -> Self {
        Self(value.into())
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretText([REDACTED])")
    }
}

#[derive(Clone)]
pub(crate) struct OpenAiCompatibleLiveSummaryConfig {
    provider: String,
    model: String,
    endpoint: Url,
    api_key: Option<SecretText>,
    max_tokens: i32,
    temperature: f32,
    top_p: f32,
    template_instructions: SecretText,
    custom_prompt: Option<SecretText>,
}

impl fmt::Debug for OpenAiCompatibleLiveSummaryConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiCompatibleLiveSummaryConfig")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field(
                "endpoint_origin",
                &self.endpoint.origin().ascii_serialization(),
            )
            .field("has_api_key", &self.api_key.is_some())
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
            .field("top_p", &self.top_p)
            .field("template_configured", &true)
            .field("custom_prompt_configured", &self.custom_prompt.is_some())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveSummaryProviderSetupError {
    pub(crate) code: &'static str,
    pub(crate) message: &'static str,
}

impl LiveSummaryProviderSetupError {
    fn model_not_configured() -> Self {
        Self {
            code: "live_summary_model_not_configured",
            message: "请先在总结设置中选择在线模型。",
        }
    }

    fn unsupported_provider() -> Self {
        Self {
            code: "live_summary_provider_unsupported",
            message:
                "当前实时总结仅支持 OpenAI、Groq、OpenRouter 和自定义 OpenAI-compatible 服务。",
        }
    }

    fn api_key_missing() -> Self {
        Self {
            code: "live_summary_api_key_missing",
            message: "所选在线总结服务尚未配置 API 密钥。",
        }
    }

    fn invalid_configuration() -> Self {
        Self {
            code: "live_summary_provider_config_invalid",
            message: "实时总结服务地址、模型或参数配置无效。",
        }
    }

    fn template_unavailable() -> Self {
        Self {
            code: "live_summary_template_unavailable",
            message: "所选实时总结模板不存在或无法读取。",
        }
    }
}

pub(crate) async fn load_openai_compatible_config(
    pool: &SqlitePool,
    template_id: &str,
    custom_prompt: Option<&str>,
) -> Result<OpenAiCompatibleLiveSummaryConfig, LiveSummaryProviderSetupError> {
    let setting = SettingsRepository::get_model_config(pool)
        .await
        .map_err(|_| LiveSummaryProviderSetupError::invalid_configuration())?
        .ok_or_else(LiveSummaryProviderSetupError::model_not_configured)?;

    let (provider, endpoint, model, api_key, max_tokens, temperature, top_p) = match setting
        .provider
        .trim()
    {
        "openai" => {
            standard_provider_config(
                pool,
                SummaryApiKeyProvider::OpenAi,
                "openai",
                OPENAI_ENDPOINT,
                &setting.model,
            )
            .await?
        }
        "groq" => {
            standard_provider_config(
                pool,
                SummaryApiKeyProvider::Groq,
                "groq",
                GROQ_ENDPOINT,
                &setting.model,
            )
            .await?
        }
        "openrouter" => {
            standard_provider_config(
                pool,
                SummaryApiKeyProvider::OpenRouter,
                "openrouter",
                OPENROUTER_ENDPOINT,
                &setting.model,
            )
            .await?
        }
        "custom-openai" => {
            let custom = SettingsRepository::get_custom_openai_config(pool)
                .await
                .map_err(|_| LiveSummaryProviderSetupError::invalid_configuration())?
                .ok_or_else(LiveSummaryProviderSetupError::model_not_configured)?;
            let endpoint = normalize_custom_openai_endpoint(&custom.endpoint)
                .and_then(|value| Url::parse(&value).map_err(|_| "invalid URL".to_string()))
                .map_err(|_| LiveSummaryProviderSetupError::invalid_configuration())?;
            let model = normalize_custom_openai_model(&custom.model)
                .map_err(|_| LiveSummaryProviderSetupError::invalid_configuration())?;
            validate_custom_openai_parameters(custom.max_tokens, custom.temperature, custom.top_p)
                .map_err(|_| LiveSummaryProviderSetupError::invalid_configuration())?;
            let api_key = custom
                .api_key
                .as_deref()
                .map(normalize_summary_api_key)
                .transpose()
                .map_err(|_| LiveSummaryProviderSetupError::invalid_configuration())?
                .map(SecretText::new);
            (
                "custom-openai".to_string(),
                endpoint,
                model,
                api_key,
                custom.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
                custom.temperature.unwrap_or(0.2),
                custom.top_p.unwrap_or(0.9),
            )
        }
        _ => return Err(LiveSummaryProviderSetupError::unsupported_provider()),
    };

    let template = crate::summary::templates::get_template(template_id)
        .map_err(|_| LiveSummaryProviderSetupError::template_unavailable())?;
    let template_instructions = format!(
        "模板：{}\n用途：{}\n{}",
        template.name,
        template.description,
        template.to_section_instructions()
    );
    if template_instructions.chars().count() > MAX_TEMPLATE_CHARS
        || custom_prompt.is_some_and(|value| value.chars().count() > MAX_CUSTOM_PROMPT_CHARS)
    {
        return Err(LiveSummaryProviderSetupError::invalid_configuration());
    }

    Ok(OpenAiCompatibleLiveSummaryConfig {
        provider,
        model,
        endpoint,
        api_key,
        max_tokens,
        temperature,
        top_p,
        template_instructions: SecretText::new(template_instructions),
        custom_prompt: custom_prompt.map(|value| SecretText::new(value.to_string())),
    })
}

#[cfg(test)]
pub(crate) fn injected_test_configuration() -> OpenAiCompatibleLiveSummaryConfig {
    OpenAiCompatibleLiveSummaryConfig {
        provider: "openai".to_string(),
        model: "fixture-model".to_string(),
        endpoint: Url::parse("https://example.invalid/v1").expect("static test endpoint"),
        api_key: Some(SecretText::new("sk-stage5-injected-test".to_string())),
        max_tokens: 256,
        temperature: 0.2,
        top_p: 0.9,
        template_instructions: SecretText::new("只用于自动测试的固定模板".to_string()),
        custom_prompt: None,
    }
}

async fn standard_provider_config(
    pool: &SqlitePool,
    key_provider: SummaryApiKeyProvider,
    provider: &str,
    endpoint: &str,
    model: &str,
) -> Result<(String, Url, String, Option<SecretText>, i32, f32, f32), LiveSummaryProviderSetupError>
{
    let endpoint = normalize_custom_openai_endpoint(endpoint)
        .and_then(|value| Url::parse(&value).map_err(|_| "invalid URL".to_string()))
        .map_err(|_| LiveSummaryProviderSetupError::invalid_configuration())?;
    let model = normalize_custom_openai_model(model)
        .map_err(|_| LiveSummaryProviderSetupError::invalid_configuration())?;
    let api_key = SettingsRepository::get_summary_api_key(pool, key_provider)
        .await
        .map_err(|_| LiveSummaryProviderSetupError::invalid_configuration())?
        .ok_or_else(LiveSummaryProviderSetupError::api_key_missing)?;
    let api_key = normalize_summary_api_key(api_key.expose_secret())
        .map_err(|_| LiveSummaryProviderSetupError::invalid_configuration())?;
    Ok((
        provider.to_string(),
        endpoint,
        model,
        Some(SecretText::new(api_key)),
        DEFAULT_MAX_TOKENS,
        0.2,
        0.9,
    ))
}

pub(crate) struct LiveSummaryHttpRequest {
    endpoint: Url,
    authorization: Option<SecretText>,
    body: Vec<u8>,
}

impl fmt::Debug for LiveSummaryHttpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveSummaryHttpRequest")
            .field(
                "endpoint_origin",
                &self.endpoint.origin().ascii_serialization(),
            )
            .field("has_authorization", &self.authorization.is_some())
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

pub(crate) struct LiveSummaryHttpResponse {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

impl fmt::Debug for LiveSummaryHttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveSummaryHttpResponse")
            .field("status", &self.status)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveSummaryHttpError {
    InvalidRequest,
    Transport,
    ResponseTooLarge,
}

#[async_trait]
pub(crate) trait LiveSummaryHttpTransport: Send + Sync {
    async fn send(
        &self,
        request: LiveSummaryHttpRequest,
    ) -> Result<LiveSummaryHttpResponse, LiveSummaryHttpError>;
}

pub(crate) struct ReqwestLiveSummaryHttpTransport {
    client: reqwest::Client,
}

impl Default for ReqwestLiveSummaryHttpTransport {
    fn default() -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(45))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("static live-summary HTTP client configuration is valid");
        Self { client }
    }
}

#[async_trait]
impl LiveSummaryHttpTransport for ReqwestLiveSummaryHttpTransport {
    async fn send(
        &self,
        request: LiveSummaryHttpRequest,
    ) -> Result<LiveSummaryHttpResponse, LiveSummaryHttpError> {
        let mut builder = self
            .client
            .post(request.endpoint)
            .header(CONTENT_TYPE, "application/json")
            .body(request.body);
        if let Some(secret) = request.authorization {
            let mut value = HeaderValue::from_str(secret.expose())
                .map_err(|_| LiveSummaryHttpError::InvalidRequest)?;
            value.set_sensitive(true);
            builder = builder.header(AUTHORIZATION, value);
        }
        let response = builder
            .send()
            .await
            .map_err(|_| LiveSummaryHttpError::Transport)?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Ok(LiveSummaryHttpResponse {
                status,
                body: Vec::new(),
            });
        }

        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| LiveSummaryHttpError::Transport)?;
            if body.len().saturating_add(chunk.len()) > MAX_HTTP_RESPONSE_BYTES {
                return Err(LiveSummaryHttpError::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(LiveSummaryHttpResponse { status, body })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LiveSummaryProviderEnvelope {
    pub(crate) scope: super::LiveSummaryScope,
    pub(crate) response: LiveSummaryProviderResponse,
}

#[derive(Clone)]
enum FactoryState {
    Unavailable(LiveSummaryProviderSetupError),
    Ready(Arc<OpenAiCompatibleLiveSummaryConfig>),
}

#[derive(Clone)]
pub struct OpenAiCompatibleLiveSummaryProviderFactory {
    state: Arc<RwLock<FactoryState>>,
    transport: Arc<dyn LiveSummaryHttpTransport>,
    sender: UnboundedSender<LiveSummaryProviderEnvelope>,
}

impl fmt::Debug for OpenAiCompatibleLiveSummaryProviderFactory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiCompatibleLiveSummaryProviderFactory")
            .field("availability", &self.availability())
            .finish()
    }
}

impl OpenAiCompatibleLiveSummaryProviderFactory {
    pub(crate) fn new(
        transport: Arc<dyn LiveSummaryHttpTransport>,
    ) -> (Self, UnboundedReceiver<LiveSummaryProviderEnvelope>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            Self {
                state: Arc::new(RwLock::new(FactoryState::Unavailable(
                    LiveSummaryProviderSetupError {
                        code: PROVIDER_UNAVAILABLE_CODE,
                        message: "请先配置实时总结模型。",
                    },
                ))),
                transport,
                sender,
            },
            receiver,
        )
    }

    pub(crate) fn production() -> (Self, UnboundedReceiver<LiveSummaryProviderEnvelope>) {
        Self::new(Arc::new(ReqwestLiveSummaryHttpTransport::default()))
    }

    pub(crate) fn configure(
        &self,
        configuration: Result<OpenAiCompatibleLiveSummaryConfig, LiveSummaryProviderSetupError>,
    ) -> LiveSummaryProviderAvailability {
        *self.state.write().expect("provider state lock poisoned") = match configuration {
            Ok(configuration) => FactoryState::Ready(Arc::new(configuration)),
            Err(error) => FactoryState::Unavailable(error),
        };
        self.availability()
    }
}

impl LiveSummaryProviderFactory for OpenAiCompatibleLiveSummaryProviderFactory {
    type Provider = OpenAiCompatibleLiveSummaryProvider;

    fn create(&self) -> Self::Provider {
        let configuration = match &*self.state.read().expect("provider state lock poisoned") {
            FactoryState::Ready(configuration) => Some(configuration.clone()),
            FactoryState::Unavailable(_) => None,
        };
        OpenAiCompatibleLiveSummaryProvider {
            configuration,
            transport: self.transport.clone(),
            sender: self.sender.clone(),
            cancellations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn availability(&self) -> LiveSummaryProviderAvailability {
        match &*self.state.read().expect("provider state lock poisoned") {
            FactoryState::Ready(configuration) => LiveSummaryProviderAvailability {
                available: true,
                provider: configuration.provider.clone(),
                model: Some(configuration.model.clone()),
                error: None,
            },
            FactoryState::Unavailable(error) => LiveSummaryProviderAvailability {
                available: false,
                provider: "unavailable".to_string(),
                model: None,
                error: Some(LiveSummaryFrontendError {
                    code: error.code.to_string(),
                    message: error.message.to_string(),
                    retryable: false,
                }),
            },
        }
    }
}

pub struct OpenAiCompatibleLiveSummaryProvider {
    configuration: Option<Arc<OpenAiCompatibleLiveSummaryConfig>>,
    transport: Arc<dyn LiveSummaryHttpTransport>,
    sender: UnboundedSender<LiveSummaryProviderEnvelope>,
    cancellations: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

impl fmt::Debug for OpenAiCompatibleLiveSummaryProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiCompatibleLiveSummaryProvider")
            .field("configured", &self.configuration.is_some())
            .finish()
    }
}

impl LiveSummaryProvider for OpenAiCompatibleLiveSummaryProvider {
    fn provider_id(&self) -> &str {
        self.configuration
            .as_ref()
            .map(|value| value.provider.as_str())
            .unwrap_or("unavailable")
    }

    fn model_id(&self) -> Option<&str> {
        self.configuration
            .as_ref()
            .map(|value| value.model.as_str())
    }

    fn start(&mut self, request: &LiveSummaryRequest) -> Result<(), LiveSummaryProviderError> {
        let Some(configuration) = self.configuration.clone() else {
            return Err(provider_error(PROVIDER_UNAVAILABLE_CODE, false));
        };
        let http_request = build_http_request(&configuration, request)?;
        let cancellation = CancellationToken::new();
        self.cancellations
            .lock()
            .expect("provider cancellation lock poisoned")
            .insert(request.request_id.clone(), cancellation.clone());
        let request_id = request.request_id.clone();
        let scope = request.scope.clone();
        let generation = request.generation;
        let snapshot_hash = request.snapshot_hash.clone();
        let sender = self.sender.clone();
        let transport = self.transport.clone();
        let cancellations = self.cancellations.clone();

        tokio::spawn(async move {
            let result = tokio::select! {
                _ = cancellation.cancelled() => None,
                result = transport.send(http_request) => Some(parse_http_result(result)),
            };
            cancellations
                .lock()
                .expect("provider cancellation lock poisoned")
                .remove(&request_id);
            let Some(result) = result else {
                return;
            };
            let completed_at_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
            let _ = sender.send(LiveSummaryProviderEnvelope {
                scope,
                response: LiveSummaryProviderResponse {
                    request_id,
                    generation,
                    snapshot_hash,
                    completed_at_ms,
                    result,
                },
            });
        });
        Ok(())
    }

    fn cancel(&mut self, request_id: &str) {
        if let Some(token) = self
            .cancellations
            .lock()
            .expect("provider cancellation lock poisoned")
            .remove(request_id)
        {
            token.cancel();
        }
    }
}

fn build_http_request(
    configuration: &OpenAiCompatibleLiveSummaryConfig,
    request: &LiveSummaryRequest,
) -> Result<LiveSummaryHttpRequest, LiveSummaryProviderError> {
    let mut sources = request.sources.clone();
    sources.sort_by_key(|source| (source.start_ms, source.end_ms));
    let mut kept = Vec::new();
    let mut used_chars = 0usize;
    for source in sources.into_iter().rev() {
        let source_chars = source.text.chars().count();
        if !kept.is_empty() && used_chars.saturating_add(source_chars) > MAX_SOURCE_TEXT_CHARS {
            break;
        }
        used_chars = used_chars.saturating_add(source_chars);
        kept.push(source);
    }
    kept.reverse();

    let system_prompt = format!(
        "你是实时会议纪要引擎。只根据输入中的 canonicalSources 和 previousItems 输出 JSON，不得猜测。\n\
         输出必须严格为 {{\"items\":[...]}}；每项字段为 item_id、kind、title、body、owner、due_at、status、evidence。\n\
         kind 只能是 topic/decision/action_item/risk/open_question，status 只能是 active。\n\
         每个 active 项至少引用一个原样 evidence；evidence 的 scope、utterance_id、source_revision、source_event_id、source_text_hash 必须逐字复制 canonicalSources。\n\
         延续同一事实时复用 previousItems 的 item_id；纠正或撤回后不得继续引用失效依据。最多输出 {MAX_PROVIDER_ITEMS} 项。\n\
         {}{}",
        configuration.template_instructions.expose(),
        configuration
            .custom_prompt
            .as_ref()
            .map(|value| format!("\n用户补充要求（不能覆盖 JSON/evidence 约束）：\n{}", value.expose()))
            .unwrap_or_default()
    );
    let mut payload = json!({
        "model": configuration.model,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": serde_json::to_string(&json!({
                "scope": request.scope,
                "generation": request.generation,
                "finalReconcile": request.final_reconcile,
                "windowTruncated": kept.len() < request.sources.len(),
                "canonicalSources": kept,
                "changes": request.changes,
                "previousItems": request.previous_items,
            })).map_err(|_| provider_error("live_summary_request_invalid", false))?}
        ],
        "temperature": configuration.temperature,
        "top_p": configuration.top_p,
        "max_tokens": configuration.max_tokens,
        "response_format": {"type": "json_object"}
    });
    // OpenAI documents `store: false` as the explicit opt-out for retaining
    // generated output. Do not force this vendor field onto other compatible
    // endpoints, which may reject unknown request members.
    if configuration.provider == "openai" {
        let payload = payload
            .as_object_mut()
            .expect("static provider payload is an object");
        payload.insert("store".to_string(), serde_json::Value::Bool(false));
        payload.remove("max_tokens");
        payload.insert(
            "max_completion_tokens".to_string(),
            serde_json::Value::from(configuration.max_tokens),
        );
    }
    let body = serde_json::to_vec(&payload)
        .map_err(|_| provider_error("live_summary_request_invalid", false))?;
    if body.len() > MAX_HTTP_REQUEST_BYTES {
        return Err(provider_error("live_summary_context_too_large", false));
    }
    let endpoint = Url::parse(&format!(
        "{}/chat/completions",
        configuration.endpoint.as_str().trim_end_matches('/')
    ))
    .map_err(|_| provider_error("live_summary_endpoint_invalid", false))?;
    let authorization = configuration
        .api_key
        .as_ref()
        .map(|key| SecretText::new(format!("Bearer {}", key.expose())));
    Ok(LiveSummaryHttpRequest {
        endpoint,
        authorization,
        body,
    })
}

#[derive(Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

#[derive(Deserialize)]
struct OpenAiMessage {
    content: String,
}

#[derive(Deserialize)]
struct StructuredSummary {
    #[serde(default)]
    items: Vec<LiveSummaryItem>,
}

fn parse_http_result(
    result: Result<LiveSummaryHttpResponse, LiveSummaryHttpError>,
) -> LiveSummaryProviderResult {
    let response = match result {
        Ok(value) => value,
        Err(LiveSummaryHttpError::ResponseTooLarge) => {
            return LiveSummaryProviderResult::Failure(provider_error(
                "live_summary_response_too_large",
                false,
            ));
        }
        Err(LiveSummaryHttpError::Transport) => {
            return LiveSummaryProviderResult::Failure(provider_error(
                "live_summary_network_failed",
                true,
            ));
        }
        Err(LiveSummaryHttpError::InvalidRequest) => {
            return LiveSummaryProviderResult::Failure(provider_error(
                "live_summary_request_invalid",
                false,
            ));
        }
    };
    match response.status {
        200..=299 => {}
        401 | 403 => {
            return LiveSummaryProviderResult::Failure(provider_error(
                "live_summary_authentication_failed",
                false,
            ));
        }
        408 | 425 | 429 | 500..=599 => {
            return LiveSummaryProviderResult::Failure(provider_error(
                "live_summary_provider_busy",
                true,
            ));
        }
        _ => {
            return LiveSummaryProviderResult::Failure(provider_error(
                "live_summary_provider_rejected",
                false,
            ));
        }
    }

    let parsed = serde_json::from_slice::<OpenAiResponse>(&response.body)
        .ok()
        .and_then(|value| value.choices.into_iter().next())
        .and_then(|choice| parse_structured_content(&choice.message.content));
    let Some(parsed) = parsed else {
        return LiveSummaryProviderResult::Failure(provider_error(
            "live_summary_response_invalid",
            true,
        ));
    };
    if parsed.items.len() > MAX_PROVIDER_ITEMS {
        return LiveSummaryProviderResult::Failure(provider_error(
            "live_summary_response_invalid",
            false,
        ));
    }
    LiveSummaryProviderResult::Success {
        items: parsed.items,
    }
}

fn parse_structured_content(content: &str) -> Option<StructuredSummary> {
    let content = content.trim();
    if let Ok(parsed) = serde_json::from_str(content) {
        return Some(parsed);
    }
    let stripped = content
        .strip_prefix("```json")
        .or_else(|| content.strip_prefix("```"))?
        .trim()
        .strip_suffix("```")?
        .trim();
    serde_json::from_str(stripped).ok()
}

fn provider_error(code: &'static str, retryable: bool) -> LiveSummaryProviderError {
    LiveSummaryProviderError { code, retryable }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summary::live::{
        source_text_hash, LiveSummaryScope, StableSummarySource, SummaryEvidence, SummaryItemKind,
        SummaryItemStatus,
    };

    struct FakeTransport {
        response: Mutex<Option<LiveSummaryHttpResponse>>,
        request: Mutex<Option<LiveSummaryHttpRequest>>,
    }

    #[async_trait]
    impl LiveSummaryHttpTransport for FakeTransport {
        async fn send(
            &self,
            request: LiveSummaryHttpRequest,
        ) -> Result<LiveSummaryHttpResponse, LiveSummaryHttpError> {
            *self.request.lock().unwrap() = Some(request);
            self.response
                .lock()
                .unwrap()
                .take()
                .ok_or(LiveSummaryHttpError::Transport)
        }
    }

    fn configuration(secret: &str) -> OpenAiCompatibleLiveSummaryConfig {
        OpenAiCompatibleLiveSummaryConfig {
            provider: "openai".to_string(),
            model: "fixture-model".to_string(),
            endpoint: Url::parse("https://example.invalid/v1").unwrap(),
            api_key: Some(SecretText::new(secret.to_string())),
            max_tokens: 256,
            temperature: 0.2,
            top_p: 0.9,
            template_instructions: SecretText::new("测试模板".to_string()),
            custom_prompt: Some(SecretText::new("测试私有提示".to_string())),
        }
    }

    fn request() -> LiveSummaryRequest {
        let text = "确认发布范围";
        let evidence = SummaryEvidence {
            scope: LiveSummaryScope::meeting("meeting-provider"),
            utterance_id: "utterance-provider".to_string(),
            source_revision: 1,
            source_event_id: "event-provider".to_string(),
            source_text_hash: source_text_hash(text),
        };
        LiveSummaryRequest {
            request_id: "request-provider".to_string(),
            scope: LiveSummaryScope::meeting("meeting-provider"),
            generation: 1,
            transcript_cursor: 1,
            snapshot_hash: "a".repeat(64),
            final_reconcile: false,
            sources: vec![StableSummarySource {
                evidence,
                text: text.to_string(),
                speaker_id: None,
                start_ms: 0,
                end_ms: 1000,
            }],
            changes: Vec::new(),
            previous_items: Vec::new(),
            provider: "openai".to_string(),
            model: Some("fixture-model".to_string()),
        }
    }

    #[tokio::test]
    async fn injected_transport_returns_structured_response_without_network() {
        const SECRET: &str = "sk-stage5-test-secret";
        let expected_item = LiveSummaryItem {
            item_id: "item-provider".to_string(),
            kind: SummaryItemKind::Decision,
            title: "发布范围".to_string(),
            body: "已确认发布范围".to_string(),
            owner: None,
            due_at: None,
            status: SummaryItemStatus::Active,
            evidence: vec![request().sources[0].evidence.clone()],
        };
        let response_body = serde_json::to_vec(&json!({
            "choices": [{"message": {"content": serde_json::to_string(&json!({
                "items": [expected_item.clone()]
            })).unwrap()}}]
        }))
        .unwrap();
        let transport = Arc::new(FakeTransport {
            response: Mutex::new(Some(LiveSummaryHttpResponse {
                status: 200,
                body: response_body,
            })),
            request: Mutex::new(None),
        });
        let (factory, mut receiver) =
            OpenAiCompatibleLiveSummaryProviderFactory::new(transport.clone());
        factory.configure(Ok(configuration(SECRET)));
        let mut provider = factory.create();
        provider.start(&request()).expect("start provider task");

        let envelope = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("response timeout")
            .expect("provider response");
        assert_eq!(
            envelope.scope,
            LiveSummaryScope::meeting("meeting-provider")
        );
        assert!(matches!(
            envelope.response.result,
            LiveSummaryProviderResult::Success { ref items } if items == &vec![expected_item]
        ));

        let captured = transport.request.lock().unwrap();
        let captured = captured.as_ref().expect("captured request");
        assert_eq!(
            captured.authorization.as_ref().unwrap().expose(),
            format!("Bearer {SECRET}")
        );
        let debug = format!("{captured:?}");
        assert!(!debug.contains(SECRET));
        assert!(!debug.contains("测试私有提示"));
    }

    #[test]
    fn configuration_and_http_debug_never_expose_secret_or_prompt() {
        const SECRET: &str = "sk-stage5-debug-secret";
        let configuration = configuration(SECRET);
        let request = build_http_request(&configuration, &request()).unwrap();
        for debug in [format!("{configuration:?}"), format!("{request:?}")] {
            assert!(!debug.contains(SECRET));
            assert!(!debug.contains("测试私有提示"));
            assert!(!debug.contains("确认发布范围"));
        }
    }

    #[test]
    fn openai_opts_out_of_storage_without_forcing_vendor_field_on_custom_endpoint() {
        let openai = configuration("sk-stage5-store-test");
        let request_body = build_http_request(&openai, &request()).unwrap().body;
        let payload: serde_json::Value = serde_json::from_slice(&request_body).unwrap();
        assert_eq!(payload["store"], false);
        assert_eq!(payload["max_completion_tokens"], 256);
        assert!(payload.get("max_tokens").is_none());

        let mut custom = openai;
        custom.provider = "custom-openai".to_string();
        let request_body = build_http_request(&custom, &request()).unwrap().body;
        let payload: serde_json::Value = serde_json::from_slice(&request_body).unwrap();
        assert!(payload.get("store").is_none());
        assert_eq!(payload["max_tokens"], 256);
        assert!(payload.get("max_completion_tokens").is_none());
    }
}
