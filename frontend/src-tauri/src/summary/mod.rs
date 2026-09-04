/// Summary module - handles all meeting summary generation functionality
///
/// This module contains:
/// - LLM client for communicating with various AI providers (OpenAI, Claude, Groq, Ollama, OpenRouter, CustomOpenAI)
/// - Processor for chunking transcripts and generating summaries
/// - Service layer for orchestrating summary generation
/// - Templates for structured meeting summary generation
/// - Tauri commands for frontend integration
use serde::{Deserialize, Serialize};
use std::fmt;

/// Custom OpenAI-compatible endpoint configuration
/// Stored as JSON in the database and used for connecting to any OpenAI-compatible API server
#[derive(Clone, Serialize, Deserialize)]
pub struct CustomOpenAIConfig {
    /// Base URL of the OpenAI-compatible API endpoint (e.g., "http://localhost:8000/v1")
    pub endpoint: String,
    /// API key for authentication (optional if server doesn't require it)
    #[serde(rename = "apiKey")]
    pub api_key: Option<String>,
    /// Model identifier to use (e.g., "gpt-4", "llama-3-70b", "mistral-7b")
    pub model: String,
    /// Maximum tokens for completion (optional)
    #[serde(rename = "maxTokens")]
    pub max_tokens: Option<i32>,
    /// Temperature parameter (0.0-2.0, optional)
    pub temperature: Option<f32>,
    /// Top-P sampling parameter (0.0-1.0, optional)
    #[serde(rename = "topP")]
    pub top_p: Option<f32>,
}

impl fmt::Debug for CustomOpenAIConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CustomOpenAIConfig")
            .field("endpoint", &self.endpoint)
            .field(
                "has_api_key",
                &self
                    .api_key
                    .as_ref()
                    .is_some_and(|key| !key.trim().is_empty()),
            )
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
            .field("top_p", &self.top_p)
            .finish()
    }
}

/// WebView-safe projection of a custom OpenAI-compatible endpoint.
///
/// The stored credential is reduced to a presence bit before this value can
/// be serialized across IPC.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomOpenAIConfigView {
    pub endpoint: String,
    pub has_api_key: bool,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
}

impl From<&CustomOpenAIConfig> for CustomOpenAIConfigView {
    fn from(config: &CustomOpenAIConfig) -> Self {
        Self {
            endpoint: config.endpoint.clone(),
            has_api_key: config
                .api_key
                .as_deref()
                .map(str::trim)
                .is_some_and(|key| !key.is_empty()),
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            temperature: config.temperature,
            top_p: config.top_p,
        }
    }
}

#[cfg(test)]
mod secret_projection_tests {
    use super::*;

    #[test]
    fn custom_config_debug_and_public_view_redact_the_key() {
        const SECRET: &str = "sk-stage5-fake-secret";
        let config = CustomOpenAIConfig {
            endpoint: "https://example.invalid/v1".to_string(),
            api_key: Some(SECRET.to_string()),
            model: "test-model".to_string(),
            max_tokens: Some(64),
            temperature: Some(0.2),
            top_p: None,
        };

        let debug = format!("{config:?}");
        assert!(!debug.contains(SECRET));
        assert!(debug.contains("has_api_key: true"));

        let public_json = serde_json::to_string(&CustomOpenAIConfigView::from(&config)).unwrap();
        assert!(!public_json.contains(SECRET));
        assert!(public_json.contains("\"hasApiKey\":true"));
    }
}

pub mod commands;
pub(crate) mod language_detection;
pub mod live;
pub mod llm_client;
pub(crate) mod metadata;
pub mod processor;
pub mod service;
pub mod summary_engine;
pub mod template_commands;
pub mod templates;

// Re-export Tauri commands (with their generated __cmd__ variants)
pub use commands::{
    __cmd__api_cancel_summary, __cmd__api_detect_transcript_summary_language,
    __cmd__api_get_meeting_detected_summary_language, __cmd__api_get_meeting_summary_language,
    __cmd__api_get_summary, __cmd__api_process_transcript,
    __cmd__api_save_meeting_detected_summary_language, __cmd__api_save_meeting_summary,
    __cmd__api_save_meeting_summary_language, __tauri_command_name_api_cancel_summary,
    __tauri_command_name_api_detect_transcript_summary_language,
    __tauri_command_name_api_get_meeting_detected_summary_language,
    __tauri_command_name_api_get_meeting_summary_language, __tauri_command_name_api_get_summary,
    __tauri_command_name_api_process_transcript,
    __tauri_command_name_api_save_meeting_detected_summary_language,
    __tauri_command_name_api_save_meeting_summary,
    __tauri_command_name_api_save_meeting_summary_language, api_cancel_summary,
    api_detect_transcript_summary_language, api_get_meeting_detected_summary_language,
    api_get_meeting_summary_language, api_get_summary, api_process_transcript,
    api_save_meeting_detected_summary_language, api_save_meeting_summary,
    api_save_meeting_summary_language,
};

// Re-export template commands
pub use template_commands::{
    __cmd__api_get_template_details, __cmd__api_list_templates, __cmd__api_validate_template,
    __tauri_command_name_api_get_template_details, __tauri_command_name_api_list_templates,
    __tauri_command_name_api_validate_template, api_get_template_details, api_list_templates,
    api_validate_template,
};

// Re-export commonly used items
pub use llm_client::LLMProvider;
pub use processor::{
    chunk_text, clean_llm_markdown_output, extract_meeting_name_from_markdown,
    generate_meeting_summary, rough_token_count,
};
pub use service::SummaryService;
