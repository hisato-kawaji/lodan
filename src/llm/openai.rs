// OpenAI Chat Completions 互換クライアント。
// - `chat`: 非ストリーム
// - `chat_stream`: SSE 経由でテキスト/ツール呼び出しデルタを再構築

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::agent::messages::{Message, ToolCall, ToolCallFunction, ToolSpec};
use crate::config::ProviderConfig;
use crate::llm::{ChatEvent, ChatResponse, LlmClient, Usage};

pub struct OpenAiClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl OpenAiClient {
    pub fn new(cfg: &ProviderConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.api_key.clone(),
            http,
        })
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    #[serde(skip_serializing_if = "<[ToolSpec]>::is_empty")]
    tools: &'a [ToolSpec<'a>],
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    stream: bool,
    /// ストリーミング時に最終チャンクへ usage を含めるよう要求する
    /// (OpenAI / vLLM / llama.cpp が対応。非対応サーバは無視する)。
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Deserialize, Debug)]
struct ChatResponseBody {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize, Debug)]
struct ChatChoice {
    message: ChatMessage,
    #[allow(dead_code)]
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
struct ChatMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<RawToolCall>,
}

#[derive(Deserialize, Debug)]
struct RawToolCall {
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    function: RawToolCallFunction,
}

#[derive(Deserialize, Debug)]
struct RawToolCallFunction {
    name: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

#[async_trait]
impl LlmClient for OpenAiClient {
    async fn chat(
        &self,
        history: &[Message],
        tools: &[ToolSpec<'_>],
        model: &str,
        max_tokens: Option<u32>,
    ) -> Result<ChatResponse> {
        let url = format!("{}/chat/completions", self.base_url);
        let req = ChatRequest {
            model,
            messages: history,
            tools,
            tool_choice: if tools.is_empty() { None } else { Some("auto") },
            max_tokens,
            stream: false,
            stream_options: None,
        };

        let mut builder = self.http.post(&url).json(&req);
        if !self.api_key.is_empty() {
            builder = builder.bearer_auth(&self.api_key);
        }

        let resp = builder.send().await.context("sending chat request")?;
        let status = resp.status();
        let text = resp.text().await.context("reading chat response")?;
        if !status.is_success() {
            return Err(anyhow!("LLM HTTP {status}: {text}"));
        }

        let body: ChatResponseBody = serde_json::from_str(&text)
            .with_context(|| format!("parsing chat response: {text}"))?;
        let usage = body.usage.map(Usage::normalized);
        let choice = body
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("LLM returned no choices"))?;

        let tool_calls = choice
            .message
            .tool_calls
            .into_iter()
            .enumerate()
            .map(|(i, tc)| ToolCall {
                id: tc.id.unwrap_or_else(|| format!("call_{i}")),
                kind: tc.kind.unwrap_or_else(|| "function".to_string()),
                function: ToolCallFunction {
                    name: tc.function.name,
                    arguments: arguments_to_string(tc.function.arguments),
                },
            })
            .collect();

        Ok(ChatResponse {
            content: choice.message.content,
            tool_calls,
            usage,
        })
    }

    async fn chat_stream(
        &self,
        history: &[Message],
        tools: &[ToolSpec<'_>],
        model: &str,
        sink: mpsc::UnboundedSender<ChatEvent>,
    ) -> Result<()> {
        let url = format!("{}/chat/completions", self.base_url);
        let req = ChatRequest {
            model,
            messages: history,
            tools,
            tool_choice: if tools.is_empty() { None } else { Some("auto") },
            max_tokens: None,
            stream: true,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
        };

        let mut builder = self.http.post(&url).json(&req);
        if !self.api_key.is_empty() {
            builder = builder.bearer_auth(&self.api_key);
        }
        let resp = builder.send().await.context("sending stream request")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("LLM HTTP {status}: {body}"));
        }

        let mut stream = resp.bytes_stream().eventsource();
        let mut text_buf = String::new();
        let mut calls: Vec<PartialCall> = Vec::new();
        let mut usage: Option<Usage> = None;

