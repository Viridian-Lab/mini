fn sessions_dir() -> Result<PathBuf> {
    let Some(paths) = Config::ensure_user_files()? else {
        anyhow::bail!("HOME is not set, so sessions cannot be stored");
    };
    let dir = paths.state_dir.join("sessions");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create '{}'", dir.display()))?;
    Ok(dir)
}

fn session_path(id: &str) -> Result<PathBuf> {
    if id.contains('/') || id.contains('\\') || id == "." || id == ".." {
        anyhow::bail!("session id must be a plain name");
    }
    Ok(sessions_dir()?.join(format!("{id}.json")))
}

fn load_session(spec: &str) -> Result<StoredSession> {
    let path = if spec == "latest" {
        let mut latest = None;
        for entry in std::fs::read_dir(sessions_dir()?)? {
            let entry = entry?;
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                let updated = entry.metadata()?.modified().ok();
                if latest
                    .as_ref()
                    .is_none_or(|(_, latest_updated)| updated > *latest_updated)
                {
                    latest = Some((path, updated));
                }
            }
        }
        latest
            .map(|(path, _)| path)
            .context("no saved sessions found")?
    } else {
        session_path(spec)?
    };
    let source = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    serde_json::from_str(&source)
        .with_context(|| format!("session file '{}' is invalid", path.display()))
}

fn save_session(app: &mut App) -> Result<()> {
    let Some(agent) = &app.agent else {
        return Ok(());
    };
    if app.session_title.is_none() {
        app.session_title = session_title_from_messages(&agent.messages);
    }

    let path = session_path(&app.session_id)?;
    if app.session_title.is_none() {
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("failed to remove empty session '{}'", path.display()))?;
        }
        return Ok(());
    }

    let session = StoredSession {
        id: app.session_id.clone(),
        title: app.session_title.clone(),
        mode: app.mode.clone(),
        system: agent.system.clone(),
        config: agent.config.clone(),
        messages: agent.messages.clone(),
    };
    std::fs::write(&path, serde_json::to_string_pretty(&session)?)
        .with_context(|| format!("failed to write '{}'", path.display()))
}

fn session_title_from_stored(session: &StoredSession) -> Option<String> {
    session
        .title
        .as_deref()
        .and_then(normalize_session_title)
        .or_else(|| session_title_from_messages(&session.messages))
}

fn session_title_from_messages(messages: &[ModelMessage]) -> Option<String> {
    messages
        .iter()
        .find(|message| {
            message.role == ModelRole::User
                && message.tool_result.is_none()
                && message.synthetic.is_none()
        })
        .and_then(|message| normalize_session_title(&message.text))
}

fn normalize_session_title(text: &str) -> Option<String> {
    let title = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        None
    } else {
        Some(fit(&title, 80))
    }
}

fn session_label(session: &StoredSession) -> String {
    let title = session_title_from_stored(session).unwrap_or_else(|| "untitled".to_string());
    format!("{} — {}", session.id, title)
}

fn messages_from_history(messages: &[ModelMessage]) -> Vec<Message> {
    let mut rendered: Vec<Message> = Vec::new();
    for message in messages {
        if let Some(result) = &message.tool_result {
            if let Some(command) = rendered
                .iter_mut()
                .rev()
                .find(|message| message.role == Role::Command && message.output.is_none())
            {
                command.output = Some(result.content.clone());
            } else {
                rendered.push(Message {
                    role: Role::Output,
                    text: result.content.clone(),
                    output: None,
                });
            }
            continue;
        }
        if !message.text.is_empty() {
            rendered.push(Message {
                role: if message.synthetic.is_some() {
                    Role::Local
                } else {
                    match message.role {
                        ModelRole::Assistant => Role::Assistant,
                        ModelRole::User => Role::User,
                    }
                },
                text: message.text.clone(),
                output: None,
            });
        }
        for call in &message.tool_calls {
            if call.name == "bash" {
                rendered.push(Message {
                    role: Role::Command,
                    text: call
                        .input
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .or_else(|| call.input.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    output: None,
                });
            }
        }
    }
    rendered
}

