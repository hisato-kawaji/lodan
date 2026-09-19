use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::hooks::HookConfig;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    Local,
    Sakana,
    Sakura,
    Kimi,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Local => "local",
            Provider::Sakana => "sakana",
            Provider::Sakura => "sakura",
            Provider::Kimi => "kimi",
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

#[derive(Debug, Clone, Serialize)]
pub struct LlmConfig {
    pub provider: Provider,
    /// primary が一時的に使えないとき (再試行を使い切った 5xx / 429 / 接続エラーなど) に
    /// 投げ直す先。未設定なら fallback しない (#70)。
    pub fallback: Option<Provider>,
    pub local: ProviderConfig,
    pub sakana: ProviderConfig,
    pub sakura: ProviderConfig,
    pub kimi: ProviderConfig,
}

/// `[llm.*]` ブロックの部分指定。未指定フィールドは**そのプロバイダの**既定を残す。
/// `ProviderConfig` をそのまま `#[serde(default)]` で読むと全スロットが
/// `default_local()` にフォールバックし、`timeout_secs` だけ書いたブロックが
/// `base_url` をローカルへ巻き戻してしまう (リモート provider が黙って
/// localhost を叩く)。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct ProviderOverlay {
    base_url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    timeout_secs: Option<u64>,
    context_window: Option<u64>,
    temperature: Option<f32>,
    max_retries: Option<u32>,
    retry_base_ms: Option<u64>,
    stream_idle_timeout_secs: Option<u64>,
}

impl ProviderOverlay {
    fn apply(self, base: ProviderConfig) -> ProviderConfig {
        // 両側を分解して組み直す。`ProviderConfig` にフィールドを足して overlay 側を
        // 忘れると、黙って無視されるのではなくここでコンパイルが止まる。
        let Self {
            base_url,
            model,
            api_key,
            timeout_secs,
            context_window,
            temperature,
            max_retries,
            retry_base_ms,
            stream_idle_timeout_secs,
        } = self;
        ProviderConfig {
            base_url: base_url.unwrap_or(base.base_url),
            model: model.unwrap_or(base.model),
            api_key: api_key.unwrap_or(base.api_key),
            timeout_secs: timeout_secs.unwrap_or(base.timeout_secs),
            context_window: context_window.unwrap_or(base.context_window),
            temperature: temperature.or(base.temperature),
            max_retries: max_retries.unwrap_or(base.max_retries),
            retry_base_ms: retry_base_ms.unwrap_or(base.retry_base_ms),
            stream_idle_timeout_secs: stream_idle_timeout_secs
                .unwrap_or(base.stream_idle_timeout_secs),
        }
    }
}

impl<'de> Deserialize<'de> for LlmConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct Raw {
            provider: Provider,
            fallback: Option<Provider>,
            local: ProviderOverlay,
            sakana: ProviderOverlay,
            sakura: ProviderOverlay,
            kimi: ProviderOverlay,
        }

        let raw = Raw::deserialize(d)?;
        Ok(Self {
            provider: raw.provider,
            fallback: raw.fallback,
            local: raw.local.apply(ProviderConfig::default_local()),
            sakana: raw.sakana.apply(ProviderConfig::default_sakana()),
            sakura: raw.sakura.apply(ProviderConfig::default_sakura()),
            kimi: raw.kimi.apply(ProviderConfig::default_kimi()),
        })
    }
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
    /// 一時的な失敗 (接続エラー / 408 / 429 / 5xx / 出力前のストリーム断) を再試行する
    /// 最大回数。`0` で無効。400 や 401 のような恒久的な失敗は再試行しない (#70)。
    pub max_retries: u32,
    /// 再試行の待ち時間の起点 (ミリ秒)。試行ごとに倍になり、サーバの `Retry-After` が
    /// あればそちらを優先する。
    pub retry_base_ms: u64,
    /// ストリーミング中にこの秒数チャンクが来なければ断とみなす。`0` (既定) は無効で、
    /// その場合はリクエスト全体の `timeout_secs` だけが効く。
    pub stream_idle_timeout_secs: u64,
}

