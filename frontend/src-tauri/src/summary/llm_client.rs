use reqwest::{header, Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::info;

const REQUEST_TIMEOUT_DURATION: Duration = Duration::from_secs(300);

// Generic structure for OpenAI-compatible API chat messages
#[derive(Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

// Generic structure for OpenAI-compatible API chat requests
#[derive(Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
}

// Generic structure for OpenAI-compatible API chat responses
#[derive(Deserialize)]
pub struct ChatResponse {
    pub choices: Vec<Choice>,
}

#[derive(Deserialize)]
pub struct Choice {
    pub message: MessageContent,
}

#[derive(Deserialize)]
pub struct MessageContent {
    pub content: String,
}

// Claude-specific request structure
#[derive(Serialize)]
pub struct ClaudeRequest {
    pub model: String,
    pub max_tokens: u32,
    pub system: String,
    pub messages: Vec<ChatMessage>,
}

// Claude-specific response structure
#[derive(Deserialize)]
pub struct ClaudeChatResponse {
    pub content: Vec<ClaudeChatContent>,
}

#[derive(Deserialize)]
pub struct ClaudeChatContent {
    pub text: String,
}

/// LLM Provider enumeration for multi-provider support
#[derive(Debug, Clone, PartialEq)]
pub enum LLMProvider {
    OpenAI,
    Claude,
    Groq,
    Ollama,
    OpenRouter,
    BuiltInAI,
    CustomOpenAI,
}

impl LLMProvider {
    /// Parse provider from string (case-insensitive)
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "openai" => Ok(Self::OpenAI),
            "claude" => Ok(Self::Claude),
            "groq" => Ok(Self::Groq),
            "ollama" => Ok(Self::Ollama),
            "openrouter" => Ok(Self::OpenRouter),
            "builtin-ai" | "local-llama" | "localllama" => Ok(Self::BuiltInAI),
            "custom-openai" => Ok(Self::CustomOpenAI),
            _ => Err(summary_error(
                "summary_provider_invalid",
                "不支持的总结服务提供商。",
            )),
        }
    }
}

fn summary_error(code: &'static str, message: &'static str) -> String {
    format!("{code}|{message}")
}

fn sensitive_header_value(value: &str) -> Result<header::HeaderValue, String> {
    let mut value = header::HeaderValue::from_bytes(value.as_bytes()).map_err(|_| {
        summary_error(
            "summary_credential_invalid",
            "API 密钥格式无效，请替换后重试。",
        )
    })?;
    value.set_sensitive(true);
    Ok(value)
}

fn safe_request_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        summary_error("summary_timeout", "总结服务请求超时，请稍后重试。")
    } else if error.is_connect() {
        summary_error(
            "summary_connection_failed",
            "无法连接总结服务，请检查网络和服务地址。",
        )
    } else {
        summary_error("summary_request_failed", "总结服务请求失败，请稍后重试。")
    }
}

fn safe_status_error(status: StatusCode) -> String {
    match status.as_u16() {
        401 | 403 => summary_error(
            "summary_authentication_failed",
            "总结服务认证失败，请替换 API 密钥后重试。",
        ),
        429 => summary_error("summary_rate_limited", "总结服务请求过于频繁，请稍后重试。"),
        500..=599 => summary_error(
            "summary_provider_unavailable",
            "总结服务暂时不可用，请稍后重试。",
        ),
        _ => summary_error(
            "summary_request_rejected",
            "总结服务拒绝了请求，请检查模型配置。",
        ),
    }
}

