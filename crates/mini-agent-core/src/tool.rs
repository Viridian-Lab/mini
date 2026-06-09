use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A tool the agent can call in addition to the always-available built-in
/// `bash`. Implementations are shared via `Arc` and must be thread-safe: the
/// agent loop runs on a worker thread and the server shares one agent across
/// connections. Tools are synchronous, matching the rest of the blocking core.
pub trait Tool: Send + Sync + std::fmt::Debug {
    /// The schema advertised to the model. The `name` must be unique and is how
    /// the model addresses the tool; it must not be `"bash"` (reserved for the
    /// built-in).
    fn spec(&self) -> ToolSpec;

    /// Execute a call. `input` is the model-supplied arguments object. Return
    /// the text to feed back to the model as the tool result. Long-running
    /// tools should poll `cancel` and abort promptly when the user interrupts.
    fn call(&self, input: &Value, cancel: &CancelToken) -> anyhow::Result<ToolOutput>;
}

/// The model-facing description of a tool.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema (an `object`) for the arguments the model must supply.
    pub input_schema: Value,
}

/// The outcome of a tool call.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// Text fed back to the model as the tool result.
    pub content: String,
    /// Whether the tool itself reported an error (the turn still continues; the
    /// model sees the error text and can react).
    pub is_error: bool,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// A cheap, cloneable handle a tool can poll to learn whether the user has
/// interrupted the run, without exposing the underlying flag type.
#[derive(Clone, Default)]
pub struct CancelToken {
    flag: Option<Arc<AtomicBool>>,
}

impl CancelToken {
    pub(crate) fn new(flag: Option<Arc<AtomicBool>>) -> Self {
        Self { flag }
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }
}
