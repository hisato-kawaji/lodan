pub mod openai;
pub mod sakana;

use anyhow::Result;
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

pub fn build_client(cfg: &Config) -> Result<Arc<dyn LlmClient>> {
    match cfg.llm.provider {
        Provider::Local => Ok(Arc::new(openai::OpenAiClient::new(&cfg.llm.local)?)),
        Provider::Sakana => Ok(Arc::new(sakana::SakanaClient::new(&cfg.llm.sakana)?)),
    }
}
