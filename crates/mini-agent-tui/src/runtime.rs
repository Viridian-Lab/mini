struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            event::DisableBracketedPaste,
            cursor::Show,
            terminal::EnableLineWrap
        );
    }
}

pub fn run(options: RunOptions) -> Result<()> {
    terminal::enable_raw_mode().context("failed to enable raw terminal mode")?;
    let _guard = TerminalGuard;

    let mut stdout = io::stdout();
    // Bracketed paste delivers a paste as one Event::Paste instead of a stream
    // of key events, so multi-line pastes do not auto-submit one line at a time.
    execute!(
        stdout,
        cursor::Hide,
        terminal::EnableLineWrap,
        event::EnableBracketedPaste
    )
    .context("failed to initialize terminal")?;

    let app_dir_name = options.app_dir_name;
    let mut agent = Agent::new(options.system_prompt, options.config);
    let mut mode = options.mode;
    let mut session_title = None;
    let session_id = if let Some(spec) = options.resume {
        let session = load_session(&app_dir_name, &spec)?;
        session_title = session_title_from_stored(&session);
        mode = session.mode.clone();
        agent = Agent {
            system: session.system,
            config: session.config,
            messages: session.messages,
            tools: Vec::new(),
        };
        session.id
    } else {
        options.session_id.unwrap_or_else(|| {
            let seconds = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            format!("s{seconds}")
        })
    };
    let provider = agent.config.model.provider.clone();
    let model = agent.config.model.model.clone();
    let effort = agent.config.model.reasoning_effort.clone();
    let context_window_tokens = agent
        .config
        .agent
        .context_window_tokens
        .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS);
    let context_percent = Some(context_percent_for(
        &agent.system,
        &agent.messages,
        context_window_tokens,
    ));
    let replay = messages_from_history(&agent.messages);
    let mut app = App {
        app_dir_name,
        messages: replay,
        history: Vec::new(),
        history_index: None,
        provider: provider.clone(),
        model: model.clone(),
        effort,
        context_window_tokens,
        context_percent,
        mode: mode.clone(),
        input: Vec::new(),
        cursor: 0,
        spinner: 0,
        printed_messages: 0,
        streaming_text: String::new(),
        streaming_started: false,
        stream_message_cutoff: None,
        streaming_rows: Vec::new(),
        streaming_committed_rows: 0,
        stream_final_skip_rows: None,
        previous_bottom_rows: 0,
        rendered_width: None,
        needs_full_redraw: false,
        running_since: None,
        session_id: session_id.clone(),
        session_title,
        selection: None,
        plugins: options.plugins,
        plugin_specs: options.plugin_specs,
        cwd: options.cwd,
        append_system_prompt: options.append_system_prompt,
        ignore_plugin_errors: options.ignore_plugin_errors,
        yolo: options.yolo,
        agent: Some(agent),
        running: None,
    };
    save_session(&mut app)?;

    let width = terminal_width();
    for row in startup_banner_rows(&app, width) {
        write!(stdout, "{row}\r\n")?;
    }
    write!(stdout, "\r\n")?;

    if !app.messages.is_empty() {
        let content_width = width.saturating_sub(4) as usize;
        let until = app.messages.len();
        print_new_messages(&mut stdout, &mut app, content_width, width as usize, until)?;
    }
    app.rendered_width = Some(width);
    stdout.flush()?;

    loop {
        let mut disconnected = false;
        if let Some(running) = app.running.take() {
            let mut keep_running = true;
            loop {
                match running.receiver.try_recv() {
                    Ok(AgentUpdate::Event(event)) => {
                        if !running.interrupted.load(Ordering::Relaxed) {
                            handle_agent_event(&mut app, event);
                        }
                    }
                    Ok(AgentUpdate::Done(agent, Ok(()))) => {
                        let was_interrupted = running.interrupted.load(Ordering::Relaxed);
                        app.agent = Some(*agent);
                        app.running_since = None;
                        if was_interrupted {
                            // The run completed just before the interrupt took
                            // effect, so its tail events were dropped above.
                            // Rebuild the view from history so the UI matches
                            // what was actually saved instead of staying stuck
                            // on "interrupting model…".
                            discard_streaming(&mut app);
                            if let Some(agent) = &app.agent {
                                app.messages = messages_from_history(&agent.messages);
                            }
                            app.printed_messages = 0;
                            app.needs_full_redraw = true;
                        }
                        save_session(&mut app)?;
                        keep_running = false;
                        break;
                    }
                    Ok(AgentUpdate::Done(agent, Err(err))) => {
                        app.agent = Some(*agent);
                        app.running_since = None;
                        if running.interrupted.load(Ordering::Relaxed) {
                            discard_streaming(&mut app);
                            app.messages.push(Message {
                                role: Role::Local,
                                text: "model interrupted".to_string(),
                                output: None,
                            });
                        } else {
                            finish_streaming(&mut app);
                            app.messages.push(Message {
                                role: Role::Local,
                                text: err.to_string(),
                                output: None,
                            });
                        }
                        save_session(&mut app)?;
                        keep_running = false;
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        keep_running = false;
                        break;
                    }
                }
            }
            if keep_running {
                app.running = Some(running);
            }
        }
        if disconnected {
            finish_streaming(&mut app);
            app.running = None;
            app.running_since = None;
            // The worker thread dropped its sender without returning the agent
            // (e.g. it panicked). Rebuild the agent from the last saved session
            // so the TUI stays usable instead of wedging on a None agent.
            if app.agent.is_none()
                && let Ok(session) = load_session(&app.app_dir_name, &app.session_id)
            {
                app.agent = Some(Agent {
                    system: session.system,
                    config: session.config,
                    messages: session.messages,
                    tools: Vec::new(),
                });
            }
            app.messages.push(Message {
                role: Role::Local,
                text: "model runner stopped without returning a result".to_string(),
                output: None,
            });
            save_session(&mut app)?;
        }

        if app.running.is_some() {
            app.spinner = (app.spinner + 1) % SPINNER.len();
        }
        app.previous_bottom_rows = render(&mut stdout, &mut app)?;

        if !event::poll(Duration::from_millis(90))? {
            continue;
        }

        match event::read()? {
            Event::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            }) => {
                if matches!((code, modifiers), (KeyCode::Char('c'), KeyModifiers::CONTROL)) {
                    // If a turn is in flight, signal the worker and give it a
                    // bounded moment to return the agent so the in-flight turn
                    // (prompt, completed tool calls, outputs) is saved before we
                    // exit, rather than abandoning the thread.
                    if let Some(running) = app.running.take() {
                        running.interrupted.store(true, Ordering::Relaxed);
                        let deadline = Instant::now() + Duration::from_secs(3);
                        loop {
                            if Instant::now() > deadline {
                                break;
                            }
                            match running.receiver.recv_timeout(Duration::from_millis(100)) {
                                Ok(AgentUpdate::Done(agent, _)) => {
                                    app.agent = Some(*agent);
                                    break;
                                }
                                Ok(_) => {}
                                Err(mpsc::RecvTimeoutError::Timeout) => {}
                                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            }
                        }
                        let _ = save_session(&mut app);
                    }
                    break;
                }

                if matches!(code, KeyCode::Esc) && app.running.is_some() {
                    interrupt_running_model(&mut app);
                    continue;
                }

                if app.selection.is_some() {
                    let command_palette = matches!(
                        app.selection.as_ref().map(|selection| selection.command),
                        Some(SelectionCommand::CommandPalette)
                    );
                    match (code, modifiers) {
                        (KeyCode::Esc, _) => {
                            app.selection = None;
                        }
                        (KeyCode::Up, _) => {
                            if let Some(selection) = &mut app.selection
                                && !selection.items.is_empty()
                            {
                                selection.selected = selection.selected.saturating_sub(1);
                            }
                        }
                        (KeyCode::Down, _) => {
                            if let Some(selection) = &mut app.selection
                                && !selection.items.is_empty()
                            {
                                selection.selected =
                                    (selection.selected + 1).min(selection.items.len() - 1);
                            }
                        }
                        (KeyCode::Enter, _) => {
                            if let Some(selection) = app.selection.take() {
                                if let Some(item) = selection.items.get(selection.selected) {
                                    let item = item.value.clone();
                                    let result = match selection.command {
                                        SelectionCommand::CommandPalette => {
                                            app.input.clear();
                                            app.cursor = 0;
                                            if item == "/model add" {
                                                app.input = "/model add ".chars().collect();
                                                app.cursor = app.input.len();
                                                Ok(())
                                            } else {
                                                handle_slash_command(&mut app, &item)
                                            }
                                        }
                                        SelectionCommand::Provider => set_provider(&mut app, &item),
                                        SelectionCommand::Model => set_model(&mut app, &item),
                                        SelectionCommand::Mode => set_mode(&mut app, &item),
                                        SelectionCommand::Effort => set_effort(&mut app, &item),
                                        SelectionCommand::Resume => resume_session(&mut app, &item),
                                    };
                                    if let Err(err) = result {
                                        app.messages.push(Message {
                                            role: Role::Local,
                                            text: err.to_string(),
                                            output: None,
                                        });
                                    }
                                }
                                save_session(&mut app)?;
                            }
                        }
                        (KeyCode::Backspace, _) if command_palette && app.cursor > 0 => {
                            app.input.remove(app.cursor - 1);
                            app.cursor -= 1;
                            app.history_index = None;
                            sync_command_palette(&mut app);
                        }
                        (KeyCode::Delete, _) if command_palette && app.cursor < app.input.len() => {
                            app.input.remove(app.cursor);
                            app.history_index = None;
                            sync_command_palette(&mut app);
                        }
                        (KeyCode::Left, _) if command_palette => {
                            app.cursor = app.cursor.saturating_sub(1);
                        }
                        (KeyCode::Right, _) if command_palette => {
                            app.cursor = (app.cursor + 1).min(app.input.len());
                        }
                        (KeyCode::Home, _) if command_palette => app.cursor = 0,
                        (KeyCode::End, _) if command_palette => app.cursor = app.input.len(),
                        (KeyCode::Char(char), _)
                            if command_palette && !modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            app.input.insert(app.cursor, char);
                            app.cursor += 1;
                            app.history_index = None;
                            sync_command_palette(&mut app);
                        }
                        _ => {}
                    }
                    continue;
                }

                match (code, modifiers) {
                    (KeyCode::Esc, _) => {}
                    (KeyCode::Enter, _) => {
                        if app.running.is_some() {
                            continue;
                        }
                        let prompt = app.input.iter().collect::<String>();
                        if prompt.trim().is_empty() {
                            continue;
                        }
                        app.history.push(prompt.clone());
                        app.history_index = None;
                        app.input.clear();
                        app.cursor = 0;
                        if prompt.trim_start().starts_with('/') {
                            if let Err(err) = handle_slash_command(&mut app, &prompt) {
                                app.messages.push(Message {
                                    role: Role::Local,
                                    text: err.to_string(),
                                    output: None,
                                });
                            }
                            save_session(&mut app)?;
                            continue;
                        }
                        app.messages.push(Message {
                            role: Role::User,
                            text: prompt.clone(),
                            output: None,
                        });
                        let (sender, receiver) = mpsc::channel();
                        let interrupted = Arc::new(AtomicBool::new(false));
                        let thread_interrupted = interrupted.clone();
                        let Some(mut agent) = app.agent.take() else {
                            app.messages.push(Message {
                                role: Role::Local,
                                text: "agent state was lost; /resume the session to continue"
                                    .to_string(),
                                output: None,
                            });
                            continue;
                        };
                        let mut context_messages = agent.messages.clone();
                        context_messages.push(ModelMessage {
                            role: ModelRole::User,
                            text: prompt.clone(),
                            tool_calls: Vec::new(),
                            tool_result: None,
                            synthetic: None,
                            thinking: Vec::new(),
                        });
                        app.context_percent = Some(context_percent_for(
                            &agent.system,
                            &context_messages,
                            app.context_window_tokens,
                        ));
                        thread::spawn(move || {
                            let event_sender = sender.clone();
                            let result = agent
                                .run_with_events_interruptible(
                                    prompt,
                                    |event| {
                                        let _ = event_sender.send(AgentUpdate::Event(event));
                                    },
                                    Some(thread_interrupted),
                                )
                                .map(|_| ());
                            let _ = sender.send(AgentUpdate::Done(Box::new(agent), result));
                        });
                        app.running = Some(RunningAgent {
                            receiver,
                            interrupted,
                        });
                        app.running_since = Some(Instant::now());
                    }
                    (KeyCode::Backspace, _) if app.cursor > 0 => {
                        app.input.remove(app.cursor - 1);
                        app.cursor -= 1;
                        app.history_index = None;
                        sync_command_palette(&mut app);
                    }
                    (KeyCode::Delete, _) if app.cursor < app.input.len() => {
                        app.input.remove(app.cursor);
                        app.history_index = None;
                        sync_command_palette(&mut app);
                    }
                    (KeyCode::Left, _) => {
                        app.cursor = app.cursor.saturating_sub(1);
                    }
                    (KeyCode::Right, _) => {
                        app.cursor = (app.cursor + 1).min(app.input.len());
                    }
                    (KeyCode::Home, _) => app.cursor = 0,
                    (KeyCode::End, _) => app.cursor = app.input.len(),
                    (KeyCode::Up, _) if !app.history.is_empty() => {
                        let index = app
                            .history_index
                            .map(|index: usize| index.saturating_sub(1))
                            .unwrap_or(app.history.len() - 1);
                        app.history_index = Some(index);
                        app.input = app.history[index].chars().collect();
                        app.cursor = app.input.len();
                    }
                    (KeyCode::Down, _) if !app.history.is_empty() => {
                        let Some(index) = app.history_index else {
                            continue;
                        };
                        if index + 1 >= app.history.len() {
                            app.history_index = None;
                            app.input.clear();
                        } else {
                            app.history_index = Some(index + 1);
                            app.input = app.history[index + 1].chars().collect();
                        }
                        app.cursor = app.input.len();
                    }
                    (KeyCode::Char(char), _) if !modifiers.contains(KeyModifiers::CONTROL) => {
                        app.input.insert(app.cursor, char);
                        app.cursor += 1;
                        app.history_index = None;
                        sync_command_palette(&mut app);
                    }
                    _ => {}
                }
            }
            Event::Paste(text) => {
                // Preserve newlines (so pasted multi-line code is not silently
                // concatenated) and expand tabs; drop other control characters.
                let normalized = text
                    .replace("\r\n", "\n")
                    .replace('\r', "\n")
                    .replace('\t', "    ");
                for char in normalized.chars() {
                    if char == '\n' || !char.is_control() {
                        app.input.insert(app.cursor, char);
                        app.cursor += 1;
                    }
                }
                app.history_index = None;
                sync_command_palette(&mut app);
            }
            Event::Resize(_, _) => {
                app.needs_full_redraw = true;
            }
            _ => {}
        }
    }

    save_session(&mut app)?;
    write!(stdout, "\r\n")?;
    stdout.flush()?;
    Ok(())
}

fn interrupt_running_model(app: &mut App) {
    let Some(running) = &app.running else {
        return;
    };
    if !running.interrupted.swap(true, Ordering::Relaxed) {
        app.selection = None;
        discard_streaming(app);
        app.messages.push(Message {
            role: Role::Local,
            text: "interrupting model…".to_string(),
            output: None,
        });
    }
}

fn discard_streaming(app: &mut App) {
    app.streaming_text.clear();
    app.streaming_started = false;
    app.stream_message_cutoff = None;
    app.stream_final_skip_rows = None;
    app.streaming_rows.clear();
    app.streaming_committed_rows = 0;
}

fn finish_streaming(app: &mut App) {
    if !app.streaming_started {
        return;
    }
    let text = std::mem::take(&mut app.streaming_text);
    app.streaming_started = false;
    app.stream_message_cutoff = None;
    if !text.is_empty() {
        app.stream_final_skip_rows = Some(app.streaming_committed_rows);
        app.messages.push(Message {
            role: Role::Assistant,
            text,
            output: None,
        });
    }
    app.streaming_rows.clear();
    app.streaming_committed_rows = 0;
}