        while let Some(event) = stream.next().await {
            let event = event.context("SSE chunk error")?;
            if event.data == "[DONE]" {
                break;
            }
            let chunk: StreamChunk = match serde_json::from_str(&event.data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // usage は最終チャンク (choices 空) に載る。来たものを常に上書き採用。
            if let Some(u) = chunk.usage {
                usage = Some(u.normalized());
            }
            for choice in chunk.choices {
                if let Some(d) = choice.delta.content
                    && !d.is_empty()
                {
                    text_buf.push_str(&d);
                    let _ = sink.send(ChatEvent::TextDelta(d));
                }
                for tc in choice.delta.tool_calls {
                    let idx = tc.index as usize;
                    while calls.len() <= idx {
                        calls.push(PartialCall::default());
                    }
                    let slot = &mut calls[idx];
                    if let Some(id) = tc.id {
                        slot.id = Some(id);
                    }
                    if let Some(k) = tc.kind {
                        slot.kind = Some(k);
                    }
                    if let Some(f) = tc.function {
                        if let Some(n) = f.name {
                            slot.name.get_or_insert_with(String::new).push_str(&n);
                        }
                        if let Some(a) = f.arguments {
                            slot.arguments.push_str(&a);
                        }
                    }
                }
            }
        }

        let tool_calls = calls
            .into_iter()
            .enumerate()
            .map(|(i, p)| ToolCall {
                id: p.id.unwrap_or_else(|| format!("call_{i}")),
                kind: p.kind.unwrap_or_else(|| "function".to_string()),
                function: ToolCallFunction {
                    name: p.name.unwrap_or_default(),
                    arguments: if p.arguments.is_empty() {
                        "{}".to_string()
                    } else {
                        p.arguments
                    },
                },
            })
            .collect::<Vec<_>>();

        let resp = ChatResponse {
            content: if text_buf.is_empty() {
                None
            } else {
                Some(text_buf)
            },
            tool_calls,
            usage,
        };
        let _ = sink.send(ChatEvent::Done(resp));
        Ok(())
    }
}

#[derive(Default)]
struct PartialCall {
    id: Option<String>,
    kind: Option<String>,
    name: Option<String>,
    arguments: String,
}

#[derive(Deserialize)]
struct StreamChunk {
    // include_usage の最終チャンクは choices が空配列 or 欠落し得る。
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    #[allow(dead_code)]
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<StreamToolCall>,
}

#[derive(Deserialize)]
struct StreamToolCall {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    function: Option<StreamFunction>,
}

#[derive(Deserialize)]
struct StreamFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

fn arguments_to_string(v: serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s,
        serde_json::Value::Null => "{}".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_stream_body_parses_usage() {
        let body: ChatResponseBody = serde_json::from_str(
            r#"{"choices":[{"message":{"content":"hi"}}],
                "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
        )
        .unwrap();
        assert_eq!(
            body.usage,
            Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            })
        );
    }

    #[test]
    fn missing_usage_is_none() {
        let body: ChatResponseBody =
            serde_json::from_str(r#"{"choices":[{"message":{"content":"hi"}}]}"#).unwrap();
        assert!(body.usage.is_none());
    }

    #[test]
    fn normalized_fills_missing_total() {
        // total_tokens を返さないサーバ (欠落 → serde default 0) を補完する。
        let u: Usage =
            serde_json::from_str(r#"{"prompt_tokens":8,"completion_tokens":4}"#).unwrap();
        assert_eq!(u.total_tokens, 0);
        assert_eq!(u.normalized().total_tokens, 12);
    }

    #[test]
    fn stream_final_usage_chunk_parses_with_empty_choices() {
        // stream_options.include_usage の最終チャンク形式。
        let chunk: StreamChunk = serde_json::from_str(
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120}}"#,
        )
        .unwrap();
        assert!(chunk.choices.is_empty());
        assert_eq!(chunk.usage.unwrap().total_tokens, 120);
    }

    #[test]
    fn stream_request_includes_stream_options() {
        let req = ChatRequest {
            model: "m",
            messages: &[],
            tools: &[],
            tool_choice: None,
            max_tokens: None,
            stream: true,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["stream_options"]["include_usage"], true);

        let req = ChatRequest {
            stream: false,
            stream_options: None,
            ..req
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("stream_options").is_none());
    }
}
