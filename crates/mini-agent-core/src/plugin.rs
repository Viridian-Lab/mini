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
    /// Front-matter keys the core does not interpret itself, surfaced verbatim
    /// so a front-end can read its own metadata. The core never inspects these.
    metadata: toml::Table,
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

pub fn load_plugin(app_dir_name: &str, spec: impl AsRef<Path>) -> Result<Plugin> {
    let path = spec.as_ref();
    // Specs with a path separator or an explicit `.md` extension are literal
    // files. A bare id is resolved from the app dirs first (below) so a
    // same-named file or directory in the cwd cannot shadow it.
    if path.components().count() > 1 || path.extension().is_some_and(|extension| extension == "md")
    {
        return Ok(Plugin::from_markdown_file(path)?);
    }

    let Some(spec) = path.to_str() else {
        anyhow::bail!("plugin spec '{}' is not UTF-8", path.display());
    };

    if let Some(paths) = Config::app_paths(app_dir_name) {
        let mode_path = paths.modes_dir.join(format!("{spec}.md"));
        if mode_path.exists() {
            return Ok(Plugin::from_markdown_file(&mode_path)?);
        }
        let plugin_path = paths.plugins_dir.join(format!("{spec}.md"));
        if plugin_path.exists() {
            return Ok(Plugin::from_markdown_file(&plugin_path)?);
        }
    }

    // Fall back to a literal cwd path only when the id is not an installed
    // mode or plugin.
    if path.exists() {
        return Ok(Plugin::from_markdown_file(path)?);
    }

    if let Some(paths) = Config::app_paths(app_dir_name) {
        anyhow::bail!(
            "unknown plugin or mode '{spec}', expected '{}' or '{}'",
            paths.modes_dir.join(format!("{spec}.md")).display(),
            paths.plugins_dir.join(format!("{spec}.md")).display()
        );
    }
    anyhow::bail!("HOME is not set, so plugin ids cannot be resolved from ~/{app_dir_name}");
}

pub fn active_plugin_ids(plugins: &[Plugin]) -> Vec<&str> {
    plugins
        .iter()
        .filter(|plugin| plugin.kind == PluginKind::Plugin)
        .map(|plugin| plugin.id.as_str())
        .collect()
}

