
fn handle_slash_command(app: &mut App, prompt: &str) -> Result<()> {
    let mut parts = prompt.split_whitespace();
    let command = parts.next().unwrap_or_default();
    match command {
        "/help" => {
            app.messages.push(Message {
                role: Role::Local,
                text: [
                    "/provider [name]",
                    "/model [name]",
                    "/model add <name>",
                    "/mode [name]",
                    "/effort [none|on|minimal|low|medium|high|xhigh|custom]",
                    "/session",
                    "/resume [session]",
                    "/compact",
                    "/compact status",
                ]
                .join("\n"),
                output: None,
            });
            Ok(())
        }
        "/provider" => match parts.next() {
            Some(provider) => set_provider(app, provider),
            None => {
                let agent = app.agent.as_ref().context("agent is already running")?;
                let mut names = BUILT_IN_PROVIDER_NAMES
                    .iter()
                    .map(|name| name.to_string())
                    .collect::<Vec<_>>();
                for name in agent.config.providers.keys() {
                    if !names.contains(name) {
                        names.push(name.clone());
                    }
                }
                open_selection(
                    app,
                    "provider",
                    SelectionCommand::Provider,
                    selection_items(names),
                    &app.provider.clone(),
                );
                Ok(())
            }
        },
        "/model" => match parts.next() {
            Some("add") => {
                let Some(model) = parts.next() else {
                    anyhow::bail!("usage: /model add <name>");
                };
                let agent = app.agent.as_mut().context("agent is already running")?;
                let provider = agent.config.model.provider(&agent.config.providers)?;
                if !provider.models.iter().any(|known| known == model) {
                    let models = &mut agent
                        .config
                        .providers
                        .entry(provider.name.clone())
                        .or_insert_with(ProviderConfig::default)
                        .models;
                    if !models.iter().any(|known| known == model) {
                        models.push(model.to_string());
                    }
                }

                agent.config.model.model = model.to_string();
                save_model_config(&agent.config)?;

                app.model = model.to_string();
                app.messages.push(Message {
                    role: Role::Local,
                    text: format!("added {model} to {}; model set to {model}", app.provider),
                    output: None,
                });
                Ok(())
            }
            Some(model) => set_model(app, model),
            None => {
                let agent = app.agent.as_ref().context("agent is already running")?;
                let models = list_models(&agent.config.model, &agent.config.providers)?;
                if models.is_empty() {
                    anyhow::bail!(
                        "provider '{}' has no models yet; use /model add <name>",
                        app.provider
                    );
                }
                open_selection(
                    app,
                    "model",
                    SelectionCommand::Model,
                    selection_items(models),
                    &app.model.clone(),
                );
                Ok(())
            }
        },
        "/mode" => match parts.next() {
            Some(mode) => set_mode(app, mode),
            None => {
                let Some(paths) = Config::ensure_user_files()? else {
                    anyhow::bail!("HOME is not set, so modes cannot be listed");
                };
                let mut names = Vec::new();
                for entry in std::fs::read_dir(&paths.modes_dir)
                    .with_context(|| format!("failed to read '{}'", paths.modes_dir.display()))?
                {
                    let entry = entry?;
                    let path = entry.path();
                    if path.extension().is_some_and(|extension| extension == "md")
                        && let Some(name) = path.file_stem().and_then(|name| name.to_str())
                    {
                        names.push(name.to_string());
                    }
                }
                if !names.contains(&app.mode) {
                    names.push(app.mode.clone());
                }
                names.sort();
                open_selection(
                    app,
                    "mode",
                    SelectionCommand::Mode,
                    selection_items(names),
                    &app.mode.clone(),
                );
                Ok(())
            }
        },
        "/effort" => match parts.next() {
            Some(effort) => set_effort(app, effort),
            None => {
                let current = app.effort.clone().unwrap_or_else(|| "none".to_string());
                open_selection(
                    app,
                    "effort",
                    SelectionCommand::Effort,
                    selection_items(
                        ["none", "on", "minimal", "low", "medium", "high", "xhigh"]
                            .into_iter()
                            .map(str::to_string)
                            .collect(),
                    ),
                    &current,
                );
                Ok(())
            }
        },
        "/session" => {
            app.messages.push(Message {
                role: Role::Local,
                text: format!(
                    "session: {}
title: {}
resume with: mini --resume {}",
                    app.session_id,
                    app.session_title.as_deref().unwrap_or("untitled"),
                    app.session_id
                ),
                output: None,
            });
            Ok(())
        }
        "/compact" => match parts.next() {
            Some("status") => {
                let agent = app.agent.as_ref().context("agent is already running")?;
                let estimated = estimate_messages_tokens(&agent.system, &agent.messages);
                let limit = agent
                    .config
                    .agent
                    .context_window_tokens
                    .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS)
                    .to_string();
                app.messages.push(Message {
                    role: Role::Local,
                    text: format!(
                        "estimated context: ~{estimated} tokens
context window: {limit}
auto compact: {}
threshold: {}
keep recent messages: {}",
                        agent.config.agent.auto_compact,
                        agent.config.agent.compact_threshold,
                        agent.config.agent.compact_keep_recent
                    ),
                    output: None,
                });
                Ok(())
            }
            Some(other) => anyhow::bail!("unknown /compact subcommand '{other}'; try /compact or /compact status"),
            None => {
                let mut agent = app.agent.take().context("agent is already running")?;
                let estimated = estimate_messages_tokens(&agent.system, &agent.messages);
                agent.compact_history(&mut |event| handle_agent_event(app, event), estimated)?;
                app.agent = Some(agent);
                Ok(())
            }
        },
        "/resume" => match parts.next() {
            Some(session) => resume_session(app, session),
            None => {
                let mut sessions = Vec::new();
                for entry in std::fs::read_dir(sessions_dir()?)? {
                    let entry = entry?;
                    let path = entry.path();
                    if path.extension().is_none_or(|extension| extension != "json") {
                        continue;
                    }
                    let Some(id) = path.file_stem().and_then(|id| id.to_str()) else {
                        continue;
                    };
                    let updated = entry
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .ok();
                    let label = load_session(id)
                        .map(|session| session_label(&session))
                        .unwrap_or_else(|_| id.to_string());
                    sessions.push((id.to_string(), label, updated));
                }
                sessions
                    .sort_by(|left, right| right.2.cmp(&left.2).then_with(|| left.0.cmp(&right.0)));
                open_selection(
                    app,
                    "resume",
                    SelectionCommand::Resume,
                    sessions
                        .into_iter()
                        .map(|(id, label, _)| SelectionItem { label, value: id })
                        .collect(),
                    &app.session_id.clone(),
                );
                Ok(())
            }
        },
        _ => anyhow::bail!("unknown slash command '{command}'; try /help"),
    }
}

