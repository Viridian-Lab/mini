use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{cursor, execute, queue, terminal};
use mini_agent_core::{
    Agent, AgentEvent, BUILT_IN_PROVIDER_NAMES, Config, DEFAULT_CONTEXT_WINDOW_TOKENS,
    DEFAULT_SYSTEM_PROMPT, ModelMessage, ModelRole, Plugin, PluginError, PluginKind,
    ProviderConfig, compose_prompt, estimate_messages_tokens, install_scripts, list_models,
    load_plugin,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::{self, Stdout, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::as_24_bit_terminal_escaped;
use unicode_width::UnicodeWidthChar;

// waverows from @agilek/cli-loaders / unicode-animations.
const SPINNER: [&str; 16] = [
    "⠖⠉⠉⠑",
    "⡠⠖⠉⠉",
    "⣠⡠⠖⠉",
    "⣄⣠⡠⠖",
    "⠢⣄⣠⡠",
    "⠙⠢⣄⣠",
    "⠉⠙⠢⣄",
    "⠊⠉⠙⠢",
    "⠜⠊⠉⠙",
    "⡤⠜⠊⠉",
    "⣀⡤⠜⠊",
    "⢤⣀⡤⠜",
    "⠣⢤⣀⡤",
    "⠑⠣⢤⣀",
    "⠉⠑⠣⢤",
    "⠋⠉⠑⠣",
];
const BANNER: &str = r"         _      _ 
  __ _  (_)__  (_)
 /  ' \/ / _ \/ / 
/_/_/_/_/_//_/_/  
                  ";
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const BRIGHT_BLACK: &str = "\x1b[90m";
const BG_USER: &str = "\x1b[40m";
const INPUT_FRAME: &str = BRIGHT_BLACK;
const BOLD_CYAN: &str = "\x1b[1;36m";
const BOLD_WHITE: &str = "\x1b[1;97m";
const MESSAGE_INDENT: usize = 3;
const OUTPUT_HEAD_LINES: usize = 24;
const OUTPUT_TAIL_LINES: usize = 8;
const STREAM_UNSTABLE_ROWS: usize = 8;
const SLASH_COMMANDS: [&str; 11] = [
    "/help",
    "/provider",
    "/model",
    "/model add",
    "/mode",
    "/effort",
    "/session",
    "/resume",
    "/reload",
    "/compact",
    "/compact status",
];

enum AgentUpdate {
    Event(AgentEvent),
    Done(Box<Agent>, Result<()>),
}

struct RunningAgent {
    receiver: Receiver<AgentUpdate>,
    interrupted: Arc<AtomicBool>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
    Command,
    Output,
    Local,
}

struct Message {
    role: Role,
    text: String,
    output: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct StoredSession {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    mode: String,
    system: String,
    config: Config,
    messages: Vec<ModelMessage>,
}

#[derive(Clone, Copy)]
enum SelectionCommand {
    CommandPalette,
    Provider,
    Model,
    Mode,
    Effort,
    Resume,
}

struct SelectionItem {
    label: String,
    value: String,
}

struct Selection {
    title: String,
    command: SelectionCommand,
    items: Vec<SelectionItem>,
    selected: usize,
}

struct App {
    messages: Vec<Message>,
    history: Vec<String>,
    history_index: Option<usize>,
    provider: String,
    model: String,
    mode: String,
    effort: Option<String>,
    context_window_tokens: usize,
    context_percent: Option<usize>,
    input: Vec<char>,
    cursor: usize,
    spinner: usize,
    printed_messages: usize,
    streaming_text: String,
    streaming_started: bool,
    stream_message_cutoff: Option<usize>,
    streaming_rows: Vec<String>,
    streaming_committed_rows: usize,
    stream_final_skip_rows: Option<usize>,
    previous_bottom_rows: u16,
    rendered_width: Option<u16>,
    needs_full_redraw: bool,
    running_since: Option<Instant>,
    session_id: String,
    session_title: Option<String>,
    selection: Option<Selection>,
    plugins: Vec<Plugin>,
    plugin_specs: Vec<PathBuf>,
    cwd: PathBuf,
    append_system_prompt: Option<String>,
    ignore_plugin_errors: bool,
    yolo: bool,
    agent: Option<Agent>,
    running: Option<RunningAgent>,
}

pub struct RunOptions {
    pub system_prompt: String,
    pub config: Config,
    pub mode: String,
    pub plugins: Vec<Plugin>,
    pub plugin_specs: Vec<PathBuf>,
    pub cwd: PathBuf,
    pub append_system_prompt: Option<String>,
    pub ignore_plugin_errors: bool,
    pub yolo: bool,
    pub resume: Option<String>,
    pub session_id: Option<String>,
}

include!("runtime.rs");
include!("sessions.rs");
include!("render.rs");
include!("commands.rs");
include!("text.rs");
