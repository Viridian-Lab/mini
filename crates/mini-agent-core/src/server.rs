use crate::plugin::active_plugin_id_set;
use crate::{
    Agent, AgentEvent, Config, DEFAULT_PLUGINS, DEFAULT_SYSTEM_PROMPT, ModelMessage, Plugin,
    PluginError, PluginKind, compose_prompt_for_app, install_scripts_for_app, load_plugin_for_app,
};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct AgentOptions {
    pub app_dir_name: String,
    pub cwd: PathBuf,
    pub plugins: Vec<PathBuf>,
    pub append_system_prompt: Option<String>,
    pub ignore_plugin_errors: bool,
    pub yolo: bool,
    pub seed_default_plugins: bool,
}

impl AgentOptions {
    pub fn new(app_dir_name: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            app_dir_name: app_dir_name.into(),
            cwd: cwd.into(),
            plugins: Vec::new(),
            append_system_prompt: None,
            ignore_plugin_errors: false,
            yolo: false,
            seed_default_plugins: false,
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
    agent: Agent,
    mode: String,
    plugins: Vec<Plugin>,
    plugin_specs: Vec<PathBuf>,
    cwd: PathBuf,
    append_system_prompt: Option<String>,
    ignore_plugin_errors: bool,
    yolo: bool,
    skipped_plugins: Vec<String>,
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
                agent,
                mode,
                plugins,
                plugin_specs: options.plugins,
                cwd: options.cwd,
                append_system_prompt: options.append_system_prompt,
                ignore_plugin_errors: options.ignore_plugin_errors,
                yolo: options.yolo,
                skipped_plugins,
            })),
        })
    }

    pub fn message(&self, text: impl Into<String>) -> Result<AgentServerMessage> {
        let mut inner = self.inner.lock().expect("agent server lock poisoned");
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
        let inner = self.inner.lock().expect("agent server lock poisoned");
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
            cwd: inner.cwd.clone(),
            home: Config::app_paths(&inner.app_dir_name).map(|paths| paths.root),
            messages: inner.agent.messages.len(),
        }
    }

    pub fn reload(&self) -> Result<AgentServerReload> {
        let mut inner = self.inner.lock().expect("agent server lock poisoned");
        let options = AgentOptions {
            app_dir_name: inner.app_dir_name.clone(),
            cwd: inner.cwd.clone(),
            plugins: inner.plugin_specs.clone(),
            append_system_prompt: inner.append_system_prompt.clone(),
            ignore_plugin_errors: inner.ignore_plugin_errors,
            yolo: inner.yolo,
            seed_default_plugins: false,
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
        self.inner
            .lock()
            .expect("agent server lock poisoned")
            .agent
            .messages
            .clone()
    }

    pub fn clear(&self) {
        self.inner
            .lock()
            .expect("agent server lock poisoned")
            .agent
            .messages
            .clear();
    }

    pub fn skipped_plugins(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("agent server lock poisoned")
            .skipped_plugins
            .clone()
    }
}

fn load_agent(options: &AgentOptions) -> Result<(Agent, String, Vec<Plugin>, Vec<String>)> {
    let config = Config::load_from_app(&options.app_dir_name).with_context(|| {
        format!(
            "failed to load ~/{}/config.toml",
            options.app_dir_name.trim_start_matches('/')
        )
    })?;
    let mode_spec = config.agent.default_mode.clone();
    let mode = load_plugin_for_app(&options.app_dir_name, &mode_spec)
        .with_context(|| format!("failed to load mode '{mode_spec}'"))?;
    if mode.kind != PluginKind::Mode {
        anyhow::bail!("plugin '{}' is not a mode", mode.id);
    }
    let (plugins, skipped_plugins) = load_plugins(
        &options.app_dir_name,
        &config,
        &options.plugins,
        &options.cwd,
        options.ignore_plugin_errors,
    )?;
    if let Some(paths) = Config::ensure_app_files(&options.app_dir_name)? {
        install_scripts_for_app(
            &options.app_dir_name,
            &paths,
            &mode,
            &plugins,
            &options.cwd,
            options.yolo,
            options.ignore_plugin_errors,
        )?;
    }
    let system = compose_prompt_for_app(
        &options.app_dir_name,
        DEFAULT_SYSTEM_PROMPT,
        Some(&mode),
        &plugins,
        &options.cwd,
        options.append_system_prompt.as_deref(),
        options.ignore_plugin_errors,
    )?;
    Ok((
        Agent::new(system, config),
        mode.id,
        plugins,
        skipped_plugins,
    ))
}

fn load_plugins(
    app_dir_name: &str,
    config: &Config,
    plugin_specs: &[PathBuf],
    cwd: &PathBuf,
    ignore_plugin_errors: bool,
) -> Result<(Vec<Plugin>, Vec<String>)> {
    let mut plugins = Vec::new();
    for spec in &config.agent.plugins {
        let plugin = match load_plugin_for_app(app_dir_name, spec)
            .with_context(|| format!("failed to load plugin '{spec}'"))
        {
            Ok(plugin) => plugin,
            Err(_) if ignore_plugin_errors => continue,
            Err(err) => return Err(err),
        };
        if plugin.kind != PluginKind::Plugin {
            if ignore_plugin_errors {
                continue;
            }
            anyhow::bail!("plugin '{}' is not a plugin", plugin.id);
        }
        plugins.push(plugin);
    }
    for path in plugin_specs {
        let plugin = match load_plugin_for_app(app_dir_name, path)
            .with_context(|| format!("failed to load plugin '{}'", path.display()))
        {
            Ok(plugin) => plugin,
            Err(_) if ignore_plugin_errors => continue,
            Err(err) => return Err(err),
        };
        if plugin.kind != PluginKind::Plugin {
            if ignore_plugin_errors {
                continue;
            }
            anyhow::bail!("plugin '{}' is not a plugin", plugin.id);
        }
        plugins.push(plugin);
    }
    plugins.sort_by(|left, right| left.id.cmp(&right.id));
    plugins.dedup_by(|left, right| left.id == right.id);

    let active_plugins = active_plugin_id_set(&plugins);
    let mut available = Vec::new();
    let mut skipped = Vec::new();
    for plugin in plugins {
        match plugin.render_for_app(app_dir_name, cwd, &active_plugins) {
            Ok(_) => available.push(plugin),
            Err(err @ PluginError::MissingCommand { .. }) => {
                skipped.push(format!("{}: {err}", plugin.id));
            }
            Err(_) if ignore_plugin_errors => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok((available, skipped))
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
    config.save_for_app(app_dir_name)
}
