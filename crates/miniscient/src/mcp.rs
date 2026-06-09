//! A small, synchronous MCP (Model Context Protocol) client that mounts a
//! server's tools onto a mini-agent as `Tool` implementations.
//!
//! Supports the two common transports:
//! - **stdio**: launch the server as a subprocess and speak newline-delimited
//!   JSON-RPC over its stdin/stdout. A dedicated reader thread feeds lines to
//!   the caller so requests can be bounded by a timeout and aborted on cancel.
//! - **Streamable HTTP**: POST each JSON-RPC message to a single endpoint and
//!   read back either a JSON response or an SSE stream, bounded by a request
//!   timeout and a response-body cap.
//!
//! Calls are serialized (the agent runs one tool at a time behind its own
//! lock), so the client does not pipeline requests: each request is written,
//! then responses are read until the matching id arrives, skipping interleaved
//! notifications or server requests.

use anyhow::{Context, Result, anyhow, bail};
use mini_agent_core::{CancelToken, Tool, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The protocol version this client advertises during initialization. Servers
/// negotiate down/across as needed and echo the agreed version.
const CLIENT_PROTOCOL_VERSION: &str = "2025-06-18";
/// Maximum time to wait for a single request's response before failing, so a
/// stalled or dead server cannot wedge the agent forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Connect timeout for the HTTP transport.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Cap on an HTTP response body, so a server streaming an unbounded SSE stream
/// cannot exhaust memory.
const MAX_RESPONSE_BODY: u64 = 16 * 1024 * 1024;
/// Cap on a tool name's length (OpenAI/Anthropic limit).
const MAX_TOOL_NAME: usize = 128;

// ---------------------------------------------------------------------------
// Configuration (deserialized from ~/.miniscient/mcp.toml)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: BTreeMap<String, ServerConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    /// stdio transport: the executable to launch.
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// HTTP transport: the MCP endpoint URL.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// Load the MCP config from `<root>/mcp.toml`, returning an empty config if the
/// file does not exist.
pub fn load_config(root: &Path) -> Result<McpConfig> {
    let path = root.join("mcp.toml");
    if !path.exists() {
        return Ok(McpConfig::default());
    }
    let source = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    toml::from_str(&source).with_context(|| format!("invalid MCP config '{}'", path.display()))
}

/// Connect to every configured MCP server and return the tools they expose,
/// already wrapped as agent `Tool`s. A server that fails to connect is reported
/// in the returned notes and skipped, so one bad server does not prevent the
/// others (or the agent) from starting. Notes also report any tool dropped or
/// renamed because of a name collision.
pub fn mount(config: &McpConfig) -> (Vec<Arc<dyn Tool>>, Vec<String>) {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    let mut notes = Vec::new();
    let mut used_names = BTreeSet::new();
    for (name, server) in &config.servers {
        match McpClient::connect(name, server) {
            Ok((client, defs)) => {
                for def in defs {
                    let base = namespaced_tool_name(name, &def.name);
                    let advertised = unique_name(&base, &mut used_names);
                    if advertised != base {
                        notes.push(format!(
                            "{name}: tool '{}' renamed to '{advertised}' to avoid a name collision",
                            def.name
                        ));
                    }
                    tools.push(Arc::new(McpTool {
                        client: client.clone(),
                        advertised_name: advertised,
                        remote_name: def.name,
                        description: def.description,
                        input_schema: def.input_schema,
                    }));
                }
            }
            Err(err) => notes.push(format!("{name}: {err:#}")),
        }
    }
    (tools, notes)
}

// ---------------------------------------------------------------------------
// Tool wrapper
// ---------------------------------------------------------------------------

struct ToolDef {
    name: String,
    description: String,
    input_schema: Value,
}

#[derive(Debug)]
struct McpTool {
    client: Arc<McpClient>,
    /// The (namespaced, charset-safe, unique) name advertised to the model.
    advertised_name: String,
    /// The original tool name on the server, used when calling it.
    remote_name: String,
    description: String,
    input_schema: Value,
}

impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.advertised_name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }

    fn call(&self, input: &Value, cancel: &CancelToken) -> Result<ToolOutput> {
        // The stdio transport polls `cancel` while waiting; the HTTP transport
        // is bounded by REQUEST_TIMEOUT and checks `cancel` before sending.
        self.client.call_tool(&self.remote_name, input, cancel)
    }
}

