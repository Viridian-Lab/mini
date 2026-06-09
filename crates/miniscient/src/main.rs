use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use mini_agent_core::{
    AgentEvent, AgentOptions, AgentServer, AgentServerMessage, AgentServerReload,
    AgentServerStatus, Config, auth_status, logout, oauth_login,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

mod mcp;

const APP_DIR: &str = ".miniscient";
const MINI_AUTH_DIR: &str = ".mini-agent";
const DEFAULT_ADDR: &str = "127.0.0.1:47873";

/// miniscient's home directory (`~/.miniscient`), where it reads `mcp.toml`.
fn miniscient_root() -> Option<PathBuf> {
    Config::app_paths(APP_DIR).map(|paths| paths.root)
}
/// Upper bound on an accepted request body, so a client-supplied Content-Length
/// cannot drive an unbounded allocation.
const MAX_REQUEST_BODY: usize = 16 * 1024 * 1024;

#[derive(Debug, Parser)]
#[command(name = "miniscient")]
#[command(about = "Always-on mini agent server for local connector processes")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    Serve {
        #[arg(long, default_value = DEFAULT_ADDR)]
        listen: String,
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
        #[arg(long = "plugin")]
        plugins: Vec<PathBuf>,
        #[arg(long)]
        append_system_prompt: Option<String>,
        #[arg(long, help = "Ignore supported non-fatal plugin errors")]
        ignore: bool,
        #[arg(long, help = "Bypass supported safety checks and confirmations")]
        yolo: bool,
    },
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    Login,
    Status,
    Logout,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientRequest {
    Message {
        text: String,
        #[serde(default)]
        metadata: BTreeMap<String, serde_json::Value>,
    },
    Status,
    Reload,
    History,
    Clear,
    Ping,
}

#[derive(Debug, Serialize)]
struct WireResponse<T: Serialize> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct MessageResult {
    final_text: String,
    turns: usize,
    events: Vec<WireEvent>,
}

#[derive(Debug, Serialize)]
struct StatusResult {
    mode: String,
    provider: String,
    model: String,
    plugins: Vec<String>,
    tools: Vec<String>,
    cwd: String,
    home: Option<String>,
    messages: usize,
}

