use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use mini_agent_core::{
    Agent, AgentEvent, Config, DEFAULT_PLUGINS, DEFAULT_SYSTEM_PROMPT, Plugin, PluginError,
    PluginKind, active_plugin_ids, auth_status, check_plugins, compose_prompt, install_scripts,
    load_plugin, logout, oauth_login,
};
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "mini")]
#[command(about = "A modular terminal agent scaffold with markdown plugins")]
#[command(subcommand_precedence_over_arg = true)]
struct Cli {
    #[arg(short = 'p', long = "print")]
    print: bool,

    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    output_format: OutputFormat,

    #[arg(long, value_enum, default_value_t = InputFormat::Text)]
    input_format: InputFormat,

    #[arg(short = 'm', long)]
    mode: Option<String>,

    #[arg(long = "plugin")]
    plugins: Vec<PathBuf>,

    #[arg(long)]
    append_system_prompt: Option<String>,

    #[arg(long)]
    explain_prompt: bool,

    #[arg(
        long,
        value_name = "SESSION",
        num_args = 0..=1,
        default_missing_value = "latest",
        conflicts_with = "print"
    )]
    resume: Option<String>,

    #[arg(long, value_name = "SESSION", conflicts_with = "print")]
    session: Option<String>,

    #[arg(long)]
    check_plugins: bool,

    #[arg(long, help = "Bypass supported safety checks and confirmations")]
    yolo: bool,

    #[arg(long, help = "Ignore supported non-fatal plugin errors")]
    ignore: bool,

    #[command(subcommand)]
    command: Option<Command>,

    prompt: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    Login,
    Status,
    Logout,
}

