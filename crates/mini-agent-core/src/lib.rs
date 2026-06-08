pub mod agent;
pub mod auth;
pub mod config;
pub mod model;
mod model_transport;
pub mod plugin;
pub mod prompt;
pub mod server;

pub use agent::{
    Agent, AgentEvent, AgentRun, CommandOutput, DEFAULT_CONTEXT_WINDOW_TOKENS,
    estimate_messages_tokens,
};
pub use auth::{
    AuthTokens, StoredAuth, auth_status, auth_status_for_app, logout, logout_for_app, oauth_login,
    oauth_login_for_app,
};
pub use config::{AgentConfig, Config, ConfigPaths, DEFAULT_PLUGINS};
pub use model::{
    BUILT_IN_PROVIDER_NAMES, ModelAuth, ModelConfig, ModelMessage, ModelProtocol, ModelResponse,
    ModelRole, ModelSyntheticKind, ModelToolCall, ModelToolResult, Provider, ProviderConfig,
    list_models, list_models_for_app, list_models_for_config,
};
pub use plugin::{
    Plugin, PluginError, PluginKind, PluginScript, active_plugin_ids, check_plugins,
    install_scripts, install_scripts_for_app, load_plugin, load_plugin_for_app,
};
pub use prompt::{DEFAULT_SYSTEM_PROMPT, compose_prompt, compose_prompt_for_app};
pub use server::{
    AgentOptions, AgentServer, AgentServerMessage, AgentServerReload, AgentServerStatus,
};