// ---------------------------------------------------------------------------
// Client (lifecycle + tools/list + tools/call)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct McpClient {
    name: String,
    transport: Mutex<Box<dyn Transport>>,
    next_id: AtomicU64,
}

impl McpClient {
    fn connect(name: &str, config: &ServerConfig) -> Result<(Arc<Self>, Vec<ToolDef>)> {
        let transport: Box<dyn Transport> = match (&config.command, &config.url) {
            (Some(command), None) => Box::new(StdioTransport::spawn(command, config)?),
            (None, Some(url)) => Box::new(HttpTransport::new(url, config)?),
            (Some(_), Some(_)) => {
                bail!("server '{name}' sets both `command` and `url`; choose one transport")
            }
            (None, None) => bail!("server '{name}' must set either `command` (stdio) or `url`"),
        };
        let client = Arc::new(Self {
            name: name.to_string(),
            transport: Mutex::new(transport),
            next_id: AtomicU64::new(1),
        });
        client.initialize()?;
        let tools = client.list_tools()?;
        Ok((client, tools))
    }

    fn request(&self, method: &str, params: Value, cancel: &CancelToken) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        // The Mutex serializes access to the transport. The agent already runs
        // one tool at a time, so this only ever contends under unexpected
        // concurrency, where REQUEST_TIMEOUT still bounds the wait.
        let mut transport = self
            .transport
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        transport
            .round_trip(&message, &json!(id), cancel)
            .with_context(|| format!("MCP '{}' {method} failed", self.name))
    }

    fn notify(&self, method: &str, params: Value) -> Result<()> {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let mut transport = self
            .transport
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        transport.send_notification(&message)
    }

    fn initialize(&self) -> Result<()> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": CLIENT_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "miniscient", "version": env!("CARGO_PKG_VERSION") },
            }),
            &CancelToken::default(),
        )?;
        let negotiated = result["protocolVersion"]
            .as_str()
            .unwrap_or(CLIENT_PROTOCOL_VERSION)
            .to_string();
        self.transport
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .finish_initialization(&negotiated);
        self.notify("notifications/initialized", json!({}))?;
        Ok(())
    }

    fn list_tools(&self) -> Result<Vec<ToolDef>> {
        const MAX_PAGES: usize = 100;
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self.request("tools/list", params, &CancelToken::default())?;
            for tool in result["tools"].as_array().into_iter().flatten() {
                let Some(name) = tool["name"].as_str() else {
                    continue;
                };
                let input_schema = if tool["inputSchema"].is_object() {
                    tool["inputSchema"].clone()
                } else {
                    json!({ "type": "object" })
                };
                tools.push(ToolDef {
                    name: name.to_string(),
                    description: tool["description"].as_str().unwrap_or_default().to_string(),
                    input_schema,
                });
            }
            match result["nextCursor"].as_str() {
                Some(next) if Some(next) != cursor.as_deref() => cursor = Some(next.to_string()),
                _ => break,
            }
        }
        Ok(tools)
    }

    fn call_tool(&self, name: &str, arguments: &Value, cancel: &CancelToken) -> Result<ToolOutput> {
        let arguments = if arguments.is_object() {
            arguments.clone()
        } else {
            json!({})
        };
        // A protocol-level failure (unknown tool, bad params) comes back as an
        // Err here; surface it to the model as an error result rather than
        // aborting, so it can self-correct. A transport failure propagates.
        let result = match self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
            cancel,
        ) {
            Ok(result) => result,
            Err(err) if is_protocol_error(&err) => {
                return Ok(ToolOutput::error(format!("{err:#}")));
            }
            Err(err) => return Err(err),
        };
        let is_error = result["isError"].as_bool().unwrap_or(false);
        Ok(ToolOutput {
            content: tool_result_text(&result),
            is_error,
        })
    }
}

/// A JSON-RPC protocol error (mapped to a tool error the model sees) versus a
/// transport failure (which should propagate and abort the call).
fn is_protocol_error(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.to_string().starts_with("MCP error "))
}

/// Build the model-facing text for a tools/call result: flatten text content
/// blocks, falling back to serialized `structuredContent` when the content
/// array is empty (servers may put the payload only there).
fn tool_result_text(result: &Value) -> String {
    let content_empty = result["content"]
        .as_array()
        .map(|content| content.is_empty())
        .unwrap_or(true);
    if content_empty
        && let Some(structured) = result.get("structuredContent").filter(|v| !v.is_null())
        && let Ok(text) = serde_json::to_string(structured)
    {
        return text;
    }
    flatten_content(&result["content"])
}