#[derive(Debug, Serialize)]
struct ReloadResult {
    mode: String,
    plugins: Vec<String>,
    skipped_plugins: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireEvent {
    AssistantDelta {
        content: String,
    },
    Assistant {
        content: String,
    },
    ToolUse {
        name: String,
        input: String,
    },
    ToolResult {
        name: String,
        status: Option<i32>,
        stdout: String,
        stderr: String,
    },
    ToolOutput {
        name: String,
        output: String,
        is_error: bool,
    },
    CompactStarted {
        estimated_tokens: usize,
    },
    CompactFinished {
        removed_messages: usize,
        summary_tokens: usize,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command.unwrap_or(Command::Serve {
        listen: DEFAULT_ADDR.to_string(),
        cwd: None,
        plugins: Vec::new(),
        append_system_prompt: None,
        ignore: false,
        yolo: false,
    }) {
        Command::Auth { command } => match command {
            AuthCommand::Login => {
                let auth =
                    oauth_login(APP_DIR, |url| eprintln!("Open this URL to sign in:\n{url}"))?;
                println!("logged in with {}", auth.auth_mode);
                Ok(())
            }
            AuthCommand::Status => {
                if let Some(auth) = auth_status(APP_DIR)? {
                    println!("{}", auth.auth_mode);
                } else if let Some(auth) = auth_status(MINI_AUTH_DIR)? {
                    println!("{} (from {MINI_AUTH_DIR})", auth.auth_mode);
                } else {
                    println!("not logged in");
                }
                Ok(())
            }
            AuthCommand::Logout => {
                logout(APP_DIR)?;
                println!("logged out");
                Ok(())
            }
        },
        Command::Serve {
            listen,
            cwd,
            plugins,
            append_system_prompt,
            ignore,
            yolo,
        } => serve(listen, cwd, plugins, append_system_prompt, ignore, yolo),
    }
}

fn serve(
    listen: String,
    cwd: Option<PathBuf>,
    plugins: Vec<PathBuf>,
    append_system_prompt: Option<String>,
    ignore_plugin_errors: bool,
    yolo: bool,
) -> Result<()> {
    let cwd = cwd
        .unwrap_or(std::env::current_dir().context("failed to resolve current directory")?)
        .canonicalize()
        .context("failed to resolve server cwd")?;
    let mut options = AgentOptions::new(APP_DIR, cwd);
    options.plugins = plugins;
    options.append_system_prompt = append_system_prompt;
    options.ignore_plugin_errors = ignore_plugin_errors;
    options.yolo = yolo;
    options.seed_default_plugins = true;
    if auth_status(APP_DIR)?.is_none() && auth_status(MINI_AUTH_DIR)?.is_some() {
        options.auth_app_dir_name = Some(MINI_AUTH_DIR.to_string());
    }

    // MCP servers configured in ~/.miniscient/mcp.toml.
    let mut mcp_config = match miniscient_root() {
        Some(root) => mcp::load_config(&root)?,
        None => mcp::McpConfig::default(),
    };

    let server = AgentServer::new(options)?;
    let skipped_plugins = server.skipped_plugins();
    if !skipped_plugins.is_empty() {
        eprintln!("skipped plugins:\n{}", skipped_plugins.join("\n"));
    }

    // Merge MCP servers declared by active plugins via an `[mcp]` front-matter
    // table; a name already set in mcp.toml takes precedence.
    for plugin in server.plugins() {
        let Some(value) = plugin.metadata("mcp") else {
            continue;
        };
        match value
            .clone()
            .try_into::<BTreeMap<String, mcp::ServerConfig>>()
        {
            Ok(servers) => {
                for (name, server_config) in servers {
                    match mcp_config.servers.entry(name) {
                        std::collections::btree_map::Entry::Vacant(slot) => {
                            slot.insert(server_config);
                        }
                        std::collections::btree_map::Entry::Occupied(slot) => eprintln!(
                            "mcp: plugin '{}' server '{}' ignored (already configured)",
                            plugin.id,
                            slot.key()
                        ),
                    }
                }
            }
            Err(err) => {
                eprintln!(
                    "mcp: plugin '{}' has an invalid [mcp] table: {err}",
                    plugin.id
                )
            }
        }
    }

    // Connect to every configured server and mount its tools.
    let (tools, notes) = mcp::mount(&mcp_config);
    for note in &notes {
        eprintln!("mcp: {note}");
    }
    if !tools.is_empty() {
        eprintln!("mounted {} MCP tool(s)", tools.len());
    }
    server.mount_tools(tools);

    let listener =
        TcpListener::bind(&listen).with_context(|| format!("failed to listen on {listen}"))?;
    eprintln!("miniscient listening on http://{listen}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let server = server.clone();
                thread::spawn(move || {
                    if let Err(err) = handle_connection(stream, server) {
                        eprintln!("connection error: {err:#}");
                    }
                });
            }
            Err(err) => eprintln!("accept error: {err}"),
        }
    }
    Ok(())
}

impl From<AgentEvent> for WireEvent {
    fn from(event: AgentEvent) -> Self {
        match event {
            AgentEvent::AssistantDelta(content) => Self::AssistantDelta { content },
            AgentEvent::Assistant(content) => Self::Assistant { content },
            AgentEvent::Command(input) => Self::ToolUse {
                name: "bash".to_string(),
                input,
            },
            AgentEvent::CommandOutput(output) => Self::ToolResult {
                name: "bash".to_string(),
                status: output.status,
                stdout: output.stdout,
                stderr: output.stderr,
            },
            AgentEvent::ToolUse { name, input } => Self::ToolUse { name, input },
            AgentEvent::ToolResult {
                name,
                output,
                is_error,
            } => Self::ToolOutput {
                name,
                output,
                is_error,
            },
            AgentEvent::CompactionStarted { estimated_tokens } => {
                Self::CompactStarted { estimated_tokens }
            }
            AgentEvent::CompactionFinished {
                removed_messages,
                summary_tokens,
            } => Self::CompactFinished {
                removed_messages,
                summary_tokens,
            },
        }
    }
}

