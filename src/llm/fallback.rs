//! primary が一時的に使えないとき、別の provider へ投げ直すクライアント (#70)。
//!
//! 投げ直すのは `TransientLlmError` のときだけ — 再試行を使い切った接続エラー / 408 / 429 /
//! 5xx、本文を 1 文字も出していないストリーム断、タイムアウト。400 や 401 は別の provider に
//! 投げても直らない設定ミスなので対象外。本文を表示した後の断も対象外 (二重表示になる)。
//!
//! 一度 fallback が成功したら、そのプロセスの間は fallback を使い続ける。primary が落ちている
//! 間、呼び出しのたびに再試行とバックオフを払い直さないため。

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

use crate::agent::messages::{Message, ToolSpec};
use crate::llm::{ChatEvent, ChatResponse, LlmClient, is_transient};

pub struct FallbackClient {
    primary: Arc<dyn LlmClient>,
    fallback: Arc<dyn LlmClient>,
    /// fallback provider の名前 (通知用)。
    fallback_name: &'static str,
    /// 呼び出し側が渡す `model` は primary のもの。fallback には自分の設定のモデルを使う。
    fallback_model: String,
    switched: AtomicBool,
}

impl FallbackClient {
    pub fn new(
        primary: Arc<dyn LlmClient>,
        fallback: Arc<dyn LlmClient>,
        fallback_name: &'static str,
        fallback_model: String,
    ) -> Self {
        Self {
            primary,
            fallback,
            fallback_name,
            fallback_model,
            switched: AtomicBool::new(false),
        }
    }

    fn announce(&self, primary_error: &anyhow::Error) {
        tracing::warn!(
            "primary LLM provider is unavailable ({primary_error:#}); falling back to {} ({})",
            self.fallback_name,
            self.fallback_model
        );
        crate::runlog::record(
            "provider_fallback",
            serde_json::json!({
                "to": self.fallback_name,
                "model": self.fallback_model,
                "why": format!("{primary_error:#}"),
            }),
        );
    }

    /// fallback の結果を確定する。成功したら以後は fallback に固定し、失敗したら
    /// primary の失敗も残す (どちらも落ちていると分かるように)。
    fn settle<T>(&self, result: Result<T>, primary_error: anyhow::Error) -> Result<T> {
        match result {
            Ok(v) => {
                self.switched.store(true, Ordering::Relaxed);
                Ok(v)
            }
            Err(e) => Err(e.context(format!(
                "fallback provider {} also failed (primary: {primary_error:#})",
                self.fallback_name
            ))),
        }
    }
}