fn sync_command_palette(app: &mut App) {
    if app
        .selection
        .as_ref()
        .is_some_and(|selection| !matches!(selection.command, SelectionCommand::CommandPalette))
    {
        return;
    }

    let prompt = app.input.iter().collect::<String>();
    if !prompt.starts_with('/') || prompt.chars().any(char::is_whitespace) {
        if app
            .selection
            .as_ref()
            .is_some_and(|selection| matches!(selection.command, SelectionCommand::CommandPalette))
        {
            app.selection = None;
        }
        return;
    }

    let selected = app
        .selection
        .as_ref()
        .and_then(|selection| selection.items.get(selection.selected))
        .map(|item| item.value.clone());
    let items = selection_items(
        SLASH_COMMANDS
            .into_iter()
            .filter(|command| command.starts_with(&prompt))
            .map(str::to_string)
            .collect(),
    );
    let selected = selected
        .and_then(|selected| items.iter().position(|item| item.value == selected))
        .unwrap_or_default();
    app.selection = Some(Selection {
        title: "commands".to_string(),
        command: SelectionCommand::CommandPalette,
        items,
        selected,
    });
}

fn resume_session(app: &mut App, session: &str) -> Result<()> {
    save_session(app)?;
    let session = load_session(session)?;
    let session_id = session.id.clone();
    let session_title = session_title_from_stored(&session);
    let mode = session.mode.clone();
    let agent = Agent {
        system: session.system,
        config: session.config,
        messages: session.messages,
    };

    app.session_id = session_id;
    app.session_title = session_title;
    app.provider = agent.config.model.provider.clone();
    app.model = agent.config.model.model.clone();
    app.effort = agent.config.model.reasoning_effort.clone();
    app.context_window_tokens = agent
        .config
        .agent
        .context_window_tokens
        .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS);
    app.mode = mode;
    app.messages = messages_from_history(&agent.messages);
    app.messages.push(Message {
        role: Role::Local,
        text: format!(
            "resumed session {}{}",
            app.session_id,
            app.session_title
                .as_deref()
                .map(|title| format!(" — {title}"))
                .unwrap_or_default()
        ),
        output: None,
    });
    app.printed_messages = 0;
    app.streaming_text.clear();
    app.streaming_started = false;
    app.stream_message_cutoff = None;
    app.streaming_rows.clear();
    app.streaming_committed_rows = 0;
    app.stream_final_skip_rows = None;
    app.agent = Some(agent);
    Ok(())
}

