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
    pub permissions: PermissionsConfig,
    pub sandbox: crate::sandbox::SandboxConfig,
    #[serde(default)]
    pub hooks: Vec<HookConfig>,
    /// 無効にする hook の `id`。レイヤー間で連結される (プロジェクト側からユーザ設定の hook を
    /// 名指しで外せる。逆も同じ)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_hooks: Vec<String>,
    #[serde(skip_serializing_if = "GoalConfig::is_unset")]
    pub goal: GoalConfig,
    /// モデルごとの単価 (`[pricing."<model>"]`)。書いたモデルだけ `/cost` に金額が出る。
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub pricing: std::collections::BTreeMap<String, crate::llm::metered::ModelPrice>,
    /// hook の終了コードの解釈。`"v1"` で「非 0 は全てブロック」の旧挙動に戻す。
    pub hooks_compat: crate::hooks::HooksCompat,
}

/// `[goal]`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GoalConfig {
    /// `/goal` の達成判定に使う provider。未設定なら作業しているモデルが自分で判定する。
    /// 別のモデルにすると「自分の仕事を自分で合格にする」偏りを避けられる (#84)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evaluator_provider: Option<Provider>,
    /// 判定に使うモデル。未設定ならその provider の `model`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evaluator_model: Option<String>,
}

impl GoalConfig {
    fn is_unset(&self) -> bool {
        self.evaluator_provider.is_none() && self.evaluator_model.is_none()
    }
}

/// 承認が要る呼び出しをどう扱うか (#74)。deny ルールはどのモードでも効く。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// 破壊的ツールは尋ねる。
    #[default]
    Default,
    /// ファイル編集 (Write / Edit / MultiEdit / NotebookEdit) は尋ねずに通す。Bash などは尋ねる。
    AcceptEdits,
    /// plan モードで開始する (承認の扱いは default と同じ)。
    Plan,
    /// 尋ねない。尋ねるはずだった呼び出しは拒否する (無人実行向け。allow ルールで通すものを決める)。
    DontAsk,
    /// 尋ねずに全て通す (`--yes` と同じ)。
    Bypass,
}

impl PermissionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "accept-edits",
            PermissionMode::Plan => "plan",
            PermissionMode::DontAsk => "dont-ask",
            PermissionMode::Bypass => "bypass",
        }
    }
}

