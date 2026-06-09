
fn print_new_messages(
    stdout: &mut Stdout,
    app: &mut App,
    content_width: usize,
    row_width: usize,
    until: usize,
) -> Result<()> {
    let until = until.min(app.messages.len());
    if app.printed_messages < until {
        let messages = &app.messages[app.printed_messages..until];
        let mut index = 0;
        while index < messages.len() {
            let message = &messages[index];
            let mut rows = message_rows(message, content_width, row_width);
            if message.role == Role::Assistant
                && let Some(skip) = app.stream_final_skip_rows.take()
            {
                rows.drain(..skip.min(rows.len()));
            }
            for row in rows {
                write!(stdout, "{row}\r\n")?;
            }
            if !matches!(
                messages.get(index + 1).map(|message| message.role),
                Some(Role::Command | Role::Output)
            ) {
                write!(stdout, "\r\n")?;
            }
            index += 1;
        }
        app.printed_messages = until;
    }
    Ok(())
}

fn message_rows(message: &Message, width: usize, row_width: usize) -> Vec<String> {
    let text_width = width.saturating_sub(MESSAGE_INDENT).max(1);
    match message.role {
        Role::Assistant => markdown_rows(&message.text, text_width, row_width),
        Role::Command => {
            let mut rows = body_rows_with_lang(message.text.lines(), row_width, "bash");
            if let Some(output) = message.output.as_deref() {
                rows.push(String::new());
                rows.extend(output_body_rows(output, row_width));
            }
            rows
        }
        Role::Output => output_body_rows(&message.text, row_width),
        Role::User => {
            let paint_row = |text: &str| {
                let mut row = fit(text, row_width);
                row.push_str(&" ".repeat(row_width.saturating_sub(visible_width(&row))));
                let row = row.replace(RESET, &format!("{RESET}{BG_USER}"));
                format!("{BG_USER}{row}{RESET}")
            };
            let wrapped = wrap_words(&message.text, text_width);
            if wrapped.is_empty() {
                vec![paint_row("")]
            } else {
                wrapped
                    .into_iter()
                    .map(|row| paint_row(&format!("{}{}", " ".repeat(MESSAGE_INDENT), row)))
                    .collect()
            }
        }
        Role::Local => indent_rows(wrap_words(&message.text, text_width)),
    }
}

fn indent_rows(rows: Vec<String>) -> Vec<String> {
    rows.into_iter()
        .map(|row| {
            if row.is_empty() {
                row
            } else {
                format!("{}{}", " ".repeat(MESSAGE_INDENT), row)
            }
        })
        .collect()
}

fn paint(text: &str, style: &str) -> String {
    if text.is_empty() || style.is_empty() || style == RESET {
        text.to_string()
    } else {
        format!("{style}{text}{RESET}")
    }
}

