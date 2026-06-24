use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::hooks::HookConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum Provider {
    Local,
    Sakana,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Local => "local",
            Provider::Sakana => "sakana",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub llm: LlmConfig,
    pub agent: AgentConfig,
    pub tools: ToolsConfig,
    #[serde(default)]
    pub hooks: Vec<HookConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub provider: Provider,
    pub local: ProviderConfig,
    pub sakana: ProviderConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub bash: BashConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BashConfig {
    pub timeout_secs: u64,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: Provider::Local,
            local: ProviderConfig::default_local(),
            sakana: ProviderConfig::default_sakana(),
        }
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self::default_local()
    }
}

impl ProviderConfig {
    pub fn default_local() -> Self {
        Self {
            base_url: "http://localhost:11434/v1".to_string(),
            model: "qwen2.5-coder:7b".to_string(),
            api_key: String::new(),
            timeout_secs: 120,
        }
    }

    pub fn default_sakana() -> Self {
        Self {
            base_url: "https://api.sakana.ai/v1".to_string(),
            model: "fugu".to_string(),
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

impl Default for BashConfig {
    fn default() -> Self {
        Self { timeout_secs: 30 }
    }
}

impl LlmConfig {
    pub fn active(&self) -> &ProviderConfig {
        match self.provider {
            Provider::Local => &self.local,
            Provider::Sakana => &self.sakana,
        }
    }

    pub fn active_mut(&mut self) -> &mut ProviderConfig {
        match self.provider {
            Provider::Local => &mut self.local,
            Provider::Sakana => &mut self.sakana,
        }
    }
}

impl Config {
    /// Load config with layering: defaults <- user <- project <- explicit path.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let mut cfg = Config::default();

        if let Some(user_path) = user_config_path()
            && let Some(loaded) = read_toml(&user_path)?
        {
            cfg = merge(cfg, loaded);
        }

        let project_path = std::env::current_dir()
            .ok()
            .map(|p| p.join(".lodan").join("config.toml"));
        if let Some(p) = project_path
            && let Some(loaded) = read_toml(&p)?
        {
            cfg = merge(cfg, loaded);
        }

        if let Some(p) = explicit {
            let loaded =
                read_toml(p)?.with_context(|| format!("config file not found: {}", p.display()))?;
            cfg = merge(cfg, loaded);
        }

        Ok(cfg)
    }

    /// CLI/env overrides. `base_url` / `model` / `api_key` act on the
    /// currently-active provider so users can flip provider once and tweak
    /// per-call without rewriting their config.
    pub fn apply_overrides(
        &mut self,
        provider: Option<Provider>,
        base_url: Option<String>,
        model: Option<String>,
        api_key: Option<String>,
        auto_approve: bool,
    ) {
        if let Some(p) = provider {
            self.llm.provider = p;
        }
        let active = self.llm.active_mut();
        if let Some(v) = base_url {
            active.base_url = v;
        }
        if let Some(v) = model {
            active.model = v;
        }
        if let Some(v) = api_key {
            active.api_key = v;
        }
        if auto_approve {
            self.agent.auto_approve = true;
        }
    }
}

fn user_config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "lodan").map(|d| d.config_dir().join("config.toml"))
}

fn read_toml(path: &Path) -> Result<Option<Config>> {
    if !path.exists() {
        return Ok(None);
    }
    let s = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config = toml::from_str(&s).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(cfg))
}

/// Shallow merge: `over` fields replace `base` fields (TOML defaults already populated).
fn merge(_base: Config, over: Config) -> Config {
    over
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_set_local_provider_and_endpoints() {
        let cfg = Config::default();
        assert_eq!(cfg.llm.provider, Provider::Local);
        assert_eq!(cfg.llm.local.base_url, "http://localhost:11434/v1");
        assert_eq!(cfg.llm.sakana.base_url, "https://api.sakana.ai/v1");
        assert_eq!(cfg.llm.sakana.model, "fugu");
        assert!(cfg.llm.sakana.api_key.is_empty());
    }

    #[test]
    fn active_follows_provider_switch() {
        let mut cfg = Config::default();
        assert_eq!(cfg.llm.active().base_url, "http://localhost:11434/v1");
        cfg.llm.provider = Provider::Sakana;
        assert_eq!(cfg.llm.active().base_url, "https://api.sakana.ai/v1");
    }

    #[test]
    fn overrides_target_active_provider_only() {
        let mut cfg = Config::default();
        cfg.apply_overrides(
            Some(Provider::Sakana),
            Some("https://example.test/v1".into()),
            Some("fugu-ultra".into()),
            Some("sk-test".into()),
            false,
        );
        assert_eq!(cfg.llm.sakana.base_url, "https://example.test/v1");
        assert_eq!(cfg.llm.sakana.model, "fugu-ultra");
        assert_eq!(cfg.llm.sakana.api_key, "sk-test");
        // local block is untouched
        assert_eq!(cfg.llm.local.base_url, "http://localhost:11434/v1");
        assert_eq!(cfg.llm.local.model, "qwen2.5-coder:7b");
        assert!(cfg.llm.local.api_key.is_empty());
    }

    #[test]
    fn auto_approve_flag_only_when_true() {
        let mut cfg = Config::default();
        cfg.apply_overrides(None, None, None, None, false);
        assert!(!cfg.agent.auto_approve);
        cfg.apply_overrides(None, None, None, None, true);
        assert!(cfg.agent.auto_approve);
    }
}
