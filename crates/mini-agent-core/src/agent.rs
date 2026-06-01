use crate::{
    Config, ModelMessage, ModelRole, ModelSyntheticKind, ModelToolResult,
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
        }
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
        self.messages.push(ModelMessage {
            role: ModelRole::User,
            text: user_prompt.into(),
            tool_calls: Vec::new(),
            tool_result: None,
            synthetic: None,
        });

        let mut turns = 0usize;
        loop {
            turns = turns.saturating_add(1);
            self.bail_if_interrupted(&interrupted)?;
            self.compact_if_needed(&mut emit)?;
            self.bail_if_interrupted(&interrupted)?;
            let response = if interrupted.is_some() {
                call_model_interruptible(
                    &self.system,
                    &self.config,
                    &self.messages,
                    |delta: &str| {
                        emit(AgentEvent::AssistantDelta(delta.to_string()));
                    },
                    interrupted.clone(),
                )?
            } else {
                call_model(&self.system, &self.config, &self.messages, |delta| {
                    emit(AgentEvent::AssistantDelta(delta.to_string()));
                })?
            };
            self.bail_if_interrupted(&interrupted)?;
            self.messages.push(ModelMessage {
                role: ModelRole::Assistant,
                text: response.text.clone(),
                tool_calls: response.tool_calls.clone(),
                tool_result: None,
                synthetic: None,
            });
            if !response.text.is_empty() {
                emit(AgentEvent::Assistant(response.text.clone()));
            }

            if response.tool_calls.is_empty() {
                return Ok(AgentRun {
                    final_text: response.text,
                    turns,
                });
            }

            for call in response.tool_calls {
                if call.name != "bash" {
                    anyhow::bail!("unsupported tool call '{}'", call.name);
                }
                let command = call
                    .input
                    .get("command")
                    .and_then(Value::as_str)
                    .or_else(|| call.input.as_str())
                    .context("bash tool call missing string 'command'")?;
                if command.trim().is_empty() {
                    anyhow::bail!("bash tool call command is empty");
                }
                self.bail_if_interrupted(&interrupted)?;
                let command = command.to_string();
                emit(AgentEvent::Command(command.clone()));
                self.bail_if_interrupted(&interrupted)?;

                let mut process = Command::new("bash");
                process.arg("-lc").arg(&command);
                if let Some(path) = Config::path_with_bin() {
                    process.env("PATH", path);
                }
                let raw_output = process.output().context("failed to run bash command")?;
                self.bail_if_interrupted(&interrupted)?;
                let output = CommandOutput {
                    status: raw_output.status.code(),
                    stdout: String::from_utf8_lossy(&raw_output.stdout).to_string(),
                    stderr: String::from_utf8_lossy(&raw_output.stderr).to_string(),
                };
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

                emit(AgentEvent::CommandOutput(output));
                self.messages.push(ModelMessage {
                    role: ModelRole::User,
                    text: String::new(),
                    tool_calls: Vec::new(),
                    tool_result: Some(ModelToolResult {
                        id: call.id,
                        name: call.name,
                        content,
                    }),
                    synthetic: None,
                });
            }
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
        let Some(split) = compaction_split(&self.messages, self.config.agent.compact_keep_recent)
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