/// Generates a summary using the specified LLM provider
///
/// # Arguments
/// * `client` - Reqwest HTTP client (reused for performance)
/// * `provider` - The LLM provider to use
/// * `model_name` - The specific model to use (e.g., "gpt-4", "claude-3-opus")
/// * `api_key` - API key for the provider (not needed for Ollama)
/// * `system_prompt` - System instructions for the LLM
/// * `user_prompt` - User query/content to process
/// * `ollama_endpoint` - Optional custom Ollama endpoint (defaults to localhost:11434)
/// * `custom_openai_endpoint` - Optional custom OpenAI-compatible endpoint
/// * `max_tokens` - Optional max tokens (for CustomOpenAI provider)
/// * `temperature` - Optional temperature (for CustomOpenAI provider)
/// * `top_p` - Optional top_p (for CustomOpenAI provider)
/// * `app_data_dir` - Optional app data directory (for BuiltInAI provider)
/// * `cancellation_token` - Optional token to cancel the request
///
/// # Returns
/// The generated summary text or an error message
pub async fn generate_summary(
    client: &Client,
    provider: &LLMProvider,
    model_name: &str,
    api_key: &str,
    system_prompt: &str,
    user_prompt: &str,
    ollama_endpoint: Option<&str>,
    custom_openai_endpoint: Option<&str>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    app_data_dir: Option<&PathBuf>,
    cancellation_token: Option<&CancellationToken>,
) -> Result<String, String> {
    // Check if cancelled before starting
    if let Some(token) = cancellation_token {
        if token.is_cancelled() {
            return Err(summary_error("summary_cancelled", "总结生成已取消。"));
        }
    }

    // Handle BuiltInAI provider separately (uses local sidecar, no HTTP API)
    if provider == &LLMProvider::BuiltInAI {
        let app_data_dir = app_data_dir.ok_or_else(|| {
            summary_error(
                "summary_local_config_missing",
                "内置总结模型的运行目录未配置。",
            )
        })?;

        return crate::summary::summary_engine::generate_with_builtin(
            app_data_dir,
            model_name,
            system_prompt,
            user_prompt,
            cancellation_token,
        )
        .await
        .map_err(|_| {
            summary_error(
                "summary_local_provider_failed",
                "内置总结模型运行失败，请稍后重试。",
            )
        });
    }

    let (api_url, mut headers) = match provider {
        LLMProvider::OpenAI => (
            "https://api.openai.com/v1/chat/completions".to_string(),
            header::HeaderMap::new(),
        ),
        LLMProvider::Groq => (
            "https://api.groq.com/openai/v1/chat/completions".to_string(),
            header::HeaderMap::new(),
        ),
        LLMProvider::OpenRouter => (
            "https://openrouter.ai/api/v1/chat/completions".to_string(),
            header::HeaderMap::new(),
        ),
        LLMProvider::Ollama => {
            let host = ollama_endpoint
                .map(|s| s.to_string())
                .unwrap_or_else(|| "http://localhost:11434".to_string());
            (
                format!("{}/v1/chat/completions", host),
                header::HeaderMap::new(),
            )
        }
        LLMProvider::CustomOpenAI => {
            let endpoint = custom_openai_endpoint.ok_or_else(|| {
                summary_error(
                    "summary_custom_endpoint_missing",
                    "尚未配置自定义模型服务地址。",
                )
            })?;
            (
                format!("{}/chat/completions", endpoint.trim_end_matches('/')),
                header::HeaderMap::new(),
            )
        }
        LLMProvider::Claude => {
            let mut header_map = header::HeaderMap::new();
            header_map.insert("x-api-key", sensitive_header_value(api_key)?);
            header_map.insert(
                "anthropic-version",
                "2023-06-01".parse().map_err(|_| {
                    summary_error(
                        "summary_client_configuration_failed",
                        "总结客户端配置无效。",
                    )
                })?,
            );
            (
                "https://api.anthropic.com/v1/messages".to_string(),
                header_map,
            )
        }
        LLMProvider::BuiltInAI => {
            // This case is handled earlier with early returns
            unreachable!("BuiltInAI is handled before this match statement")
        }
    };

    // Add authorization header for non-Claude providers
    if provider != &LLMProvider::Claude && !api_key.is_empty() {
        headers.insert(
            header::AUTHORIZATION,
            sensitive_header_value(&format!("Bearer {api_key}"))?,
        );
    }
    headers.insert(
        header::CONTENT_TYPE,
        "application/json".parse().map_err(|_| {
            summary_error(
                "summary_client_configuration_failed",
                "总结客户端配置无效。",
            )
        })?,
    );

    // Build request body based on provider
    let request_body = if provider != &LLMProvider::Claude {
        // For CustomOpenAI, apply optional parameters if provided
        let (max_tokens_val, temperature_val, top_p_val) = if provider == &LLMProvider::CustomOpenAI
        {
            (max_tokens, temperature, top_p)
        } else {
            (None, None, None)
        };

        serde_json::json!(ChatRequest {
            model: model_name.to_string(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: system_prompt.to_string(),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: user_prompt.to_string(),
                }
            ],
            max_tokens: max_tokens_val,
            temperature: temperature_val,
            top_p: top_p_val,
        })
    } else {
        serde_json::json!(ClaudeRequest {
            system: system_prompt.to_string(),
            model: model_name.to_string(),
            max_tokens: 2048,
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: user_prompt.to_string(),
            }]
        })
    };

    info!(
        "🐞 LLM Request to {}: model={}",
        provider_name(provider),
        model_name
    );

    // Send request with timeout and cancellation support
    let request_future = client
        .post(api_url)
        .headers(headers)
        .json(&request_body)
        .timeout(REQUEST_TIMEOUT_DURATION)
        .send();

    // Use tokio::select to race between cancellation and request completion
    let response = if let Some(token) = cancellation_token {
        tokio::select! {
            result = request_future => {
                result.map_err(|error| safe_request_error(&error))?
            }
            _ = token.cancelled() => {
                return Err(summary_error("summary_cancelled", "总结生成已取消。"));
            }
        }
    } else {
        request_future
            .await
            .map_err(|error| safe_request_error(&error))?
    };

    if !response.status().is_success() {
        return Err(safe_status_error(response.status()));
    }

    // Parse response based on provider
    if provider == &LLMProvider::Claude {
        let chat_response = response.json::<ClaudeChatResponse>().await.map_err(|_| {
            summary_error("summary_invalid_response", "总结服务返回了无法解析的响应。")
        })?;

        info!("🐞 LLM Response received from Claude");

        let content = chat_response
            .content
            .get(0)
            .ok_or_else(|| summary_error("summary_empty_response", "总结服务未返回可用内容。"))?
            .text
            .trim();
        Ok(content.to_string())
    } else {
        let chat_response = response.json::<ChatResponse>().await.map_err(|_| {
            summary_error("summary_invalid_response", "总结服务返回了无法解析的响应。")
        })?;

        info!("🐞 LLM Response received from {}", provider_name(provider));

        let content = chat_response
            .choices
            .get(0)
            .ok_or_else(|| summary_error("summary_empty_response", "总结服务未返回可用内容。"))?
            .message
            .content
            .trim();
        Ok(content.to_string())
    }
}

