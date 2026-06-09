use crate::{
    Agent, AgentEvent, Config, DEFAULT_PLUGINS, DEFAULT_SYSTEM_PROMPT, ModelMessage, Plugin,
    PluginKind, Tool, compose_prompt, install_scripts, load_active_plugins, load_plugin,
};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct AgentOptions {
    pub app_dir_name: String,
    pub auth_app_dir_name: Option<String>,
    pub cwd: PathBuf,
    pub plugins: Vec<PathBuf>,
    pub append_system_prompt: Option<String>,
    pub ignore_plugin_errors: bool,
    pub yolo: bool,
    pub seed_default_plugins: bool,
    /// Tools to mount in addition to the built-in `bash` (e.g. MCP tools).
    pub tools: Vec<Arc<dyn Tool>>,
}

impl AgentOptions {
    pub fn new(app_dir_name: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            app_dir_name: app_dir_name.into(),
            auth_app_dir_name: None,
            cwd: cwd.into(),
            plugins: Vec::new(),
            append_system_prompt: None,
            ignore_plugin_errors: false,
            yolo: false,
            seed_default_plugins: false,
            tools: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentServer {
    inner: Arc<Mutex<AgentServerInner>>,
}

#[derive(Debug, Clone)]
struct AgentServerInner {
    app_dir_name: String,
    auth_app_dir_name: Option<String>,
    agent: Agent,
    mode: String,
    plugins: Vec<Plugin>,
    plugin_specs: Vec<PathBuf>,
    cwd: PathBuf,
    append_system_prompt: Option<String>,
    ignore_plugin_errors: bool,
    yolo: bool,
    skipped_plugins: Vec<String>,
    /// Mounted tools, kept so `reload` can re-attach them to the rebuilt agent
    /// without reconnecting (MCP connections are live).
    tools: Vec<Arc<dyn Tool>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentServerMessage {
    pub final_text: String,
    pub turns: usize,
    pub events: Vec<AgentEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentServerStatus {
    pub app_dir_name: String,
    pub mode: String,
    pub provider: String,
    pub model: String,
    pub plugins: Vec<String>,
    /// Names of mounted tools (beyond the built-in `bash`), e.g. MCP tools.
    pub tools: Vec<String>,
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
    pub messages: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentServerReload {
    pub mode: String,
    pub plugins: Vec<String>,
    pub skipped_plugins: Vec<String>,
}

impl AgentServer {
    pub fn new(options: AgentOptions) -> Result<Self> {
        if options.seed_default_plugins {
            seed_default_plugins(&options.app_dir_name)?;
        }
        let (agent, mode, plugins, skipped_plugins) = load_agent(&options)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(AgentServerInner {
                app_dir_name: options.app_dir_name,
                auth_app_dir_name: options.auth_app_dir_name,
                agent,
                mode,
                plugins,
                plugin_specs: options.plugins,
                cwd: options.cwd,
                append_system_prompt: options.append_system_prompt,
                ignore_plugin_errors: options.ignore_plugin_errors,
                yolo: options.yolo,
                skipped_plugins,
                tools: options.tools,
            })),
        })
    }

    pub fn message(&self, text: impl Into<String>) -> Result<AgentServerMessage> {
        let mut inner = self.lock();
        let mut events = Vec::new();
        let run = inner.agent.run_with_events(text, |event| {
            events.push(event);
        })?;
        Ok(AgentServerMessage {
            final_text: run.final_text,
            turns: run.turns,
            events,
        })
    }

    pub fn status(&self) -> AgentServerStatus {
        let inner = self.lock();
        AgentServerStatus {
            app_dir_name: inner.app_dir_name.clone(),
            mode: inner.mode.clone(),
            provider: inner.agent.config.model.provider.clone(),
            model: inner.agent.config.model.model.clone(),
            plugins: inner
                .plugins
                .iter()
                .map(|plugin| plugin.id.clone())
                .collect(),
            tools: inner
                .agent
                .tools
                .iter()
                .map(|tool| tool.spec().name)
                .collect(),
            cwd: inner.cwd.clone(),
            home: Config::app_paths(&inner.app_dir_name).map(|paths| paths.root),
            messages: inner.agent.messages.len(),
        }
    }

    pub fn reload(&self) -> Result<AgentServerReload> {
        let mut inner = self.lock();
        let options = AgentOptions {
            app_dir_name: inner.app_dir_name.clone(),
            auth_app_dir_name: inner.auth_app_dir_name.clone(),
            cwd: inner.cwd.clone(),
            plugins: inner.plugin_specs.clone(),
            append_system_prompt: inner.append_system_prompt.clone(),
            ignore_plugin_errors: inner.ignore_plugin_errors,
            yolo: inner.yolo,
            seed_default_plugins: false,
            tools: inner.tools.clone(),
        };
        let (mut agent, mode, plugins, skipped_plugins) = load_agent(&options)?;
        agent.messages = inner.agent.messages.clone();
        inner.agent = agent;
        inner.mode = mode.clone();
        inner.plugins = plugins.clone();
        inner.skipped_plugins = skipped_plugins.clone();
        Ok(AgentServerReload {
            mode,
            plugins: plugins.into_iter().map(|plugin| plugin.id).collect(),
            skipped_plugins,
        })
    }

    pub fn history(&self) -> Vec<ModelMessage> {
        self.lock().agent.messages.clone()
    }

    pub fn clear(&self) {
        self.lock().agent.messages.clear();
    }

    pub fn skipped_plugins(&self) -> Vec<String> {
        self.lock().skipped_plugins.clone()
    }

    /// The active plugins, so a front-end can read their front-matter metadata.
    pub fn plugins(&self) -> Vec<Plugin> {
        self.lock().plugins.clone()
    }

    /// Attach more tools to the running agent (in addition to the built-in
    /// `bash` and any already mounted). They are also retained so a `reload`
    /// re-attaches them to the rebuilt agent.
    pub fn mount_tools(&self, tools: Vec<Arc<dyn Tool>>) {
        let mut inner = self.lock();
        inner.agent.mount_tools(tools.iter().cloned());
        inner.tools.extend(tools);
    }

    /// Lock the inner state, recovering the data if a previous holder panicked
    /// rather than poisoning the mutex and bricking every future request.
    fn lock(&self) -> std::sync::MutexGuard<'_, AgentServerInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

fn load_agent(options: &AgentOptions) -> Result<(Agent, String, Vec<Plugin>, Vec<String>)> {
    let mut config = Config::load_from_app(&options.app_dir_name).with_context(|| {
        format!(
            "failed to load ~/{}/config.toml",
            options.app_dir_name.trim_start_matches('/')
        )
    })?;
    config.auth_app_dir_name = options.auth_app_dir_name.clone();
    let mode_spec = config.agent.default_mode.clone();
    let mode = load_plugin(&options.app_dir_name, &mode_spec)
        .with_context(|| format!("failed to load mode '{mode_spec}'"))?;
    if mode.kind != PluginKind::Mode {
        anyhow::bail!("plugin '{}' is not a mode", mode.id);
    }
    let (plugins, skipped_plugins) = load_active_plugins(
        &options.app_dir_name,
        &config.agent.plugins,
        &options.plugins,
        &options.cwd,
        options.ignore_plugin_errors,
    )?;
    if let Some(paths) = Config::ensure_app_files(&options.app_dir_name)? {
        install_scripts(
            &options.app_dir_name,
            &paths,
            &mode,
            &plugins,
            &options.cwd,
            options.yolo,
            options.ignore_plugin_errors,
        )?;
    }
    let system = compose_prompt(
        &options.app_dir_name,
        DEFAULT_SYSTEM_PROMPT,
        Some(&mode),
        &plugins,
        &options.cwd,
        options.append_system_prompt.as_deref(),
        options.ignore_plugin_errors,
    )?;
    let mut agent = Agent::new(system, config);
    agent.mount_tools(options.tools.iter().cloned());
    Ok((agent, mode.id, plugins, skipped_plugins))
}

fn seed_default_plugins(app_dir_name: &str) -> Result<()> {
    let Some(paths) = Config::ensure_app_files(app_dir_name)? else {
        return Ok(());
    };
    let mut config = Config::load_from_app(app_dir_name)?;
    for (file_name, source) in DEFAULT_PLUGINS {
        let path = paths.plugins_dir.join(file_name);
        if !path.exists() {
            std::fs::write(&path, source)
                .with_context(|| format!("failed to write '{}'", path.display()))?;
        }
        let id = file_name.strip_suffix(".md").unwrap_or(file_name);
        if !config.agent.plugins.iter().any(|plugin| plugin == id) {
            config.agent.plugins.push(id.to_string());
        }
    }
    config.save()
}
