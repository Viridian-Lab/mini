use crate::{Config, auth::codex_auth_for_app};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

pub const BUILT_IN_PROVIDER_NAMES: &[&str] = &[
    "codex",
    "openai",
    "openai-chat-completions",
    "openrouter",
    "anthropic",
    "gemini",
];
const CODEX_MODELS_CLIENT_VERSION_FALLBACK: &str = "0.135.0";
static CODEX_MODELS_CLIENT_VERSION: OnceLock<String> = OnceLock::new();

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<ModelProtocol>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<ModelAuth>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: "codex".to_string(),
            model: "gpt-5.5".to_string(),
            protocol: None,
            auth: None,
            api_key_env: None,
            base_url: None,
            max_output_tokens: None,
            temperature: None,
            reasoning_effort: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProviderConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<ModelProtocol>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<ModelAuth>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    pub name: String,
    pub protocol: ModelProtocol,
    pub auth: ModelAuth,
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum ModelProtocol {
    #[default]
    #[serde(rename = "openai-responses", alias = "open-ai-responses")]
    OpenAiResponses,
    #[serde(
        rename = "openai-chat-completions",
        alias = "open-ai-chat-completions",
        alias = "openai-completions"
    )]
    OpenAiChatCompletions,
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "gemini")]
    Gemini,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum ModelAuth {
    #[default]
    #[serde(rename = "api-key")]
    ApiKey,
    #[serde(rename = "codex-oauth", alias = "codex")]
    CodexOauth,
    #[serde(rename = "none")]
    None,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ModelMessage {
    pub role: ModelRole,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ModelToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<ModelToolResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthetic: Option<ModelSyntheticKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelSyntheticKind {
    CompactionSummary,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ModelToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ModelToolResult {
    pub id: String,
    pub name: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelResponse {
    pub text: String,
    pub tool_calls: Vec<ModelToolCall>,
}

pub(crate) use crate::model_transport::{
    call_model, call_model_interruptible, call_model_without_tools,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelRole {
    User,
    Assistant,
}

fn reasoning_effort(config: &ModelConfig) -> Option<&str> {
    let effort = config.reasoning_effort.as_deref()?.trim();
    if effort.is_empty()
        || matches!(
            effort.to_ascii_lowercase().as_str(),
            "none" | "off" | "false" | "no" | "0"
        )
    {
        None
    } else {
        Some(effort)
    }
}

include!("model_provider.rs");
include!("model_request.rs");
include!("model_auth.rs");
