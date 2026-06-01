use crate::config::ACTION_INSTRUCTION;
use crate::plugin::{Plugin, PluginKind, active_plugin_id_set};
use anyhow::Result;
use std::path::Path;

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are mini, a coding agent.";

pub fn compose_prompt(
    system_prompt: &str,
    mode: Option<&Plugin>,
    plugins: &[Plugin],
    cwd: &Path,
    append_system_prompt: Option<&str>,
    ignore_plugin_errors: bool,
) -> Result<String> {
    let active_plugins = active_plugin_id_set(plugins);

    let system_prompt = match mode {
        Some(mode) => {
            if mode.kind != PluginKind::Mode {
                anyhow::bail!("plugin '{}' is not a mode", mode.id);
            }
            mode.render(cwd, &active_plugins)?
        }
        None => system_prompt.to_string(),
    };

    let mut parts = vec![system_prompt, ACTION_INSTRUCTION.to_string()];
    for plugin in plugins {
        if plugin.kind != PluginKind::Plugin {
            anyhow::bail!("plugin '{}' is not a plugin", plugin.id);
        }
        match plugin.render(cwd, &active_plugins) {
            Ok(prompt) => parts.push(prompt),
            Err(_) if ignore_plugin_errors => {}
            Err(err) => return Err(err.into()),
        }
    }

    if let Some(extra) = append_system_prompt
        && !extra.trim().is_empty()
    {
        parts.push(extra.trim().to_string());
    }

    Ok(parts.join("\n\n"))
}
