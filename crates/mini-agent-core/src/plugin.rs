use crate::config::{Config, ConfigPaths};
use anyhow::{Context, Result};
use minijinja::{Environment, UndefinedBehavior, context};
use serde::{Deserialize, Serialize};
use serde_json::{Map, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("plugin file must be UTF-8 markdown: {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("plugin file is missing TOML front matter delimited by +++")]
    MissingFrontMatter,
    #[error("plugin front matter is invalid TOML")]
    InvalidFrontMatter(#[from] toml::de::Error),
    #[error("plugin template is invalid")]
    InvalidTemplate(#[from] minijinja::Error),
    #[error("plugin id must be a plain name: {0}")]
    InvalidId(String),
    #[error("installed script name must be a plain name: {0}")]
    InvalidScriptName(String),
    #[error("installed script '{name}' must start with its own shebang")]
    MissingScriptShebang { name: String },
    #[error("installed script '{name}' block is not closed")]
    UnclosedInstalledScript { name: String },
    #[error("plugin installs script '{0}' more than once")]
    DuplicateScript(String),
    #[error("plugin '{plugin}' requires command '{command}': {reason}")]
    MissingCommand {
        plugin: String,
        command: String,
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct Plugin {
    pub id: String,
    pub title: String,
    pub kind: PluginKind,
    pub source: Option<String>,
    pub scripts: Vec<PluginScript>,
    body: String,
    commands: BTreeMap<String, CommandProbe>,
}

#[derive(Debug, Clone)]
pub struct PluginScript {
    pub name: String,
    content: String,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PluginKind {
    #[default]
    Plugin,
    Mode,
}

#[derive(Debug, Clone, Deserialize)]
struct PluginHeader {
    id: String,
    title: String,
    #[serde(rename = "type", default)]
    kind: PluginKind,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    commands: BTreeMap<String, CommandProbe>,
}

#[derive(Debug, Clone, Deserialize)]
struct CommandProbe {
    command: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    reason: Option<String>,
}

pub fn load_plugin(spec: impl AsRef<Path>) -> Result<Plugin> {
    load_plugin_for_app(".mini-agent", spec)
}

pub fn load_plugin_for_app(app_dir_name: &str, spec: impl AsRef<Path>) -> Result<Plugin> {
    let path = spec.as_ref();
    if path.exists()
        || path.components().count() > 1
        || path.extension().is_some_and(|extension| extension == "md")
    {
        return Ok(Plugin::from_markdown_file(path)?);
    }

    let Some(spec) = path.to_str() else {
        anyhow::bail!("plugin spec '{}' is not UTF-8", path.display());
    };

    let Some(paths) = Config::app_paths(app_dir_name) else {
        anyhow::bail!("HOME is not set, so plugin ids cannot be resolved from ~/{app_dir_name}");
    };
    let mode_path = paths.modes_dir.join(format!("{spec}.md"));
    if mode_path.exists() {
        return Ok(Plugin::from_markdown_file(&mode_path)?);
    }
    let plugin_path = paths.plugins_dir.join(format!("{spec}.md"));
    Plugin::from_markdown_file(&plugin_path).with_context(|| {
        format!(
            "unknown plugin or mode '{spec}', expected '{}' or '{}'",
            mode_path.display(),
            plugin_path.display()
        )
    })
}

pub fn active_plugin_ids(plugins: &[Plugin]) -> Vec<&str> {
    plugins
        .iter()
        .filter(|plugin| plugin.kind == PluginKind::Plugin)
        .map(|plugin| plugin.id.as_str())
        .collect()
}

pub fn install_scripts(
    paths: &ConfigPaths,
    mode: &Plugin,
    plugins: &[Plugin],
    cwd: &Path,
    yolo: bool,
    ignore: bool,
) -> Result<()> {
    install_scripts_for_app(".mini-agent", paths, mode, plugins, cwd, yolo, ignore)
}

pub fn install_scripts_for_app(
    app_dir_name: &str,
    paths: &ConfigPaths,
    mode: &Plugin,
    plugins: &[Plugin],
    cwd: &Path,
    yolo: bool,
    ignore: bool,
) -> Result<()> {
    let active_plugins = active_plugin_id_set(plugins);
    let scripts_dir = paths.state_dir.join("scripts");
    fs::create_dir_all(&paths.bin_dir)
        .with_context(|| format!("failed to create '{}'", paths.bin_dir.display()))?;
    fs::create_dir_all(&scripts_dir)
        .with_context(|| format!("failed to create '{}'", scripts_dir.display()))?;

    for plugin in std::iter::once(mode).chain(plugins) {
        let result = (|| {
            let manifest = scripts_dir.join(&plugin.id);
            let old_scripts = fs::read_to_string(&manifest).unwrap_or_default();
            let script_names = plugin
                .scripts
                .iter()
                .map(|script| script.name.as_str())
                .collect::<Vec<_>>();

            for old_name in old_scripts.lines() {
                if !script_names.contains(&old_name) {
                    let _ = fs::remove_file(paths.bin_dir.join(old_name));
                }
            }

            for script in &plugin.scripts {
                let path = paths.bin_dir.join(&script.name);
                if path.exists()
                    && !yolo
                    && !old_scripts.lines().any(|old_name| old_name == script.name)
                {
                    anyhow::bail!(
                        "refusing to overwrite '{}'; it is not tracked by plugin '{}'",
                        path.display(),
                        plugin.id
                    );
                }

                let mut content = plugin.render_template(
                    app_dir_name,
                    &script.name,
                    &script.content,
                    cwd,
                    &active_plugins,
                )?;
                if script.content.ends_with('\n') && !content.ends_with('\n') {
                    content.push('\n');
                }
                if !content.starts_with("#!") {
                    anyhow::bail!(
                        "rendered script '{}' from plugin '{}' must start with its own shebang",
                        script.name,
                        plugin.id
                    );
                }

                fs::write(&path, content)
                    .with_context(|| format!("failed to write '{}'", path.display()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;

                    let mut permissions = fs::metadata(&path)?.permissions();
                    permissions.set_mode(0o755);
                    fs::set_permissions(&path, permissions)
                        .with_context(|| format!("failed to chmod '{}'", path.display()))?;
                }
            }

            if script_names.is_empty() {
                let _ = fs::remove_file(&manifest);
            } else {
                fs::write(&manifest, format!("{}\n", script_names.join("\n")))
                    .with_context(|| format!("failed to write '{}'", manifest.display()))?;
            }

            Ok(())
        })();

        match result {
            Ok(()) => {}
            Err(_) if ignore => {}
            Err(err) => return Err(err),
        }
    }

    Ok(())
}

pub(crate) fn active_plugin_id_set(plugins: &[Plugin]) -> BTreeSet<String> {
    active_plugin_ids(plugins)
        .into_iter()
        .map(str::to_string)
        .collect()
}

pub fn check_plugins(
    plugins: &[Plugin],
    cwd: &Path,
    ignore_errors: bool,
) -> Result<Vec<(String, Option<String>)>> {
    let active_plugins = active_plugin_id_set(plugins);
    let mut checks = Vec::new();
    for plugin in plugins {
        match plugin.render(cwd, &active_plugins) {
            Ok(_) => checks.push((plugin.id.clone(), None)),
            Err(err) if ignore_errors => checks.push((plugin.id.clone(), Some(err.to_string()))),
            Err(err) => return Err(err.into()),
        }
    }
    Ok(checks)
}

impl Plugin {
    pub fn from_markdown_file(path: impl AsRef<Path>) -> Result<Self, PluginError> {
        let path = path.as_ref();
        let source = std::fs::read_to_string(path).map_err(|source| PluginError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_markdown(&source)
    }

    pub fn from_markdown(source: &str) -> Result<Self, PluginError> {
        let source = source
            .strip_prefix("+++\n")
            .ok_or(PluginError::MissingFrontMatter)?;
        let Some((front_matter, body)) = source.split_once("\n+++\n") else {
            return Err(PluginError::MissingFrontMatter);
        };
        let header: PluginHeader = toml::from_str(front_matter)?;
        if !is_plain_name(&header.id) {
            return Err(PluginError::InvalidId(header.id));
        }

        let mut prompt_body = String::new();
        let mut scripts = Vec::new();
        let mut names = BTreeSet::new();
        let mut lines = body.split_inclusive('\n');
        while let Some(line) = lines.next() {
            let trimmed = line.trim_start();
            let fence_len = trimmed.chars().take_while(|char| *char == '`').count();
            let mut install_name = None;
            if fence_len >= 3 {
                for token in trimmed[fence_len..].split_whitespace() {
                    if let Some(name) = token.strip_prefix("install=") {
                        let name = name.trim_matches('"').trim_matches('\'');
                        if !is_plain_name(name) {
                            return Err(PluginError::InvalidScriptName(name.to_string()));
                        }
                        install_name = Some(name.to_string());
                        break;
                    }
                }
            }

            let Some(name) = install_name else {
                prompt_body.push_str(line);
                continue;
            };

            if !names.insert(name.clone()) {
                return Err(PluginError::DuplicateScript(name));
            }

            let mut content = String::new();
            let mut closed = false;
            for code_line in lines.by_ref() {
                let trimmed = code_line.trim_start();
                let backticks = trimmed.chars().take_while(|char| *char == '`').count();
                if backticks >= fence_len && trimmed[backticks..].trim().is_empty() {
                    closed = true;
                    break;
                }
                content.push_str(code_line);
            }
            if !closed {
                return Err(PluginError::UnclosedInstalledScript { name });
            }
            if !content.starts_with("#!") {
                return Err(PluginError::MissingScriptShebang { name });
            }

            scripts.push(PluginScript { name, content });
        }

        let mut env = Environment::new();
        env.add_template("plugin", &prompt_body)?;
        for script in &scripts {
            env.add_template(&script.name, &script.content)?;
        }

        Ok(Self {
            id: header.id,
            title: header.title,
            kind: header.kind,
            source: header.source,
            scripts,
            body: prompt_body,
            commands: header.commands,
        })
    }

    pub fn render(
        &self,
        cwd: &Path,
        active_plugins: &BTreeSet<String>,
    ) -> Result<String, PluginError> {
        self.render_for_app(".mini-agent", cwd, active_plugins)
    }

    pub fn render_for_app(
        &self,
        app_dir_name: &str,
        cwd: &Path,
        active_plugins: &BTreeSet<String>,
    ) -> Result<String, PluginError> {
        Ok(self
            .render_template(app_dir_name, "plugin", &self.body, cwd, active_plugins)?
            .trim()
            .to_string())
    }

    fn render_template(
        &self,
        app_dir_name: &str,
        name: &str,
        source: &str,
        cwd: &Path,
        active_plugins: &BTreeSet<String>,
    ) -> Result<String, PluginError> {
        let mut commands = Map::new();
        for (name, probe) in &self.commands {
            let exists = which::which(&probe.command).is_ok()
                || Config::app_paths(app_dir_name)
                    .map(|paths| paths.bin_dir.join(&probe.command).is_file())
                    .unwrap_or(false);
            if probe.required && !exists {
                return Err(PluginError::MissingCommand {
                    plugin: self.id.clone(),
                    command: probe.command.clone(),
                    reason: probe.reason.clone().unwrap_or_else(|| {
                        format!("install '{}' or edit this plugin", probe.command)
                    }),
                });
            }

            commands.insert(name.clone(), json!({ "exists": exists }));
        }

        let mut plugins = Map::new();
        for plugin in active_plugins {
            plugins.insert(plugin.clone(), json!({ "exists": true }));
        }

        let mut env = Environment::new();
        env.set_undefined_behavior(UndefinedBehavior::Chainable);
        env.add_template(name, source)?;
        let template = env.get_template(name)?;
        let prompt = template.render(context! {
            cwd => cwd.display().to_string(),
            plugin => json!({
                "id": self.id,
                "title": self.title,
                "type": self.kind,
                "source": self.source,
            }),
            commands => json!(commands),
            plugins => plugins,
        })?;

        Ok(prompt)
    }
}

fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}
