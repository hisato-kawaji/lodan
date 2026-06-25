// MCP sampling: サーバが `sampling/createMessage` で client 側の LLM 補完を要求する。
// 信頼するサーバのみ opt-in する (config の allowSampling)。ここでは MCP の
// sampling params を内部の `Message` 履歴へ変換し、`LlmClient::chat` を呼んで
// 結果を MCP result 形へ包む。
// (server→client リクエストは stdio transport のみ対応)

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::agent::messages::Message;
use crate::llm::LlmClient;
use crate::mcp::protocol::{CreateMessageParams, CreateMessageResult};

/// opt-in したサーバの sampling 要求を 1 つの LLM へ橋渡しする。
#[derive(Clone)]
pub struct SamplingProvider {
    llm: Arc<dyn LlmClient>,
    model: String,
}

impl SamplingProvider {
    pub fn new(llm: Arc<dyn LlmClient>, model: String) -> Self {
        Self { llm, model }
    }

    /// `sampling/createMessage` の params (JSON) を処理し、result JSON を返す。
    /// パース失敗や LLM エラーは `Err` として返し、呼び出し側で JSON-RPC error 化する。
    pub async fn create_message(&self, params: Value) -> Result<Value> {
        let params: CreateMessageParams = serde_json::from_value(params)?;
        let history = Self::to_history(&params);
        // サーバ指定の maxTokens を生成上限として渡し、無制限生成を防ぐ。
        let resp = self
            .llm
            .chat(&history, &[], &self.model, params.max_tokens)
            .await?;
        let text = resp.content.unwrap_or_default();
        let result = CreateMessageResult::assistant_text(text, self.model.clone());
        Ok(serde_json::to_value(result)?)
    }

    /// sampling params を内部 Message 履歴へ変換する。
    /// systemPrompt があれば先頭の System に、各メッセージは role に応じて
    /// User / Assistant に写す。非テキストブロックは捨てる。
    fn to_history(params: &CreateMessageParams) -> Vec<Message> {
        let mut history = Vec::new();
        if let Some(sys) = &params.system_prompt
            && !sys.is_empty()
        {
            history.push(Message::System {
                content: sys.clone(),
            });
        }
        for msg in &params.messages {
            let Some(text) = msg.text() else { continue };
            let content = text.to_string();
            if msg.role == "assistant" {
                history.push(Message::Assistant {
                    content: Some(content),
                    tool_calls: Vec::new(),
                });
            } else {
                history.push(Message::User { content });
            }
        }
        history
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::messages::{Message, ToolSpec};
    use crate::llm::{ChatEvent, ChatResponse};
    use async_trait::async_trait;
    use tokio::sync::mpsc;

    /// 受け取った履歴と maxTokens を控えて固定文を返すスタブ LLM。
    struct EchoLlm {
        seen: std::sync::Mutex<Vec<Message>>,
        seen_max_tokens: std::sync::Mutex<Option<u32>>,
    }

    #[async_trait]
    impl LlmClient for EchoLlm {
        async fn chat(
            &self,
            history: &[Message],
            _tools: &[ToolSpec<'_>],
            _model: &str,
            max_tokens: Option<u32>,
        ) -> Result<ChatResponse> {
            *self.seen.lock().unwrap() = history.to_vec();
            *self.seen_max_tokens.lock().unwrap() = max_tokens;
            Ok(ChatResponse {
                content: Some("stub reply".into()),
                tool_calls: Vec::new(),
            })
        }

        async fn chat_stream(
            &self,
            _history: &[Message],
            _tools: &[ToolSpec<'_>],
            _model: &str,
            _sink: mpsc::UnboundedSender<ChatEvent>,
        ) -> Result<()> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn create_message_converts_history_and_wraps_result() {
        let llm = Arc::new(EchoLlm {
            seen: std::sync::Mutex::new(Vec::new()),
            seen_max_tokens: std::sync::Mutex::new(None),
        });
        let provider = SamplingProvider::new(llm.clone(), "test-model".into());
        let params = serde_json::json!({
            "systemPrompt": "be terse",
            "maxTokens": 50,
            "messages": [
                {"role":"user","content":{"type":"text","text":"ping"}},
                {"role":"assistant","content":{"type":"text","text":"pong"}},
                {"role":"user","content":{"type":"image","data":"..","mimeType":"image/png"}}
            ]
        });
        let result = provider.create_message(params).await.unwrap();

        // result は MCP の createMessage 形。
        assert_eq!(result["role"], "assistant");
        assert_eq!(result["content"]["text"], "stub reply");
        assert_eq!(result["model"], "test-model");

        // 履歴: system + user + assistant。image は捨てられている。
        let seen = llm.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(matches!(&seen[0], Message::System { content } if content == "be terse"));
        assert!(matches!(&seen[1], Message::User { content } if content == "ping"));
        assert!(
            matches!(&seen[2], Message::Assistant { content, .. } if content.as_deref() == Some("pong"))
        );

        // サーバ指定の maxTokens が LLM へ生成上限として渡る。
        assert_eq!(*llm.seen_max_tokens.lock().unwrap(), Some(50));
    }

    #[tokio::test]
    async fn create_message_rejects_malformed_params() {
        let llm = Arc::new(EchoLlm {
            seen: std::sync::Mutex::new(Vec::new()),
            seen_max_tokens: std::sync::Mutex::new(None),
        });
        let provider = SamplingProvider::new(llm, "m".into());
        // messages が配列でない → パース失敗。
        let err = provider
            .create_message(serde_json::json!({"messages": "nope"}))
            .await;
        assert!(err.is_err());
    }
}