/// Flatten an MCP `content` array into a single string for the model: collect
/// the text of every `text` block (and an embedded resource's inline text);
/// represent other blocks with a short placeholder so nothing silently vanishes.
fn flatten_content(content: &Value) -> String {
    let mut parts = Vec::new();
    for block in content.as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => parts.push(block["text"].as_str().unwrap_or_default().to_string()),
            Some("resource") => {
                let resource = &block["resource"];
                if let Some(text) = resource["text"].as_str() {
                    parts.push(text.to_string());
                } else {
                    parts.push(format!(
                        "[resource {}]",
                        resource["uri"].as_str().unwrap_or("")
                    ));
                }
            }
            Some("resource_link") => parts.push(format!(
                "[resource {}]",
                block["uri"].as_str().unwrap_or("")
            )),
            Some(kind) => parts.push(format!("[{kind}]")),
            None => {}
        }
    }
    if parts.is_empty() {
        "(no content)".to_string()
    } else {
        parts.join("\n")
    }
}

// ---------------------------------------------------------------------------
// Transports
// ---------------------------------------------------------------------------

trait Transport: Send + std::fmt::Debug {
    /// Send a JSON-RPC request and return the matching response's `result`,
    /// erroring on a JSON-RPC error, transport failure, timeout, or cancel.
    fn round_trip(&mut self, message: &Value, id: &Value, cancel: &CancelToken) -> Result<Value>;
    /// Send a JSON-RPC notification (no response expected).
    fn send_notification(&mut self, message: &Value) -> Result<()>;
    /// Record the negotiated protocol version and mark initialization complete
    /// (HTTP starts sending the version header only afterward).
    fn finish_initialization(&mut self, _version: &str) {}
}

/// Serialize a JSON-RPC message to one compact line with no raw embedded
/// newlines, as the stdio transport requires.
fn encode_line(message: &Value) -> Result<String> {
    let line = serde_json::to_string(message).context("failed to encode MCP message")?;
    Ok(format!("{line}\n"))
}

/// Extract a JSON-RPC `result` from a parsed response, mapping a JSON-RPC
/// `error` object to an `Err`.
fn result_from_response(value: &Value) -> Result<Value> {
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        let code = error["code"].as_i64().unwrap_or_default();
        let message = error["message"].as_str().unwrap_or("unknown error");
        return Err(anyhow!("MCP error {code}: {message}"));
    }
    Ok(value.get("result").cloned().unwrap_or_else(|| json!({})))
}

/// Compare JSON-RPC ids tolerantly: exact match, or equal as numbers (so an
/// integer id echoed as `1.0` still correlates).
fn json_id_eq(a: &Value, b: &Value) -> bool {
    if a == b {
        return true;
    }
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

#[derive(Debug)]
struct StdioTransport {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    _reader: JoinHandle<()>,
}

impl StdioTransport {
    fn spawn(command: &str, config: &ServerConfig) -> Result<Self> {
        let mut process = Command::new(command);
        process
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The server logs to stderr; inherit it so logs surface in
            // miniscient's stderr (and the pipe can never fill and deadlock).
            .stderr(Stdio::inherit());
        for (key, value) in &config.env {
            process.env(key, value);
        }
        let mut child = process
            .spawn()
            .with_context(|| format!("failed to launch MCP server '{command}'"))?;
        let stdin = child.stdin.take().context("missing child stdin")?;
        let stdout = child.stdout.take().context("missing child stdout")?;
        // Read lines on a dedicated thread so round_trip can wait with a
        // timeout / cancel instead of blocking on the pipe forever.
        let (sender, lines) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break, // EOF or read error
                    Ok(_) => {
                        if sender.send(line).is_err() {
                            break; // receiver dropped (transport gone)
                        }
                    }
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            lines,
            _reader: reader,
        })
    }

    fn write_message(&mut self, message: &Value) -> Result<()> {
        self.stdin
            .write_all(encode_line(message)?.as_bytes())
            .context("failed to write to MCP server")?;
        self.stdin.flush().context("failed to flush MCP server")?;
        Ok(())
    }
}

