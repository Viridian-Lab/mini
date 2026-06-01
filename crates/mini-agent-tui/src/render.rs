fn startup_banner_rows(app: &App, max_width: u16) -> Vec<String> {
    let workspace = if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Ok(relative) = app.cwd.strip_prefix(home)
    {
        if relative.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", relative.display())
        }
    } else {
        app.cwd.display().to_string()
    };
    let info = [
        ("mini", env!("CARGO_PKG_VERSION").to_string()),
        (
            "plugins",
            Config::user_paths()
                .and_then(|paths| {
                    std::fs::read_dir(paths.plugins_dir).ok().map(|entries| {
                        entries
                            .filter_map(Result::ok)
                            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "md"))
                            .count()
                    })
                })
                .map(|installed| format!("{} active / {installed} installed", app.plugins.len()))
                .unwrap_or_else(|| format!("{} active", app.plugins.len())),
        ),
        ("workspace", workspace),
        (
            "session",
            app.session_title
                .as_deref()
                .map(|title| format!("{} — {title}", app.session_id))
                .unwrap_or_else(|| app.session_id.clone()),
        ),
        ("model", app.model.clone()),
        ("mode", app.mode.clone()),
    ];
    let label_width = info
        .iter()
        .map(|(label, _)| label.len())
        .max()
        .unwrap_or_default();
    let info = info
        .iter()
        .map(|(label, value)| {
            format!(
                "{}  {}",
                paint(&format!("{label:<label_width$}"), BOLD_WHITE),
                paint(value, BRIGHT_BLACK)
            )
        })
        .collect::<Vec<_>>();
    let wordmark = BANNER.lines().collect::<Vec<_>>();
    let left_width = BANNER.lines().map(visible_width).max().unwrap_or_default();
    let separator_width = visible_width(" │ ");
    let right_width = info
        .iter()
        .map(|row| visible_width(row))
        .max()
        .unwrap_or_default();
    let width = (left_width + separator_width + right_width + 4)
        .min(max_width as usize)
        .max(4) as u16;
    let divider_column = left_width + 3;
    let has_divider = width >= 4
        && width as usize - 4 > left_width + separator_width
        && divider_column < width as usize - 1;
    let height = info.len().max(wordmark.len());
    let wordmark_top = height.saturating_sub(wordmark.len()) / 2;
    let inner = width as usize - 4;
    let separator = format!(" {} ", paint("│", INPUT_FRAME));
    let mut rows = if has_divider {
        let before = divider_column - 1;
        let after = width as usize - 3 - before;
        vec![paint(
            &format!("╭{}┬{}╮", "─".repeat(before), "─".repeat(after)),
            INPUT_FRAME,
        )]
    } else {
        vec![top_border(width)]
    };
    for index in 0..height {
        let left = index
            .checked_sub(wordmark_top)
            .and_then(|index| wordmark.get(index))
            .copied()
            .unwrap_or_default();
        let right = info.get(index).map(String::as_str).unwrap_or_default();
        let text = if inner > left_width + separator_width + 4 {
            let right_width = inner - left_width - separator_width;
            let left_padding = left_width.saturating_sub(visible_width(left)) / 2;
            let wordmark = paint(
                &format!(
                    "{}{}{}",
                    " ".repeat(left_padding),
                    left,
                    " ".repeat(left_width - left_padding - visible_width(left))
                ),
                BOLD_WHITE,
            );
            format!("{}{}{}", wordmark, separator, fit(right, right_width))
        } else {
            fit(right, inner)
        };

        rows.push(format!(
            "{} {}{} {}",
            paint("│", INPUT_FRAME),
            text,
            " ".repeat(inner.saturating_sub(visible_width(&text))),
            paint("│", INPUT_FRAME)
        ));
    }
    if has_divider {
        let before = divider_column - 1;
        let after = width as usize - 3 - before;
        rows.push(paint(
            &format!("╰{}┴{}╯", "─".repeat(before), "─".repeat(after)),
            INPUT_FRAME,
        ));
    } else {
        rows.push(bottom_border(width, "", ""));
    }
    rows
}