impl From<AgentServerMessage> for MessageResult {
    fn from(message: AgentServerMessage) -> Self {
        Self {
            final_text: message.final_text,
            turns: message.turns,
            events: message.events.into_iter().map(WireEvent::from).collect(),
        }
    }
}

impl From<AgentServerStatus> for StatusResult {
    fn from(status: AgentServerStatus) -> Self {
        Self {
            mode: status.mode,
            provider: status.provider,
            model: status.model,
            plugins: status.plugins,
            tools: status.tools,
            cwd: status.cwd.display().to_string(),
            home: status.home.map(|home| home.display().to_string()),
            messages: status.messages,
        }
    }
}

impl From<AgentServerReload> for ReloadResult {
    fn from(reload: AgentServerReload) -> Self {
        Self {
            mode: reload.mode,
            plugins: reload.plugins,
            skipped_plugins: reload.skipped_plugins,
        }
    }
}

fn handle_connection(mut stream: TcpStream, server: AgentServer) -> Result<()> {
    // Bound how long a single connection can stall the worker thread waiting on
    // a slow or idle client (slowloris).
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let (method, path, body) = read_http_request(&stream)?;
    let response = route_request(&method, &path, body.as_deref(), server);
    let (status, value) = match response {
        Ok(value) => ("200 OK", value),
        Err(err) => (
            "500 Internal Server Error",
            serde_json::to_value(WireResponse::<serde_json::Value> {
                ok: false,
                result: None,
                error: Some(err.to_string()),
            })?,
        ),
    };
    let body = serde_json::to_vec_pretty(&value)?;
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    Ok(())
}

fn route_request(
    method: &str,
    path: &str,
    body: Option<&str>,
    server: AgentServer,
) -> Result<serde_json::Value> {
    let request = match (method, path) {
        ("GET", "/health") => ClientRequest::Ping,
        ("GET", "/status") => ClientRequest::Status,
        ("GET", "/history") => ClientRequest::History,
        ("POST", "/message") => serde_json::from_str(body.unwrap_or(""))?,
        ("POST", "/reload") => ClientRequest::Reload,
        ("POST", "/clear") => ClientRequest::Clear,
        ("POST", "/rpc") => serde_json::from_str(body.unwrap_or(""))?,
        _ => anyhow::bail!("unknown endpoint {method} {path}"),
    };
    let result = match request {
        ClientRequest::Message { text, metadata } => {
            let _ = metadata;
            serde_json::to_value(MessageResult::from(server.message(text)?))?
        }
        ClientRequest::Status => serde_json::to_value(StatusResult::from(server.status()))?,
        ClientRequest::Reload => serde_json::to_value(ReloadResult::from(server.reload()?))?,
        ClientRequest::History => serde_json::to_value(server.history())?,
        ClientRequest::Clear => {
            server.clear();
            json!({ "cleared": true })
        }
        ClientRequest::Ping => json!({ "status": "ok" }),
    };
    Ok(serde_json::to_value(WireResponse {
        ok: true,
        result: Some(result),
        error: None,
    })?)
}

fn read_http_request(stream: &TcpStream) -> Result<(String, String, Option<String>)> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().context("missing HTTP method")?.to_string();
    let path = parts.next().context("missing HTTP path")?.to_string();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().context("invalid content-length")?;
            if content_length > MAX_REQUEST_BODY {
                anyhow::bail!("request body too large ({content_length} bytes)");
            }
        }
    }
    let body = if content_length == 0 {
        None
    } else {
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body)?;
        Some(String::from_utf8(body).context("request body is not UTF-8")?)
    };
    Ok((method, path, body))
}