/// `[permissions]`。ルールの構文は `src/permission_rules.rs`。
/// `allow` / `deny` / `ask` は設定ファイルのレイヤー間で**連結**される (hooks と同じ) —
/// プロジェクト設定がユーザ設定の deny を消せてはいけない。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsConfig {
    pub mode: PermissionMode,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub ask: Vec<String>,
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
    reasoning_effort: Option<String>,
    plan_reasoning_effort: Option<String>,
    reasoning_roundtrip: Option<bool>,
    extra_body: Option<serde_json::Map<String, serde_json::Value>>,
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
            reasoning_effort,
            plan_reasoning_effort,
            reasoning_roundtrip,
            extra_body,
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
            reasoning_effort: reasoning_effort.or(base.reasoning_effort),
            plan_reasoning_effort: plan_reasoning_effort.or(base.plan_reasoning_effort),
            reasoning_roundtrip: reasoning_roundtrip.unwrap_or(base.reasoning_roundtrip),
            // レイヤー間の重ね合わせは `merge_table` がキー単位で済ませている (他のテーブルと同じ)。
            // 組み込みの既定値は常に空なので、ここは「書かれていればそれ」で足りる。
            extra_body: extra_body.unwrap_or_default(),
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
    /// 推論の深さ。リクエストの `reasoning_effort` にそのまま入れる (`low` / `medium` / `high`、
    /// サーバによっては `none` / `max` / `minimal`)。None (既定) は送らずサーバ既定に従う。
    /// 値は検査しない — 受け付ける語彙はサーバとモデルごとに違う (#78)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// プランモード (`/plan`) の間だけ使う推論の深さ。未設定なら `reasoning_effort` のまま。
    /// 調査と計画にだけ深い推論を使い、実行は軽く回す、という使い分けのため。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_reasoning_effort: Option<String>,
    /// ツール往復の間、モデルの思考過程 (`reasoning_content`) を送り返す。既定 true。
    /// 送り返された思考を拒否するサーバでは false にする。ターンを跨いだ思考はどちらでも送らない。
    pub reasoning_roundtrip: bool,
    /// リクエスト body にそのまま足すサーバ固有のパラメータ
    /// (例: vLLM / llama.cpp の `chat_template_kwargs = { enable_thinking = false }`)。
    /// lodan 自身が組み立てるキー (`model` / `messages` / `tools` …) は書けない。
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra_body: serde_json::Map<String, serde_json::Value>,
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
    /// このプロセスが送る LLM リクエスト数の上限 (サブエージェントや `/goal` の評価器も含む)。
    /// 使い切ったら次のリクエストを送らずにターンを打ち切る。None (既定) は無制限 (#84)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests: Option<u64>,
    /// 同じく合計トークン数の上限。判定は各リクエストの前なので、超過は最後の 1 回ぶんまで。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_tokens: Option<u64>,
    /// 自動圧縮を発火するコンテキスト使用率 (`context_window` に対する %)。既定 80。
    /// 小さい窓のモデルでは、1 回のツール出力で残りを使い切る前に畳めるよう下げるとよい。
    pub auto_compact_percent: u8,
    /// モデルの思考過程 (`reasoning_content`) を画面に全文流す。既定は畳んで長さだけ示す (#78)。
    pub show_reasoning: bool,
    pub auto_approve: bool,
    /// 本文もツール呼び出しも無い応答 (典型は thinking モデルの「思考だけ」) を、1 回だけ「答えを
    /// 書け」と促して続ける (#111)。既定 true。思考の有無は問わない (空の応答は答えではない)。
    pub empty_reply_nudge: bool,
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
    /// 同じ応答の中で連続する並列可能なツール呼び出し (Read / Grep / Glob / WebFetch /
    /// WebSearch / Task) を同時に実行する (#73)。既定 true。無効化できるのは ablation 用。
    pub parallel_tools: bool,
    /// モデルに見せるツールの範囲。既定 `full`。
    pub tool_profile: ToolProfile,
    /// モデルに見せるツールの明示リスト。空でなければ `tool_profile` より優先する。
    pub tools: Vec<String>,
    /// system prompt の末尾に足す指示 (`--append-system-prompt`)。メモリより後ろに置かれる。
    /// サブエージェントには渡さない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append_system_prompt: Option<String>,
    /// プロファイルで隠したツールと MCP ツールを、定義を送らずに `ToolSearch` で読み込ませる (#72)。
    /// 既定 false (`core` はこれまでどおり 6 個だけ、`full` は全部をそのまま送る)。
    pub tool_search: bool,
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

/// 自動圧縮を発火する既定のコンテキスト使用率 (%)。
pub const DEFAULT_AUTO_COMPACT_PERCENT: u8 = 80;

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
            reasoning_effort: None,
            plan_reasoning_effort: None,
            reasoning_roundtrip: true,
            extra_body: serde_json::Map::new(),
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
            reasoning_effort: None,
            plan_reasoning_effort: None,
            reasoning_roundtrip: true,
            extra_body: serde_json::Map::new(),
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
            reasoning_effort: None,
            plan_reasoning_effort: None,
            reasoning_roundtrip: true,
            extra_body: serde_json::Map::new(),
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
            reasoning_effort: None,
            plan_reasoning_effort: None,
            reasoning_roundtrip: true,
            extra_body: serde_json::Map::new(),
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
            max_requests: None,
            max_total_tokens: None,
            auto_compact_percent: DEFAULT_AUTO_COMPACT_PERCENT,
            show_reasoning: false,
            auto_approve: false,
            finish_nudge: false,
            empty_reply_nudge: true,
            malformed_retry: true,
            dup_suppress: true,
            parallel_tools: true,
            tool_profile: ToolProfile::Full,
            tools: Vec::new(),
            append_system_prompt: None,
            tool_search: false,
        }
    }
}

impl Default for BashConfig {
    fn default() -> Self {
        Self { timeout_secs: 30 }
    }
}

/// `lodan config` が秘密の値の代わりに出す文字列。
pub const REDACTED: &str = "***";