fn render(stdout: &mut Stdout, app: &mut App) -> Result<u16> {
    if app.previous_bottom_rows > 0 {
        queue!(stdout, cursor::MoveUp(app.previous_bottom_rows))?;
    }
    queue!(
        stdout,
        cursor::MoveToColumn(0),
        terminal::Clear(terminal::ClearType::FromCursorDown)
    )?;

    let (width, _) = terminal::size().unwrap_or((80, 24));
    let width = width.max(32);
    let content_width = width.saturating_sub(4) as usize;

    let stream_message_cutoff = app.stream_message_cutoff.unwrap_or(app.messages.len());
    print_new_messages(
        stdout,
        app,
        content_width,
        width as usize,
        stream_message_cutoff,
    )?;

    let mut bottom_rows = Vec::new();
    if app.streaming_started {
        let stream_message = Message {
            role: Role::Assistant,
            text: app.streaming_text.clone(),
            output: None,
        };
        let rows = message_rows(&stream_message, content_width, width as usize);
        let common_rows = app
            .streaming_rows
            .iter()
            .zip(&rows)
            .take_while(|(old, new)| old == new)
            .count();
        app.streaming_committed_rows = app.streaming_committed_rows.min(rows.len());
        let commit_until = common_rows
            .saturating_sub(STREAM_UNSTABLE_ROWS)
            .max(app.streaming_committed_rows)
            .min(rows.len());
        for row in &rows[app.streaming_committed_rows..commit_until] {
            write!(stdout, "{row}\r\n")?;
        }
        app.streaming_committed_rows = commit_until;
        app.streaming_rows = rows.clone();
        let rows = rows
            .into_iter()
            .skip(app.streaming_committed_rows)
            .collect::<Vec<_>>();
        bottom_rows.extend(rows);
        bottom_rows.push(String::new());
    } else {
        print_new_messages(
            stdout,
            app,
            content_width,
            width as usize,
            app.messages.len(),
        )?;
        app.stream_message_cutoff = None;
    }

    if let Some(selection) = &app.selection {
        let visible = selection.items.len().min(8);
        let mut start = selection.selected.saturating_sub(visible / 2);
        start = start.min(selection.items.len().saturating_sub(visible));
        let end = (start + visible).min(selection.items.len());

        bottom_rows.push(paint(&format!("/{}", selection.title), BOLD_WHITE));
        if selection.items.is_empty() {
            bottom_rows.push(paint("  no matches", BRIGHT_BLACK));
        } else {
            for index in start..end {
                let prefix = if index == selection.selected {
                    "›"
                } else {
                    " "
                };
                bottom_rows.push(paint(
                    &fit(
                        &format!("{prefix} {}", selection.items[index].label),
                        content_width,
                    ),
                    if index == selection.selected {
                        BOLD_WHITE
                    } else {
                        BRIGHT_BLACK
                    },
                ));
            }
        }
        bottom_rows.push(String::new());
    }

    let mut input_chars = app.input.clone();
    input_chars.insert(app.cursor.min(input_chars.len()), '▌');
    let input_text = if app.input.is_empty() {
        "▌".to_string()
    } else {
        input_chars.into_iter().collect::<String>()
    };
    let mut input_lines = wrap_chars(&input_text, content_width.max(1));
    for line in &mut input_lines {
        if line.contains('▌') {
            *line = line.replace('▌', &paint("▌", BOLD_WHITE));
        }
    }
    let input_rows = input_lines.len().min(5);
    let status = if app.running.is_some() {
        let elapsed = app
            .running_since
            .map(|started| started.elapsed())
            .unwrap_or_default();
        let seconds = elapsed.as_secs();
        let elapsed = if seconds < 60 {
            format!("{seconds}s")
        } else if seconds < 3600 {
            format!("{}m {:02}s", seconds / 60, seconds % 60)
        } else {
            format!(
                "{}h {:02}m {:02}s",
                seconds / 3600,
                seconds % 3600 / 60,
                seconds % 60
            )
        };
        format!("{} {}", SPINNER[app.spinner], elapsed)
    } else {
        String::new()
    };
    let context = context_status(app);
    let mut model = format!("{}/{}", app.provider, app.model);
    if let Some(effort) = &app.effort {
        model.push(' ');
        model.push_str(effort);
    }
    let status = paint(&status, BOLD_WHITE);
    let model = paint(&format!("{context} {model}"), BRIGHT_BLACK);
    bottom_rows.push(top_border(width));
    for line in input_lines.iter().take(input_rows) {
        if width < 4 {
            bottom_rows.push(fit(line, width as usize));
        } else {
            let inner = width as usize - 4;
            let line = fit(line, inner);
            bottom_rows.push(format!(
                "{} {}{} {}",
                paint("│", INPUT_FRAME),
                line,
                " ".repeat(inner - visible_width(&line)),
                paint("│", INPUT_FRAME)
            ));
        }
    }
    bottom_rows.push(bottom_border(width, &status, &model));

    for (index, row) in bottom_rows.iter().enumerate() {
        if index + 1 == bottom_rows.len() {
            write!(stdout, "{row}")?;
        } else {
            write!(stdout, "{row}\r\n")?;
        }
    }
    stdout.flush()?;
    Ok(bottom_rows.len().saturating_sub(1) as u16)
}