/// Helper function to get provider name for logging
fn provider_name(provider: &LLMProvider) -> &str {
    match provider {
        LLMProvider::OpenAI => "OpenAI",
        LLMProvider::Claude => "Claude",
        LLMProvider::Groq => "Groq",
        LLMProvider::Ollama => "Ollama",
        LLMProvider::BuiltInAI => "Built-in AI",
        LLMProvider::OpenRouter => "OpenRouter",
        LLMProvider::CustomOpenAI => "Custom OpenAI",
    }
}

#[cfg(test)]
mod secret_error_tests {
    use super::*;

    #[test]
    fn sensitive_headers_and_errors_do_not_expose_secret_or_provider_body() {
        const SECRET: &str = "sk-stage5-fake-secret";
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            sensitive_header_value(&format!("Bearer {SECRET}")).unwrap(),
        );
        let debug = format!("{headers:?}");
        assert!(!debug.contains(SECRET));

        let raw_body = format!("provider rejected {SECRET}");
        let public_error = safe_status_error(StatusCode::UNAUTHORIZED);
        assert!(!public_error.contains(SECRET));
        assert!(!public_error.contains(&raw_body));
        assert!(public_error.starts_with("summary_authentication_failed|"));
    }

    #[test]
    fn rate_limit_and_server_errors_use_stable_codes() {
        assert!(
            safe_status_error(StatusCode::TOO_MANY_REQUESTS).starts_with("summary_rate_limited|")
        );
        assert!(
            safe_status_error(StatusCode::BAD_GATEWAY).starts_with("summary_provider_unavailable|")
        );
    }
}
