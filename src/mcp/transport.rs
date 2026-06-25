//! MCP transport 抽象。JSON-RPC の 1 行を送って応答 (`JsonRpcIncoming`) を受け取る
//! ワイヤ層を差し替え可能にする。高レベルの request/notify デコードは `client.rs`。
//!
//! - `StdioTransport`: 子プロセスの stdin/stdout を newline-delimited JSON で使う
//! - `HttpTransport`: Streamable HTTP。POST で JSON-RPC を送り、`application/json` か
//!   `text/event-stream` (SSE) の応答を受ける。`Mcp-Session-Id` を引き継ぐ
//!
//! stdio は server→client リクエスト (roots/list, sampling/createMessage) に
//! `ServerRequestHandler` 経由で応答する。HTTP は server→client チャネルを
//! 持たないため handler を使わない。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::timeout;

use crate::mcp::config::McpServerSpec;
use crate::mcp::protocol::JsonRpcIncoming;

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// server→client リクエストの処理結果。
pub enum HandlerOutcome {
    /// 正常応答 (JSON-RPC result)。
    Result(serde_json::Value),
    /// 処理したが失敗した (JSON-RPC error)。method-not-found とは区別する。
    Error { code: i64, message: String },
    /// 未対応のメソッド (呼び出し側で method-not-found 応答)。
    Unhandled,
}

/// server→client リクエストのハンドラ。method と params を受け取り、結果を
/// 非同期に返す。sampling は LLM 呼び出しを伴うため async。
pub type ServerRequestHandler = Arc<
    dyn Fn(
            String,
            Option<serde_json::Value>,
        ) -> Pin<Box<dyn Future<Output = HandlerOutcome> + Send>>
        + Send
        + Sync,
>;

#[async_trait]
pub trait Transport: Send + Sync {
    /// `line` (JSON-RPC リクエスト) を送り、`id` で相関した応答を返す。
    async fn send_request(&self, id: u64, line: String) -> Result<JsonRpcIncoming>;
    /// 通知 (応答なし) を送る。
    async fn send_notification(&self, line: String) -> Result<()>;
}

// ---------------- stdio ----------------

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcIncoming>>>>;

pub struct StdioTransport {
    label: String,
    outbound: mpsc::UnboundedSender<String>,
    pending: Pending,
    child: Mutex<Option<Child>>,
}

impl StdioTransport {
    pub async fn connect(
        label: &str,
        command: &str,
        spec: &McpServerSpec,
        handler: ServerRequestHandler,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(&spec.args)
            .envs(spec.env.iter())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning MCP server `{command}`"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("child stdin missing"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("child stdout missing"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("child stderr missing"))?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        // Writer task: drain outbound channel into the child stdin.
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<String>();
        let mut stdin = stdin;
        tokio::spawn(async move {
            while let Some(line) = outbound_rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
        });

        // Reader task: newline-delimited JSON from stdout.
        // 三分岐: method+id = server→client リクエスト (応答する) /
        //         id のみ = 自分のリクエストへの応答 (pending へ) /
        //         method のみ = 通知 (無視)。
        let pending_clone = Arc::clone(&pending);
        let label_for_reader = label.to_string();
        let reader_outbound = outbound_tx.clone();
        let handler = Arc::clone(&handler);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let inc: JsonRpcIncoming = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::debug!(server=%label_for_reader, %e, "mcp: unparseable frame");
                        continue;
                    }
                };
                match (inc.method.clone(), inc.id) {
                    // server→client リクエスト (roots/list, sampling/createMessage 等)。
                    // handler は async (sampling は LLM 呼び出しを伴う) なので、reader を
                    // 塞がないよう要求ごとに task を spawn して応答する。
                    (Some(method), Some(id)) => {
                        let handler = Arc::clone(&handler);
                        let out = reader_outbound.clone();
                        let params = inc.params.clone();
                        tokio::spawn(async move {
                            let response = match handler(method.clone(), params).await {
                                HandlerOutcome::Result(result) => {
                                    serde_json::json!({"jsonrpc":"2.0","id":id,"result":result})
                                }
                                HandlerOutcome::Error { code, message } => serde_json::json!({
                                    "jsonrpc":"2.0","id":id,
                                    "error":{"code":code,"message":message}
                                }),
                                HandlerOutcome::Unhandled => serde_json::json!({
                                    "jsonrpc":"2.0","id":id,
                                    "error":{"code":-32601,"message":format!("method not found: {method}")}
                                }),
                            };
                            let _ = out.send(response.to_string());
                        });
                    }
                    (Some(method), None) => {
                        tracing::debug!(server=%label_for_reader, method, "mcp: notification ignored");
                    }
                    (None, Some(id)) => {
                        if let Some(tx) = pending_clone.lock().await.remove(&id) {
                            let _ = tx.send(inc);
                        } else {
                            tracing::debug!(server=%label_for_reader, id, "mcp: response with no waiter");
                        }
                    }
                    (None, None) => {
                        tracing::debug!(server=%label_for_reader, "mcp: malformed frame (no method/id)");
                    }
                }
            }
        });

        // Stderr drain (avoid filling the pipe buffer).
        let label_for_stderr = label.to_string();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                tracing::debug!(server=%label_for_stderr, "mcp[stderr] {}", line);
            }
        });

        Ok(Self {
            label: label.to_string(),
            outbound: outbound_tx,
            pending,
            child: Mutex::new(Some(child)),
        })
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn send_request(&self, id: u64, line: String) -> Result<JsonRpcIncoming> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.outbound
            .send(line)
            .map_err(|_| anyhow!("mcp[{}]: writer channel closed", self.label))?;
        match timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => Err(anyhow!(
                "mcp[{}]: response channel dropped (server died?)",
                self.label
            )),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(anyhow!(
                    "mcp[{}]: request timed out after {:?}",
                    self.label,
                    REQUEST_TIMEOUT
                ))
            }
        }
    }

    async fn send_notification(&self, line: String) -> Result<()> {
        self.outbound
            .send(line)
            .map_err(|_| anyhow!("mcp[{}]: writer channel closed", self.label))
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.child.try_lock()
            && let Some(child) = guard.as_mut()
        {
            let _ = child.start_kill();
        }
    }
}

