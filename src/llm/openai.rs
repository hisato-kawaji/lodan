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
use crate::llm::{ChatEvent, ChatResponse, LlmClient};

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
}

#[derive(Deserialize, Debug)]
struct ChatResponseBody {
    choices: Vec<ChatChoice>,
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

        while let Some(event) = stream.next().await {
            let event = event.context("SSE chunk error")?;
            if event.data == "[DONE]" {
                break;
            }
            let chunk: StreamChunk = match serde_json::from_str(&event.data) {
                Ok(v) => v,
                Err(_) => continue,
            };
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
    choices: Vec<StreamChoice>,
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
