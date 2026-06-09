pub mod agent;
pub mod auth;
pub mod config;
pub mod model;
mod model_transport;
pub mod plugin;
pub mod prompt;
pub mod server;
pub mod tool;

pub use agent::{
    Agent, AgentEvent, AgentRun, CommandOutput, DEFAULT_CONTEXT_WINDOW_TOKENS,
    estimate_messages_tokens,
};
pub use auth::{AuthTokens, StoredAuth, auth_status, logout, oauth_login};
pub use config::{AgentConfig, Config, ConfigPaths, DEFAULT_PLUGINS};
pub use model::{
    BUILT_IN_PROVIDER_NAMES, ModelAuth, ModelConfig, ModelMessage, ModelProtocol, ModelResponse,
    ModelRole, ModelSyntheticKind, ModelToolCall, ModelToolResult, Provider, ProviderConfig,
    list_models,
};
pub use plugin::{
    Plugin, PluginError, PluginKind, PluginScript, active_plugin_ids, check_plugins,
    install_scripts, load_active_plugins, load_plugin,
};
pub use prompt::{DEFAULT_SYSTEM_PROMPT, compose_prompt};
pub use server::{
    AgentOptions, AgentServer, AgentServerMessage, AgentServerReload, AgentServerStatus,
};
pub use tool::{CancelToken, Tool, ToolOutput, ToolSpec};