fn markdown_rows(text: &str, width: usize, block_width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut paragraph = String::new();
    let mut in_code = false;
    let mut code_lang = String::new();
    let mut code_fence_len = 0;
    let mut code = Vec::new();

    let flush_paragraph = |paragraph: &mut String, rows: &mut Vec<String>| {
        if !paragraph.trim().is_empty() {
            rows.extend(indent_rows(wrap_words(
                &render_inline_markdown(paragraph.trim()),
                width,
            )));
            rows.push(String::new());
            paragraph.clear();
        }
    };

    for raw_line in text.lines() {
        if in_code {
            let trimmed = raw_line.trim_start();
            let closing_len = trimmed.bytes().take_while(|byte| *byte == b'`').count();
            if closing_len >= code_fence_len
                && closing_len >= 3
                && trimmed[closing_len..].trim().is_empty()
            {
                if !code_lang.is_empty() {
                    rows.push(format!(
                        "{}{}",
                        " ".repeat(MESSAGE_INDENT),
                        paint(&code_lang, CYAN)
                    ));
                }
                rows.extend(body_rows_with_lang(
                    code.iter().map(String::as_str),
                    block_width,
                    &code_lang,
                ));
                rows.push(String::new());
                in_code = false;
                code_fence_len = 0;
                code_lang.clear();
                code.clear();
            } else {
                code.push(raw_line.to_string());
            }
            continue;
        }

        let line = raw_line.trim_end();
        let trimmed = line.trim_start();
        let opening_len = trimmed.bytes().take_while(|byte| *byte == b'`').count();

        if opening_len >= 3 {
            flush_paragraph(&mut paragraph, &mut rows);
            code_fence_len = opening_len;
            code_lang = trimmed[opening_len..]
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string();
            in_code = true;
            continue;
        }

        if trimmed.is_empty() {
            flush_paragraph(&mut paragraph, &mut rows);
            if rows.last().is_some_and(|row| !row.is_empty()) {
                rows.push(String::new());
            }
            continue;
        }

        if trimmed.chars().all(|char| matches!(char, '-' | '_' | '*')) && trimmed.len() >= 3 {
            flush_paragraph(&mut paragraph, &mut rows);
            rows.push(format!(
                "{}{}",
                " ".repeat(MESSAGE_INDENT),
                paint(&"─".repeat(width.min(96)), BRIGHT_BLACK)
            ));
            rows.push(String::new());
            continue;
        }

        let heading_marks = trimmed.bytes().take_while(|byte| *byte == b'#').count();
        if (1..=6).contains(&heading_marks) && trimmed.as_bytes().get(heading_marks) == Some(&b' ')
        {
            flush_paragraph(&mut paragraph, &mut rows);
            let heading = trimmed[heading_marks + 1..].trim();
            rows.extend(indent_rows(
                wrap_words(heading.trim(), width)
                    .into_iter()
                    .map(|row| paint(&row, if heading_marks == 1 { BOLD_CYAN } else { BOLD }))
                    .collect(),
            ));
            if heading_marks == 1 {
                rows.push(format!(
                    "{}{}",
                    " ".repeat(MESSAGE_INDENT),
                    paint(&"─".repeat(visible_width(heading).min(width)), BRIGHT_BLACK,)
                ));
            }
            rows.push(String::new());
            continue;
        }

        if let Some(quote) = trimmed.strip_prefix(">") {
            flush_paragraph(&mut paragraph, &mut rows);
            rows.extend(indent_rows(prefixed_rows(
                &format!("{} ", paint("│", BRIGHT_BLACK)),
                &format!("{} ", paint("│", BRIGHT_BLACK)),
                &render_inline_markdown(quote.trim_start()),
                width,
            )));
            continue;
        }

        if let Some(item) = ["- ", "* ", "+ "]
            .iter()
            .find_map(|prefix| trimmed.strip_prefix(prefix))
        {
            flush_paragraph(&mut paragraph, &mut rows);
            rows.extend(indent_rows(prefixed_rows(
                &format!("{} ", paint("•", YELLOW)),
                "  ",
                &render_inline_markdown(item),
                width,
            )));
            continue;
        }

        if let Some(dot) = trimmed.find(". ") {
            let number = &trimmed[..dot];
            if number.chars().all(|char| char.is_ascii_digit()) {
                flush_paragraph(&mut paragraph, &mut rows);
                let prefix = format!("{number}. ");
                let continuation = " ".repeat(visible_width(&prefix));
                rows.extend(indent_rows(prefixed_rows(
                    &paint(&prefix, YELLOW),
                    &continuation,
                    &render_inline_markdown(&trimmed[dot + 2..]),
                    width,
                )));
                continue;
            }
        }

        if !paragraph.is_empty() {
            paragraph.push(' ');
        }
        paragraph.push_str(trimmed);
    }

    if in_code {
        if !code_lang.is_empty() {
            rows.push(format!(
                "{}{}",
                " ".repeat(MESSAGE_INDENT),
                paint(&code_lang, CYAN)
            ));
        }
        rows.extend(body_rows_with_lang(
            code.iter().map(String::as_str),
            block_width,
            &code_lang,
        ));
    }
    flush_paragraph(&mut paragraph, &mut rows);
    while rows.last().is_some_and(|row| row.is_empty()) {
        rows.pop();
    }
    rows
}