impl Config {
    /// 画面に出してよい形の写し (#88)。`lodan config` の出力は画面共有や issue にそのまま貼られる。
    ///
    /// - `api_key`: 空でなければ伏せる
    /// - `extra_body`: キーは残し、値を全て伏せる (何が秘密かはキー名からは分からない)
    /// - `base_url`: `https://user:pass@host/…` の資格情報とクエリ文字列を伏せる
    pub fn redacted(&self) -> Config {
        let mut shown = self.clone();
        let llm = &mut shown.llm;
        for provider in [
            &mut llm.local,
            &mut llm.sakana,
            &mut llm.sakura,
            &mut llm.kimi,
        ] {
            if !provider.api_key.is_empty() {
                provider.api_key = REDACTED.to_string();
            }
            for value in provider.extra_body.values_mut() {
                *value = serde_json::Value::String(REDACTED.to_string());
            }
            provider.base_url = redact_url(&provider.base_url);
        }
        shown
    }
}

/// URL に埋め込まれた資格情報 (userinfo)・クエリ文字列・フラグメントを伏せる。
///
/// URL パーサには頼らず文字列として処理する。`lodan config` を見るのは設定が壊れているときで、
/// ポート番号の打ち間違いのような「URL として読めない値」こそ貼り付けられる。パーサが読めなかったら
/// そのまま出す、という作りでは、一番漏れやすい場面で漏れる。秘密を含まない値は一字も変えない。
fn redact_url(url: &str) -> String {
    // 後ろから順に切り分ける: `#fragment`、`?query`、残りが scheme://authority/path。
    let (rest, fragment) = match url.split_once('#') {
        Some((rest, _)) => (rest, true),
        None => (url, false),
    };
    let (rest, query) = match rest.split_once('?') {
        Some((rest, _)) => (rest, true),
        None => (rest, false),
    };
    // authority は `://` の後ろから最初の `/` まで。`://` が無ければ先頭から。
    let authority_start = rest.find("://").map_or(0, |at| at + 3);
    let authority_end = rest[authority_start..]
        .find('/')
        .map_or(rest.len(), |at| authority_start + at);
    // パスワードに生の `/` が入っていると、authority を最初の `/` で切った時点で `@` を見失う。
    // そういう値は URL として無効なので、無効な値に限って「最後の `@` まで」を userinfo とみなす
    // (有効な URL では、パスの中の `@` に触らない性質を保つ)。
    let authority_end = match rest[authority_end..].rfind('@') {
        Some(at)
            if !rest[authority_start..authority_end].contains('@')
                && reqwest::Url::parse(url).is_err() =>
        {
            let after = authority_end + at + 1;
            rest[after..]
                .find('/')
                .map_or(rest.len(), |slash| after + slash)
        }
        _ => authority_end,
    };
    let authority = &rest[authority_start..authority_end];
    // userinfo は最後の `@` まで (パスワードに `@` が入っていても取りこぼさない)。
    let host = authority.rfind('@').map(|at| &authority[at + 1..]);

    if host.is_none() && !query && !fragment {
        return url.to_string();
    }
    let mut out = String::with_capacity(url.len());
    out.push_str(&rest[..authority_start]);
    match host {
        Some(host) => {
            out.push_str(REDACTED);
            out.push('@');
            out.push_str(host);
        }
        None => out.push_str(authority),
    }
    out.push_str(&rest[authority_end..]);
    if query {
        out.push('?');
        out.push_str(REDACTED);
    }
    if fragment {
        out.push('#');
        out.push_str(REDACTED);
    }
    out
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

        // プロジェクトが持ち込む設定は、信頼済みのディレクトリでだけ読む (#75)。
        let project_trusted = crate::trust::project_trusted();
        let project_path = std::env::current_dir()
            .ok()
            .map(|p| p.join(".lodan").join("config.toml"));
        if project_trusted
            && let Some(p) = project_path
            && let Some(table) = read_toml(&p)?
        {
            layers.push((p, table));
        }

        // 個人用のプロジェクト設定 (コミットしない)。承認プロンプトの「このプロジェクトで常に
        // 許可」がここへ書く。共有の config.toml を汚さないために分けてある。
        let local_path = std::env::current_dir()
            .ok()
            .map(|p| p.join(LOCAL_CONFIG_DIR).join(LOCAL_CONFIG_FILE));
        if project_trusted
            && let Some(p) = local_path
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
        if let Some(v) = o.reasoning_effort {
            active.reasoning_effort = Some(v);
            mark(format!("llm.{provider}.reasoning_effort"));
        }
        if o.auto_approve {
            self.agent.auto_approve = true;
            mark("agent.auto_approve".into());
        }
        if let Some(v) = o.finish_nudge {
            self.agent.finish_nudge = v;
            mark("agent.finish_nudge".into());
        }
        if let Some(v) = o.empty_reply_nudge {
            self.agent.empty_reply_nudge = v;
            mark("agent.empty_reply_nudge".into());
        }
        if let Some(v) = o.malformed_retry {
            self.agent.malformed_retry = v;
            mark("agent.malformed_retry".into());
        }
        if let Some(v) = o.dup_suppress {
            self.agent.dup_suppress = v;
            mark("agent.dup_suppress".into());
        }
        if let Some(v) = o.sandbox {
            self.sandbox.mode = v;
            mark("sandbox.mode".into());
        }
        if let Some(v) = o.sandbox_network {
            self.sandbox.network = v;
            mark("sandbox.network".into());
        }
        if let Some(v) = o.permission_mode {
            self.permissions.mode = v;
            mark("permissions.mode".into());
        }
        // 実行時の指定は設定ファイルのルールに足す (置き換えない: deny を消せてはいけない)。
        if !o.allowed_tools.is_empty() {
            self.permissions.allow.extend(o.allowed_tools);
            mark("permissions.allow".into());
        }
        if !o.disallowed_tools.is_empty() {
            self.permissions.deny.extend(o.disallowed_tools);
            mark("permissions.deny".into());
        }
        if let Some(v) = o.show_reasoning {
            self.agent.show_reasoning = v;
            mark("agent.show_reasoning".into());
        }
        if let Some(v) = o.parallel_tools {
            self.agent.parallel_tools = v;
            mark("agent.parallel_tools".into());
        }
        if let Some(v) = o.max_requests {
            self.agent.max_requests = Some(v);
            mark("agent.max_requests".into());
        }
        if let Some(v) = o.max_total_tokens {
            self.agent.max_total_tokens = Some(v);
            mark("agent.max_total_tokens".into());
        }
        if let Some(v) = o.max_iterations {
            self.agent.max_iterations = v;
            mark("agent.max_iterations".into());
        }
        if let Some(v) = o.append_system_prompt {
            self.agent.append_system_prompt = Some(v);
            mark("agent.append_system_prompt".into());
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
        if let Some(v) = o.tool_search {
            self.agent.tool_search = v;
            mark("agent.tool_search".into());
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
    pub reasoning_effort: Option<String>,
    /// `true` のときだけ有効化する (既存 `--yes` の意味を保つ)。
    pub auto_approve: bool,
    pub finish_nudge: Option<bool>,
    pub empty_reply_nudge: Option<bool>,
    pub malformed_retry: Option<bool>,
    pub dup_suppress: Option<bool>,
    pub parallel_tools: Option<bool>,
    pub show_reasoning: Option<bool>,
    pub sandbox: Option<crate::sandbox::SandboxMode>,
    pub sandbox_network: Option<bool>,
    pub max_requests: Option<u64>,
    pub max_total_tokens: Option<u64>,
    /// `--max-turns`: `agent.max_iterations` を上書きする。
    pub max_iterations: Option<usize>,
    pub append_system_prompt: Option<String>,
    pub permission_mode: Option<PermissionMode>,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub tool_profile: Option<ToolProfile>,
    pub tools: Option<Vec<String>>,
    pub tool_search: Option<bool>,
}

fn user_config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "lodan").map(|d| d.config_dir().join("config.toml"))
}