#[async_trait]
impl LlmClient for FallbackClient {
    async fn chat(
        &self,
        history: &[Message],
        tools: &[ToolSpec<'_>],
        model: &str,
        max_tokens: Option<u32>,
    ) -> Result<ChatResponse> {
        if self.switched.load(Ordering::Relaxed) {
            return self
                .fallback
                .chat(history, tools, &self.fallback_model, max_tokens)
                .await;
        }
        match self.primary.chat(history, tools, model, max_tokens).await {
            Err(e) if is_transient(&e) => {
                self.announce(&e);
                let result = self
                    .fallback
                    .chat(history, tools, &self.fallback_model, max_tokens)
                    .await;
                self.settle(result, e)
            }
            other => other,
        }
    }

    async fn chat_stream(
        &self,
        history: &[Message],
        tools: &[ToolSpec<'_>],
        model: &str,
        sink: mpsc::UnboundedSender<ChatEvent>,
    ) -> Result<()> {
        if self.switched.load(Ordering::Relaxed) {
            return self
                .fallback
                .chat_stream(history, tools, &self.fallback_model, sink)
                .await;
        }
        // 一時的な失敗は「sink に何も流していない」ことを含意するので、同じ sink を
        // fallback に渡しても利用者に二重の本文は見えない。
        match self
            .primary
            .chat_stream(history, tools, model, sink.clone())
            .await
        {
            Err(e) if is_transient(&e) => {
                self.announce(&e);
                let result = self
                    .fallback
                    .chat_stream(history, tools, &self.fallback_model, sink)
                    .await;
                self.settle(result, e)
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::TransientLlmError;
    use std::sync::Mutex;

    /// 台本どおりに成功 / 一時的な失敗 / 恒久的な失敗を返し、受け取った model を記録する。
    struct Scripted {
        outcomes: Mutex<Vec<Outcome>>,
        models: Mutex<Vec<String>>,
    }

    #[derive(Clone, Copy)]
    enum Outcome {
        Ok,
        Transient,
        Permanent,
    }

    impl Scripted {
        fn new(outcomes: &[Outcome]) -> Arc<Self> {
            Arc::new(Self {
                outcomes: Mutex::new(outcomes.to_vec()),
                models: Mutex::new(Vec::new()),
            })
        }

        fn next(&self, model: &str) -> Result<ChatResponse> {
            self.models.lock().unwrap().push(model.to_string());
            let mut outcomes = self.outcomes.lock().unwrap();
            let outcome = if outcomes.len() > 1 {
                outcomes.remove(0)
            } else {
                outcomes[0]
            };
            match outcome {
                Outcome::Ok => Ok(ChatResponse {
                    content: Some(format!("from {model}")),
                    tool_calls: Vec::new(),
                    usage: None,
                }),
                Outcome::Transient => Err(TransientLlmError("HTTP 503".into()).into()),
                Outcome::Permanent => Err(anyhow::anyhow!("LLM HTTP 401: bad key")),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.models.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LlmClient for Scripted {
        async fn chat(
            &self,
            _: &[Message],
            _: &[ToolSpec<'_>],
            model: &str,
            _: Option<u32>,
        ) -> Result<ChatResponse> {
            self.next(model)
        }

        async fn chat_stream(
            &self,
            _: &[Message],
            _: &[ToolSpec<'_>],
            model: &str,
            sink: mpsc::UnboundedSender<ChatEvent>,
        ) -> Result<()> {
            let resp = self.next(model)?;
            let _ = sink.send(ChatEvent::TextDelta(resp.content.clone().unwrap()));
            let _ = sink.send(ChatEvent::Done(resp));
            Ok(())
        }
    }

    fn client(primary: &Arc<Scripted>, fallback: &Arc<Scripted>) -> FallbackClient {
        FallbackClient::new(
            primary.clone(),
            fallback.clone(),
            "sakura",
            "fb-model".into(),
        )
    }

    #[tokio::test]
    async fn a_healthy_primary_never_touches_the_fallback() {
        let (p, f) = (Scripted::new(&[Outcome::Ok]), Scripted::new(&[Outcome::Ok]));
        let resp = client(&p, &f)
            .chat(&[], &[], "main-model", None)
            .await
            .unwrap();
        assert_eq!(resp.content.as_deref(), Some("from main-model"));
        assert!(f.calls().is_empty());
    }

    #[tokio::test]
    async fn a_transient_failure_falls_back_with_the_fallbacks_own_model_and_sticks() {
        let (p, f) = (
            Scripted::new(&[Outcome::Transient]),
            Scripted::new(&[Outcome::Ok]),
        );
        let c = client(&p, &f);
        let first = c.chat(&[], &[], "main-model", None).await.unwrap();
        assert_eq!(first.content.as_deref(), Some("from fb-model"));
        // 2 回目は primary を試さない (落ちている間、毎回バックオフを払わない)。
        c.chat(&[], &[], "main-model", None).await.unwrap();
        assert_eq!(p.calls(), ["main-model"]);
        assert_eq!(f.calls(), ["fb-model", "fb-model"]);
    }

    #[tokio::test]
    async fn a_permanent_failure_is_not_retried_elsewhere() {
        let (p, f) = (
            Scripted::new(&[Outcome::Permanent]),
            Scripted::new(&[Outcome::Ok]),
        );
        let err = client(&p, &f).chat(&[], &[], "m", None).await.unwrap_err();
        assert!(format!("{err:#}").contains("401"));
        assert!(
            f.calls().is_empty(),
            "a bad key is not fixed by another provider"
        );
    }

    #[tokio::test]
    async fn when_both_fail_the_error_names_both_and_primary_is_retried_next_time() {
        let (p, f) = (
            Scripted::new(&[Outcome::Transient]),
            Scripted::new(&[Outcome::Permanent]),
        );
        let c = client(&p, &f);
        let err = format!("{:#}", c.chat(&[], &[], "m", None).await.unwrap_err());
        assert!(
            err.contains("sakura also failed") && err.contains("503") && err.contains("401"),
            "{err}"
        );
        let _ = c.chat(&[], &[], "m", None).await;
        assert_eq!(
            p.calls().len(),
            2,
            "a failed fallback must not become sticky"
        );
    }

    #[tokio::test]
    async fn streaming_falls_back_without_duplicating_text() {
        let (p, f) = (
            Scripted::new(&[Outcome::Transient]),
            Scripted::new(&[Outcome::Ok]),
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        client(&p, &f)
            .chat_stream(&[], &[], "main-model", tx)
            .await
            .unwrap();
        let mut text = String::new();
        while let Some(ev) = rx.recv().await {
            if let ChatEvent::TextDelta(d) = ev {
                text.push_str(&d);
            }
        }
        assert_eq!(text, "from fb-model");
    }
}
