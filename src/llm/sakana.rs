// Sakana AI (Fugu) adapter.
// Sakana speaks OpenAI-compatible Chat Completions, so we delegate to
// OpenAiClient and only own the bits that differ: API-key resolution
// (falls back to SAKANA_API_KEY env) and a hard-fail when no key is set.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::agent::messages::{Message, ToolSpec};
use crate::config::ProviderConfig;
use crate::llm::openai::OpenAiClient;
use crate::llm::{ChatEvent, ChatResponse, LlmClient};

const API_KEY_ENV: &str = "SAKANA_API_KEY";

pub struct SakanaClient {
    inner: OpenAiClient,
}

impl SakanaClient {
    pub fn new(cfg: &ProviderConfig) -> Result<Self> {
        let mut effective = cfg.clone();
        if effective.api_key.is_empty()
            && let Ok(k) = std::env::var(API_KEY_ENV)
        {
            effective.api_key = k;
        }
        if effective.api_key.is_empty() {
            return Err(anyhow!(
                "Sakana provider requires an API key (set [llm.sakana].api_key, --api-key, or {API_KEY_ENV})"
            ));
        }
        Ok(Self {
            inner: OpenAiClient::new(&effective)?,
        })
    }
}

#[async_trait]
impl LlmClient for SakanaClient {
    async fn chat(
        &self,
        history: &[Message],
        tools: &[ToolSpec<'_>],
        model: &str,
    ) -> Result<ChatResponse> {
        self.inner.chat(history, tools, model).await
    }

    async fn chat_stream(
        &self,
        history: &[Message],
        tools: &[ToolSpec<'_>],
        model: &str,
        sink: mpsc::UnboundedSender<ChatEvent>,
    ) -> Result<()> {
        self.inner.chat_stream(history, tools, model, sink).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_api_key_is_rejected() {
        unsafe {
            std::env::remove_var(API_KEY_ENV);
        }
        let cfg = ProviderConfig::default_sakana();
        let err = match SakanaClient::new(&cfg) {
            Ok(_) => panic!("expected build error when no API key is configured"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("API key"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn explicit_config_key_allows_build() {
        unsafe {
            std::env::remove_var(API_KEY_ENV);
        }
        let mut cfg = ProviderConfig::default_sakana();
        cfg.api_key = "sk-test".into();
        assert!(SakanaClient::new(&cfg).is_ok());
    }
}
