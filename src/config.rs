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
    /// モデルのコンテキスト窓 (トークン)。自動圧縮のしきい値計算に使う。
    /// `0` で自動圧縮を無効化。サービング側の実効窓 (例: ollama の `num_ctx`)
    /// と合わせること。
    pub context_window: u64,
    /// サンプリング温度。None (既定) はリクエストに含めずサーバ既定に従う。
    /// 小型ローカルモデルはツールコール整形が崩れやすいため 0.1-0.2 を推奨 (#61)。
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub max_iterations: usize,
    pub auto_approve: bool,
    /// ターン終了直前に 1 回だけ自己検証を促す (#63)。小型ローカルモデルの
    /// 「計画だけ述べて実行しない」「要件の実装漏れ」対策。既定 false
    /// (良行儀なモデルに余計な LLM ラウンドトリップを課さない)。
    pub finish_nudge: bool,
    /// テキストとして漏れたツールコールを検知して正しい形式での再発行を求める
    /// (#61)。既定 true。無効化できるのは ablation で寄与を測るため。
    pub malformed_retry: bool,
    /// 直前と同一の read-only 呼び出しを実行せず別の行動を促す (#61)。
    /// 既定 true。無効化できるのは ablation で寄与を測るため。
    pub dup_suppress: bool,
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

/// context_window の既定値。qwen2.5-coder 系の 32k を採用 (モデルに合わせて要調整)。
pub const DEFAULT_CONTEXT_WINDOW: u64 = 32_768;

impl ProviderConfig {
    pub fn default_local() -> Self {
        Self {
            base_url: "http://localhost:11434/v1".to_string(),
            model: "qwen2.5-coder:7b".to_string(),
            api_key: String::new(),
            timeout_secs: 120,
            context_window: DEFAULT_CONTEXT_WINDOW,
            temperature: None,
        }
    }

    pub fn default_sakana() -> Self {
        Self {
            base_url: "https://api.sakana.ai/v1".to_string(),
            model: "fugu".to_string(),
            api_key: String::new(),
            timeout_secs: 120,
            context_window: DEFAULT_CONTEXT_WINDOW,
            temperature: None,
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 25,
            auto_approve: false,
            finish_nudge: false,
            malformed_retry: true,
            dup_suppress: true,
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

    /// CLI/env overrides. `base_url` / `model` / `api_key` / `temperature` act on
    /// the currently-active provider so users can flip provider once and tweak
    /// per-call without rewriting their config.
    pub fn apply_overrides(&mut self, o: Overrides) {
        if let Some(p) = o.provider {
            self.llm.provider = p;
        }
        let active = self.llm.active_mut();
        if let Some(v) = o.base_url {
            active.base_url = v;
        }
        if let Some(v) = o.model {
            active.model = v;
        }
        if let Some(v) = o.api_key {
            active.api_key = v;
        }
        if let Some(v) = o.temperature {
            active.temperature = Some(v);
        }
        if o.auto_approve {
            self.agent.auto_approve = true;
        }
        if let Some(v) = o.finish_nudge {
            self.agent.finish_nudge = v;
        }
        if let Some(v) = o.malformed_retry {
            self.agent.malformed_retry = v;
        }
        if let Some(v) = o.dup_suppress {
            self.agent.dup_suppress = v;
        }
    }
}

/// 設定ファイルより優先される実行時の上書き。`None` は「上書きしない」。
/// 真偽値を `Option<bool>` にしているのは、設定ファイルで有効にした緩和策を
/// 評価実行から明示的に切れるようにするため (ablation)。
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub provider: Option<Provider>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub temperature: Option<f32>,
    /// `true` のときだけ有効化する (既存 `--yes` の意味を保つ)。
    pub auto_approve: bool,
    pub finish_nudge: Option<bool>,
    pub malformed_retry: Option<bool>,
    pub dup_suppress: Option<bool>,
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
        assert_eq!(cfg.llm.local.context_window, DEFAULT_CONTEXT_WINDOW);
        assert_eq!(cfg.llm.sakana.context_window, DEFAULT_CONTEXT_WINDOW);
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
        cfg.apply_overrides(Overrides {
            provider: Some(Provider::Sakana),
            base_url: Some("https://example.test/v1".into()),
            model: Some("fugu-ultra".into()),
            api_key: Some("sk-test".into()),
            temperature: Some(0.2),
            ..Default::default()
        });
        assert_eq!(cfg.llm.sakana.base_url, "https://example.test/v1");
        assert_eq!(cfg.llm.sakana.model, "fugu-ultra");
        assert_eq!(cfg.llm.sakana.api_key, "sk-test");
        assert_eq!(cfg.llm.sakana.temperature, Some(0.2));
        // local block is untouched
        assert_eq!(cfg.llm.local.base_url, "http://localhost:11434/v1");
        assert_eq!(cfg.llm.local.model, "qwen2.5-coder:7b");
        assert!(cfg.llm.local.api_key.is_empty());
        assert_eq!(cfg.llm.local.temperature, None);
    }

    #[test]
    fn auto_approve_flag_only_when_true() {
        let mut cfg = Config::default();
        cfg.apply_overrides(Overrides::default());
        assert!(!cfg.agent.auto_approve);
        cfg.apply_overrides(Overrides {
            auto_approve: true,
            ..Default::default()
        });
        assert!(cfg.agent.auto_approve);
    }

    #[test]
    fn mitigations_default_on_and_are_overridable_both_ways() {
        let mut cfg = Config::default();
        assert!(cfg.agent.malformed_retry);
        assert!(cfg.agent.dup_suppress);
        assert!(!cfg.agent.finish_nudge);

        // 指定なしの上書きは既定を変えない。
        cfg.apply_overrides(Overrides::default());
        assert!(cfg.agent.malformed_retry);
        assert!(cfg.agent.dup_suppress);

        // ablation で明示的に切れる / 入れられる。
        cfg.apply_overrides(Overrides {
            malformed_retry: Some(false),
            dup_suppress: Some(false),
            finish_nudge: Some(true),
            ..Default::default()
        });
        assert!(!cfg.agent.malformed_retry);
        assert!(!cfg.agent.dup_suppress);
        assert!(cfg.agent.finish_nudge);
    }
}