/// モデルに見せるツールの範囲 (#72)。ツール定義は毎リクエスト全量が送られるので、
/// 小型モデルでは固定費 (実測 1.8k-2.4k tok/呼び出し) がそのまま所要時間になる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ToolProfile {
    /// 登録された全ツール (built-in / Task / Skill / MCP)。
    #[default]
    Full,
    /// コーディングに最低限必要な 6 個: Read / Write / Edit / Bash / Grep / Glob。
    Core,
    /// 破壊的でないツールだけ。
    Readonly,
}

impl ToolProfile {
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolProfile::Full => "full",
            ToolProfile::Core => "core",
            ToolProfile::Readonly => "readonly",
        }
    }
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
    /// モデルに見せるツールの範囲。既定 `full`。
    pub tool_profile: ToolProfile,
    /// モデルに見せるツールの明示リスト。空でなければ `tool_profile` より優先する。
    pub tools: Vec<String>,
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
            fallback: None,
            local: ProviderConfig::default_local(),
            sakana: ProviderConfig::default_sakana(),
            sakura: ProviderConfig::default_sakura(),
            kimi: ProviderConfig::default_kimi(),
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

/// 一時的な LLM API 失敗の再試行回数の既定値。
pub const DEFAULT_MAX_RETRIES: u32 = 3;
/// 再試行待ちの起点 (ミリ秒) の既定値。500 → 1000 → 2000ms と伸びる。
pub const DEFAULT_RETRY_BASE_MS: u64 = 500;

/// Kimi の既定 timeout。reasoning_effort 既定 (max) の思考込みで 120 秒を超え得る。
const KIMI_TIMEOUT_SECS: u64 = 600;

impl ProviderConfig {
    pub fn default_local() -> Self {
        Self {
            base_url: "http://localhost:11434/v1".to_string(),
            model: "qwen2.5-coder:7b".to_string(),
            api_key: String::new(),
            timeout_secs: 120,
            context_window: DEFAULT_CONTEXT_WINDOW,
            temperature: None,
            max_retries: DEFAULT_MAX_RETRIES,
            retry_base_ms: DEFAULT_RETRY_BASE_MS,
            stream_idle_timeout_secs: 0,
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
            max_retries: DEFAULT_MAX_RETRIES,
            retry_base_ms: DEFAULT_RETRY_BASE_MS,
            stream_idle_timeout_secs: 0,
        }
    }

    /// さくらのAI Engine。OpenAI 互換で、既定は tool calling を確認済みの
    /// `gpt-oss-120b` (preview/* のモデルは予告なく入れ替わるため既定にしない)。
    pub fn default_sakura() -> Self {
        Self {
            base_url: "https://api.ai.sakura.ad.jp/v1".to_string(),
            model: "gpt-oss-120b".to_string(),
            api_key: String::new(),
            timeout_secs: 120,
            context_window: DEFAULT_CONTEXT_WINDOW,
            temperature: None,
            max_retries: DEFAULT_MAX_RETRIES,
            retry_base_ms: DEFAULT_RETRY_BASE_MS,
            stream_idle_timeout_secs: 0,
        }
    }

    /// Moonshot AI の Kimi。K3 は常に思考するため 1 応答が長く、timeout を広めに取る。
    /// `temperature` は 1 以外を送ると 400 になる (サーバ固定) ので既定どおり省略すること。
    pub fn default_kimi() -> Self {
        Self {
            base_url: "https://api.moonshot.ai/v1".to_string(),
            model: "kimi-k3".to_string(),
            api_key: String::new(),
            timeout_secs: KIMI_TIMEOUT_SECS,
            context_window: DEFAULT_CONTEXT_WINDOW,
            temperature: None,
            max_retries: DEFAULT_MAX_RETRIES,
            retry_base_ms: DEFAULT_RETRY_BASE_MS,
            stream_idle_timeout_secs: 0,
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
            tool_profile: ToolProfile::Full,
            tools: Vec::new(),
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
        self.get(self.provider)
    }

    pub fn get(&self, provider: Provider) -> &ProviderConfig {
        match provider {
            Provider::Local => &self.local,
            Provider::Sakana => &self.sakana,
            Provider::Sakura => &self.sakura,
            Provider::Kimi => &self.kimi,
        }
    }

    pub fn active_mut(&mut self) -> &mut ProviderConfig {
        match self.provider {
            Provider::Local => &mut self.local,
            Provider::Sakana => &mut self.sakana,
            Provider::Sakura => &mut self.sakura,
            Provider::Kimi => &mut self.kimi,
        }
    }
}

impl Config {
    /// Load config with layering: defaults <- user <- project <- explicit path.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        Ok(Self::load_with_origins(explicit)?.0)
    }

