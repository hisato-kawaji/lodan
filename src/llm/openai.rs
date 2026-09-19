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
use crate::llm::{ChatEvent, ChatResponse, LlmClient, TransientLlmError, Usage};

pub struct OpenAiClient {
    base_url: String,
    api_key: String,
    /// None はリクエストに含めない (サーバ既定)。#61: 小型モデルの整形安定用。
    temperature: Option<f32>,
    retry: RetryPolicy,
    /// ストリームの無通信許容時間。None は無効。
    stream_idle_timeout: Option<Duration>,
    http: reqwest::Client,
}

/// 再試行可能なステータスに付いてくるエラーボディを読む上限。ボディは表示用でしかないのに、
/// 黙ったサーバ相手に `timeout_secs` を丸ごと使ってから再試行するのは割に合わない。
const ERROR_BODY_READ_LIMIT: Duration = Duration::from_secs(5);

/// エラーメッセージに載せるレスポンスボディの上限 (文字数)。ボディはそのまま stderr・
/// runlog・`stream-json` の stdout に出るので、サーバが返した巨大な HTML や、エコーされた
/// リクエスト内容を丸ごと流さない。
const ERROR_BODY_MAX_CHARS: usize = 500;

fn clip_body(body: &str) -> String {
    match body.char_indices().nth(ERROR_BODY_MAX_CHARS) {
        Some((cut, _)) => format!("{}… ({} bytes total)", &body[..cut], body.len()),
        None => body.to_string(),
    }
}

/// 利用者にまだ何も見せていない一時的な失敗。fallback provider が拾える (`llm::is_transient`)。
fn transient(message: String) -> anyhow::Error {
    TransientLlmError(message).into()
}

/// `source()` をたどって原因を 1 行にする。reqwest の Display は種類 (`error decoding
/// response body`) で止まり、切断なのか UTF-8 破損なのかが分からないため。
///
/// URL は落とす。reqwest は送信エラーに ` for url (…)` を付けるが、`base_url` に資格情報を
/// 埋め込む構成 (`https://user:pass@host/…` やクエリのトークン) では、それが runlog と
/// `stream-json` の stdout に流れてしまう。接続先は設定を見れば分かる。
fn error_chain(e: &reqwest::Error) -> String {
    let mut out = e.to_string();
    if let Some(url) = e.url() {
        out = out.replace(&format!(" for url ({url})"), "");
    }
    let e: &dyn std::error::Error = e;
    let mut source = e.source();
    while let Some(s) = source {
        out.push_str(": ");
        out.push_str(&s.to_string());
        source = s.source();
    }
    out
}

/// 再試行待ちの上限。`Retry-After` がこれより長くても待たない (対話が固まるため)。
const RETRY_MAX_DELAY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy)]
struct RetryPolicy {
    max_retries: u32,
    base: Duration,
}

/// 1 回の `chat` / `chat_stream` 呼び出しの中で共有する再試行の残数。
/// 送信の失敗とストリーム途中の断を同じ予算から引く。
struct RetryBudget {
    policy: RetryPolicy,
    used: u32,
}

impl RetryBudget {
    fn new(policy: RetryPolicy) -> Self {
        Self { policy, used: 0 }
    }

    /// 予算が残っていれば待ってから `true`。使い切っていれば待たずに `false`。
    async fn wait(&mut self, why: &str, retry_after: Option<Duration>) -> bool {
        if self.used >= self.policy.max_retries {
            return false;
        }
        self.used += 1;
        let delay = backoff_delay(self.policy.base, self.used, retry_after, jitter_seed());
        tracing::warn!(
            "LLM request failed ({why}); retry {}/{} in {}ms",
            self.used,
            self.policy.max_retries,
            delay.as_millis()
        );
        crate::runlog::record(
            "api_retry",
            serde_json::json!({
                "attempt": self.used,
                "max_retries": self.policy.max_retries,
                "why": why,
                "delay_ms": delay.as_millis() as u64,
            }),
        );
        tokio::time::sleep(delay).await;
        true
    }
}

