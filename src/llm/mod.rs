pub mod openai;

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::agent::messages::{Message, ToolCall, ToolSpec};
use crate::config::Config;

#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
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
    async fn chat(
        &self,
        history: &[Message],
        tools: &[ToolSpec<'_>],
        model: &str,
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
    let client = openai::OpenAiClient::new(&cfg.llm)?;
    Ok(Arc::new(client))
}
