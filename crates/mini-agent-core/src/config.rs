use crate::model::{ModelConfig, ProviderConfig};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

pub const ACTION_INSTRUCTION: &str = "When shell work is needed, call the `bash` tool with a `command` string. The runtime executes each command with `bash -lc` in the current workspace. Use normal markdown code blocks only for examples; code blocks are never executed. When the user's request is complete and no shell work is needed, respond normally without a tool call; the runtime will stop and return that response to the user. You should use the tools provided when they seem useful (i.e. creating memories when something is important, using subagents when the problem can be broken into smaller parallel tasks, etc.)";

const DEFAULT_CONFIG: &str = r#"[agent]
default_mode = "default"
plugins = []
auto_compact = true
compact_threshold = 0.7
compact_keep_recent = 20
# context_window_tokens defaults to 128000 when unset.
# context_window_tokens = 200000

[model]
provider = "codex"
model = "gpt-5.5"
"#;

const DEFAULT_MODES: &[(&str, &str)] = &[
    ("default.md", include_str!("../assets/modes/default.md")),
    ("shell.md", include_str!("../assets/modes/shell.md")),
    ("review.md", include_str!("../assets/modes/review.md")),
];

pub const DEFAULT_PLUGINS: &[(&str, &str)] = &[
    ("jj.md", include_str!("../assets/plugins/jj.md")),
    (
        "subagents.md",
        include_str!("../assets/plugins/subagents.md"),
    ),
    ("memories.md", include_str!("../assets/plugins/memories.md")),
    (
        "codex-computer-use.md",
        include_str!("../assets/plugins/codex-computer-use.md"),
    ),
];

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub model: ModelConfig,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderConfig>,
    /// The app's state directory name under the user's home directory, or an
    /// absolute path. Set by the caller via `load_from_app`; the core ships no
    /// default so it stays free of any particular front-end's identity.
    #[serde(skip)]
    pub app_dir_name: String,
    /// Optional app state directory name/path used only for auth. This lets a
    /// front-end keep its own config/state while sharing login state with
    /// another front-end, without the core knowing either identity.
    #[serde(skip)]
    pub auth_app_dir_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentConfig {
    pub default_mode: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugins: Vec<String>,
    pub auto_compact: bool,
    pub compact_threshold: f32,
    pub compact_keep_recent: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<usize>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            default_mode: "default".to_string(),
            plugins: Vec::new(),
            auto_compact: true,
            compact_threshold: 0.7,
            compact_keep_recent: 20,
            context_window_tokens: None,
        }
    }
}

impl Config {
    pub fn load_from_app(app_dir_name: &str) -> anyhow::Result<Self> {
        let Some(paths) = Self::ensure_app_files(app_dir_name)? else {
            return Ok(Self {
                app_dir_name: app_dir_name.to_string(),
                ..Self::default()
            });
        };

        let source = std::fs::read_to_string(&paths.config_file)?;
        let mut config: Self = toml::from_str(&source)?;
        config.app_dir_name = app_dir_name.to_string();
        config.auth_app_dir_name = None;
        Ok(config)
    }

    /// Persist this config to its own app directory (`self.app_dir_name`).
    pub fn save(&self) -> anyhow::Result<()> {
        let Some(paths) = Self::ensure_app_files(&self.app_dir_name)? else {
            anyhow::bail!(
                "HOME is not set, so {}/config.toml cannot be updated",
                self.app_dir_name
            );
        };
        std::fs::write(&paths.config_file, toml::to_string_pretty(self)?)
            .with_context(|| format!("failed to write '{}'", paths.config_file.display()))?;
        Ok(())
    }

    pub fn app_paths(app_dir_name: &str) -> Option<ConfigPaths> {
        if app_dir_name.is_empty() {
            return None;
        }
        let root = Self::app_root(app_dir_name)?;
        Some(ConfigPaths {
            config_file: root.join("config.toml"),
            modes_dir: root.join("modes"),
            plugins_dir: root.join("plugins"),
            bin_dir: root.join("bin"),
            state_dir: root.join("state"),
            app_dir_name: app_dir_name.to_string(),
            root,
        })
    }

    pub fn app_root(app_dir_name: &str) -> Option<PathBuf> {
        if app_dir_name.is_empty() {
            return None;
        }
        let path = PathBuf::from(app_dir_name);
        if path.is_absolute() {
            Some(path)
        } else {
            Self::home_dir().map(|home| home.join(path))
        }
    }

    pub fn home_dir() -> Option<PathBuf> {
        non_empty_env_path("HOME").or_else(dirs::home_dir)
    }

    pub fn ensure_app_files(app_dir_name: &str) -> anyhow::Result<Option<ConfigPaths>> {
        let Some(paths) = Self::app_paths(app_dir_name) else {
            return Ok(None);
        };

        std::fs::create_dir_all(&paths.modes_dir)
            .with_context(|| format!("failed to create '{}'", paths.modes_dir.display()))?;
        std::fs::create_dir_all(&paths.plugins_dir)
            .with_context(|| format!("failed to create '{}'", paths.plugins_dir.display()))?;
        std::fs::create_dir_all(&paths.bin_dir)
            .with_context(|| format!("failed to create '{}'", paths.bin_dir.display()))?;
        std::fs::create_dir_all(&paths.state_dir)
            .with_context(|| format!("failed to create '{}'", paths.state_dir.display()))?;

        if !paths.config_file.exists() {
            std::fs::write(&paths.config_file, DEFAULT_CONFIG)
                .with_context(|| format!("failed to write '{}'", paths.config_file.display()))?;
        }

        for (file_name, source) in DEFAULT_MODES {
            let path = paths.modes_dir.join(file_name);
            if !path.exists() {
                std::fs::write(&path, source)
                    .with_context(|| format!("failed to write '{}'", path.display()))?;
            }
        }

        Ok(Some(paths))
    }

    pub fn path_with_app_bin(app_dir_name: &str) -> Option<OsString> {
        let paths = Self::app_paths(app_dir_name)?;
        let mut path = vec![paths.bin_dir];
        if let Some(current) = std::env::var_os("PATH") {
            path.extend(std::env::split_paths(&current));
        }
        std::env::join_paths(path).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    pub root: PathBuf,
    pub config_file: PathBuf,
    pub modes_dir: PathBuf,
    pub plugins_dir: PathBuf,
    pub bin_dir: PathBuf,
    pub state_dir: PathBuf,
    pub app_dir_name: String,
}

fn non_empty_env_path(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name)?;
    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}