// ---------------- Streamable HTTP ----------------

const SESSION_HEADER: &str = "Mcp-Session-Id";

pub struct HttpTransport {
    label: String,
    url: String,
    client: reqwest::Client,
    headers: Vec<(String, String)>,
    session_id: Mutex<Option<String>>,
}

impl HttpTransport {
    pub fn connect(label: &str, url: &str, spec: &McpServerSpec) -> Result<Self> {
        // http:// に認証ヘッダを載せると平文でトークンが流れる。明示的に警告する。
        if !url.starts_with("https://") && !spec.headers.is_empty() {
            eprintln!(
                "mcp[{label}]: warning: sending headers over non-HTTPS url ({url}) — \
                 credentials are sent in cleartext"
            );
        }
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building HTTP client")?;
        let headers = spec
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Ok(Self {
            label: label.to_string(),
            url: url.to_string(),
            client,
            headers,
            session_id: Mutex::new(None),
        })
    }

    async fn post(&self, line: String) -> Result<reqwest::Response> {
        let mut req = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(line);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Some(sid) = self.session_id.lock().await.as_ref() {
            req = req.header(SESSION_HEADER, sid);
        }
        req.send()
            .await
            .with_context(|| format!("mcp[{}]: HTTP POST to {}", self.label, self.url))
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send_request(&self, id: u64, line: String) -> Result<JsonRpcIncoming> {
        let resp = self.post(line).await?;
        let status = resp.status();
        // サーバが割り当てたセッション ID を引き継ぐ。
        if let Some(sid) = resp.headers().get(SESSION_HEADER)
            && let Ok(s) = sid.to_str()
        {
            *self.session_id.lock().await = Some(s.to_string());
        }
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "mcp[{}]: HTTP {} from server: {}",
                self.label,
                status,
                body.trim()
            ));
        }
        let body = resp
            .text()
            .await
            .with_context(|| format!("mcp[{}]: reading HTTP response body", self.label))?;

        if ctype.contains("text/event-stream") {
            extract_response_from_sse(&body, id)
                .ok_or_else(|| anyhow!("mcp[{}]: no JSON-RPC response in SSE stream", self.label))
        } else {
            serde_json::from_str(&body)
                .with_context(|| format!("mcp[{}]: decoding JSON response", self.label))
        }
    }

    async fn send_notification(&self, line: String) -> Result<()> {
        let resp = self.post(line).await?;
        if let Some(sid) = resp.headers().get(SESSION_HEADER)
            && let Ok(s) = sid.to_str()
        {
            *self.session_id.lock().await = Some(s.to_string());
        }
        // 通知は 202 Accepted 等。本文があっても無視する。
        Ok(())
    }
}

/// SSE 本文から、`id` に一致する JSON-RPC 応答を取り出す。一致が無ければ最初に
/// 見つかった result/error を持つ応答を返す。`data:` 行のみを解釈する。
fn extract_response_from_sse(body: &str, id: u64) -> Option<JsonRpcIncoming> {
    let mut fallback: Option<JsonRpcIncoming> = None;
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        let Ok(inc) = serde_json::from_str::<JsonRpcIncoming>(data) else {
            continue;
        };
        if inc.id == Some(id) {
            return Some(inc);
        }
        if fallback.is_none() && (inc.result.is_some() || inc.error.is_some()) {
            fallback = Some(inc);
        }
    }
    fallback
}

// ---------------- dispatch ----------------

/// spec の transport 種別に応じて接続し、boxed transport を返す。
/// `handler` は server→client リクエスト (roots/list 等) の応答に使う。
/// HTTP は server→client チャネルを持たないため handler を使わない。
pub async fn connect(
    label: &str,
    spec: &McpServerSpec,
    handler: ServerRequestHandler,
) -> Result<Box<dyn Transport>> {
    use crate::mcp::config::Transport as Kind;
    match spec.transport()? {
        Kind::Stdio { command } => Ok(Box::new(
            StdioTransport::connect(label, command, spec, handler).await?,
        )),
        Kind::Http { url } => Ok(Box::new(HttpTransport::connect(label, url, spec)?)),
    }
}

/// Atomically increasing JSON-RPC id source (shared by client).
pub fn id_source() -> AtomicU64 {
    AtomicU64::new(1)
}

pub fn next_id(src: &AtomicU64) -> u64 {
    src.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_extracts_matching_id() {
        let body = "event: message\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n";
        let inc = extract_response_from_sse(body, 7).unwrap();
        assert_eq!(inc.id, Some(7));
        assert!(inc.result.is_some());
    }

    #[test]
    fn sse_falls_back_to_first_result_when_id_absent() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"a\":1}}\n";
        // request id 99 has no exact match → fallback to the id:1 result.
        let inc = extract_response_from_sse(body, 99).unwrap();
        assert_eq!(inc.id, Some(1));
    }

    #[test]
    fn sse_ignores_non_data_and_unparseable() {
        let body = ": comment\nevent: ping\ndata: not-json\n";
        assert!(extract_response_from_sse(body, 1).is_none());
    }
}