    /// `load` と同じ結果に、各キーがどのファイル由来かを添えて返す
    /// (`lodan config --show-origin` 用)。
    pub fn load_with_origins(explicit: Option<&Path>) -> Result<(Self, Origins)> {
        let mut layers = Vec::new();

        if let Some(user_path) = user_config_path()
            && let Some(table) = read_toml(&user_path)?
        {
            layers.push((user_path, table));
        }

        let project_path = std::env::current_dir()
            .ok()
            .map(|p| p.join(".lodan").join("config.toml"));
        if let Some(p) = project_path
            && let Some(table) = read_toml(&p)?
        {
            layers.push((p, table));
        }

        if let Some(p) = explicit {
            let table =
                read_toml(p)?.with_context(|| format!("config file not found: {}", p.display()))?;
            layers.push((p.to_path_buf(), table));
        }

        from_layers(layers)
    }

    /// CLI/env overrides. `base_url` / `model` / `api_key` / `temperature` act on
    /// the currently-active provider so users can flip provider once and tweak
    /// per-call without rewriting their config.
    pub fn apply_overrides(&mut self, o: Overrides) {
        self.apply_overrides_tracked(o, &mut Origins::new());
    }

    /// `apply_overrides` と同じ。上書きしたキーを `origins` にも反映する
    /// (`--show-origin` が、env で潰された値をファイル由来と表示しないように)。
    pub fn apply_overrides_tracked(&mut self, o: Overrides, origins: &mut Origins) {
        let mut mark = |key: String| {
            origins.insert(key, Origin::Override);
        };
        if let Some(p) = o.provider {
            self.llm.provider = p;
            mark("llm.provider".into());
        }
        if let Some(p) = o.fallback {
            self.llm.fallback = Some(p);
            mark("llm.fallback".into());
        }
        let provider = self.llm.provider.as_str();
        let active = self.llm.active_mut();
        if let Some(v) = o.base_url {
            active.base_url = v;
            mark(format!("llm.{provider}.base_url"));
        }
        if let Some(v) = o.model {
            active.model = v;
            mark(format!("llm.{provider}.model"));
        }
        if let Some(v) = o.api_key {
            active.api_key = v;
            mark(format!("llm.{provider}.api_key"));
        }
        if let Some(v) = o.temperature {
            active.temperature = Some(v);
            mark(format!("llm.{provider}.temperature"));
        }
        if o.auto_approve {
            self.agent.auto_approve = true;
            mark("agent.auto_approve".into());
        }
        if let Some(v) = o.finish_nudge {
            self.agent.finish_nudge = v;
            mark("agent.finish_nudge".into());
        }
        if let Some(v) = o.malformed_retry {
            self.agent.malformed_retry = v;
            mark("agent.malformed_retry".into());
        }
        if let Some(v) = o.dup_suppress {
            self.agent.dup_suppress = v;
            mark("agent.dup_suppress".into());
        }
        if let Some(v) = o.tool_profile {
            self.agent.tool_profile = v;
            mark("agent.tool_profile".into());
            // 明示リストはプロファイルより優先される。設定ファイルの `tools = [...]` を残すと、
            // 実行時に指定した `--tool-profile` が黙って無視される (ablation が空振りする)。
            // より具体的な指定元 (CLI / env) を勝たせる。同時に `--tools` があればそれが効く。
            if o.tools.is_none() && !self.agent.tools.is_empty() {
                self.agent.tools.clear();
                mark("agent.tools".into());
            }
        }
        if let Some(v) = o.tools {
            self.agent.tools = v;
            mark("agent.tools".into());
        }
    }
}