fn render_inline_markdown(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;

    while let Some(start) = rest.find('[') {
        let Some(label_end) = rest[start + 1..].find("](").map(|index| start + 1 + index) else {
            break;
        };
        let Some(url_end) = rest[label_end + 2..]
            .find(')')
            .map(|index| label_end + 2 + index)
        else {
            break;
        };
        out.push_str(&rest[..start]);
        out.push_str(&rest[start + 1..label_end]);
        out.push_str(" (");
        out.push_str(&rest[label_end + 2..url_end]);
        out.push(')');
        rest = &rest[url_end + 1..];
    }
    out.push_str(rest);

    let mut rendered = String::new();
    let mut in_code = false;
    for part in out.split('`') {
        if in_code {
            rendered.push_str(part);
        } else {
            rendered.push_str(&part.replace("**", "").replace("__", "").replace('*', ""));
        }
        in_code = !in_code;
    }
    rendered
}

fn prefixed_rows(prefix: &str, continuation: &str, text: &str, width: usize) -> Vec<String> {
    let body_width = width.saturating_sub(visible_width(prefix)).max(1);
    let wrapped = wrap_words(text, body_width);
    if wrapped.is_empty() {
        return vec![prefix.trim_end().to_string()];
    }
    wrapped
        .iter()
        .enumerate()
        .map(|(index, row)| {
            if index == 0 {
                format!("{prefix}{row}")
            } else {
                format!("{continuation}{row}")
            }
        })
        .collect()
}

fn body_rows_with_lang<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    width: usize,
    lang: &str,
) -> Vec<String> {
    static ASSETS: OnceLock<(SyntaxSet, Theme)> = OnceLock::new();
    let (syntaxes, theme) = ASSETS.get_or_init(|| {
        let mut themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .remove("base16-ocean.dark")
            .or_else(|| themes.themes.remove("Solarized (dark)"))
            .or_else(|| themes.themes.into_values().next())
            .unwrap_or_default();
        (SyntaxSet::load_defaults_newlines(), theme)
    });
    let lang = lang.trim().trim_start_matches('.').to_ascii_lowercase();
    let lang = match lang.as_str() {
        "bash" | "shell" | "zsh" => "sh",
        "py" => "python",
        "rs" => "rust",
        "js" => "javascript",
        "ts" => "typescript",
        "md" => "markdown",
        other => other,
    };
    let syntax = syntaxes
        .find_syntax_by_token(lang)
        .or_else(|| syntaxes.find_syntax_by_extension(lang))
        .unwrap_or_else(|| syntaxes.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, theme);
    let body_width = width.saturating_sub(MESSAGE_INDENT).max(1);
    let mut rows = Vec::new();
    for line in lines {
        let line = expand_tabs(line);
        if line.is_empty() {
            rows.push(String::new());
        } else {
            rows.extend(wrap_chars(&line, body_width).into_iter().map(|row| {
                let mut row = match highlighter.highlight_line(&row, syntaxes) {
                    Ok(regions) => as_24_bit_terminal_escaped(&regions, false),
                    Err(_) => row,
                };
                row.push_str(RESET);
                format!("{}{}", " ".repeat(MESSAGE_INDENT), row)
            }));
        }
    }
    rows
}

fn output_body_rows(text: &str, width: usize) -> Vec<String> {
    let body_width = width.saturating_sub(MESSAGE_INDENT).max(1);
    let mut rows = Vec::new();
    for raw_line in text.lines() {
        let line = expand_tabs(&strip_ansi(raw_line));
        let line = line.as_str();
        let color = if line.starts_with("command failed")
            || line.starts_with("command terminated")
            || line == "stderr:"
        {
            RED
        } else if line == "command completed with no output"
            || (line.starts_with("[... ") && line.ends_with(" ...]"))
        {
            BRIGHT_BLACK
        } else {
            RESET
        };

        if line.is_empty() {
            rows.push(String::new());
        } else {
            rows.extend(wrap_chars(line, body_width).into_iter().map(|row| {
                let row = if color == RESET {
                    row
                } else {
                    paint(&row, color)
                };
                format!("{}{}", " ".repeat(MESSAGE_INDENT), row)
            }));
        }
    }
    rows
}