fn set_provider(app: &mut App, provider: &str) -> Result<()> {
    let agent = app.agent.as_mut().context("agent is already running")?;
    let mut model_config = agent.config.model.clone();
    model_config.provider = provider.to_string();
    let provider_info = model_config.provider(&agent.config.providers)?;

    agent.config.model.provider = provider.to_string();
    if !provider_info.models.is_empty() && !provider_info.models.contains(&agent.config.model.model)
    {
        agent.config.model.model = provider_info.models[0].clone();
    }
    save_model_config(&agent.config)?;

    app.provider = provider.to_string();
    app.model = agent.config.model.model.clone();
    app.messages.push(Message {
        role: Role::Local,
        text: format!("provider set to {}; model is {}", app.provider, app.model),
        output: None,
    });
    Ok(())
}

fn set_model(app: &mut App, model: &str) -> Result<()> {
    let agent = app.agent.as_mut().context("agent is already running")?;
    let provider = agent.config.model.provider(&agent.config.providers)?;
    let models = list_models(&agent.config.model, &agent.config.providers)?;
    if !models.is_empty() && !models.iter().any(|known| known == model) {
        anyhow::bail!(
            "model '{}' is not listed for provider '{}'; use /model add {}",
            model,
            provider.name,
            model
        );
    }

    agent.config.model.model = model.to_string();
    save_model_config(&agent.config)?;

    app.model = model.to_string();
    app.messages.push(Message {
        role: Role::Local,
        text: format!("model set to {model}"),
        output: None,
    });
    Ok(())
}

fn set_effort(app: &mut App, effort: &str) -> Result<()> {
    let agent = app.agent.as_mut().context("agent is already running")?;
    let effort = effort.trim();
    let known = effort.to_ascii_lowercase();
    let effort = match known.as_str() {
        "" | "none" | "off" | "clear" | "false" | "no" | "0" => None,
        "minimal" | "low" | "medium" | "high" | "xhigh" | "on" | "true" | "yes" => Some(known),
        _ => Some(effort.to_string()),
    };
    agent.config.model.reasoning_effort = effort.clone();
    save_model_config(&agent.config)?;

    app.effort = effort;
    app.messages.push(Message {
        role: Role::Local,
        text: match &app.effort {
            Some(effort) => format!("reasoning effort set to {effort}"),
            None => "reasoning effort cleared".to_string(),
        },
        output: None,
    });
    Ok(())
}

fn set_mode(app: &mut App, mode: &str) -> Result<()> {
    let mode = load_plugin(mode).with_context(|| format!("failed to load mode '{mode}'"))?;
    if mode.kind != PluginKind::Mode {
        anyhow::bail!("plugin '{}' is not a mode", mode.id);
    }

    let agent = app.agent.as_mut().context("agent is already running")?;
    agent.config.agent.default_mode = mode.id.clone();
    agent.system = compose_prompt(
        DEFAULT_SYSTEM_PROMPT,
        Some(&mode),
        &app.plugins,
        &app.cwd,
        app.append_system_prompt.as_deref(),
        app.ignore_plugin_errors,
    )?;
    let mut saved = Config::load_default()?;
    saved.agent.default_mode = agent.config.agent.default_mode.clone();
    saved.save_default()?;

    app.mode = mode.id;
    app.messages.push(Message {
        role: Role::Local,
        text: if app.mode == "default" {
            "mode set to default".to_string()
        } else {
            format!("mode set to {}", app.mode)
        },
        output: None,
    });
    Ok(())
}

fn save_model_config(config: &Config) -> Result<()> {
    let mut saved = Config::load_default()?;
    saved.model = config.model.clone();
    saved.providers = config.providers.clone();
    saved.save_default()
}

fn open_selection(
    app: &mut App,
    title: &str,
    command: SelectionCommand,
    items: Vec<SelectionItem>,
    current: &str,
) {
    if items.is_empty() {
        app.messages.push(Message {
            role: Role::Local,
            text: format!("no {title}s configured"),
            output: None,
        });
        return;
    }
    let selected = items
        .iter()
        .position(|item| item.value == current)
        .unwrap_or_default();
    app.selection = Some(Selection {
        title: title.to_string(),
        command,
        items,
        selected,
    });
}

fn selection_items(items: Vec<String>) -> Vec<SelectionItem> {
    items
        .into_iter()
        .map(|item| SelectionItem {
            label: item.clone(),
            value: item,
        })
        .collect()
}