impl Transport for StdioTransport {
    fn round_trip(&mut self, message: &Value, id: &Value, cancel: &CancelToken) -> Result<Value> {
        self.write_message(message)?;
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            if cancel.is_cancelled() {
                bail!("MCP call interrupted");
            }
            let now = Instant::now();
            if now >= deadline {
                bail!("MCP request timed out");
            }
            let slice = (deadline - now).min(Duration::from_millis(100));
            match self.lines.recv_timeout(slice) {
                Ok(line) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    // Tolerate non-protocol noise leaked to stdout; skip
                    // notifications and server requests (we ignore them).
                    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
                        continue;
                    };
                    if json_id_eq(&value["id"], id) {
                        return result_from_response(&value);
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("MCP server closed the connection");
                }
            }
        }
    }

    fn send_notification(&mut self, message: &Value) -> Result<()> {
        self.write_message(message)
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        // Best-effort shutdown: closing stdin signals EOF, then ensure the child
        // is reaped. The reader thread exits when stdout hits EOF.
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug)]
struct HttpTransport {
    client: reqwest::blocking::Client,
    url: String,
    headers: BTreeMap<String, String>,
    session_id: Option<String>,
    protocol_version: String,
    /// Whether initialization has completed; the MCP-Protocol-Version header is
    /// only sent on requests after initialize (no version is negotiated yet on
    /// the initialize request itself).
    initialized: bool,
}

impl HttpTransport {
    fn new(url: &str, config: &ServerConfig) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            client,
            url: url.to_string(),
            headers: config.headers.clone(),
            session_id: None,
            protocol_version: CLIENT_PROTOCOL_VERSION.to_string(),
            initialized: false,
        })
    }

    fn post(&mut self, message: &Value) -> Result<reqwest::blocking::Response> {
        let mut request = self
            .client
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json");
        if self.initialized {
            request = request.header("MCP-Protocol-Version", &self.protocol_version);
        }
        if let Some(session) = &self.session_id {
            request = request.header("MCP-Session-Id", session);
        }
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        let response = request
            .json(message)
            .send()
            .context("MCP HTTP request failed")?;
        // Capture/refresh the session id (header names are case-insensitive).
        if let Some(session) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            self.session_id = Some(session.to_string());
        }
        Ok(response)
    }
}

impl Transport for HttpTransport {
    fn round_trip(&mut self, message: &Value, id: &Value, cancel: &CancelToken) -> Result<Value> {
        if cancel.is_cancelled() {
            bail!("MCP call interrupted");
        }
        let response = self.post(message)?;
        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let body = read_capped(response)?;
        if !status.is_success() {
            bail!("MCP HTTP request failed with status {status}: {body}");
        }

        if content_type.contains("text/event-stream") {
            // Find the SSE event whose data parses to a JSON-RPC message with
            // the matching id; server requests/notifications may precede it.
            for data in sse_data_messages(&body) {
                if let Ok(value) = serde_json::from_str::<Value>(&data)
                    && json_id_eq(&value["id"], id)
                {
                    return result_from_response(&value);
                }
            }
            bail!("MCP SSE stream did not contain a response for the request");
        }

        let value: Value = serde_json::from_str(&body).context("MCP HTTP response was not JSON")?;
        result_from_response(&value)
    }

    fn send_notification(&mut self, message: &Value) -> Result<()> {
        let response = self.post(message)?;
        let status = response.status();
        // 202 Accepted (no body) is the success signal for a notification.
        if !status.is_success() {
            let body = read_capped(response).unwrap_or_default();
            bail!("MCP HTTP notification failed with status {status}: {body}");
        }
        Ok(())
    }

    fn finish_initialization(&mut self, version: &str) {
        self.protocol_version = version.to_string();
        self.initialized = true;
    }
}

impl Drop for HttpTransport {
    fn drop(&mut self) {
        // Best-effort session teardown so server-side state does not linger.
        if let Some(session) = self.session_id.clone() {
            let _ = self
                .client
                .delete(&self.url)
                .header("MCP-Session-Id", session)
                .header("MCP-Protocol-Version", &self.protocol_version)
                .send();
        }
    }
}