/// 設定ファイルより優先される実行時の上書き。`None` は「上書きしない」。
/// 真偽値を `Option<bool>` にしているのは、設定ファイルで有効にした緩和策を
/// 評価実行から明示的に切れるようにするため (ablation)。
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub provider: Option<Provider>,
    pub fallback: Option<Provider>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub temperature: Option<f32>,
    /// `true` のときだけ有効化する (既存 `--yes` の意味を保つ)。
    pub auto_approve: bool,
    pub finish_nudge: Option<bool>,
    pub malformed_retry: Option<bool>,
    pub dup_suppress: Option<bool>,
    pub tool_profile: Option<ToolProfile>,
    pub tools: Option<Vec<String>>,
}

fn user_config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "lodan").map(|d| d.config_dir().join("config.toml"))
}

/// キー (`llm.kimi.model` のようなドット区切り) → その値を最後に決めたもの。
/// どこからも設定されていないキーは既定値のままなので載らない。
pub type Origins = std::collections::BTreeMap<String, Origin>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    File(PathBuf),
    /// env か CLI フラグ (`apply_overrides`)。ファイルの値より後に効く。
    Override,
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Origin::File(p) => write!(f, "{}", p.display()),
            Origin::Override => f.write_str("env or CLI flag"),
        }
    }
}

/// レイヤー間で後勝ちにせず連結する配列キー。hook はユーザ設定とプロジェクト設定の
/// 両方を効かせたい (プロジェクト側に 1 つ足しただけでユーザの hook が消えるのは事故)。
const CONCAT_ARRAY_KEYS: &[&str] = &["hooks"];

fn read_toml(path: &Path) -> Result<Option<toml::Table>> {
    if !path.exists() {
        return Ok(None);
    }
    let s = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let table: toml::Table =
        toml::from_str(&s).with_context(|| format!("parsing {}", path.display()))?;
    // 型の誤りは合成後だとどのファイルのせいか分からなくなるので、ここで単体検証する。
    let _: Config = toml::Value::Table(table.clone())
        .try_into()
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(table))
}

/// レイヤー (優先度の低い順) をフィールド単位で重ねてから 1 回だけ `Config` にする。
/// 先に `Config` へ落としてから重ねると、書かれなかったフィールドが既定値で埋まって
/// 「未指定」と「既定値を明示」の区別が消え、後段のファイルが前段を丸ごと潰す。
fn from_layers(layers: Vec<(PathBuf, toml::Table)>) -> Result<(Config, Origins)> {
    let mut merged = toml::Table::new();
    let mut origins = Origins::new();
    for (path, table) in layers {
        merge_table(&mut merged, table, "", &path, &mut origins);
    }
    let cfg: Config = toml::Value::Table(merged)
        .try_into()
        .context("merging config layers")?;
    Ok((cfg, origins))
}

