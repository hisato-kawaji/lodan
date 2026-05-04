use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub llm: LlmConfig,
    pub agent: AgentConfig,
    pub tools: ToolsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub max_iterations: usize,
    pub auto_approve: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub bash: BashConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BashConfig {
    pub timeout_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            llm: LlmConfig::default(),
            agent: AgentConfig::default(),
            tools: ToolsConfig::default(),
        }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:11434/v1".to_string(),
            model: "qwen2.5-coder:7b".to_string(),
            api_key: String::new(),
            timeout_secs: 120,
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 25,
            auto_approve: false,
        }
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            bash: BashConfig::default(),
        }
    }
}

impl Default for BashConfig {
    fn default() -> Self {
        Self { timeout_secs: 30 }
    }
}

impl Config {
    /// Load config with layering: defaults <- user <- project <- explicit path.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let mut cfg = Config::default();

        if let Some(user_path) = user_config_path() {
            if let Some(loaded) = read_toml(&user_path)? {
                cfg = merge(cfg, loaded);
            }
        }

        let project_path = std::env::current_dir()
            .ok()
            .map(|p| p.join(".lodan").join("config.toml"));
        if let Some(p) = project_path {
            if let Some(loaded) = read_toml(&p)? {
                cfg = merge(cfg, loaded);
            }
        }

        if let Some(p) = explicit {
            let loaded = read_toml(p)?
                .with_context(|| format!("config file not found: {}", p.display()))?;
            cfg = merge(cfg, loaded);
        }

        Ok(cfg)
    }

    pub fn apply_overrides(
        &mut self,
        base_url: Option<String>,
        model: Option<String>,
        api_key: Option<String>,
        auto_approve: bool,
    ) {
        if let Some(v) = base_url {
            self.llm.base_url = v;
        }
        if let Some(v) = model {
            self.llm.model = v;
        }
        if let Some(v) = api_key {
            self.llm.api_key = v;
        }
        if auto_approve {
            self.agent.auto_approve = true;
        }
    }
}

fn user_config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "lodan")
        .map(|d| d.config_dir().join("config.toml"))
}

fn read_toml(path: &Path) -> Result<Option<Config>> {
    if !path.exists() {
        return Ok(None);
    }
    let s = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config =
        toml::from_str(&s).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(cfg))
}

/// Shallow merge: `over` fields replace `base` fields (TOML defaults already populated).
fn merge(_base: Config, over: Config) -> Config {
    over
}