fn top_border(width: u16) -> String {
    if width < 4 {
        return paint(&"─".repeat(width as usize), INPUT_FRAME);
    }
    let inner = width as usize - 2;
    paint(&format!("╭{}╮", "─".repeat(inner)), INPUT_FRAME)
}

fn bottom_border(width: u16, left: &str, right: &str) -> String {
    if width < 4 {
        return paint(&"─".repeat(width as usize), INPUT_FRAME);
    }
    let inner = width as usize - 2;
    let left = if left.is_empty() {
        String::new()
    } else {
        format!(" {left} ")
    };
    let right = if right.is_empty() {
        String::new()
    } else {
        format!(" {right} ")
    };
    let left = fit(&left, inner);
    let right = fit(&right, inner.saturating_sub(visible_width(&left)));
    let fill = "─".repeat(inner.saturating_sub(visible_width(&left) + visible_width(&right)));
    format!(
        "{}{}{}{}{}",
        paint("╰", INPUT_FRAME),
        left,
        paint(&fill, INPUT_FRAME),
        right,
        paint("╯", INPUT_FRAME)
    )
}

fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for paragraph in text.lines() {
        if paragraph.is_empty() {
            rows.push(String::new());
            continue;
        }

        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let next_width =
                visible_width(&line) + usize::from(!line.is_empty()) + visible_width(word);
            if next_width <= width {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(word);
            } else {
                if !line.is_empty() {
                    rows.push(line);
                    line = String::new();
                }
                if visible_width(word) > width {
                    // Keep the final fragment in the in-progress line so the
                    // next word can continue beside it instead of being
                    // stranded on its own row.
                    let mut chunks = wrap_chars(word, width);
                    if let Some(last) = chunks.pop() {
                        rows.extend(chunks);
                        line = last;
                    }
                } else {
                    line.push_str(word);
                }
            }
        }
        if !line.is_empty() {
            rows.push(line);
        }
    }
    rows
}

// Width math works on grapheme clusters, not Unicode scalars: string-level
// width (UnicodeWidthStr) special-cases ZWJ emoji sequences and VS16 emoji
// presentation, and never splitting a cluster keeps a family emoji or flag from
// being broken across rows — per-scalar math both miscounts and splits them.

fn wrap_chars(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut line = String::new();
    let mut line_width = 0;

    for grapheme in text.graphemes(true) {
        let grapheme_width = grapheme.width();
        if line_width + grapheme_width > width && !line.is_empty() {
            rows.push(line);
            line = String::new();
            line_width = 0;
        }
        line.push_str(grapheme);
        line_width += grapheme_width;
    }

    if !line.is_empty() {
        rows.push(line);
    }
    rows
}

fn is_ansi_terminator(grapheme: &str) -> bool {
    grapheme.len() == 1 && grapheme.as_bytes()[0].is_ascii_alphabetic()
}

fn fit(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    let mut graphemes = text.graphemes(true);
    let mut styled = false;
    while let Some(grapheme) = graphemes.next() {
        if grapheme == "\x1b" {
            out.push_str(grapheme);
            if graphemes.next() == Some("[") {
                out.push('[');
                for grapheme in graphemes.by_ref() {
                    out.push_str(grapheme);
                    if is_ansi_terminator(grapheme) {
                        styled = true;
                        break;
                    }
                }
            }
            continue;
        }
        let grapheme_width = grapheme.width();
        if used + grapheme_width > width {
            break;
        }
        out.push_str(grapheme);
        used += grapheme_width;
    }
    if styled {
        out.push_str(RESET);
    }
    out
}

fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut graphemes = text.graphemes(true);
    while let Some(grapheme) = graphemes.next() {
        if grapheme == "\x1b" {
            if graphemes.next() == Some("[") {
                for grapheme in graphemes.by_ref() {
                    if is_ansi_terminator(grapheme) {
                        break;
                    }
                }
            }
            continue;
        }
        width += grapheme.width();
    }
    width
}

/// Expand tabs to spaces so counted width matches what the terminal renders
/// (`'\t'.width()` is `None`, i.e. counted as 0, but terminals advance columns).
fn expand_tabs(text: &str) -> String {
    text.replace('\t', "    ")
}

/// Remove ANSI CSI escape sequences from text that we re-style ourselves
/// (command output), so they neither inflate the wrap width nor get split
/// across rows.
fn strip_ansi(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(char) = chars.next() {
        if char == '\x1b' {
            if chars.next() == Some('[') {
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(char);
    }
    out
}

#[cfg(test)]
mod text_tests {
    use super::*;

    #[test]
    fn visible_width_ignores_ansi_escapes() {
        assert_eq!(visible_width("plain"), 5);
        assert_eq!(visible_width(&format!("{RED}red{RESET}")), 3);
        // CJK glyphs occupy two columns each.
        assert_eq!(visible_width("日本"), 4);
    }

    #[test]
    fn wrap_chars_respects_display_width_for_wide_glyphs() {
        // Width 3 cannot fit two double-width glyphs on one row.
        let rows = wrap_chars("日本語", 3);
        assert!(rows.iter().all(|row| visible_width(row) <= 3));
        let recombined = rows.concat();
        assert_eq!(recombined, "日本語");
    }

    #[test]
    fn width_math_treats_zwj_emoji_as_one_cluster() {
        // Family emoji: 3 scalars joined by ZWJ render as one 2-column glyph.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        assert_eq!(visible_width(family), 2);
        // Wrapping must never break inside the cluster.
        let rows = wrap_chars(&format!("{family}{family}"), 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], family);
        assert_eq!(rows[1], family);
        // fit() must drop the whole cluster rather than truncate mid-sequence.
        assert_eq!(fit(family, 1), "");
        assert_eq!(fit(family, 2), family);
    }

    #[test]
    fn width_math_counts_vs16_emoji_as_two_columns() {
        // Red heart: U+2764 (width 1 alone) + VS16 forces 2-column emoji
        // presentation in terminals.
        let heart = "\u{2764}\u{FE0F}";
        assert_eq!(visible_width(heart), 2);
        let rows = wrap_chars(&format!("{heart}{heart}"), 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.concat(), format!("{heart}{heart}"));
    }

    #[test]
    fn wrap_chars_never_splits_a_multibyte_char() {
        // A single 4-byte emoji must survive intact even when narrower than its width.
        let rows = wrap_chars("😀😀", 1);
        assert_eq!(rows.concat(), "😀😀");
        for row in &rows {
            assert!(row.chars().count() >= 1);
        }
    }

    #[test]
    fn wrap_words_breaks_overlong_words_by_char() {
        let rows = wrap_words("supercalifragilistic", 5);
        assert!(rows.iter().all(|row| visible_width(row) <= 5));
        assert_eq!(rows.concat(), "supercalifragilistic");
    }

    #[test]
    fn wrap_words_preserves_blank_lines() {
        let rows = wrap_words("a\n\nb", 10);
        assert_eq!(rows, vec!["a".to_string(), String::new(), "b".to_string()]);
    }

    #[test]
    fn fit_truncates_to_width_and_closes_styles() {
        // Multi-byte input must not panic and must respect display width.
        let out = fit("日本語テスト", 4);
        assert!(visible_width(&out) <= 4);
        // A styled string that gets truncated is reset so color does not bleed.
        let styled = fit(&format!("{RED}hello world"), 3);
        assert!(styled.ends_with(RESET));
    }

    #[test]
    fn markdown_rows_handles_empty_and_code_without_panic() {
        assert!(markdown_rows("", 20, 40).is_empty());
        let rows = markdown_rows("```rust\nfn main() {}\n```", 20, 40);
        assert!(rows.iter().any(|row| row.contains("main")));
    }
}
