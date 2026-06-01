use crate::model::{ModelConfig, ProviderConfig};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

pub const ACTION_INSTRUCTION: &str = "When shell work is needed, call the `bash` tool with a `command` string. The runtime executes each command with `bash -lc` in the current workspace. Use normal markdown code blocks only for examples; code blocks are never executed. When the user's request is complete and no shell work is needed, respond normally without a tool call; the runtime will stop and return that response to the user.";

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
    (
        "default.md",
        include_str!("../../../examples/modes/default.md"),
    ),
    ("shell.md", include_str!("../../../examples/modes/shell.md")),
    (
        "review.md",
        include_str!("../../../examples/modes/review.md"),
    ),
];

pub const DEFAULT_PLUGINS: &[(&str, &str)] = &[
    ("jj.md", include_str!("../../../examples/plugins/jj.md")),
    (
        "subagents.md",
        include_str!("../../../examples/plugins/subagents.md"),
    ),
    (
        "memories.md",
        include_str!("../../../examples/plugins/memories.md"),
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
    pub fn load_default() -> anyhow::Result<Self> {
        let Some(paths) = Self::ensure_user_files()? else {
            return Ok(Self::default());
        };

        let source = std::fs::read_to_string(&paths.config_file)?;
        Ok(toml::from_str(&source)?)
    }

    pub fn save_default(&self) -> anyhow::Result<()> {
        let Some(paths) = Self::ensure_user_files()? else {
            anyhow::bail!("HOME is not set, so ~/.mini-agent/config.toml cannot be updated");
        };
        std::fs::write(&paths.config_file, toml::to_string_pretty(self)?)
            .with_context(|| format!("failed to write '{}'", paths.config_file.display()))?;
        Ok(())
    }

    pub fn user_paths() -> Option<ConfigPaths> {
        let home = std::env::var_os("HOME")?;
        let root = PathBuf::from(home).join(".mini-agent");
        Some(ConfigPaths {
            config_file: root.join("config.toml"),
            modes_dir: root.join("modes"),
            plugins_dir: root.join("plugins"),
            bin_dir: root.join("bin"),
            state_dir: root.join("state"),
            root,
        })
    }

    pub fn ensure_user_files() -> anyhow::Result<Option<ConfigPaths>> {
        let Some(paths) = Self::user_paths() else {
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

    pub fn mode_file(id: &str) -> Option<PathBuf> {
        Some(Self::user_paths()?.modes_dir.join(format!("{id}.md")))
    }

    pub fn path_with_bin() -> Option<OsString> {
        let paths = Self::user_paths()?;
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
}