/// Read a response body, capped so an unbounded stream cannot exhaust memory.
fn read_capped(response: reqwest::blocking::Response) -> Result<String> {
    let mut buffer = Vec::new();
    response
        .take(MAX_RESPONSE_BODY)
        .read_to_end(&mut buffer)
        .context("failed to read MCP HTTP response")?;
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

/// Parse an SSE body into the concatenated `data:` payload of each event (one
/// string per event). Events are separated by a blank line; a single event may
/// carry multiple `data:` lines that are joined with newlines.
fn sse_data_messages(body: &str) -> Vec<String> {
    let mut messages = Vec::new();
    let mut data_lines: Vec<String> = Vec::new();
    let flush = |data_lines: &mut Vec<String>, messages: &mut Vec<String>| {
        if !data_lines.is_empty() {
            messages.push(data_lines.join("\n"));
            data_lines.clear();
        }
    };
    for raw in body.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() {
            flush(&mut data_lines, &mut messages);
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
        // Other SSE fields (event:, id:, retry:) are not needed here.
    }
    flush(&mut data_lines, &mut messages);
    messages
}

/// Build a charset-safe, namespaced tool name (`<server>__<tool>`) that both
/// the OpenAI and Anthropic tool-name validators accept (`[A-Za-z0-9_-]`,
/// length <= 128).
fn namespaced_tool_name(server: &str, tool: &str) -> String {
    let sanitize = |text: &str| -> String {
        text.chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                    ch
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    let mut name = format!("{}__{}", sanitize(server), sanitize(tool));
    if name.len() > MAX_TOOL_NAME {
        name.truncate(MAX_TOOL_NAME);
    }
    name
}

/// Ensure `base` is unique among `used`, appending a numeric suffix (within the
/// length cap) when it collides, so a clash never silently drops a tool.
fn unique_name(base: &str, used: &mut BTreeSet<String>) -> String {
    if used.insert(base.to_string()) {
        return base.to_string();
    }
    for suffix in 2..10_000 {
        let tag = format!("_{suffix}");
        let trimmed = if base.len() + tag.len() > MAX_TOOL_NAME {
            &base[..MAX_TOOL_NAME - tag.len()]
        } else {
            base
        };
        let candidate = format!("{trimmed}{tag}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    base.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_joins_text_blocks_and_marks_others() {
        let content = json!([
            { "type": "text", "text": "first" },
            { "type": "image", "data": "...", "mimeType": "image/png" },
            { "type": "text", "text": "second" },
        ]);
        assert_eq!(flatten_content(&content), "first\n[image]\nsecond");
        assert_eq!(flatten_content(&json!([])), "(no content)");
    }

    #[test]
    fn embedded_resource_text_is_included() {
        let content = json!([
            { "type": "resource", "resource": { "uri": "file:///x", "text": "hello" } },
            { "type": "resource", "resource": { "uri": "file:///bin" } },
        ]);
        assert_eq!(flatten_content(&content), "hello\n[resource file:///bin]");
    }

    #[test]
    fn structured_content_used_when_content_empty() {
        let result = json!({ "content": [], "structuredContent": { "temp": 22 } });
        assert_eq!(tool_result_text(&result), "{\"temp\":22}");
        // When content has text, it wins.
        let result = json!({ "content": [{ "type": "text", "text": "t" }], "structuredContent": { "x": 1 } });
        assert_eq!(tool_result_text(&result), "t");
    }

    #[test]
    fn result_from_response_maps_jsonrpc_error() {
        let ok = json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": [] } });
        assert!(result_from_response(&ok).is_ok());
        let err =
            json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32602, "message": "bad" } });
        let message = result_from_response(&err).unwrap_err().to_string();
        assert!(message.contains("-32602") && message.contains("bad"));
    }

    #[test]
    fn ids_match_across_integer_and_float_forms() {
        assert!(json_id_eq(&json!(1u64), &json!(1)));
        assert!(json_id_eq(&json!(1u64), &json!(1.0)));
        assert!(!json_id_eq(&json!(1u64), &json!(2)));
        assert!(!json_id_eq(&json!(1u64), &Value::Null));
    }

    #[test]
    fn sse_parses_multiple_events_and_data_lines() {
        let body = "event: message\ndata: {\"id\":1}\n\nid: 7\ndata: {\"id\"\ndata: :2}\n\n";
        let messages = sse_data_messages(body);
        assert_eq!(
            messages,
            vec!["{\"id\":1}".to_string(), "{\"id\"\n:2}".to_string()]
        );
    }

    #[test]
    fn tool_names_are_namespaced_and_charset_safe() {
        assert_eq!(namespaced_tool_name("fs", "read_file"), "fs__read_file");
        // Dots and slashes (rejected by OpenAI/Anthropic) become underscores.
        assert_eq!(namespaced_tool_name("a.b", "x/y"), "a_b__x_y");
    }

    #[test]
    fn unique_name_disambiguates_collisions() {
        let mut used = BTreeSet::new();
        assert_eq!(unique_name("fs__read", &mut used), "fs__read");
        assert_eq!(unique_name("fs__read", &mut used), "fs__read_2");
        assert_eq!(unique_name("fs__read", &mut used), "fs__read_3");
    }
}