pub const LOCAL_CONFIG_DIR: &str = ".lodan";
pub const LOCAL_CONFIG_FILE: &str = "config.local.toml";

/// `<cwd>/.lodan/config.local.toml` の `[permissions] allow` に `rule` を足す (既にあれば何もしない)。
/// ファイルは lodan が管理するもので、書き戻しでコメントは失われる。
pub fn append_local_allow_rule(cwd: &Path, rule: &str) -> Result<PathBuf> {
    let dir = cwd.join(LOCAL_CONFIG_DIR);
    let path = dir.join(LOCAL_CONFIG_FILE);
    // symlink の先を書き換えない。リポジトリに仕込まれた `.lodan/config.local.toml -> ~/.config/…`
    // や `.lodan -> /somewhere` を辿ると、承認 1 回で別の設定ファイルを上書きしてしまう。
    for p in [&dir, &path] {
        if std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) {
            anyhow::bail!("{} is a symlink; refusing to write through it", p.display());
        }
    }
    let mut table: toml::Table = match std::fs::read_to_string(&path) {
        Ok(text) => parse_toml_table(&path, &text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let permissions = table
        .entry("permissions")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .with_context(|| format!("{}: `permissions` is not a table", path.display()))?;
    let allow = permissions
        .entry("allow")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .with_context(|| format!("{}: `permissions.allow` is not an array", path.display()))?;
    if !allow.iter().any(|v| v.as_str() == Some(rule)) {
        allow.push(toml::Value::String(rule.to_string()));
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let body = toml::to_string_pretty(&table).context("serializing local config")?;
    // 途中で落ちても既存の設定を壊さないよう、別名で書いてから置き換える。
    let tmp = dir.join(format!(".{LOCAL_CONFIG_FILE}.{}.tmp", std::process::id()));
    std::fs::write(
        &tmp,
        format!("# lodan が管理する個人用のプロジェクト設定。コミットしないこと (.gitignore に追加)。\n{body}"),
    )
    .with_context(|| format!("writing {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("replacing {}", path.display()));
    }
    Ok(path)
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
const CONCAT_ARRAY_KEYS: &[&str] = &[
    "hooks",
    "disabled_hooks",
    "permissions.allow",
    "permissions.deny",
    "permissions.ask",
];

fn read_toml(path: &Path) -> Result<Option<toml::Table>> {
    if !path.exists() {
        return Ok(None);
    }
    let s = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let table = parse_toml_table(path, &s)?;
    // 型の誤りは合成後だとどのファイルのせいか分からなくなるので、ここで単体検証する。
    // 文字列から読み直すのは、位置 (行・列) を得るため。
    let _: Config = toml::from_str(&s).map_err(|e| {
        anyhow::anyhow!(
            "parsing {}: {}",
            path.display(),
            describe_toml_error(&e, &s)
        )
    })?;
    Ok(Some(table))
}

/// 設定ファイルの TOML を読む。構文エラーは「行・列・メッセージ」だけにする — `toml` クレートの
/// 表示は該当行をそのまま引用するので、`api_key = "sk-…` の閉じ忘れで値が画面に出る (#114)。
fn parse_toml_table(path: &Path, text: &str) -> Result<toml::Table> {
    toml::from_str(text).map_err(|e| {
        anyhow::anyhow!(
            "parsing {}: {}",
            path.display(),
            describe_toml_error(&e, text)
        )
    })
}

/// `toml::de::Error` を、ファイルの中身を引用せずに言い直す。メッセージ中の引用文字列
/// (型エラーの `invalid type: string "sk-…"`) も伏せる。
fn describe_toml_error(e: &toml::de::Error, text: &str) -> String {
    let message = redact_quoted(e.message());
    match e.span() {
        Some(span) => {
            let before = &text[..span.start.min(text.len())];
            let line = before.matches('\n').count() + 1;
            let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
            format!("TOML error at line {line}, column {column}: {message}")
        }
        None => message,
    }
}

/// 二重引用符の中身を `…` にする。
fn redact_quoted(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut inside = false;
    for c in message.chars() {
        if c == '"' {
            inside = !inside;
            out.push(c);
            if inside {
                out.push('…');
            }
        } else if !inside {
            out.push(c);
        }
    }
    out
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
        let concat = CONCAT_ARRAY_KEYS.contains(&full.as_str());
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

    /// 構文エラーの表示に、壊れた行の中身 (値) が出ない (#114)。
    #[test]
    fn a_toml_syntax_error_names_the_position_but_not_the_broken_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[llm.local]\napi_key = \"sk-BROKEN-SECRET\n").unwrap();
        let err = format!("{:#}", read_toml(&path).unwrap_err());
        assert!(!err.contains("BROKEN"), "{err}");
        assert!(
            err.contains("config.toml") && err.contains("line 2, column"),
            "{err}"
        );

        // 型エラーも、メッセージに引用される値を伏せる。
        std::fs::write(&path, "[agent]\nmax_iterations = \"many-SECRET\"\n").unwrap();
        let err = format!("{:#}", read_toml(&path).unwrap_err());
        assert!(!err.contains("SECRET"), "{err}");
        assert!(
            err.contains("config.toml") && err.contains("expected usize"),
            "{err}"
        );
    }

    #[test]
    fn redact_quoted_hides_every_quoted_segment() {
        assert_eq!(
            redact_quoted(r#"invalid type: string "sk-x", expected usize"#),
            r#"invalid type: string "…", expected usize"#
        );
        assert_eq!(redact_quoted("no quotes"), "no quotes");
        assert_eq!(redact_quoted(r#"unterminated "abc"#), r#"unterminated "…"#);
    }

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
    fn permission_rules_concatenate_across_layers_and_runtime_rules_are_appended() {
        let user = layer(
            "user.toml",
            "[permissions]\nmode = \"accept-edits\"\ndeny = [\"Read(**/.env)\"]\nallow = [\"Bash(git status)\"]\n",
        );
        let project = layer(
            "project.toml",
            "[permissions]\ndeny = [\"Bash(git push *)\"]\nallow = [\"Bash(cargo *)\"]\n",
        );
        let (mut cfg, origins) = from_layers(vec![user, project]).unwrap();
        assert_eq!(cfg.permissions.mode, PermissionMode::AcceptEdits);
        // プロジェクト設定はユーザ設定の deny を消せない。
        assert_eq!(cfg.permissions.deny, ["Read(**/.env)", "Bash(git push *)"]);
        assert_eq!(cfg.permissions.allow, ["Bash(git status)", "Bash(cargo *)"]);
        assert_eq!(
            origins["permissions.deny[0]"],
            Origin::File(PathBuf::from("user.toml"))
        );
        assert_eq!(
            origins["permissions.deny[1]"],
            Origin::File(PathBuf::from("project.toml"))
        );

        cfg.apply_overrides(Overrides {
            permission_mode: Some(PermissionMode::DontAsk),
            disallowed_tools: vec!["WebFetch".into()],
            ..Default::default()
        });
        assert_eq!(cfg.permissions.mode, PermissionMode::DontAsk);
        assert_eq!(
            cfg.permissions.deny.len(),
            3,
            "runtime rules add to the file's, never replace"
        );
    }

    #[test]
    fn appending_a_local_allow_rule_is_idempotent_and_keeps_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join(LOCAL_CONFIG_DIR).join(LOCAL_CONFIG_FILE);
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(
            &local,
            "[agent]\nmax_iterations = 40\n\n[permissions]\ndeny = [\"Read(.env)\"]\n",
        )
        .unwrap();

        append_local_allow_rule(dir.path(), "Bash(git status)").unwrap();
        append_local_allow_rule(dir.path(), "Bash(git status)").unwrap();
        append_local_allow_rule(dir.path(), "Edit").unwrap();

        let cfg: Config = toml::from_str(&std::fs::read_to_string(&local).unwrap()).unwrap();
        assert_eq!(cfg.permissions.allow, ["Bash(git status)", "Edit"]);
        assert_eq!(
            cfg.permissions.deny,
            ["Read(.env)"],
            "existing rules survive the rewrite"
        );
        assert_eq!(cfg.agent.max_iterations, 40);
    }

    #[cfg(unix)]
    #[test]
    fn the_local_config_is_never_written_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim.toml");
        std::fs::write(&victim, "[agent]\nmax_iterations = 7\n").unwrap();
        let cwd = dir.path().join("repo");
        std::fs::create_dir_all(cwd.join(LOCAL_CONFIG_DIR)).unwrap();
        std::os::unix::fs::symlink(&victim, cwd.join(LOCAL_CONFIG_DIR).join(LOCAL_CONFIG_FILE))
            .unwrap();

        let err = append_local_allow_rule(&cwd, "Edit").unwrap_err();
        assert!(format!("{err:#}").contains("symlink"), "{err:#}");
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "[agent]\nmax_iterations = 7\n"
        );

        // `.lodan` 自体が symlink でも同じ。
        let cwd2 = dir.path().join("repo2");
        std::fs::create_dir_all(&cwd2).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, cwd2.join(LOCAL_CONFIG_DIR)).unwrap();
        assert!(append_local_allow_rule(&cwd2, "Edit").is_err());
        assert!(!elsewhere.join(LOCAL_CONFIG_FILE).exists());
    }

    #[test]
    fn a_broken_local_config_is_an_error_not_an_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join(LOCAL_CONFIG_DIR).join(LOCAL_CONFIG_FILE);
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, "this is not toml [").unwrap();
        assert!(append_local_allow_rule(dir.path(), "Edit").is_err());
        assert_eq!(
            std::fs::read_to_string(&local).unwrap(),
            "this is not toml ["
        );
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
    fn hooks_compat_defaults_to_v2_and_can_be_set_back() {
        assert_eq!(
            Config::default().hooks_compat,
            crate::hooks::HooksCompat::V2
        );
        let cfg: Config = toml::from_str(
            r#"
            hooks_compat = "v1"
            [[hooks]]
            event = "PreToolUse"
            command = "guard.sh"
            timeout_secs = 5
            "#,
        )
        .unwrap();
        assert_eq!(cfg.hooks_compat, crate::hooks::HooksCompat::V1);
        assert_eq!(cfg.hooks[0].timeout_secs, Some(5));
    }

    /// #87 からの持ち越し: 連結される hook を、上のレイヤーから名指しで外せる。
    #[test]
    fn a_later_layer_can_disable_or_replace_a_named_hook_from_an_earlier_one() {
        let user = layer(
            "user.toml",
            r#"
            [[hooks]]
            id = "notify"
            event = "Stop"
            command = "user-notify"

            [[hooks]]
            id = "lint"
            event = "PostToolUse"
            command = "user-lint"
            "#,
        );
        let project = layer(
            "project.toml",
            r#"
            disabled_hooks = ["notify"]

            [[hooks]]
            id = "lint"
            event = "PostToolUse"
            command = "project-lint"
            "#,
        );
        let (cfg, _) = from_layers(vec![user, project]).unwrap();
        assert_eq!(cfg.hooks.len(), 3, "the file layers still concatenate");
        let active: Vec<String> = crate::hooks::effective(&cfg.hooks, &cfg.disabled_hooks)
            .into_iter()
            .map(|h| h.command)
            .collect();
        assert_eq!(active, ["project-lint"]);
    }

    #[test]
    fn reasoning_settings_parse_layer_and_stay_out_of_the_dump_when_unset() {
        let cfg: Config = toml::from_str(
            r#"
            [llm.local]
            reasoning_effort = "low"
            [llm.local.extra_body]
            top_k = 20
            chat_template_kwargs = { enable_thinking = false }
            "#,
        )
        .unwrap();
        assert_eq!(cfg.llm.local.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(
            cfg.llm.local.extra_body["chat_template_kwargs"]["enable_thinking"],
            false
        );
        // 未設定のプロバイダは、`lodan config` の出力にもキーを出さない。
        let dumped = toml::to_string(&Config::default()).unwrap();
        assert!(!dumped.contains("reasoning_effort") && !dumped.contains("extra_body"));

        let mut cfg = Config::default();
        cfg.apply_overrides(Overrides {
            reasoning_effort: Some("high".into()),
            ..Default::default()
        });
        assert_eq!(cfg.llm.active().reasoning_effort.as_deref(), Some("high"));
    }

    /// `extra_body` も他のテーブルと同じく、レイヤー間ではキー単位で重なる。
    #[test]
    fn extra_body_merges_per_key_across_layers() {
        let user = layer(
            "user.toml",
            "[llm.local.extra_body]\ntop_k = 20\nmin_p = 0.1\n",
        );
        let project = layer(
            "project.toml",
            "[llm.local.extra_body]\ntop_k = 40\nchat_template_kwargs = { enable_thinking = false }\n",
        );
        let (cfg, origins) = from_layers(vec![user, project]).unwrap();
        let extra = &cfg.llm.local.extra_body;
        assert_eq!(extra["top_k"], 40, "the later layer wins per key");
        assert_eq!(extra["min_p"], 0.1, "keys it does not mention survive");
        assert_eq!(extra["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(
            origins["llm.local.extra_body.top_k"],
            Origin::File(PathBuf::from("project.toml"))
        );
    }

    #[test]
    fn the_goal_evaluator_can_be_pointed_at_another_provider() {
        let cfg: Config = toml::from_str(
            "[goal]\nevaluator_provider = \"kimi\"\nevaluator_model = \"kimi-k3\"\n",
        )
        .unwrap();
        assert_eq!(cfg.goal.evaluator_provider, Some(Provider::Kimi));
        assert_eq!(cfg.goal.evaluator_model.as_deref(), Some("kimi-k3"));
        // 未設定なら `lodan config` にも出ない。
        assert!(
            !toml::to_string(&Config::default())
                .unwrap()
                .contains("goal")
        );
    }

    #[test]
    fn pricing_and_the_auto_compact_threshold_parse_and_stay_out_of_the_dump_when_unset() {
        let cfg: Config = toml::from_str(
            r#"
            [agent]
            auto_compact_percent = 60
            [pricing."kimi-k3"]
            input_per_mtok = 0.6
            output_per_mtok = 2.5
            "#,
        )
        .unwrap();
        assert_eq!(cfg.agent.auto_compact_percent, 60);
        assert_eq!(cfg.pricing["kimi-k3"].output_per_mtok, 2.5);
        assert_eq!(Config::default().agent.auto_compact_percent, 80);
        assert!(
            !toml::to_string(&Config::default())
                .unwrap()
                .contains("pricing")
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

    /// `lodan config` の既定の出力には、秘密の値が 1 つも入らない (#88)。
    #[test]
    fn the_redacted_config_shows_no_secret_values_but_keeps_the_shape() {
        let mut cfg = Config::default();
        cfg.llm.sakura.api_key = "sk-SAKURA-SECRET".into();
        cfg.llm.kimi.base_url = "https://alice:hunter2@gateway.example/v1?token=URLSECRET".into();
        cfg.llm.local.extra_body.insert(
            "auth".into(),
            serde_json::json!({ "bearer": "BODYSECRET", "n": 1 }),
        );
        let shown = toml::to_string_pretty(&cfg.redacted()).unwrap();
        for secret in [
            "sk-SAKURA-SECRET",
            "hunter2",
            "alice",
            "URLSECRET",
            "BODYSECRET",
        ] {
            assert!(!shown.contains(secret), "{secret} leaked:\n{shown}");
        }
        // どこに何が設定されているかは分かる。
        assert!(shown.contains("api_key = \"***\""), "{shown}");
        assert!(shown.contains("auth = \"***\""), "{shown}");
        assert!(shown.contains("gateway.example/v1"), "{shown}");

        // URL として読めない値でも、`file:` のような資格情報を持てないはずのスキームでも、伏せる
        // (レビューで、ポート番号を打ち間違えた URL のパスワードがそのまま出た)。
        for (given, shown) in [
            (
                "https://u:FLAGPASS@gw.example:99999/v1",
                "https://***@gw.example:99999/v1",
            ),
            ("https://alice:hunter2@", "https://***@"),
            ("file://alice:hunter2@host/v1", "file://***@host/v1"),
            ("https://a:p@ss@gw.example/v1", "https://***@gw.example/v1"),
            ("alice:hunter2@gw.example/v1", "***@gw.example/v1"),
            (
                "https://a:b@gw.example/v1?t=Q#sk-FRAG-SECRET",
                "https://***@gw.example/v1?***#***",
            ),
            (
                "http://localhost:11434/v1#tok",
                "http://localhost:11434/v1#***",
            ),
            // 秘密を含まない値は一字も変えない (パスの中の `@` は userinfo ではない)。
            ("http://localhost:11434/v1/", "http://localhost:11434/v1/"),
            (
                "https://gw.example/v1/users/me@example.com",
                "https://gw.example/v1/users/me@example.com",
            ),
            // パスワードに生の `/` (base64 など)、スキーム無しの `//`。どちらも URL としては無効。
            (
                "https://user:aB3/xY9+Qw==@gw.example/v1",
                "https://***@gw.example/v1",
            ),
            // (スキームが無いので先頭の `//` ごと userinfo 扱いになる。無効な値なので形は問わない)
            ("//user:hunter2@gw.example/v1", "***@gw.example/v1"),
            ("not a url", "not a url"),
            ("", ""),
        ] {
            assert_eq!(redact_url(given), shown, "{given}");
        }

        // 設定していないキーは空のまま (「設定してある」と見せかけない)。秘密の無い URL は一字も変えない。
        let plain = Config::default();
        let plain_shown = plain.redacted();
        assert_eq!(plain_shown.llm.local.api_key, "");
        assert_eq!(plain_shown.llm.local.base_url, plain.llm.local.base_url);
        // 伏せるのは写しだけ。
        assert_eq!(cfg.llm.sakura.api_key, "sk-SAKURA-SECRET");
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