fn handle_agent_event(app: &mut App, event: AgentEvent) {
    match event {
        AgentEvent::AssistantDelta(delta) => {
            if !app.streaming_started {
                app.streaming_started = true;
                app.stream_message_cutoff = Some(app.messages.len());
                app.streaming_rows.clear();
                app.streaming_committed_rows = 0;
                app.stream_final_skip_rows = None;
            }
            app.streaming_text.push_str(&delta);
        }
        AgentEvent::Assistant(text) => {
            if app.streaming_started {
                app.streaming_text = text;
                app.streaming_started = false;
                app.stream_message_cutoff = None;
                app.stream_final_skip_rows = Some(app.streaming_committed_rows);
                app.streaming_rows.clear();
                app.streaming_committed_rows = 0;
                app.messages.push(Message {
                    role: Role::Assistant,
                    text: std::mem::take(&mut app.streaming_text),
                    output: None,
                });
            } else {
                app.messages.push(Message {
                    role: Role::Assistant,
                    text,
                    output: None,
                });
            }
        }
        AgentEvent::Command(command) => app.messages.push(Message {
            role: Role::Command,
            text: command,
            output: None,
        }),
        AgentEvent::CommandOutput(output) => {
            let mut text = String::new();
            if output.status != Some(0) {
                let status = output
                    .status
                    .map(|status| format!("command failed with exit status {status}"))
                    .unwrap_or_else(|| "command terminated by signal".to_string());
                text.push_str(&status);
            }
            let stdout = output.stdout.trim_end();
            if !stdout.is_empty() {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&truncate_output(stdout));
            }
            let stderr = output.stderr.trim_end();
            if !stderr.is_empty() {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str("stderr:\n");
                text.push_str(&truncate_output(stderr));
            }
            if text.is_empty() {
                text.push_str("command completed with no output");
            }
            if let Some(command) = app
                .messages
                .iter_mut()
                .rev()
                .take_while(|message| message.role != Role::User)
                .find(|message| message.role == Role::Command && message.output.is_none())
            {
                command.output = Some(text);
            } else {
                app.messages.push(Message {
                    role: Role::Output,
                    text,
                    output: None,
                });
            }
        }
        AgentEvent::CompactionStarted { estimated_tokens } => app.messages.push(Message {
            role: Role::Local,
            text: format!("compacting conversation history (~{estimated_tokens} tokens)"),
            output: None,
        }),
        AgentEvent::CompactionFinished {
            removed_messages,
            summary_tokens,
        } => app.messages.push(Message {
            role: Role::Local,
            text: format!(
                "compacted {removed_messages} earlier messages into summary (~{summary_tokens} tokens)"
            ),
            output: None,
        }),
    }
}

fn truncate_output(output: &str) -> String {
    let lines = output.lines().collect::<Vec<_>>();
    let limit = OUTPUT_HEAD_LINES + OUTPUT_TAIL_LINES;
    if lines.len() <= limit {
        return output.to_string();
    }

    let omitted = lines.len() - limit;
    let mut truncated = String::new();
    truncated.push_str(&lines[..OUTPUT_HEAD_LINES].join("\n"));
    truncated.push_str("\n\n");
    truncated.push_str(&format!(
        "[... {omitted} lines omitted; showing last {OUTPUT_TAIL_LINES} lines ...]"
    ));
    truncated.push_str("\n\n");
    truncated.push_str(&lines[lines.len() - OUTPUT_TAIL_LINES..].join("\n"));
    truncated
}

fn context_status(app: &App) -> String {
    let percent = app
        .agent
        .as_ref()
        .map(|agent| context_percent_for(&agent.system, &agent.messages, app.context_window_tokens))
        .or(app.context_percent)
        .unwrap_or(0);
    format!("{percent}%")
}

fn context_percent_for(
    system: &str,
    messages: &[ModelMessage],
    context_window_tokens: usize,
) -> usize {
    if context_window_tokens == 0 {
        return 0;
    }
    let estimated = estimate_messages_tokens(system, messages);
    ((estimated as f64 / context_window_tokens as f64) * 100.0).round() as usize
}