#[derive(Debug, Subcommand)]
enum PluginCommand {
    Add {
        url: String,
    },
    Update {
        id: Option<String>,
    },
    List,
    Info {
        id: String,
    },
    #[command(alias = "remove")]
    Rm {
        id: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum InputFormat {
    Text,
    StreamJson,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(command) = &cli.command {
        return match command {
            Command::Auth { command } => match command {
                AuthCommand::Login => {
                    let auth = oauth_login(|url| eprintln!("Open this URL to sign in:\n{url}"))?;
                    println!("logged in with {}", auth.auth_mode);
                    Ok(())
                }
                AuthCommand::Status => {
                    if let Some(auth) = auth_status()? {
                        println!("{}", auth.auth_mode);
                    } else {
                        println!("not logged in");
                    }
                    Ok(())
                }
                AuthCommand::Logout => {
                    logout()?;
                    println!("logged out");
                    Ok(())
                }
            },
            Command::Plugin { command } => {
                let paths = Config::ensure_user_files()?
                    .context("HOME is not set, so plugins cannot be managed")?;
                match command {
                    PluginCommand::Add { url } => {
                        let source_url =
                            if url.starts_with("http://") || url.starts_with("https://") {
                                url.clone()
                            } else {
                                std::fs::canonicalize(url)
                                    .with_context(|| format!("failed to resolve '{url}'"))?
                                    .display()
                                    .to_string()
                            };
                        let mut source = read_plugin_source(&source_url)?;
                        source = source_with_url(&source, &source_url)?;
                        let plugin = Plugin::from_markdown(&source)?;
                        if plugin.kind != PluginKind::Plugin {
                            anyhow::bail!("plugin '{}' is not a plugin", plugin.id);
                        }
                        let path = paths.plugins_dir.join(format!("{}.md", plugin.id));
                        if path.exists() && !cli.yolo {
                            anyhow::bail!(
                                "plugin '{}' is already installed; use `mini plugin update {}` or --yolo",
                                plugin.id,
                                plugin.id
                            );
                        }
                        std::fs::write(&path, source)
                            .with_context(|| format!("failed to write '{}'", path.display()))?;
                        println!("installed {} -> {}", plugin.id, path.display());
                        Ok(())
                    }
                    PluginCommand::Update { id } => {
                        let mut ids = Vec::new();
                        if let Some(id) = id {
                            ids.push(id.clone());
                        } else {
                            for entry in
                                std::fs::read_dir(&paths.plugins_dir).with_context(|| {
                                    format!("failed to read '{}'", paths.plugins_dir.display())
                                })?
                            {
                                let entry = entry?;
                                if entry.path().extension().is_some_and(|ext| ext == "md")
                                    && let Some(id) =
                                        entry.path().file_stem().and_then(|id| id.to_str())
                                {
                                    ids.push(id.to_string());
                                }
                            }
                            ids.sort();
                        }

                        for id in ids {
                            let path = paths.plugins_dir.join(format!("{id}.md"));
                            let old = std::fs::read_to_string(&path)
                                .with_context(|| format!("failed to read '{}'", path.display()))?;
                            let plugin = Plugin::from_markdown(&old)?;
                            let Some(source_url) = plugin.source.as_deref() else {
                                eprintln!("skipped {id}: no source in plugin front matter");
                                continue;
                            };
                            let new =
                                source_with_url(&read_plugin_source(source_url)?, source_url)?;
                            let updated = Plugin::from_markdown(&new)?;
                            if updated.id != plugin.id {
                                anyhow::bail!(
                                    "refusing to update '{}': source contains plugin id '{}'",
                                    plugin.id,
                                    updated.id
                                );
                            }
                            if old == new {
                                println!("{id}: already up to date");
                                continue;
                            }
                            print_diff(&old, &new, &format!("{id}:current"), source_url);
                            if cli.yolo || confirm("Apply update? [y/N] ")? {
                                std::fs::write(&path, new).with_context(|| {
                                    format!("failed to write '{}'", path.display())
                                })?;
                                println!("{id}: updated");
                            } else {
                                println!("{id}: skipped");
                            }
                        }
                        Ok(())
                    }
                    PluginCommand::List => {
                        let mut rows = Vec::new();
                        for entry in std::fs::read_dir(&paths.plugins_dir).with_context(|| {
                            format!("failed to read '{}'", paths.plugins_dir.display())
                        })? {
                            let path = entry?.path();
                            if path.extension().is_some_and(|ext| ext == "md") {
                                match Plugin::from_markdown_file(&path) {
                                    Ok(plugin) => {
                                        rows.push(format!("{}\t{}", plugin.id, plugin.title))
                                    }
                                    Err(err) => rows.push(format!("{}\t{err}", path.display())),
                                }
                            }
                        }
                        rows.sort();
                        for row in rows {
                            println!("{row}");
                        }
                        Ok(())
                    }
                    PluginCommand::Info { id } => {
                        let path = paths.plugins_dir.join(format!("{id}.md"));
                        let plugin = Plugin::from_markdown_file(&path)
                            .with_context(|| format!("failed to load plugin '{id}'"))?;
                        println!("id: {}", plugin.id);
                        println!("title: {}", plugin.title);
                        println!("path: {}", path.display());
                        println!("source: {}", plugin.source.as_deref().unwrap_or("(none)"));
                        println!("scripts: {}", plugin.scripts.len());
                        Ok(())
                    }
                    PluginCommand::Rm { id } => {
                        let path = paths.plugins_dir.join(format!("{id}.md"));
                        let plugin = Plugin::from_markdown_file(&path)
                            .with_context(|| format!("failed to load plugin '{id}'"))?;
                        std::fs::remove_file(&path)
                            .with_context(|| format!("failed to remove '{}'", path.display()))?;
                        let manifest = paths.state_dir.join("scripts").join(&plugin.id);
                        for script in std::fs::read_to_string(&manifest)
                            .unwrap_or_default()
                            .lines()
                        {
                            let _ = std::fs::remove_file(paths.bin_dir.join(script));
                        }
                        let _ = std::fs::remove_file(manifest);
                        println!("removed {id}");
                        Ok(())
                    }
                }
            }
        };
    }

    let cwd = std::env::current_dir().context("failed to resolve current directory")?;
    let first_run = Config::user_paths().is_some_and(|paths| !paths.config_file.exists());
    let mut config = Config::load_default().context("failed to load ~/.mini-agent/config.toml")?;
    if first_run
        && !cli.print
        && io::stdin().is_terminal()
        && io::stderr().is_terminal()
        && confirm("Install and enable bundled plugins? [y/N] ")?
        && let Some(paths) = Config::user_paths()
    {
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
        config.save_default()?;
    }
    if !first_run
        && config.agent.plugins.is_empty()
        && let Some(paths) = Config::user_paths()
    {
        let source = std::fs::read_to_string(&paths.config_file).unwrap_or_default();
        let has_plugins_key = source.lines().any(|line| {
            line.trim_start()
                .strip_prefix("plugins")
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        });
        if !has_plugins_key {
            for (file_name, _) in DEFAULT_PLUGINS {
                if paths.plugins_dir.join(file_name).exists() {
                    let id = file_name.strip_suffix(".md").unwrap_or(file_name);
                    config.agent.plugins.push(id.to_string());
                }
            }
            if !config.agent.plugins.is_empty() {
                config.save_default()?;
            }
        }
    }
    let mode_spec = cli.mode.as_deref().unwrap_or(&config.agent.default_mode);
    let mode =
        load_plugin(mode_spec).with_context(|| format!("failed to load mode '{mode_spec}'"))?;
    if mode.kind != PluginKind::Mode {
        anyhow::bail!("plugin '{}' is not a mode", mode.id);
    }

    let mut plugins = Vec::new();
    for spec in &config.agent.plugins {
        let plugin =
            match load_plugin(spec).with_context(|| format!("failed to load plugin '{spec}'")) {
                Ok(plugin) => plugin,
                Err(_) if cli.ignore => continue,
                Err(err) => return Err(err),
            };
        if plugin.kind != PluginKind::Plugin {
            if cli.ignore {
                continue;
            }
            anyhow::bail!("plugin '{}' is not a plugin", plugin.id);
        }
        plugins.push(plugin);
    }
    for path in &cli.plugins {
        let plugin = match load_plugin(path)
            .with_context(|| format!("failed to load plugin '{}'", path.display()))
        {
            Ok(plugin) => plugin,
            Err(_) if cli.ignore => continue,
            Err(err) => return Err(err),
        };
        if plugin.kind != PluginKind::Plugin {
            if cli.ignore {
                continue;
            }
            anyhow::bail!("plugin '{}' is not a plugin", plugin.id);
        }
        plugins.push(plugin);
    }
    plugins.sort_by(|left, right| left.id.cmp(&right.id));
    plugins.dedup_by(|left, right| left.id == right.id);

    let active_plugins = plugins
        .iter()
        .map(|plugin| plugin.id.clone())
        .collect::<BTreeSet<_>>();
    let mut available_plugins = Vec::new();
    for plugin in plugins {
        match plugin.render(&cwd, &active_plugins) {
            Ok(_) => available_plugins.push(plugin),
            Err(err @ PluginError::MissingCommand { .. }) => {
                eprintln!("skipped plugin '{}': {err}", plugin.id);
            }
            Err(_) if cli.ignore => {}
            Err(err) => return Err(err.into()),
        }
    }
    let plugins = available_plugins;

    if let Some(paths) = Config::user_paths() {
        install_scripts(&paths, &mode, &plugins, &cwd, cli.yolo, cli.ignore)?;
    } else if !mode.scripts.is_empty() || plugins.iter().any(|plugin| !plugin.scripts.is_empty()) {
        anyhow::bail!(
            "HOME is not set, so plugin scripts cannot be installed under ~/.mini-agent/bin"
        );
    }

    if cli.check_plugins {
        for (id, error) in check_plugins(&plugins, &cwd, cli.ignore)? {
            match error {
                None => eprintln!("ok: {id}"),
                Some(err) => eprintln!("ignored: {id}: {err}"),
            }
        }
        return Ok(());
    }

    let composed_prompt = compose_prompt(
        DEFAULT_SYSTEM_PROMPT,
        Some(&mode),
        &plugins,
        &cwd,
        cli.append_system_prompt.as_deref(),
        cli.ignore,
    )?;

    if cli.explain_prompt {
        println!("{composed_prompt}");
        return Ok(());
    }

    if !cli.print {
        mini_agent_tui::run(mini_agent_tui::RunOptions {
            system_prompt: composed_prompt,
            config,
            mode: mode.id,
            plugins,
            cwd,
            append_system_prompt: cli.append_system_prompt,
            ignore_plugin_errors: cli.ignore,
            resume: cli.resume,
            session_id: cli.session,
        })?;
        return Ok(());
    }

    let inline = cli.prompt.join(" ");
    let user_prompt = if inline.trim().is_empty() {
        let mut input = String::new();
        let context = match cli.input_format {
            InputFormat::Text => "failed to read prompt from stdin",
            InputFormat::StreamJson => "failed to read stream-json input from stdin",
        };
        io::stdin().read_to_string(&mut input).context(context)?;
        match cli.input_format {
            InputFormat::Text => input,
            InputFormat::StreamJson => {
                let mut parts = Vec::new();
                for (index, line) in input.lines().enumerate() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let value: Value = serde_json::from_str(line).with_context(|| {
                        format!("stream-json input line {} is invalid JSON", index + 1)
                    })?;
                    if value["type"].as_str().is_some_and(|kind| kind != "user") {
                        continue;
                    }
                    let message = value.get("message").unwrap_or(&value);
                    if message["role"].as_str().is_some_and(|role| role != "user") {
                        continue;
                    }
                    match &message["content"] {
                        Value::String(text) if !text.trim().is_empty() => {
                            parts.push(text.to_string());
                        }
                        Value::Array(content) => {
                            for item in content {
                                if item["type"].as_str().unwrap_or("text") == "text"
                                    && let Some(text) = item["text"].as_str()
                                    && !text.trim().is_empty()
                                {
                                    parts.push(text.to_string());
                                }
                            }
                        }
                        Value::Object(_) => {
                            if message["content"]["type"].as_str().unwrap_or("text") == "text"
                                && let Some(text) = message["content"]["text"].as_str()
                                && !text.trim().is_empty()
                            {
                                parts.push(text.to_string());
                            }
                        }
                        _ => {}
                    }
                }

                if parts.is_empty() {
                    anyhow::bail!("stream-json input did not contain a user text message");
                }
                parts.join("\n")
            }
        }
    } else {
        inline
    };
    let active_provider = config.model.provider(&config.providers)?;

    if matches!(cli.output_format, OutputFormat::StreamJson) {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "type": "system",
                "subtype": "init",
                "mode": mode.id,
                "model": config.model.model,
                "provider": &active_provider.name,
                "protocol": active_provider.protocol,
                "yolo": cli.yolo,
                "ignore": cli.ignore,
                "plugins": active_plugin_ids(&plugins),
            }))?
        );
        io::stdout().flush()?;
    }

    match cli.output_format {
        OutputFormat::Text | OutputFormat::Json => {
            let mut agent = Agent::new(composed_prompt, config.clone());
            let run = agent.run(user_prompt.trim().to_string())?;
            if matches!(cli.output_format, OutputFormat::Text) {
                println!("{}", run.final_text.trim());
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "type": "result",
                        "subtype": "success",
                        "is_error": false,
                        "num_turns": run.turns,
                        "result": run.final_text.trim(),
                        "mode": mode.id,
                        "model": config.model.model,
                        "provider": &active_provider.name,
                        "protocol": active_provider.protocol,
                        "yolo": cli.yolo,
                        "ignore": cli.ignore,
                        "plugins": active_plugin_ids(&plugins),
                    }))?
                );
            }
        }
        OutputFormat::StreamJson => {
            let mut agent = Agent::new(composed_prompt, config.clone());
            let mut stream_error = None;
            let run = agent.run_with_events(user_prompt.trim().to_string(), |event| {
                if stream_error.is_some() {
                    return;
                }
                let value = match event {
                    AgentEvent::AssistantDelta(delta) => serde_json::json!({
                        "type": "assistant_delta",
                        "content": delta,
                    }),
                    AgentEvent::Assistant(text) => serde_json::json!({
                        "type": "assistant",
                        "content": text,
                    }),
                    AgentEvent::Command(command) => serde_json::json!({
                        "type": "tool_use",
                        "name": "bash",
                        "input": command,
                    }),
                    AgentEvent::CommandOutput(output) => serde_json::json!({
                        "type": "tool_result",
                        "name": "bash",
                        "status": output.status,
                        "stdout": output.stdout,
                        "stderr": output.stderr,
                    }),
                    AgentEvent::CompactionStarted { estimated_tokens } => serde_json::json!({
                        "type": "system",
                        "subtype": "compact_started",
                        "estimated_tokens": estimated_tokens,
                    }),
                    AgentEvent::CompactionFinished {
                        removed_messages,
                        summary_tokens,
                    } => serde_json::json!({
                        "type": "system",
                        "subtype": "compact_finished",
                        "removed_messages": removed_messages,
                        "summary_tokens": summary_tokens,
                    }),
                };
                match serde_json::to_string(&value) {
                    Ok(line) => {
                        println!("{line}");
                        if let Err(err) = io::stdout().flush() {
                            stream_error = Some(err.into());
                        }
                    }
                    Err(err) => stream_error = Some(err.into()),
                }
            })?;
            if let Some(err) = stream_error {
                return Err(err);
            }
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "type": "result",
                    "subtype": "success",
                    "is_error": false,
                    "num_turns": run.turns,
                    "result": run.final_text.trim(),
                }))?
            );
        }
    }

    Ok(())
}

