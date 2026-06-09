use crate::{
    CancelToken, Config, ModelMessage, ModelRole, ModelSyntheticKind, ModelToolResult, Tool,
    ToolSpec,
    model::{call_model, call_model_interruptible, call_model_without_tools},
};
use anyhow::{Context, Result};
use serde_json::Value;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 128_000;

#[derive(Debug, Clone)]
pub struct Agent {
    pub system: String,
    pub config: Config,
    pub messages: Vec<ModelMessage>,
    /// Tools available in addition to the built-in `bash`. Attached at
    /// construction by the front-end; never serialized, so they must be
    /// re-mounted after a resume/reload.
    pub tools: Vec<Arc<dyn Tool>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRun {
    pub final_text: String,
    pub turns: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    AssistantDelta(String),
    Assistant(String),
    Command(String),
    CommandOutput(CommandOutput),
    /// A call to a mounted (non-bash) tool, with its arguments rendered as JSON.
    ToolUse {
        name: String,
        input: String,
    },
    /// The result of a mounted tool call.
    ToolResult {
        name: String,
        output: String,
        is_error: bool,
    },
    CompactionStarted {
        estimated_tokens: usize,
    },
    CompactionFinished {
        removed_messages: usize,
        summary_tokens: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Agent {
    pub fn new(system: impl Into<String>, config: Config) -> Self {
        Self {
            system: system.into(),
            config,
            messages: Vec::new(),
            tools: Vec::new(),
        }
    }

    /// Attach tools (in addition to the built-in `bash`). Tools whose name is
    /// `"bash"` or duplicates an already-mounted tool are ignored so the model
    /// never sees an ambiguous schema.
    pub fn mount_tools(&mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) {
        for tool in tools {
            let name = tool.spec().name;
            if name == "bash" || self.tools.iter().any(|mounted| mounted.spec().name == name) {
                continue;
            }
            self.tools.push(tool);
        }
    }

    fn tool_specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|tool| tool.spec()).collect()
    }

    pub fn run(&mut self, user_prompt: impl Into<String>) -> Result<AgentRun> {
        self.run_with_events(user_prompt, |_| {})
    }

    pub fn run_with_events(
        &mut self,
        user_prompt: impl Into<String>,
        emit: impl FnMut(AgentEvent),
    ) -> Result<AgentRun> {
        self.run_with_events_interruptible(user_prompt, emit, None)
    }

    pub fn run_with_events_interruptible(
        &mut self,
        user_prompt: impl Into<String>,
        mut emit: impl FnMut(AgentEvent),
        interrupted: Option<Arc<AtomicBool>>,
    ) -> Result<AgentRun> {
        self.discard_incomplete_tool_turn();
        self.messages.push(ModelMessage {
            role: ModelRole::User,
            text: user_prompt.into(),
            tool_calls: Vec::new(),
            tool_result: None,
            synthetic: None,
            thinking: Vec::new(),
        });

        let mut turns = 0usize;
        loop {
            turns = turns.saturating_add(1);
            self.bail_if_interrupted(&interrupted)?;
            self.compact_if_needed(&mut emit)?;
            self.bail_if_interrupted(&interrupted)?;
            let tool_specs = self.tool_specs();
            let response = if interrupted.is_some() {
                call_model_interruptible(
                    &self.system,
                    &self.config,
                    &self.messages,
                    &tool_specs,
                    |delta: &str| {
                        emit(AgentEvent::AssistantDelta(delta.to_string()));
                    },
                    interrupted.clone(),
                )?
            } else {
                call_model(
                    &self.system,
                    &self.config,
                    &self.messages,
                    &tool_specs,
                    |delta| {
                        emit(AgentEvent::AssistantDelta(delta.to_string()));
                    },
                )?
            };
            self.bail_if_interrupted(&interrupted)?;

            if response.tool_calls.is_empty() {
                self.messages.push(ModelMessage {
                    role: ModelRole::Assistant,
                    text: response.text.clone(),
                    tool_calls: Vec::new(),
                    tool_result: None,
                    synthetic: None,
                    thinking: response.thinking,
                });
                if !response.text.is_empty() {
                    emit(AgentEvent::Assistant(response.text.clone()));
                }
                return Ok(AgentRun {
                    final_text: response.text,
                    turns,
                });
            }

            // Record the assistant turn (text + tool calls + thinking) before
            // running anything, so an error or interrupt mid-execution cannot
            // erase it from history. discard_incomplete_tool_turn cleans up any
            // calls left unanswered if the run aborts here.
            self.messages.push(ModelMessage {
                role: ModelRole::Assistant,
                text: response.text.clone(),
                tool_calls: response.tool_calls.clone(),
                tool_result: None,
                synthetic: None,
                thinking: response.thinking,
            });
            if !response.text.is_empty() {
                emit(AgentEvent::Assistant(response.text.clone()));
            }

            for call in &response.tool_calls {
                self.bail_if_interrupted(&interrupted)?;

                if call.name == "bash" {
                    self.run_bash_call(call, &interrupted, &mut emit)?;
                    self.bail_if_interrupted(&interrupted)?;
                    continue;
                }

                // A mounted (non-bash) tool. Clone the Arc so the registry is
                // not borrowed across the mutable history push below.
                if let Some(tool) = self
                    .tools
                    .iter()
                    .find(|tool| tool.spec().name == call.name)
                    .cloned()
                {
                    emit(AgentEvent::ToolUse {
                        name: call.name.clone(),
                        input: call.input.to_string(),
                    });
                    let cancel = CancelToken::new(interrupted.clone());
                    let (content, is_error) = match tool.call(&call.input, &cancel) {
                        Ok(output) => (output.content, output.is_error),
                        Err(err) => (format!("error: {err}"), true),
                    };
                    emit(AgentEvent::ToolResult {
                        name: call.name.clone(),
                        output: content.clone(),
                        is_error,
                    });
                    self.push_tool_result(call, content, is_error);
                    self.bail_if_interrupted(&interrupted)?;
                    continue;
                }

                // Unknown tool: surface an error result the model can react to
                // rather than aborting the run.
                self.push_tool_result(
                    call,
                    format!("error: unsupported tool '{}'", call.name),
                    true,
                );
            }
        }
    }

    fn run_bash_call(
        &mut self,
        call: &crate::ModelToolCall,
        interrupted: &Option<Arc<AtomicBool>>,
        emit: &mut impl FnMut(AgentEvent),
    ) -> Result<()> {
        // Turn malformed input into an error result the model can react to.
        let command = match call
            .input
            .get("command")
            .and_then(Value::as_str)
            .or_else(|| call.input.as_str())
        {
            Some(command) if !command.trim().is_empty() => command.to_string(),
            Some(_) => {
                self.push_tool_result(
                    call,
                    "error: bash tool call command is empty".to_string(),
                    true,
                );
                return Ok(());
            }
            None => {
                self.push_tool_result(
                    call,
                    "error: bash tool call missing string 'command'".to_string(),
                    true,
                );
                return Ok(());
            }
        };

        self.bail_if_interrupted(interrupted)?;
        emit(AgentEvent::Command(command.clone()));
        self.bail_if_interrupted(interrupted)?;

        let output = self.run_command(&command, interrupted)?;
        let mut content = String::new();
        if output.status != Some(0) {
            content.push_str(&output.status.map_or_else(
                || "command terminated by signal".to_string(),
                |status| format!("command failed with exit status {status}"),
            ));
        }
        let stdout = output.stdout.trim_end();
        if !stdout.is_empty() {
            if !content.is_empty() {
                content.push_str("\n\n");
            }
            content.push_str("stdout:\n");
            content.push_str(stdout);
        }
        let stderr = output.stderr.trim_end();
        if !stderr.is_empty() {
            if !content.is_empty() {
                content.push_str("\n\n");
            }
            content.push_str("stderr:\n");
            content.push_str(stderr);
        }
        if content.is_empty() {
            content.push_str("command completed with no output");
        }

        let is_error = output.status != Some(0);
        emit(AgentEvent::CommandOutput(output));
        self.push_tool_result(call, content, is_error);
        Ok(())
    }

    fn push_tool_result(&mut self, call: &crate::ModelToolCall, content: String, is_error: bool) {
        self.messages.push(ModelMessage {
            role: ModelRole::User,
            text: String::new(),
            tool_calls: Vec::new(),
            tool_result: Some(ModelToolResult {
                id: call.id.clone(),
                name: call.name.clone(),
                content,
                is_error,
            }),
            synthetic: None,
            thinking: Vec::new(),
        });
    }

    /// Run a `bash -lc` command, polling for completion so an interrupt can kill
    /// it. stdout/stderr are drained on worker threads to avoid a pipe-buffer
    /// deadlock. (A killed shell's own grandchildren may briefly outlive it.)
    fn run_command(
        &self,
        command: &str,
        interrupted: &Option<Arc<AtomicBool>>,
    ) -> Result<CommandOutput> {
        use std::io::Read;
        use std::process::Stdio;
        use std::time::Duration;

        let mut process = Command::new("bash");
        process
            .arg("-lc")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(path) = Config::path_with_app_bin(&self.config.app_dir_name) {
            process.env("PATH", path);
            process.env(
                "AGENT_HOME",
                Config::app_paths(&self.config.app_dir_name)
                    .map(|paths| paths.root)
                    .unwrap_or_default(),
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: `setsid` is async-signal-safe and runs in the forked child
            // before exec. It makes the command a session/process-group leader
            // with no controlling terminal: a child it spawns (e.g. `ssh` for
            // `git push`) cannot read or write `/dev/tty`, and the whole tree
            // can be signalled as one process group when interrupted.
            unsafe {
                process.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }

        let mut child = process.spawn().context("failed to run bash command")?;
        let stdout_reader = child.stdout.take().map(|mut pipe| {
            std::thread::spawn(move || {
                let mut buffer = Vec::new();
                let _ = pipe.read_to_end(&mut buffer);
                buffer
            })
        });
        let stderr_reader = child.stderr.take().map(|mut pipe| {
            std::thread::spawn(move || {
                let mut buffer = Vec::new();
                let _ = pipe.read_to_end(&mut buffer);
                buffer
            })
        });

        let status = loop {
            if let Some(status) = child.try_wait().context("failed to wait on bash command")? {
                break status;
            }
            if interrupted
                .as_ref()
                .is_some_and(|interrupted| interrupted.load(Ordering::Relaxed))
            {
                // Kill the command's whole process group (it leads its own via
                // setsid), so children like `ssh` die too and release the
                // stdout/stderr pipes — otherwise a surviving grandchild keeps
                // them open and the reader threads' `read_to_end` blocks forever.
                #[cfg(unix)]
                unsafe {
                    libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                // The reader threads get EOF once the group releases the pipes
                // and exit on their own; don't block joining them here.
                drop(stdout_reader);
                drop(stderr_reader);
                anyhow::bail!("model request interrupted");
            }
            std::thread::sleep(Duration::from_millis(100));
        };

        let stdout = join_reader(stdout_reader);
        let stderr = join_reader(stderr_reader);
        Ok(CommandOutput {
            status: status.code(),
            stdout: String::from_utf8_lossy(&stdout).to_string(),
            stderr: String::from_utf8_lossy(&stderr).to_string(),
        })
    }

    fn discard_incomplete_tool_turn(&mut self) {
        let Some(assistant_index) = self
            .messages
            .iter()
            .rposition(|message| !message.tool_calls.is_empty())
        else {
            return;
        };
        let pending_call_ids: Vec<&str> = self.messages[assistant_index]
            .tool_calls
            .iter()
            .map(|call| call.id.as_str())
            .collect();
        if pending_call_ids.is_empty() {
            return;
        }

        let mut result_index = assistant_index + 1;
        for call_id in pending_call_ids {
            let Some(result) = self
                .messages
                .get(result_index)
                .and_then(|message| message.tool_result.as_ref())
            else {
                self.messages.truncate(assistant_index);
                return;
            };
            if result.id != call_id {
                self.messages.truncate(assistant_index);
                return;
            }
            result_index += 1;
        }
    }

    fn bail_if_interrupted(&self, interrupted: &Option<Arc<AtomicBool>>) -> Result<()> {
        if interrupted
            .as_ref()
            .is_some_and(|interrupted| interrupted.load(Ordering::Relaxed))
        {
            anyhow::bail!("model request interrupted");
        }
        Ok(())
    }

    fn compact_if_needed(&mut self, emit: &mut impl FnMut(AgentEvent)) -> Result<()> {
        if !self.config.agent.auto_compact {
            return Ok(());
        }
        let context_window = self
            .config
            .agent
            .context_window_tokens
            .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS);
        let threshold = self.config.agent.compact_threshold.clamp(0.1, 0.95);
        let estimated_tokens = estimate_messages_tokens(&self.system, &self.messages);
        if (estimated_tokens as f32) < (context_window as f32 * threshold) {
            return Ok(());
        }
        self.compact_history(emit, estimated_tokens)
    }

    pub fn compact_history(
        &mut self,
        emit: &mut impl FnMut(AgentEvent),
        estimated_tokens: usize,
    ) -> Result<()> {
        let Some(split) =
            compaction_split_progressive(&self.messages, self.config.agent.compact_keep_recent)
        else {
            return Ok(());
        };
        let older = self.messages[..split].to_vec();
        let recent = self.messages[split..].to_vec();
        if older.is_empty() {
            return Ok(());
        }

        emit(AgentEvent::CompactionStarted { estimated_tokens });
        let transcript = transcript_for_compaction(&older);
        let prompt = format!(
            "Compact the following terminal coding-agent conversation into a concise but complete handoff summary. Preserve user intent, constraints/preferences, repository/workspace state, files read or modified, commands run and important outputs, errors encountered, decisions made, current pending task, and exact next steps. Do not include filler. Do not invent facts.

Transcript to compact:
{transcript}"
        );
        let response = call_model_without_tools(
            "You compact coding-agent transcripts into durable continuation summaries. Return only the summary.",
            &self.config,
            &[ModelMessage {
                role: ModelRole::User,
                text: prompt,
                tool_calls: Vec::new(),
                tool_result: None,
                synthetic: None,
                thinking: Vec::new(),
            }],
        )?;
        let summary = response.text.trim();
        if summary.is_empty() {
            anyhow::bail!("compaction returned an empty summary");
        }
        let summary_message = ModelMessage {
            role: ModelRole::User,
            text: format!(
                "[Compacted conversation history]

{summary}"
            ),
            tool_calls: Vec::new(),
            tool_result: None,
            synthetic: Some(ModelSyntheticKind::CompactionSummary),
            thinking: Vec::new(),
        };
        let summary_tokens = estimate_text_tokens(&summary_message.text);
        self.messages = std::iter::once(summary_message).chain(recent).collect();
        emit(AgentEvent::CompactionFinished {
            removed_messages: older.len(),
            summary_tokens,
        });
        Ok(())
    }
}

fn join_reader(reader: Option<std::thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    reader
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default()
}

pub fn estimate_messages_tokens(system: &str, messages: &[ModelMessage]) -> usize {
    estimate_text_tokens(system)
        + messages
            .iter()
            .map(|message| {
                estimate_text_tokens(&message.text)
                    + message
                        .tool_calls
                        .iter()
                        .map(|call| {
                            estimate_text_tokens(&call.name)
                                + estimate_text_tokens(&call.input.to_string())
                        })
                        .sum::<usize>()
                    + message
                        .tool_result
                        .as_ref()
                        .map(|result| {
                            estimate_text_tokens(&result.name)
                                + estimate_text_tokens(&result.content)
                        })
                        .unwrap_or_default()
            })
            .sum::<usize>()
}

fn estimate_text_tokens(text: &str) -> usize {
    text.len().div_ceil(4).max(1)
}

/// Find a compaction split point, shrinking `keep_recent` if necessary. The
/// trigger to compact is token-based but the split is message-count-based, so
/// when most of the bulk sits inside the recent window (e.g. one huge tool
/// result) the configured `keep_recent` can yield no split at all. Shrinking it
/// keeps compaction from being a silent no-op that leaves the request over
/// budget on every turn.
fn compaction_split_progressive(messages: &[ModelMessage], keep_recent: usize) -> Option<usize> {
    if let Some(split) = compaction_split(messages, keep_recent) {
        return Some(split);
    }
    (1..keep_recent)
        .rev()
        .find_map(|smaller| compaction_split(messages, smaller))
}

fn compaction_split(messages: &[ModelMessage], keep_recent: usize) -> Option<usize> {
    if messages.len() <= keep_recent.saturating_add(1) {
        return None;
    }
    let mut split = messages.len().saturating_sub(keep_recent.max(1));
    while split < messages.len() && messages[split].tool_result.is_some() {
        split += 1;
    }
    if split == 0 || split >= messages.len() {
        None
    } else {
        Some(split)
    }
}

fn transcript_for_compaction(messages: &[ModelMessage]) -> String {
    let mut transcript = String::new();
    for message in messages {
        if let Some(kind) = &message.synthetic {
            transcript.push_str(&format!("[synthetic {kind:?}]\n"));
        }
        if let Some(result) = &message.tool_result {
            transcript.push_str(&format!(
                "[tool result: {} / {}]\n{}\n\n",
                result.name, result.id, result.content
            ));
            continue;
        }
        transcript.push_str(match message.role {
            ModelRole::User => "[user]\n",
            ModelRole::Assistant => "[assistant]\n",
        });
        if !message.text.is_empty() {
            transcript.push_str(&message.text);
            transcript.push('\n');
        }
        for call in &message.tool_calls {
            transcript.push_str(&format!(
                "[tool call: {} / {}]\n{}\n",
                call.name, call.id, call.input
            ));
        }
        transcript.push('\n');
    }
    transcript
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CancelToken, ModelToolCall, ModelToolResult, ToolOutput, ToolSpec};
    use serde_json::json;

    #[derive(Debug)]
    struct FakeTool {
        name: String,
    }

    impl Tool for FakeTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.clone(),
                description: "fake".to_string(),
                input_schema: json!({ "type": "object" }),
            }
        }
        fn call(&self, _input: &Value, _cancel: &CancelToken) -> Result<ToolOutput> {
            Ok(ToolOutput::text("ok"))
        }
    }

    fn fake(name: &str) -> Arc<dyn Tool> {
        Arc::new(FakeTool {
            name: name.to_string(),
        })
    }

    #[test]
    fn run_command_captures_output() {
        let agent = Agent::new("", Config::default());
        let output = agent.run_command("printf 'hello'", &None).expect("run");
        assert_eq!(output.status, Some(0));
        assert_eq!(output.stdout, "hello");
    }

    #[cfg(unix)]
    #[test]
    fn run_command_runs_in_its_own_process_group() {
        // setsid makes the command its own session/group leader, which both
        // keeps it off the controlling terminal and lets an interrupt kill the
        // whole tree via killpg. Verify its process group differs from ours.
        let agent = Agent::new("", Config::default());
        let output = agent.run_command("ps -o pgid= -p $$", &None).expect("run");
        let child_pgid: i32 = output.stdout.trim().parse().expect("pgid");
        let our_pgid = unsafe { libc::getpgrp() };
        assert_ne!(child_pgid, our_pgid);
    }

    #[test]
    fn mount_tools_reserves_bash_and_dedups_by_name() {
        let mut agent = Agent::new("", Config::default());
        agent.mount_tools([fake("search"), fake("bash"), fake("search"), fake("fetch")]);
        let names: Vec<String> = agent.tools.iter().map(|tool| tool.spec().name).collect();
        // "bash" is reserved for the built-in and the duplicate "search" is dropped.
        assert_eq!(names, vec!["search".to_string(), "fetch".to_string()]);
        assert_eq!(agent.tool_specs().len(), 2);
    }

    fn message(role: ModelRole, text: impl Into<String>) -> ModelMessage {
        ModelMessage {
            role,
            text: text.into(),
            tool_calls: Vec::new(),
            tool_result: None,
            synthetic: None,
            thinking: Vec::new(),
        }
    }

    fn bash_call(id: &str) -> ModelToolCall {
        ModelToolCall {
            id: id.to_string(),
            name: "bash".to_string(),
            input: json!({ "command": "echo hi" }),
        }
    }

    fn bash_result(id: &str) -> ModelMessage {
        ModelMessage {
            role: ModelRole::User,
            text: String::new(),
            tool_calls: Vec::new(),
            tool_result: Some(ModelToolResult {
                id: id.to_string(),
                name: "bash".to_string(),
                content: "stdout:\nhi".to_string(),
                is_error: false,
            }),
            synthetic: None,
            thinking: Vec::new(),
        }
    }

    #[test]
    fn discard_incomplete_tool_turn_removes_unanswered_assistant_call() {
        let mut agent = Agent::new("", Config::default());
        agent.messages = vec![
            message(ModelRole::User, "one"),
            ModelMessage {
                role: ModelRole::Assistant,
                text: String::new(),
                tool_calls: vec![bash_call("call_1")],
                tool_result: None,
                synthetic: None,
                thinking: Vec::new(),
            },
        ];

        agent.discard_incomplete_tool_turn();

        assert_eq!(agent.messages, vec![message(ModelRole::User, "one")]);
    }

    #[test]
    fn discard_incomplete_tool_turn_keeps_complete_tool_turn() {
        let mut agent = Agent::new("", Config::default());
        let assistant = ModelMessage {
            role: ModelRole::Assistant,
            text: String::new(),
            tool_calls: vec![bash_call("call_1")],
            tool_result: None,
            synthetic: None,
            thinking: Vec::new(),
        };
        agent.messages = vec![
            message(ModelRole::User, "one"),
            assistant.clone(),
            bash_result("call_1"),
            message(ModelRole::Assistant, "done"),
        ];

        agent.discard_incomplete_tool_turn();

        assert_eq!(
            agent.messages,
            vec![
                message(ModelRole::User, "one"),
                assistant,
                bash_result("call_1"),
                message(ModelRole::Assistant, "done"),
            ]
        );
    }

    #[test]
    fn discard_incomplete_tool_turn_removes_partially_answered_multi_call_turn() {
        let mut agent = Agent::new("", Config::default());
        agent.messages = vec![
            message(ModelRole::User, "one"),
            ModelMessage {
                role: ModelRole::Assistant,
                text: String::new(),
                tool_calls: vec![bash_call("call_1"), bash_call("call_2")],
                tool_result: None,
                synthetic: None,
                thinking: Vec::new(),
            },
            bash_result("call_1"),
        ];

        agent.discard_incomplete_tool_turn();

        assert_eq!(agent.messages, vec![message(ModelRole::User, "one")]);
    }

    #[test]
    fn estimate_text_tokens_is_never_zero() {
        assert_eq!(estimate_text_tokens(""), 1);
        assert_eq!(estimate_text_tokens("abcd"), 1);
        assert_eq!(estimate_text_tokens("abcde"), 2);
    }

    #[test]
    fn compaction_split_keeps_short_histories_intact() {
        let messages = vec![message(ModelRole::User, "one"); 5];
        assert_eq!(compaction_split(&messages, 20), None);
    }

    #[test]
    fn compaction_split_returns_boundary_for_long_histories() {
        let messages = vec![message(ModelRole::User, "m"); 50];
        let split = compaction_split(&messages, 10).expect("split");
        // Keep at least `keep_recent` messages and leave older ones to summarize.
        assert!(split > 0 && split < messages.len());
        assert!(messages.len() - split >= 10);
    }

    #[test]
    fn compaction_split_progressive_shrinks_keep_recent() {
        // 21 messages with the default keep_recent of 20 yields no split, but
        // progressive shrinking should still find one so compaction is not a
        // silent no-op.
        let messages = vec![message(ModelRole::User, "m"); 21];
        assert_eq!(compaction_split(&messages, 20), None);
        let split = compaction_split_progressive(&messages, 20).expect("split");
        assert!(split > 0 && split < messages.len());
    }

    #[test]
    fn compaction_split_does_not_orphan_tool_results() {
        // A tool result at the split boundary must not become the first kept
        // message without its originating assistant tool call.
        let mut messages = vec![message(ModelRole::User, "start")];
        for index in 0..15 {
            messages.push(ModelMessage {
                role: ModelRole::Assistant,
                text: String::new(),
                tool_calls: vec![bash_call(&format!("call_{index}"))],
                tool_result: None,
                synthetic: None,
                thinking: Vec::new(),
            });
            messages.push(bash_result(&format!("call_{index}")));
        }
        let split = compaction_split(&messages, 10).expect("split");
        assert!(
            messages[split].tool_result.is_none(),
            "split must not start on a dangling tool result"
        );
    }
}
