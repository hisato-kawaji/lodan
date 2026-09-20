pub mod fallback;
pub mod kimi;
pub mod metered;
pub mod openai;
pub mod sakana;
pub mod sakura;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::agent::messages::{Message, ToolCall, ToolSpec};
use crate::config::{Config, Provider};

/// 1 応答分のトークン使用量 (OpenAI 互換 `usage` オブジェクト)。
/// `total_tokens` を返さないサーバがあるため `normalized` で補完してから使う。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

impl Usage {
    /// `total_tokens` 欠落 (0) 時に prompt + completion で補う。
    pub fn normalized(mut self) -> Self {
        if self.total_tokens == 0 {
            self.total_tokens = self.prompt_tokens + self.completion_tokens;
        }
        self
    }
}

#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    /// サーバが usage を返さない場合は `None` (呼び出し側で概算フォールバック)。
    pub usage: Option<Usage>,
}

/// Streaming events emitted by `chat_stream`.
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// Incremental assistant text.
    TextDelta(String),
    /// Final assembled response (sent once at the end).
    Done(ChatResponse),
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    /// `max_tokens` で 1 応答の生成上限を渡せる (`None` はモデル既定)。MCP sampling は
    /// 外部サーバ由来の上限をここで適用して無制限生成を防ぐ。
    async fn chat(
        &self,
        history: &[Message],
        tools: &[ToolSpec<'_>],
        model: &str,
        max_tokens: Option<u32>,
    ) -> Result<ChatResponse>;

    async fn chat_stream(
        &self,
        history: &[Message],
        tools: &[ToolSpec<'_>],
        model: &str,
        sink: mpsc::UnboundedSender<ChatEvent>,
    ) -> Result<()>;
}

/// 時間を置くか、別の provider に投げれば通り得る失敗。再試行を使い切った接続エラー /
/// 408 / 429 / 5xx、本文を出す前のストリーム断、タイムアウトがこれで返る。
/// **利用者にまだ何も見せていない**ことを含意する (fallback が同じ sink を使い回せる根拠)。
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct TransientLlmError(pub String);

/// `e` が (context を重ねた後でも) 一時的な失敗か。
pub fn is_transient(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|c| c.downcast_ref::<TransientLlmError>().is_some())
}

/// 設定どおりのクライアントと、その使用量の台帳。台帳は provider ごとのクライアントの**内側**で
/// 共有するので、fallback に切り替わって送り直したリクエストも 1 件ずつ数えられる。
pub fn build_metered(cfg: &Config) -> Result<(Arc<dyn LlmClient>, Arc<metered::Ledger>)> {
    let ledger = Arc::new(
        metered::Ledger::new(metered::Budget {
            max_requests: cfg.agent.max_requests,
            max_total_tokens: cfg.agent.max_total_tokens,
        })
        .with_pricing(cfg.pricing.clone()),
    );
    let client = build_with(cfg, &|inner| {
        Arc::new(metered::MeteredClient::new(inner, ledger.clone())) as Arc<dyn LlmClient>
    })?;
    Ok((client, ledger))
}

/// `/goal` の評価器を別のモデルにする設定があれば、そのクライアントとモデル名。同じ台帳に載せるので、
/// 評価器の呼び出しも `/cost` と予算に入る。設定が無ければ None (作業側のクライアントを使う)。
pub fn build_goal_evaluator(
    cfg: &Config,
    ledger: &Arc<metered::Ledger>,
) -> Result<Option<(Arc<dyn LlmClient>, String)>> {
    let goal = &cfg.goal;
    if goal.evaluator_provider.is_none() && goal.evaluator_model.is_none() {
        return Ok(None);
    }
    let provider = goal.evaluator_provider.unwrap_or(cfg.llm.provider);
    let inner = build_for(provider, cfg)
        .with_context(|| format!("building the /goal evaluator ({})", provider.as_str()))?;
    let model = goal
        .evaluator_model
        .clone()
        .unwrap_or_else(|| cfg.llm.get(provider).model.clone());
    let client: Arc<dyn LlmClient> = Arc::new(metered::MeteredClient::new(inner, ledger.clone()));
    Ok(Some((client, model)))
}

/// 計上なしのクライアント。**本体では使わない** (使用量も予算も効かなくなる) — `build_metered` を使うこと。
/// 台帳を持たないテスト用に残してある。
pub fn build_client(cfg: &Config) -> Result<Arc<dyn LlmClient>> {
    build_with(cfg, &|inner| inner)
}

/// `wrap` は provider ごとのクライアント 1 つずつに掛かる。
fn build_with(
    cfg: &Config,
    wrap: &dyn Fn(Arc<dyn LlmClient>) -> Arc<dyn LlmClient>,
) -> Result<Arc<dyn LlmClient>> {
    let primary = wrap(build_for(cfg.llm.provider, cfg)?);
    let Some(fallback) = cfg.llm.fallback else {
        return Ok(primary);
    };
    if fallback == cfg.llm.provider {
        tracing::warn!(
            "llm.fallback is the same as llm.provider ({}); ignoring it",
            fallback.as_str()
        );
        return Ok(primary);
    }
    // fallback は保険。キーが無いなどで組めなくても、primary での実行は止めない。
    match build_for(fallback, cfg) {
        Ok(client) => Ok(Arc::new(fallback::FallbackClient::new(
            primary,
            wrap(client),
            fallback.as_str(),
            cfg.llm.get(fallback).model.clone(),
        ))),
        Err(e) => {
            tracing::warn!(
                "llm.fallback = {} is unusable, continuing without it: {e:#}",
                fallback.as_str()
            );
            Ok(primary)
        }
    }
}

fn build_for(provider: Provider, cfg: &Config) -> Result<Arc<dyn LlmClient>> {
    Ok(match provider {
        Provider::Local => Arc::new(openai::OpenAiClient::new(&cfg.llm.local)?),
        Provider::Sakana => Arc::new(sakana::SakanaClient::new(&cfg.llm.sakana)?),
        Provider::Sakura => Arc::new(sakura::SakuraClient::new(&cfg.llm.sakura)?),
        Provider::Kimi => Arc::new(kimi::KimiClient::new(&cfg.llm.kimi)?),
    })
}