fn read_plugin_source(source: &str) -> Result<String> {
    if source.starts_with("http://") || source.starts_with("https://") {
        let response = reqwest::blocking::get(source)
            .with_context(|| format!("failed to fetch plugin '{source}'"))?;
        if !response.status().is_success() {
            anyhow::bail!(
                "plugin fetch failed with status {}: {}",
                response.status(),
                response.text().unwrap_or_default()
            );
        }
        response
            .text()
            .with_context(|| format!("failed to read plugin '{source}'"))
    } else {
        std::fs::read_to_string(source).with_context(|| format!("failed to read '{source}'"))
    }
}

fn source_with_url(source: &str, url: &str) -> Result<String> {
    if !source.starts_with("+++\n") {
        anyhow::bail!("plugin source is missing TOML front matter delimited by +++");
    }
    let Some((front_matter, body)) = source[4..].split_once("\n+++\n") else {
        anyhow::bail!("plugin source is missing TOML front matter delimited by +++");
    };
    let lines = front_matter.lines().collect::<Vec<_>>();
    if lines
        .iter()
        .take_while(|line| !line.trim_start().starts_with('['))
        .any(|line| line.trim_start().starts_with("source"))
    {
        Ok(source.to_string())
    } else {
        let source_line = format!("source = {}", serde_json::to_string(url)?);
        let mut front_matter = String::new();
        let mut inserted = false;
        for line in lines {
            if !inserted && line.trim_start().starts_with('[') {
                front_matter.push_str(&source_line);
                front_matter.push('\n');
                inserted = true;
            }
            front_matter.push_str(line);
            front_matter.push('\n');
        }
        if !inserted {
            front_matter.push_str(&source_line);
            front_matter.push('\n');
        }
        Ok(format!(
            "+++\n{}\n+++\n{body}",
            front_matter.trim_end_matches('\n')
        ))
    }
}

fn confirm(prompt: &str) -> Result<bool> {
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

fn print_diff(old: &str, new: &str, old_label: &str, new_label: &str) {
    println!("--- {old_label}");
    println!("+++ {new_label}");
    let old_lines = old.lines().collect::<Vec<_>>();
    let new_lines = new.lines().collect::<Vec<_>>();
    let mut index = 0;
    while index < old_lines.len().max(new_lines.len()) {
        match (old_lines.get(index), new_lines.get(index)) {
            (Some(left), Some(right)) if left == right => println!(" {left}"),
            (Some(left), Some(right)) => {
                println!("-{left}");
                println!("+{right}");
            }
            (Some(left), None) => println!("-{left}"),
            (None, Some(right)) => println!("+{right}"),
            (None, None) => {}
        }
        index += 1;
    }
}