/// Load the plugins named in config plus any extra specs, validate they are
/// plugins (not modes), de-duplicate by id, and render-check each. Returns the
/// usable plugins and a list of `"<id>: <reason>"` strings for plugins skipped
/// because a required command is missing. Used by the CLI, the TUI, and the
/// server so the loading rules stay identical.
pub fn load_active_plugins(
    app_dir_name: &str,
    config_plugin_ids: &[String],
    extra_specs: &[PathBuf],
    cwd: &Path,
    ignore_errors: bool,
) -> Result<(Vec<Plugin>, Vec<String>)> {
    let mut plugins = Vec::new();
    let mut load = |spec: &str, display: String| -> Result<()> {
        let plugin = match load_plugin(app_dir_name, spec)
            .with_context(|| format!("failed to load plugin '{display}'"))
        {
            Ok(plugin) => plugin,
            Err(_) if ignore_errors => return Ok(()),
            Err(err) => return Err(err),
        };
        if plugin.kind != PluginKind::Plugin {
            if ignore_errors {
                return Ok(());
            }
            anyhow::bail!("plugin '{}' is not a plugin", plugin.id);
        }
        plugins.push(plugin);
        Ok(())
    };
    for spec in config_plugin_ids {
        load(spec, spec.clone())?;
    }
    for path in extra_specs {
        let Some(spec) = path.to_str() else {
            if ignore_errors {
                continue;
            }
            anyhow::bail!("plugin spec '{}' is not UTF-8", path.display());
        };
        load(spec, path.display().to_string())?;
    }
    plugins.sort_by(|left, right| left.id.cmp(&right.id));
    plugins.dedup_by(|left, right| left.id == right.id);

    let active_plugins = active_plugin_id_set(&plugins);
    let mut available = Vec::new();
    let mut skipped = Vec::new();
    for plugin in plugins {
        match plugin.render(app_dir_name, cwd, &active_plugins) {
            Ok(_) => available.push(plugin),
            Err(err @ PluginError::MissingCommand { .. }) => {
                skipped.push(format!("{}: {err}", plugin.id));
            }
            Err(_) if ignore_errors => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok((available, skipped))
}

pub fn install_scripts(
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

            // Record ownership before writing any files. If a later script
            // fails to render, the ones already written stay tracked, so the
            // untracked-overwrite guard does not wedge the next reinstall.
            if script_names.is_empty() {
                let _ = fs::remove_file(&manifest);
            } else {
                fs::write(&manifest, format!("{}\n", script_names.join("\n")))
                    .with_context(|| format!("failed to write '{}'", manifest.display()))?;
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
    app_dir_name: &str,
    plugins: &[Plugin],
    cwd: &Path,
    ignore_errors: bool,
) -> Result<Vec<(String, Option<String>)>> {
    let active_plugins = active_plugin_id_set(plugins);
    let mut checks = Vec::new();
    for plugin in plugins {
        match plugin.render(app_dir_name, cwd, &active_plugins) {
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

        // Capture any front-matter keys the core does not model itself, so a
        // front-end can read its own metadata. The core never interprets these.
        let mut metadata: toml::Table = toml::from_str(front_matter)?;
        for known in ["id", "title", "type", "source", "commands"] {
            metadata.remove(known);
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
                // A regular (non-install) code fence: copy its contents through
                // verbatim until the matching closer, so an `install=` example
                // documented inside it is not parsed as a real script.
                if fence_len >= 3 {
                    for code_line in lines.by_ref() {
                        prompt_body.push_str(code_line);
                        let trimmed = code_line.trim_start();
                        let backticks = trimmed.chars().take_while(|char| *char == '`').count();
                        if backticks >= fence_len && trimmed[backticks..].trim().is_empty() {
                            break;
                        }
                    }
                }
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
            metadata,
        })
    }

    /// A front-matter value the core does not model itself (anything beyond
    /// `id`/`title`/`type`/`source`/`commands`), for a front-end to read its own
    /// metadata. Returns `None` if the key is absent.
    pub fn metadata(&self, key: &str) -> Option<&toml::Value> {
        self.metadata.get(key)
    }

    pub fn render(
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    fn render(plugin: &Plugin) -> String {
        plugin
            .render("test-app", Path::new("/tmp"), &BTreeSet::new())
            .expect("render")
    }

    #[test]
    fn parses_front_matter_and_body() {
        let plugin = Plugin::from_markdown(
            "+++\nid = \"demo\"\ntitle = \"Demo\"\ntype = \"plugin\"\n+++\nHello body\n",
        )
        .expect("parse");
        assert_eq!(plugin.id, "demo");
        assert_eq!(plugin.title, "Demo");
        assert_eq!(plugin.kind, PluginKind::Plugin);
        assert_eq!(render(&plugin), "Hello body");
        assert!(plugin.scripts.is_empty());
    }

    #[test]
    fn missing_front_matter_is_rejected() {
        assert!(matches!(
            Plugin::from_markdown("no front matter here"),
            Err(PluginError::MissingFrontMatter)
        ));
        // Opening delimiter without a closing one is also rejected.
        assert!(matches!(
            Plugin::from_markdown("+++\nid = \"x\"\ntitle = \"X\"\n"),
            Err(PluginError::MissingFrontMatter)
        ));
    }

    #[test]
    fn invalid_id_is_rejected() {
        assert!(matches!(
            Plugin::from_markdown("+++\nid = \"../escape\"\ntitle = \"X\"\n+++\nbody\n"),
            Err(PluginError::InvalidId(_))
        ));
    }

    #[test]
    fn extracts_installed_script_and_strips_it_from_body() {
        let source = "+++\nid = \"demo\"\ntitle = \"Demo\"\n+++\nIntro text\n\n```bash install=helper\n#!/usr/bin/env bash\necho hi\n```\n\nOutro text\n";
        let plugin = Plugin::from_markdown(source).expect("parse");
        assert_eq!(plugin.scripts.len(), 1);
        assert_eq!(plugin.scripts[0].name, "helper");
        assert!(plugin.scripts[0].content.starts_with("#!/usr/bin/env bash"));
        let body = render(&plugin);
        assert!(body.contains("Intro text"));
        assert!(body.contains("Outro text"));
        assert!(!body.contains("echo hi"));
    }

    #[test]
    fn script_without_shebang_is_rejected() {
        let source =
            "+++\nid = \"demo\"\ntitle = \"Demo\"\n+++\n```bash install=helper\necho hi\n```\n";
        assert!(matches!(
            Plugin::from_markdown(source),
            Err(PluginError::MissingScriptShebang { .. })
        ));
    }

    #[test]
    fn unclosed_script_block_is_rejected() {
        let source = "+++\nid = \"demo\"\ntitle = \"Demo\"\n+++\n```bash install=helper\n#!/bin/sh\necho hi\n";
        assert!(matches!(
            Plugin::from_markdown(source),
            Err(PluginError::UnclosedInstalledScript { .. })
        ));
    }

    #[test]
    fn duplicate_script_names_are_rejected() {
        let source = "+++\nid = \"demo\"\ntitle = \"Demo\"\n+++\n```sh install=dup\n#!/bin/sh\n```\n```sh install=dup\n#!/bin/sh\n```\n";
        assert!(matches!(
            Plugin::from_markdown(source),
            Err(PluginError::DuplicateScript(_))
        ));
    }

    #[test]
    fn invalid_script_name_is_rejected() {
        let source = "+++\nid = \"demo\"\ntitle = \"Demo\"\n+++\n```sh install=\"../evil\"\n#!/bin/sh\n```\n";
        assert!(matches!(
            Plugin::from_markdown(source),
            Err(PluginError::InvalidScriptName(_))
        ));
    }

    #[test]
    fn install_example_inside_a_regular_code_fence_is_not_a_script() {
        // An `install=` shown inside a documentation fence must not be parsed as
        // a real installed script.
        let source = "+++\nid = \"demo\"\ntitle = \"Demo\"\n+++\nHere is how to install a script:\n\n````md\n```sh install=example\n#!/bin/sh\necho hi\n```\n````\n";
        let plugin = Plugin::from_markdown(source).expect("parse");
        assert!(plugin.scripts.is_empty());
        assert!(render(&plugin).contains("install=example"));
    }

    #[test]
    fn unknown_front_matter_is_surfaced_as_metadata() {
        let source = "+++\nid = \"fs\"\ntitle = \"FS\"\n\n[mcp.filesystem]\ncommand = \"npx\"\nargs = [\"-y\", \"server-filesystem\"]\n+++\nbody\n";
        let plugin = Plugin::from_markdown(source).expect("parse");
        // Known keys are not in metadata; unknown tables are.
        assert!(plugin.metadata("id").is_none());
        let mcp = plugin.metadata("mcp").expect("mcp table");
        assert_eq!(mcp["filesystem"]["command"].as_str(), Some("npx"));
        assert_eq!(plugin.metadata("nope"), None);
    }

    #[test]
    fn mode_kind_is_parsed() {
        let plugin =
            Plugin::from_markdown("+++\nid = \"m\"\ntitle = \"M\"\ntype = \"mode\"\n+++\nbody\n")
                .expect("parse");
        assert_eq!(plugin.kind, PluginKind::Mode);
    }

    #[test]
    fn is_plain_name_guards_path_separators() {
        assert!(is_plain_name("plugin-1.2_x"));
        assert!(!is_plain_name(""));
        assert!(!is_plain_name("."));
        assert!(!is_plain_name(".."));
        assert!(!is_plain_name("a/b"));
        assert!(!is_plain_name("a b"));
    }
}