/// テーブルは再帰、`CONCAT_ARRAY_KEYS` は連結、それ以外の値は `over` で置き換える。
///
/// 前提: 各レイヤーは `read_toml` で単体でも `Config` として検証済み。だから既知のキーで
/// レイヤー間の型が食い違う (スカラーの上にテーブル、など) ことは無く、部分木を丸ごと
/// 置き換える腕で古い `origins` を掃除する必要も無い。この検証を緩めるなら、ここも見直すこと。
fn merge_table(
    base: &mut toml::Table,
    over: toml::Table,
    prefix: &str,
    origin: &Path,
    origins: &mut Origins,
) {
    use toml::Value;
    for (key, value) in over {
        let full = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        let concat = prefix.is_empty() && CONCAT_ARRAY_KEYS.contains(&key.as_str());
        match (base.get_mut(&key), value) {
            (Some(Value::Table(b)), Value::Table(o)) => merge_table(b, o, &full, origin, origins),
            (existing, Value::Array(o)) if concat => {
                // 連結される配列は要素ごとに由来が違うので `hooks[0]` の形で残す。
                let start = match &existing {
                    Some(Value::Array(b)) => b.len(),
                    _ => 0,
                };
                for i in 0..o.len() {
                    origins.insert(
                        format!("{full}[{}]", start + i),
                        Origin::File(origin.to_path_buf()),
                    );
                }
                match existing {
                    Some(Value::Array(b)) => b.extend(o),
                    _ => {
                        base.insert(key, Value::Array(o));
                    }
                }
            }
            (_, Value::Table(o)) => {
                // base 側が無い (か型違い) テーブル。葉ごとに由来を残すため空から重ねる。
                let mut fresh = toml::Table::new();
                merge_table(&mut fresh, o, &full, origin, origins);
                base.insert(key, Value::Table(fresh));
            }
            (_, v) => {
                base.insert(key, v);
                origins.insert(full, Origin::File(origin.to_path_buf()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(name: &str, toml_src: &str) -> (PathBuf, toml::Table) {
        (PathBuf::from(name), toml::from_str(toml_src).unwrap())
    }

    #[test]
    fn later_layer_keeps_fields_it_does_not_mention() {
        let user = layer(
            "user.toml",
            r#"
            [llm]
            provider = "kimi"
            [llm.kimi]
            model = "kimi-k2.6"
            [agent]
            finish_nudge = true
            "#,
        );
        let project = layer(
            "project.toml",
            r#"
            [agent]
            max_iterations = 40
            "#,
        );
        let (cfg, origins) = from_layers(vec![user, project]).unwrap();

        assert_eq!(cfg.llm.provider, Provider::Kimi);
        assert_eq!(cfg.llm.kimi.model, "kimi-k2.6");
        assert!(cfg.agent.finish_nudge);
        assert_eq!(cfg.agent.max_iterations, 40);
        // 書かれなかったフィールドはそのプロバイダの既定のまま。
        assert_eq!(
            cfg.llm.kimi.base_url,
            ProviderConfig::default_kimi().base_url
        );

        assert_eq!(
            origins["llm.kimi.model"],
            Origin::File(PathBuf::from("user.toml"))
        );
        assert_eq!(
            origins["agent.max_iterations"],
            Origin::File(PathBuf::from("project.toml"))
        );
        assert!(!origins.contains_key("llm.kimi.base_url"));
    }

    #[test]
    fn overrides_take_over_the_origin_of_keys_they_replace() {
        let (mut cfg, mut origins) = from_layers(vec![layer(
            "user.toml",
            "[llm]\nprovider = \"kimi\"\n[llm.kimi]\nmodel = \"kimi-k2.6\"\napi_key = \"k\"\n",
        )])
        .unwrap();
        cfg.apply_overrides_tracked(
            Overrides {
                model: Some("kimi-k3".into()),
                dup_suppress: Some(false),
                ..Default::default()
            },
            &mut origins,
        );
        assert_eq!(cfg.llm.kimi.model, "kimi-k3");
        assert_eq!(origins["llm.kimi.model"], Origin::Override);
        assert_eq!(origins["agent.dup_suppress"], Origin::Override);
        // 触っていないキーはファイル由来のまま。
        assert_eq!(
            origins["llm.kimi.api_key"],
            Origin::File(PathBuf::from("user.toml"))
        );
    }

    #[test]
    fn tool_profile_and_explicit_tools_parse_and_override() {
        let (mut cfg, _) = from_layers(vec![layer(
            "user.toml",
            "[agent]\ntool_profile = \"core\"\ntools = [\"Read\", \"Grep\"]\n",
        )])
        .unwrap();
        assert_eq!(cfg.agent.tool_profile, ToolProfile::Core);
        assert_eq!(cfg.agent.tools, ["Read", "Grep"]);

        // 実行時のプロファイル指定は、設定ファイルの明示リストに負けない。
        let mut by_profile = cfg.clone();
        let mut origins = Origins::new();
        by_profile.apply_overrides_tracked(
            Overrides {
                tool_profile: Some(ToolProfile::Readonly),
                ..Default::default()
            },
            &mut origins,
        );
        assert_eq!(by_profile.agent.tool_profile, ToolProfile::Readonly);
        assert!(
            by_profile.agent.tools.is_empty(),
            "a stale list would silently win"
        );
        assert_eq!(origins["agent.tools"], Origin::Override);

        // 両方指定されたら、実行時の明示リストが効く。
        cfg.apply_overrides(Overrides {
            tool_profile: Some(ToolProfile::Core),
            tools: Some(vec!["Glob".into()]),
            ..Default::default()
        });
        assert_eq!(cfg.agent.tools, ["Glob"]);
        assert_eq!(Config::default().agent.tool_profile, ToolProfile::Full);
    }

    #[test]
    fn scalars_are_last_writer_wins() {
        let (cfg, origins) = from_layers(vec![
            layer("user.toml", "[agent]\nmax_iterations = 10\n"),
            layer("project.toml", "[agent]\nmax_iterations = 40\n"),
            layer("explicit.toml", "[agent]\nmax_iterations = 5\n"),
        ])
        .unwrap();
        assert_eq!(cfg.agent.max_iterations, 5);
        assert_eq!(
            origins["agent.max_iterations"],
            Origin::File(PathBuf::from("explicit.toml"))
        );
    }

    #[test]
    fn hooks_concatenate_across_layers_in_layer_order() {
        let user = layer(
            "user.toml",
            r#"
            [[hooks]]
            event = "Stop"
            command = "user-stop"
            "#,
        );
        let project = layer(
            "project.toml",
            r#"
            [[hooks]]
            event = "PreToolUse"
            matcher = "Bash"
            command = "project-pre"
            "#,
        );
        let (cfg, origins) = from_layers(vec![user, project]).unwrap();
        let commands: Vec<&str> = cfg.hooks.iter().map(|h| h.command.as_str()).collect();
        assert_eq!(commands, ["user-stop", "project-pre"]);
        assert_eq!(
            origins["hooks[0]"],
            Origin::File(PathBuf::from("user.toml"))
        );
        assert_eq!(
            origins["hooks[1]"],
            Origin::File(PathBuf::from("project.toml"))
        );
    }

    #[test]
    fn no_layers_is_all_defaults() {
        let (cfg, origins) = from_layers(Vec::new()).unwrap();
        assert_eq!(cfg.llm.provider, Provider::Local);
        assert_eq!(
            cfg.agent.max_iterations,
            AgentConfig::default().max_iterations
        );
        assert!(origins.is_empty());
    }

    #[test]
    fn type_error_names_the_offending_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "[agent]\nmax_iterations = \"many\"\n").unwrap();
        let err = format!("{:#}", read_toml(&path).unwrap_err());
        assert!(err.contains("bad.toml"), "{err}");
    }

    #[test]
    fn fallback_parses_round_trips_and_is_overridable() {
        assert_eq!(Config::default().llm.fallback, None);
        let (mut cfg, origins) = from_layers(vec![layer(
            "user.toml",
            "[llm]\nprovider = \"kimi\"\nfallback = \"sakura\"\n",
        )])
        .unwrap();
        assert_eq!(cfg.llm.fallback, Some(Provider::Sakura));
        assert_eq!(
            origins["llm.fallback"],
            Origin::File(PathBuf::from("user.toml"))
        );

        // `lodan config` の出力を貼り戻しても fallback が残る (None のときはキーごと出ない)。
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.llm.fallback, Some(Provider::Sakura));
        assert!(
            !toml::to_string_pretty(&Config::default())
                .unwrap()
                .contains("fallback")
        );

        let mut origins = Origins::new();
        cfg.apply_overrides_tracked(
            Overrides {
                fallback: Some(Provider::Sakana),
                ..Default::default()
            },
            &mut origins,
        );
        assert_eq!(cfg.llm.fallback, Some(Provider::Sakana));
        assert_eq!(origins["llm.fallback"], Origin::Override);
    }

    #[test]
    fn config_output_round_trips() {
        // `lodan config` の出力をそのまま config.toml に貼れること。
        let mut cfg = Config::default();
        cfg.llm.provider = Provider::Kimi;
        cfg.llm.sakura.temperature = Some(0.2);
        cfg.llm.kimi.model = "kimi-k2.6".into();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(toml::to_string_pretty(&back).unwrap(), text);
    }

    #[test]
    fn defaults_set_local_provider_and_endpoints() {
        let cfg = Config::default();
        assert_eq!(cfg.llm.provider, Provider::Local);
        assert_eq!(cfg.llm.local.base_url, "http://localhost:11434/v1");
        assert_eq!(cfg.llm.sakana.base_url, "https://api.sakana.ai/v1");
        assert_eq!(cfg.llm.sakana.model, "fugu");
        assert!(cfg.llm.sakana.api_key.is_empty());
        assert_eq!(cfg.llm.sakura.base_url, "https://api.ai.sakura.ad.jp/v1");
        assert_eq!(cfg.llm.sakura.model, "gpt-oss-120b");
        assert!(cfg.llm.sakura.api_key.is_empty());
        assert_eq!(cfg.llm.local.context_window, DEFAULT_CONTEXT_WINDOW);
        assert_eq!(cfg.llm.sakana.context_window, DEFAULT_CONTEXT_WINDOW);
        assert_eq!(cfg.llm.sakura.context_window, DEFAULT_CONTEXT_WINDOW);
    }

    #[test]
    fn active_follows_provider_switch() {
        let mut cfg = Config::default();
        assert_eq!(cfg.llm.active().base_url, "http://localhost:11434/v1");
        cfg.llm.provider = Provider::Sakana;
        assert_eq!(cfg.llm.active().base_url, "https://api.sakana.ai/v1");
        cfg.llm.provider = Provider::Sakura;
        assert_eq!(cfg.llm.active().base_url, "https://api.ai.sakura.ad.jp/v1");
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
        // so is sakura
        assert_eq!(cfg.llm.sakura.base_url, "https://api.ai.sakura.ad.jp/v1");
        assert!(cfg.llm.sakura.api_key.is_empty());
    }

    #[test]
    fn partial_provider_block_keeps_that_providers_defaults() {
        // ラダーのハーネスが書く形。timeout だけ上書きして他は各既定を保つ。
        let cfg: Config = toml::from_str(
            "[llm.local]\ntimeout_secs = 900\n\n[llm.sakana]\ntimeout_secs = 900\n\n[llm.sakura]\ntimeout_secs = 900\n\n[llm.kimi]\ntimeout_secs = 900\n",
        )
        .expect("partial provider blocks should parse");

        assert_eq!(cfg.llm.local.base_url, "http://localhost:11434/v1");
        assert_eq!(cfg.llm.sakana.base_url, "https://api.sakana.ai/v1");
        assert_eq!(cfg.llm.sakana.model, "fugu");
        assert_eq!(cfg.llm.sakura.base_url, "https://api.ai.sakura.ad.jp/v1");
        assert_eq!(cfg.llm.sakura.model, "gpt-oss-120b");
        assert_eq!(cfg.llm.kimi.base_url, "https://api.moonshot.ai/v1");
        assert_eq!(cfg.llm.kimi.model, "kimi-k3");
        for p in [
            &cfg.llm.local,
            &cfg.llm.sakana,
            &cfg.llm.sakura,
            &cfg.llm.kimi,
        ] {
            assert_eq!(p.timeout_secs, 900);
            assert_eq!(p.context_window, DEFAULT_CONTEXT_WINDOW);
        }
    }

    #[test]
    fn empty_config_falls_back_to_all_defaults() {
        let cfg: Config = toml::from_str("").expect("empty config should parse");
        assert_eq!(cfg.llm.provider, Provider::Local);
        assert_eq!(cfg.llm.sakura.base_url, "https://api.ai.sakura.ad.jp/v1");
        assert_eq!(cfg.llm.sakana.base_url, "https://api.sakana.ai/v1");
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
