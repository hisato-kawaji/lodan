// Moonshot AI (Kimi) adapter.
// Moonshot speaks OpenAI-compatible Chat Completions, so we delegate to
// OpenAiClient and only own the bits that differ: API-key resolution
// (falls back to KIMI_API_KEY env) and a hard-fail when no key is set.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::agent::messages::{Message, ToolSpec};
use crate::config::ProviderConfig;
use crate::llm::openai::OpenAiClient;
use crate::llm::{ChatEvent, ChatResponse, LlmClient};

const API_KEY_ENV: &str = "KIMI_API_KEY";

/// K3 系は temperature が 1 固定で、それ以外を送ると 400 を返す (2026-09 実測)。
const FIXED_TEMPERATURE_MODEL_PREFIX: &str = "kimi-k3";
const FIXED_TEMPERATURE: f32 = 1.0;

pub struct KimiClient {
    inner: OpenAiClient,
}

impl KimiClient {
    pub fn new(cfg: &ProviderConfig) -> Result<Self> {
        let mut effective = cfg.clone();
        if effective.api_key.is_empty()
            && let Ok(k) = std::env::var(API_KEY_ENV)
        {
            effective.api_key = k;
        }
        if effective.api_key.is_empty() {
            return Err(anyhow!(
                "Kimi provider requires an API key (set [llm.kimi].api_key, --api-key, or {API_KEY_ENV})"
            ));
        }
        effective.temperature = effective_temperature(&effective.model, effective.temperature);
        Ok(Self {
            inner: OpenAiClient::new(&effective)?,
        })
    }
}

/// K3 に 1 以外の temperature が来たら送らない。`--temperature 0.2` は小型モデル向けの
/// 推奨値として他 provider と共用されるので (#61)、ここで落とさないと上流の素っ気ない
/// 400 でターンごと失敗する。
fn effective_temperature(model: &str, requested: Option<f32>) -> Option<f32> {
    match requested {
        Some(t) if model.starts_with(FIXED_TEMPERATURE_MODEL_PREFIX) && t != FIXED_TEMPERATURE => {
            tracing::warn!(
                "{model} only accepts temperature={FIXED_TEMPERATURE}; ignoring temperature={t}"
            );
            None
        }
        other => other,
    }
}

#[async_trait]
impl LlmClient for KimiClient {
    async fn chat(
        &self,
        history: &[Message],
        tools: &[ToolSpec<'_>],
        model: &str,
        max_tokens: Option<u32>,
    ) -> Result<ChatResponse> {
        self.inner.chat(history, tools, model, max_tokens).await
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
    fn k3_drops_any_temperature_other_than_one() {
        assert_eq!(effective_temperature("kimi-k3", Some(0.2)), None);
        assert_eq!(effective_temperature("kimi-k3", Some(1.0)), Some(1.0));
        assert_eq!(effective_temperature("kimi-k3", None), None);
    }

    #[test]
    fn other_kimi_models_keep_their_temperature() {
        assert_eq!(effective_temperature("kimi-k2.6", Some(0.2)), Some(0.2));
    }

    #[test]
    fn missing_api_key_is_rejected() {
        unsafe {
            std::env::remove_var(API_KEY_ENV);
        }
        let cfg = ProviderConfig::default_kimi();
        let err = match KimiClient::new(&cfg) {
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
        let mut cfg = ProviderConfig::default_kimi();
        cfg.api_key = "sk-test".into();
        assert!(KimiClient::new(&cfg).is_ok());
    }
}