/// 待ち時間。`Retry-After` があればそれ、無ければ `base * 2^(attempt-1)` に最大 25% の
/// 揺らぎを足す (同時に落ちたクライアントが同時に戻らないように)。どちらも上限つき。
fn backoff_delay(
    base: Duration,
    attempt: u32,
    retry_after: Option<Duration>,
    jitter_seed: u64,
) -> Duration {
    if let Some(d) = retry_after {
        return d.min(RETRY_MAX_DELAY);
    }
    let exp = base.saturating_mul(1u32 << attempt.saturating_sub(1).min(16));
    let capped = exp.min(RETRY_MAX_DELAY);
    let jitter_range_ms = (capped.as_millis() as u64) / 4;
    let jitter = if jitter_range_ms == 0 {
        0
    } else {
        jitter_seed % jitter_range_ms
    };
    (capped + Duration::from_millis(jitter)).min(RETRY_MAX_DELAY)
}

/// 揺らぎの種。暗号的な乱数は要らないので時計の下位ビットで足りる。
fn jitter_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
}

/// 時間を置けば通り得る応答か。400 / 401 / 404 のような恒久的な失敗は含めない。
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

/// `Retry-After` の秒数形式だけを読む (HTTP-date 形式は無視して通常のバックオフに任せる)。
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
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
            temperature: cfg.temperature,
            retry: RetryPolicy {
                max_retries: cfg.max_retries,
                base: Duration::from_millis(cfg.retry_base_ms),
            },
            stream_idle_timeout: (cfg.stream_idle_timeout_secs > 0)
                .then(|| Duration::from_secs(cfg.stream_idle_timeout_secs)),
            http,
        })
    }

    /// リクエストを送り、成功ステータスの応答を返す。接続エラーと再試行可能な
    /// ステータスは予算の範囲で送り直す。リクエスト全体のタイムアウトは再試行しない
    /// (`timeout_secs` ぶん待った末の失敗をもう一度待つのは、遅いモデルでは害のほうが大きい)。
    async fn send(
        &self,
        req: &ChatRequest<'_>,
        budget: &mut RetryBudget,
    ) -> Result<reqwest::Response> {
        let url = format!("{}/chat/completions", self.base_url);
        loop {
            let mut builder = self.http.post(&url).json(req);
            if !self.api_key.is_empty() {
                builder = builder.bearer_auth(&self.api_key);
            }
            match builder.send().await {
                Ok(resp) if resp.status().is_success() => return Ok(resp),
                Ok(resp) => {
                    let status = resp.status();
                    let after = retry_after(&resp);
                    let body = tokio::time::timeout(ERROR_BODY_READ_LIMIT, resp.text())
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .unwrap_or_default();
                    let body = clip_body(&body);
                    if !is_retryable_status(status) {
                        return Err(anyhow!("LLM HTTP {status}: {body}"));
                    }
                    if budget.wait(&format!("HTTP {status}"), after).await {
                        continue;
                    }
                    return Err(transient(format!("LLM HTTP {status}: {body}")));
                }
                Err(e) if e.is_connect() => {
                    if budget
                        .wait(&format!("connect error: {}", error_chain(&e)), None)
                        .await
                    {
                        continue;
                    }
                    return Err(transient(format!(
                        "sending chat request: {}",
                        error_chain(&e)
                    )));
                }
                // 再試行はしないが (同じ待ちを繰り返すだけ)、別の provider なら通り得る。
                Err(e) if e.is_timeout() => {
                    return Err(transient(format!(
                        "sending chat request: {}",
                        error_chain(&e)
                    )));
                }
                Err(e) => return Err(e.without_url()).context("sending chat request"),
            }
        }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
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
        let req = ChatRequest {
            model,
            messages: history,
            tools,
            tool_choice: if tools.is_empty() { None } else { Some("auto") },
            max_tokens,
            temperature: self.temperature,
            stream: false,
            stream_options: None,
        };

        let mut budget = RetryBudget::new(self.retry);
        // 200 の後に本文が途中で切れるのも一時的な失敗。非ストリームでは利用者にまだ何も
        // 見せていないので、ストリームの「出力前の断」と同じく送り直してよい。
        let text = loop {
            let resp = self.send(&req, &mut budget).await?;
            match resp.text().await {
                Ok(text) => break text,
                // `timeout_secs` は本文の読み込みまで含む。使い切った失敗は送り直さない。
                Err(e)
                    if !e.is_timeout()
                        && budget
                            .wait(
                                &format!("response body interrupted: {}", error_chain(&e)),
                                None,
                            )
                            .await =>
                {
                    continue;
                }
                // 非ストリームでは利用者にまだ何も見せていない。
                Err(e) => {
                    return Err(transient(format!(
                        "reading chat response: {}",
                        error_chain(&e)
                    )));
                }
            }
        };

        let body: ChatResponseBody = serde_json::from_str(&text)
            .with_context(|| format!("parsing chat response: {}", clip_body(&text)))?;
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
        let req = ChatRequest {
            model,
            messages: history,
            tools,
            tool_choice: if tools.is_empty() { None } else { Some("auto") },
            max_tokens: None,
            temperature: self.temperature,
            stream: true,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
        };

        let mut budget = RetryBudget::new(self.retry);
        let StreamParts {
            text_buf,
            calls,
            usage,
        } = loop {
            let resp = self.send(&req, &mut budget).await?;
            match self.consume_stream(resp, &sink).await {
                Ok(parts) => break parts,
                // 本文を 1 文字でも流した後にやり直すと、利用者には同じ文が二重に見える。
                // 何も出していない断だけを送り直す (ツールコールの断片は sink に流して
                // いないので捨てて構わない)。
                Err(f)
                    if f.retryable()
                        && budget
                            .wait(&format!("stream interrupted: {:#}", f.error), None)
                            .await =>
                {
                    continue;
                }
                // 本文を出す前の断は fallback が拾える。出した後は拾わせない (二重表示)。
                Err(f) if !f.emitted_text => return Err(transient(format!("{:#}", f.error))),
                Err(f) => return Err(f.error),
            }
        };

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

impl OpenAiClient {
    /// SSE を最後まで読み、本文デルタを `sink` へ流しながら応答の部品を組み立てる。
    async fn consume_stream(
        &self,
        resp: reqwest::Response,
        sink: &mpsc::UnboundedSender<ChatEvent>,
    ) -> std::result::Result<StreamParts, StreamFailure> {
        let mut stream = resp.bytes_stream().eventsource();
        let mut text_buf = String::new();
        let mut calls: Vec<PartialCall> = Vec::new();
        let mut usage: Option<Usage> = None;

        loop {
            let next = match self.stream_idle_timeout {
                Some(idle) => match tokio::time::timeout(idle, stream.next()).await {
                    Ok(next) => next,
                    Err(_) => {
                        return Err(StreamFailure {
                            emitted_text: !text_buf.is_empty(),
                            request_timed_out: false,
                            error: anyhow!("LLM stream idle for {}s", idle.as_secs()),
                        });
                    }
                },
                None => stream.next().await,
            };
            let Some(event) = next else { break };
            let event = match event {
                Ok(event) => event,
                Err(e) => {
                    let request_timed_out = matches!(
                        &e,
                        eventsource_stream::EventStreamError::Transport(t) if t.is_timeout()
                    );
                    return Err(StreamFailure {
                        emitted_text: !text_buf.is_empty(),
                        request_timed_out,
                        // eventsource の Error は source() を実装していないので、`{:#}` では
                        // 「Transport error: error decoding response body」で止まる。
                        // 転送エラーは中の reqwest エラーを自前でたどって原因まで出す。
                        error: match &e {
                            eventsource_stream::EventStreamError::Transport(t) => {
                                anyhow!("SSE chunk error: {}", error_chain(t))
                            }
                            _ => anyhow::Error::new(e).context("SSE chunk error"),
                        },
                    });
                }
            };
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

        Ok(StreamParts {
            text_buf,
            calls,
            usage,
        })
    }
}

struct StreamParts {
    text_buf: String,
    calls: Vec<PartialCall>,
    usage: Option<Usage>,
}

/// ストリーム途中の失敗。`emitted_text` が false なら利用者には何も見えていない。
struct StreamFailure {
    emitted_text: bool,
    /// リクエスト全体の `timeout_secs` を使い切った (idle timeout とは別物)。
    request_timed_out: bool,
    error: anyhow::Error,
}

impl StreamFailure {
    /// 送り直してよいか。表示済みの本文があれば二重表示になり、`timeout_secs` 切れは
    /// 同じ待ちを繰り返すだけなので、どちらも再試行しない。
    fn retryable(&self) -> bool {
        !self.emitted_text && !self.request_timed_out
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

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const OK_JSON: &str = r#"{"choices":[{"message":{"content":"hi"}}]}"#;
    const STALL: &str = "STALL:";
    const SSE_HEAD: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 1000\r\nconnection: close\r\n\r\n";

    fn http(status: &str, extra_headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{extra_headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// 接続ごとに台本の次の応答を返すサーバ。台本が尽きたら最後の応答を繰り返す。
    /// 戻り値は base_url と、受けたリクエスト数。
    async fn scripted_server(script: Vec<String>) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let reply = script[n.min(script.len() - 1)].clone();
                tokio::spawn(async move {
                    // ヘッダと JSON 本文を読み切る (content-length ぶん)。
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        let Ok(read) = sock.read(&mut chunk).await else {
                            return;
                        };
                        if read == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..read]);
                        let text = String::from_utf8_lossy(&buf);
                        if let Some(head_end) = text.find("\r\n\r\n") {
                            let len = text[..head_end]
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if buf.len() >= head_end + 4 + len {
                                break;
                            }
                        }
                    }
                    // `STALL:` で始まる台本は、残りを送ったあと黙ったまま接続を保つ。
                    if let Some(head) = reply.strip_prefix(STALL) {
                        let _ = sock.write_all(head.as_bytes()).await;
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        return;
                    }
                    let _ = sock.write_all(reply.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (base_url, hits)
    }

    fn client(base_url: &str, max_retries: u32) -> OpenAiClient {
        let mut cfg = ProviderConfig::default_local();
        cfg.base_url = base_url.to_string();
        cfg.max_retries = max_retries;
        cfg.retry_base_ms = 1;
        OpenAiClient::new(&cfg).unwrap()
    }

    #[tokio::test]
    async fn retries_5xx_then_succeeds() {
        let (url, hits) = scripted_server(vec![
            http("503 Service Unavailable", "", "busy"),
            http("503 Service Unavailable", "", "busy"),
            http("200 OK", "", OK_JSON),
        ])
        .await;
        let resp = client(&url, 3).chat(&[], &[], "m", None).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some("hi"));
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gives_up_after_max_retries_and_reports_last_status() {
        let (url, hits) =
            scripted_server(vec![http("429 Too Many Requests", "", "slow down")]).await;
        let err = client(&url, 2).chat(&[], &[], "m", None).await.unwrap_err();
        assert!(format!("{err:#}").contains("429"), "{err:#}");
        assert_eq!(hits.load(Ordering::SeqCst), 3, "1 try + 2 retries");
    }

    #[tokio::test]
    async fn errors_carry_neither_the_request_url_nor_an_unbounded_body() {
        // 誰も listen していないポート。base_url に埋めた資格情報がエラー文に出ないこと。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let secret_url = format!("http://user:hunter2@127.0.0.1:{port}/v1");
        let err = client(&secret_url, 0)
            .chat(&[], &[], "m", None)
            .await
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(
            !text.contains("hunter2") && !text.contains("127.0.0.1"),
            "{text}"
        );
        assert!(text.contains("sending chat request"), "{text}");

        let huge = "x".repeat(50_000);
        let (url, _) = scripted_server(vec![http("500 Internal Server Error", "", &huge)]).await;
        let text = format!(
            "{:#}",
            client(&url, 0).chat(&[], &[], "m", None).await.unwrap_err()
        );
        assert!(
            text.len() < 1_000,
            "body must be clipped, got {} chars",
            text.len()
        );
        assert!(text.contains("50000 bytes total"), "{text}");
    }

    #[tokio::test]
    async fn an_unparseable_success_body_is_clipped_in_the_error() {
        // 200 で巨大な HTML を返すプロキシ。パース失敗のエラーに本文を丸ごと載せない。
        let html = format!("<html>{}</html>", "y".repeat(50_000));
        let (url, _) = scripted_server(vec![http("200 OK", "", &html)]).await;
        let text = format!(
            "{:#}",
            client(&url, 0).chat(&[], &[], "m", None).await.unwrap_err()
        );
        assert!(
            text.contains("parsing chat response"),
            "{}",
            &text[..200.min(text.len())]
        );
        assert!(text.len() < 1_500, "got {} chars", text.len());
    }

    #[test]
    fn clip_body_cuts_on_a_char_boundary() {
        let body = "あ".repeat(ERROR_BODY_MAX_CHARS + 10);
        let clipped = clip_body(&body);
        assert!(clipped.starts_with(&"あ".repeat(ERROR_BODY_MAX_CHARS)));
        assert!(clipped.contains("bytes total"));
        assert_eq!(clip_body("short"), "short");
    }

    #[tokio::test]
    async fn exhausted_retries_are_transient_but_client_errors_are_not() {
        use crate::llm::is_transient;
        let (url, _) = scripted_server(vec![http("503 Service Unavailable", "", "busy")]).await;
        let err = client(&url, 1).chat(&[], &[], "m", None).await.unwrap_err();
        assert!(is_transient(&err), "{err:#}");
        assert!(format!("{err:#}").contains("503"));

        let (url, _) = scripted_server(vec![http("401 Unauthorized", "", "bad key")]).await;
        let err = client(&url, 1).chat(&[], &[], "m", None).await.unwrap_err();
        assert!(
            !is_transient(&err),
            "another provider cannot fix a bad key: {err:#}"
        );
    }

    #[tokio::test]
    async fn a_stream_cut_after_text_is_not_transient() {
        use crate::llm::is_transient;
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
        let cut = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}",
            sse.len() + 100
        );
        let (url, _) = scripted_server(vec![cut]).await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let err = client(&url, 0)
            .chat_stream(&[], &[], "m", tx)
            .await
            .unwrap_err();
        assert!(
            !is_transient(&err),
            "text was shown; a fallback would print it twice"
        );

        // 本文を出す前の断は一時的な失敗。
        let head_only = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 100\r\nconnection: close\r\n\r\n".to_string();
        let (url, _) = scripted_server(vec![head_only]).await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let err = client(&url, 0)
            .chat_stream(&[], &[], "m", tx)
            .await
            .unwrap_err();
        assert!(is_transient(&err), "{err:#}");
    }

    #[tokio::test]
    async fn a_stalled_error_body_does_not_eat_the_request_timeout() {
        // 503 のヘッダだけ返して本文を送らないサーバ。ボディ読みで timeout_secs (既定 120s) を
        // 待たずに、上限 (5s) で切り上げて再試行へ進む。
        let head =
            "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 500\r\nconnection: close\r\n\r\n";
        let (url, hits) =
            scripted_server(vec![format!("{STALL}{head}"), http("200 OK", "", OK_JSON)]).await;
        let started = std::time::Instant::now();
        let resp = tokio::time::timeout(
            Duration::from_secs(20),
            client(&url, 3).chat(&[], &[], "m", None),
        )
        .await
        .expect("must not wait out timeout_secs")
        .unwrap();
        assert_eq!(resp.content.as_deref(), Some("hi"));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert!(started.elapsed() < Duration::from_secs(15));
    }

    #[tokio::test]
    async fn permanent_client_errors_are_not_retried() {
        for status in ["400 Bad Request", "401 Unauthorized", "404 Not Found"] {
            let (url, hits) = scripted_server(vec![http(status, "", "nope")]).await;
            assert!(client(&url, 3).chat(&[], &[], "m", None).await.is_err());
            assert_eq!(
                hits.load(Ordering::SeqCst),
                1,
                "{status} must not be retried"
            );
        }
    }

    #[tokio::test]
    async fn zero_max_retries_disables_retry() {
        let (url, hits) = scripted_server(vec![
            http("503 Service Unavailable", "", "busy"),
            http("200 OK", "", OK_JSON),
        ])
        .await;
        assert!(client(&url, 0).chat(&[], &[], "m", None).await.is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stream_retries_failed_status_then_streams() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\ndata: [DONE]\n\n";
        let ok = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}",
            sse.len()
        );
        let (url, hits) = scripted_server(vec![http("502 Bad Gateway", "", "upstream"), ok]).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        client(&url, 3)
            .chat_stream(&[], &[], "m", tx)
            .await
            .unwrap();

        let mut text = String::new();
        let mut done = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                ChatEvent::TextDelta(d) => text.push_str(&d),
                ChatEvent::Done(r) => done = Some(r),
            }
        }
        assert_eq!(
            text, "hello",
            "no duplicated deltas from the failed attempt"
        );
        assert_eq!(done.unwrap().content.as_deref(), Some("hello"));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn stream_cut_after_text_is_not_retried() {
        // content-length を実体より長く宣言して途中で閉じる = 本文の途中で断。
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
        let cut = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}",
            sse.len() + 100
        );
        let (url, hits) = scripted_server(vec![cut]).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let res = client(&url, 3).chat_stream(&[], &[], "m", tx).await;
        assert!(res.is_err(), "a truncated stream must surface as an error");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "text was already shown; retrying would duplicate it"
        );
        assert!(matches!(rx.recv().await, Some(ChatEvent::TextDelta(d)) if d == "partial"));
    }

    #[tokio::test]
    async fn stream_cut_before_any_text_is_retried() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
        let ok = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}",
            sse.len()
        );
        // ヘッダだけ返して本文を 1 バイトも送らずに閉じる。
        let cut = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 100\r\nconnection: close\r\n\r\n".to_string();
        let (url, hits) = scripted_server(vec![cut, ok]).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        client(&url, 3)
            .chat_stream(&[], &[], "m", tx)
            .await
            .unwrap();
        assert!(matches!(rx.recv().await, Some(ChatEvent::TextDelta(d)) if d == "ok"));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    fn sse_ok(text: &str) -> String {
        let sse = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\ndata: [DONE]\n\n"
        );
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}",
            sse.len()
        )
    }

    /// STALL サーバ相手のテストが、退行時に 30 秒ぶら下がらないようにする上限。
    async fn within<T>(fut: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(10), fut)
            .await
            .expect("timed out: the client kept waiting on a stalled server")
    }

    fn short_timeout_client(base_url: &str) -> OpenAiClient {
        let mut cfg = ProviderConfig::default_local();
        cfg.base_url = base_url.to_string();
        cfg.retry_base_ms = 1;
        cfg.timeout_secs = 1;
        OpenAiClient::new(&cfg).unwrap()
    }

    #[tokio::test]
    async fn request_timeout_while_reading_body_is_not_retried() {
        let head = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 500\r\nconnection: close\r\n\r\n";
        let (url, hits) =
            scripted_server(vec![format!("{STALL}{head}"), http("200 OK", "", OK_JSON)]).await;
        let res = within(short_timeout_client(&url).chat(&[], &[], "m", None)).await;
        assert!(res.is_err());
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "timeout_secs was spent; do not spend it again"
        );
    }

    #[tokio::test]
    async fn request_timeout_mid_stream_is_not_retried() {
        let (url, hits) = scripted_server(vec![format!("{STALL}{SSE_HEAD}"), sse_ok("ok")]).await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let res = within(short_timeout_client(&url).chat_stream(&[], &[], "m", tx)).await;
        assert!(res.is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    fn idle_client(base_url: &str) -> OpenAiClient {
        let mut cfg = ProviderConfig::default_local();
        cfg.base_url = base_url.to_string();
        cfg.retry_base_ms = 1;
        cfg.stream_idle_timeout_secs = 1;
        OpenAiClient::new(&cfg).unwrap()
    }

    #[tokio::test]
    async fn idle_stream_before_any_text_is_retried() {
        let (url, hits) = scripted_server(vec![format!("{STALL}{SSE_HEAD}"), sse_ok("ok")]).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        within(idle_client(&url).chat_stream(&[], &[], "m", tx))
            .await
            .unwrap();
        assert!(matches!(rx.recv().await, Some(ChatEvent::TextDelta(d)) if d == "ok"));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn idle_stream_after_text_is_an_error_not_a_retry() {
        let partial = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
        let (url, hits) =
            scripted_server(vec![format!("{STALL}{SSE_HEAD}{partial}"), sse_ok("ok")]).await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let err = within(idle_client(&url).chat_stream(&[], &[], "m", tx))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("idle"), "{err:#}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn truncated_non_stream_body_is_retried() {
        let cut = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 500\r\nconnection: close\r\n\r\n{\"choi".to_string();
        let (url, hits) = scripted_server(vec![cut, http("200 OK", "", OK_JSON)]).await;
        let resp = client(&url, 3).chat(&[], &[], "m", None).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some("hi"));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn dropping_the_call_cancels_a_pending_backoff() {
        // Ctrl-C は run_turn の future を drop する。バックオフ待ちがそれを妨げないこと。
        let (url, hits) = scripted_server(vec![http("503 Service Unavailable", "", "busy")]).await;
        let mut cfg = ProviderConfig::default_local();
        cfg.base_url = url;
        cfg.retry_base_ms = 60_000;
        let c = OpenAiClient::new(&cfg).unwrap();
        let started = std::time::Instant::now();
        let res =
            tokio::time::timeout(Duration::from_millis(300), c.chat(&[], &[], "m", None)).await;
        assert!(res.is_err(), "still backing off when the caller gave up");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn backoff_doubles_is_capped_and_prefers_retry_after() {
        let base = Duration::from_millis(500);
        assert_eq!(backoff_delay(base, 1, None, 0), Duration::from_millis(500));
        assert_eq!(backoff_delay(base, 2, None, 0), Duration::from_millis(1000));
        assert_eq!(backoff_delay(base, 3, None, 0), Duration::from_millis(2000));
        // 揺らぎは待ち時間の 25% 未満。
        assert!(backoff_delay(base, 1, None, u64::MAX) < Duration::from_millis(625));
        assert_eq!(backoff_delay(base, 30, None, 0), RETRY_MAX_DELAY);
        assert_eq!(
            backoff_delay(base, 1, Some(Duration::from_secs(7)), 123),
            Duration::from_secs(7)
        );
        assert_eq!(
            backoff_delay(base, 1, Some(Duration::from_secs(3600)), 0),
            RETRY_MAX_DELAY
        );
    }

    #[test]
    fn retryable_statuses() {
        use reqwest::StatusCode as S;
        for s in [
            S::REQUEST_TIMEOUT,
            S::TOO_MANY_REQUESTS,
            S::INTERNAL_SERVER_ERROR,
            S::BAD_GATEWAY,
            S::SERVICE_UNAVAILABLE,
        ] {
            assert!(is_retryable_status(s), "{s}");
        }
        for s in [S::BAD_REQUEST, S::UNAUTHORIZED, S::FORBIDDEN, S::NOT_FOUND] {
            assert!(!is_retryable_status(s), "{s}");
        }
    }

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
            temperature: None,
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

    /// temperature は None なら省略・Some なら数値で入る (#61)。
    #[test]
    fn request_temperature_omitted_unless_set() {
        let req = ChatRequest {
            model: "m",
            messages: &[],
            tools: &[],
            tool_choice: None,
            max_tokens: None,
            temperature: None,
            stream: false,
            stream_options: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(
            json.get("temperature").is_none(),
            "unset temperature must not appear in the request"
        );

        let req = ChatRequest {
            temperature: Some(0.2),
            ..req
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!((json["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6);
    }
}
