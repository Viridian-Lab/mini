use crate::{Config, ToolSpec, auth::codex_auth};
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
    /// false → non-streaming request + JSON response (else SSE). default true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
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
            stream: None,
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
    /// Provider-native reasoning content blocks (currently Anthropic
    /// `thinking`/`redacted_thinking`) captured verbatim so they can be
    /// replayed before tool results, as the API requires when thinking is on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking: Vec<Value>,
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
    /// Whether the tool reported failure. Replayed to providers that support a
    /// tool-result error flag (Anthropic) and otherwise marked in the content,
    /// so the model reliably sees that a tool call failed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModelResponse {
    pub text: String,
    pub tool_calls: Vec<ModelToolCall>,
    /// Reasoning content blocks (Anthropic) captured for replay; empty for
    /// providers that do not expose them.
    pub thinking: Vec<Value>,
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

#[cfg(test)]
mod model_tests {
    use super::*;

    fn model_config(provider: &str) -> ModelConfig {
        ModelConfig {
            provider: provider.to_string(),
            ..ModelConfig::default()
        }
    }

    #[test]
    fn built_in_codex_provider_resolves() {
        let provider = model_config("codex")
            .provider(&BTreeMap::new())
            .expect("resolve");
        assert_eq!(provider.protocol, ModelProtocol::OpenAiResponses);
        assert_eq!(provider.auth, ModelAuth::CodexOauth);
        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://chatgpt.com/backend-api/codex")
        );
        assert!(provider.models.contains(&"gpt-5.5".to_string()));
    }

    #[test]
    fn unknown_provider_without_protocol_errors() {
        let result = model_config("mystery").provider(&BTreeMap::new());
        assert!(result.is_err());
    }

    #[test]
    fn custom_provider_via_providers_table_resolves() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "local".to_string(),
            ProviderConfig {
                protocol: Some(ModelProtocol::OpenAiChatCompletions),
                auth: Some(ModelAuth::None),
                base_url: Some("http://127.0.0.1:11434/v1".to_string()),
                models: vec!["llama".to_string()],
                ..ProviderConfig::default()
            },
        );
        let provider = model_config("local").provider(&providers).expect("resolve");
        assert_eq!(provider.protocol, ModelProtocol::OpenAiChatCompletions);
        assert_eq!(provider.auth, ModelAuth::None);
        assert_eq!(provider.models, vec!["llama".to_string()]);
    }

    #[test]
    fn explicit_model_config_overrides_built_in() {
        let config = ModelConfig {
            provider: "anthropic".to_string(),
            base_url: Some("https://proxy.example/v1".to_string()),
            ..ModelConfig::default()
        };
        let provider = config.provider(&BTreeMap::new()).expect("resolve");
        // The built-in anthropic protocol is kept, but the base_url is overridden.
        assert_eq!(provider.protocol, ModelProtocol::Anthropic);
        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://proxy.example/v1")
        );
    }

    #[test]
    fn endpoint_paths_match_protocol() {
        let anthropic = model_config("anthropic")
            .provider(&BTreeMap::new())
            .expect("resolve");
        assert_eq!(
            anthropic.endpoint("claude"),
            "https://api.anthropic.com/v1/messages"
        );
        let gemini = model_config("gemini")
            .provider(&BTreeMap::new())
            .expect("resolve");
        assert_eq!(
            gemini.endpoint("gemini-2.5-pro"),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent"
        );
    }

    fn anthropic_provider() -> Provider {
        model_config("anthropic")
            .provider(&BTreeMap::new())
            .expect("resolve")
    }

    #[test]
    fn anthropic_replays_thinking_blocks_before_tool_results() {
        // With reasoning effort on, a captured thinking block must lead the
        // assistant message so the follow-up tool_result turn is accepted.
        let config = ModelConfig {
            provider: "anthropic".to_string(),
            reasoning_effort: Some("high".to_string()),
            ..ModelConfig::default()
        };
        let thinking_block =
            json!({ "type": "thinking", "thinking": "reason", "signature": "sig" });
        let messages = vec![
            ModelMessage {
                role: ModelRole::User,
                text: "hi".to_string(),
                tool_calls: Vec::new(),
                tool_result: None,
                synthetic: None,
                thinking: Vec::new(),
            },
            ModelMessage {
                role: ModelRole::Assistant,
                text: String::new(),
                tool_calls: vec![ModelToolCall {
                    id: "toolu_1".to_string(),
                    name: "bash".to_string(),
                    input: json!({ "command": "ls" }),
                }],
                tool_result: None,
                synthetic: None,
                thinking: vec![thinking_block.clone()],
            },
        ];
        let body = request_body("sys", &messages, &config, &anthropic_provider(), &[]);
        let assistant = body["messages"]
            .as_array()
            .and_then(|messages| messages.iter().find(|m| m["role"] == "assistant"))
            .expect("assistant message");
        let first_block = &assistant["content"][0];
        assert_eq!(first_block["type"], "thinking");
        assert_eq!(first_block["signature"], "sig");
    }

    #[test]
    fn anthropic_omits_thinking_when_reasoning_disabled() {
        let config = model_config("anthropic");
        let messages = vec![ModelMessage {
            role: ModelRole::Assistant,
            text: "done".to_string(),
            tool_calls: Vec::new(),
            tool_result: None,
            synthetic: None,
            thinking: vec![json!({ "type": "thinking", "thinking": "x", "signature": "s" })],
        }];
        let body = request_body("sys", &messages, &config, &anthropic_provider(), &[]);
        // With reasoning off the assistant content stays a plain string, never
        // a thinking block the API would reject.
        assert!(body["messages"][0]["content"].is_string());
    }

    #[test]
    fn mounted_tools_are_emitted_alongside_bash() {
        let config = model_config("anthropic");
        let tools = vec![ToolSpec {
            name: "search".to_string(),
            description: "search the web".to_string(),
            input_schema: json!({ "type": "object", "properties": { "q": { "type": "string" } } }),
        }];
        let body = request_body("sys", &[], &config, &anthropic_provider(), &tools);
        let names: Vec<&str> = body["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        // Anthropic shape uses `name` + `input_schema`; bash always leads.
        assert_eq!(names, vec!["bash", "search"]);
        assert!(body["tools"][1]["input_schema"]["properties"]["q"].is_object());
    }

    #[test]
    fn openai_responses_marks_only_bash_strict() {
        let config = model_config("openai");
        let tools = vec![ToolSpec {
            name: "search".to_string(),
            description: "search".to_string(),
            input_schema: json!({ "type": "object" }),
        }];
        let provider = config.provider(&BTreeMap::new()).expect("resolve");
        let body = request_body("sys", &[], &config, &provider, &tools);
        // bash is strict; arbitrary mounted schemas are sent non-strict so the
        // Responses API does not reject them.
        assert_eq!(body["tools"][0]["name"], "bash");
        assert_eq!(body["tools"][0]["strict"], true);
        assert_eq!(body["tools"][1]["name"], "search");
        assert!(body["tools"][1].get("strict").is_none());
    }
}
