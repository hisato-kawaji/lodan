//! MCP transport 抽象。JSON-RPC の 1 行を送って応答 (`JsonRpcIncoming`) を受け取る
//! ワイヤ層を差し替え可能にする。高レベルの request/notify デコードは `client.rs`。
//!
//! - `StdioTransport`: 子プロセスの stdin/stdout を newline-delimited JSON で使う
//! - `HttpTransport`: Streamable HTTP。POST で JSON-RPC を送り、`application/json` か
//!   `text/event-stream` (SSE) の応答を受ける。`Mcp-Session-Id` を引き継ぐ
//!
//! server→client リクエスト (roots/list, sampling/createMessage, elicitation/create) には
//! `ServerRequestHandler` 経由で応答する。stdio は stdout から読み、HTTP は POST の応答の SSE と、
//! 初期化のあとに開く GET の SSE ストリームから読んで、答えを POST で返す (#83)。

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

// clippy 1.99 の `double_must_use`: `#[async_trait]` が `async fn` を `#[must_use]` つきの
// `Pin<Box<dyn Future>>` に展開するため、trait 側の must_use と二重になる。展開結果の話で
// こちらに直せるものが無いので、trait 定義で抑える (CI の toolchain は stable 無ピン)。
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait Transport: Send + Sync {
    /// `line` (JSON-RPC リクエスト) を送り、`id` で相関した応答を返す。`timeout` を超えたら諦める
    /// (ツール呼び出しは `toolTimeoutSecs`、それ以外は `REQUEST_TIMEOUT`)。
    async fn send_request(
        &self,
        id: u64,
        line: String,
        timeout: Duration,
    ) -> Result<JsonRpcIncoming>;
    /// 通知 (応答なし) を送る。
    async fn send_notification(&self, line: String) -> Result<()>;
    /// サーバから `notifications/tools/list_changed` が届いていたら true を返し、印を消す (#83)。
    /// 受信の口が無い transport (HTTP の POST だけ) は常に false。
    fn take_tools_changed(&self) -> bool {
        false
    }
    /// `notifications/initialized` を送った直後に呼ばれる。HTTP はここで GET のストリームを開く
    /// (セッション ID は initialize の応答で決まるので、それより前には開けない)。
    fn on_initialized(&self) {}
}

/// サーバから届いた 1 フレームの種類。server→client リクエストの id は文字列でもよい (仕様上
/// `string | number`) ので、`JsonRpcIncoming` (id が数値) に通す前に生の JSON で見分ける。
enum Frame {
    /// server→client リクエスト。答えを返す。
    Request {
        id: serde_json::Value,
        method: String,
        params: Option<serde_json::Value>,
    },
    /// 通知 (答えない)。
    Notification { method: String },
    /// 自分のリクエストへの応答。
    Response(JsonRpcIncoming),
}

fn classify(text: &str) -> Option<Frame> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let method = v.get("method").and_then(|m| m.as_str()).map(str::to_string);
    let id = v.get("id").filter(|id| !id.is_null()).cloned();
    match (method, id) {
        (Some(method), Some(id)) => Some(Frame::Request {
            id,
            method,
            params: v.get("params").cloned(),
        }),
        (Some(method), None) => Some(Frame::Notification { method }),
        (None, Some(_)) => serde_json::from_value(v).ok().map(Frame::Response),
        (None, None) => None,
    }
}

/// server→client リクエストに `handler` で答えた JSON-RPC 応答 (stdio / HTTP で共通)。
async fn answer(
    handler: &ServerRequestHandler,
    id: serde_json::Value,
    method: String,
    params: Option<serde_json::Value>,
) -> serde_json::Value {
    match handler(method.clone(), params).await {
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
    }
}

// ---------------- stdio ----------------

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcIncoming>>>>;

pub struct StdioTransport {
    label: String,
    outbound: mpsc::UnboundedSender<String>,
    pending: Pending,
    child: Mutex<Option<Child>>,
    /// `notifications/tools/list_changed` を受けた印。次のターンの入口で読まれる (#83)。
    tools_changed: Arc<std::sync::atomic::AtomicBool>,
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

        // Reader task: newline-delimited JSON from stdout。フレームの種類は `classify`。
        let pending_clone = Arc::clone(&pending);
        let label_for_reader = label.to_string();
        let tools_changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tools_changed_for_reader = Arc::clone(&tools_changed);
        let reader_outbound = outbound_tx.clone();
        let handler = Arc::clone(&handler);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let Some(frame) = classify(&line) else {
                    tracing::debug!(server=%label_for_reader, "mcp: unparseable frame");
                    continue;
                };
                match frame {
                    // server→client リクエスト (roots/list, sampling/createMessage 等)。
                    // handler は async (sampling は LLM 呼び出しを伴う) なので、reader を
                    // 塞がないよう要求ごとに task を spawn して応答する。
                    Frame::Request { id, method, params } => {
                        let handler = Arc::clone(&handler);
                        let out = reader_outbound.clone();
                        tokio::spawn(async move {
                            let response = answer(&handler, id, method, params).await;
                            let _ = out.send(response.to_string());
                        });
                    }
                    Frame::Notification { method } => {
                        if method == "notifications/tools/list_changed" {
                            tools_changed_for_reader
                                .store(true, std::sync::atomic::Ordering::SeqCst);
                        } else {
                            tracing::debug!(server=%label_for_reader, method, "mcp: notification ignored");
                        }
                    }
                    Frame::Response(inc) => {
                        let Some(id) = inc.id else {
                            tracing::debug!(server=%label_for_reader, "mcp: response with a non-numeric id");
                            continue;
                        };
                        if let Some(tx) = pending_clone.lock().await.remove(&id) {
                            let _ = tx.send(inc);
                        } else {
                            tracing::debug!(server=%label_for_reader, id, "mcp: response with no waiter");
                        }
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
            tools_changed,
        })
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn send_request(
        &self,
        id: u64,
        line: String,
        limit: Duration,
    ) -> Result<JsonRpcIncoming> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.outbound
            .send(line)
            .map_err(|_| anyhow!("mcp[{}]: writer channel closed", self.label))?;
        match timeout(limit, rx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => Err(anyhow!(
                "mcp[{}]: response channel dropped (server died?)",
                self.label
            )),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(anyhow!(
                    "mcp[{}]: request timed out after {}s",
                    self.label,
                    limit.as_secs()
                ))
            }
        }
    }

    async fn send_notification(&self, line: String) -> Result<()> {
        self.outbound
            .send(line)
            .map_err(|_| anyhow!("mcp[{}]: writer channel closed", self.label))
    }

    fn take_tools_changed(&self) -> bool {
        self.tools_changed
            .swap(false, std::sync::atomic::Ordering::SeqCst)
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
/// GET のストリームが続けて失敗したら諦める回数 (間隔は 1, 2, 4, 8 … 秒)。
const STREAM_MAX_FAILURES: u32 = 5;

pub struct HttpTransport {
    inner: Arc<HttpInner>,
    /// GET のストリームを読むタスク。transport を捨てたら止める。
    listener: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// POST と GET のストリームで共有する部分。
struct HttpInner {
    label: String,
    url: String,
    client: reqwest::Client,
    /// GET のストリーム用。全体のタイムアウトを付けない (開いたままが正常)。
    stream_client: reqwest::Client,
    headers: Vec<(String, String)>,
    session_id: Mutex<Option<String>>,
    handler: ServerRequestHandler,
    /// `notifications/tools/list_changed` を受けた印 (#83)。
    tools_changed: Arc<std::sync::atomic::AtomicBool>,
}

impl HttpTransport {
    pub fn connect(
        label: &str,
        url: &str,
        spec: &McpServerSpec,
        handler: ServerRequestHandler,
    ) -> Result<Self> {
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
        let stream_client = reqwest::Client::builder()
            .connect_timeout(REQUEST_TIMEOUT)
            .build()
            .context("building HTTP client")?;
        let headers = spec
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Ok(Self {
            inner: Arc::new(HttpInner {
                label: label.to_string(),
                url: url.to_string(),
                client,
                stream_client,
                headers,
                session_id: Mutex::new(None),
                handler,
                tools_changed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }),
            listener: std::sync::Mutex::new(None),
        })
    }
}

impl HttpInner {
    async fn post(&self, line: String, limit: Duration) -> Result<reqwest::Response> {
        let mut req = self
            .client
            .post(&self.url)
            .timeout(limit)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(line);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Some(sid) = self.session_id.lock().await.as_ref() {
            req = req.header(SESSION_HEADER, sid);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("mcp[{}]: HTTP POST to {}", self.label, self.url))?;
        // サーバが割り当てたセッション ID を引き継ぐ。
        if let Some(sid) = resp.headers().get(SESSION_HEADER)
            && let Ok(s) = sid.to_str()
        {
            *self.session_id.lock().await = Some(s.to_string());
        }
        Ok(resp)
    }

    /// 応答以外のフレームを処理する。リクエストには別タスクで答えて POST で返し (応答を待っている
    /// ストリームの読み取りを塞がない)、通知は印にする。応答ならそのまま返す。
    fn handle(self: &Arc<Self>, frame: Frame) -> Option<JsonRpcIncoming> {
        match frame {
            Frame::Request { id, method, params } => {
                let inner = Arc::clone(self);
                tokio::spawn(async move {
                    let response = answer(&inner.handler, id, method, params).await;
                    if let Err(e) = inner.post(response.to_string(), REQUEST_TIMEOUT).await {
                        tracing::debug!(server=%inner.label, "mcp: could not post the answer: {e:#}");
                    }
                });
                None
            }
            Frame::Notification { method } => {
                if method == "notifications/tools/list_changed" {
                    self.tools_changed
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                } else {
                    tracing::debug!(server=%self.label, method, "mcp: notification ignored");
                }
                None
            }
            Frame::Response(inc) => Some(inc),
        }
    }

    /// server→client のストリーム (GET) を読み続ける。サーバが対応していない (405) / セッションが
    /// 無い (4xx) なら黙ってやめる。切れたら `Last-Event-ID` を付けて張り直す。
    async fn listen(self: Arc<Self>) {
        let mut last_event_id: Option<String> = None;
        let mut failures = 0u32;
        loop {
            let mut req = self
                .stream_client
                .get(&self.url)
                .header("Accept", "text/event-stream");
            for (k, v) in &self.headers {
                req = req.header(k, v);
            }
            if let Some(sid) = self.session_id.lock().await.as_ref() {
                req = req.header(SESSION_HEADER, sid);
            }
            if let Some(last) = &last_event_id {
                req = req.header("Last-Event-ID", last);
            }
            match req.send().await {
                Ok(mut resp) if resp.status().is_success() && is_event_stream(&resp) => {
                    let mut sse = SseParser::default();
                    let mut delivered = false;
                    while let Ok(Some(chunk)) = resp.chunk().await {
                        for event in sse.feed(&chunk) {
                            delivered = true;
                            if let Some(frame) = classify(&event) {
                                // GET のストリームに応答は来ない (来ても待っている人がいない)。
                                let _ = self.handle(frame);
                            }
                        }
                        if sse.last_event_id.is_some() {
                            last_event_id = sse.last_event_id.clone();
                        }
                    }
                    // 何も届かずに閉じたストリームは失敗に数える (すぐ閉じるサーバに毎秒張り直さない)。
                    failures = if delivered { 0 } else { failures + 1 };
                }
                Ok(resp) if resp.status().is_client_error() => {
                    tracing::debug!(server=%self.label, status=%resp.status(), "mcp: no server→client stream");
                    return;
                }
                Ok(resp) => {
                    tracing::debug!(server=%self.label, status=%resp.status(), "mcp: stream refused");
                    failures += 1;
                }
                Err(e) => {
                    tracing::debug!(server=%self.label, "mcp: stream failed: {e:#}");
                    failures += 1;
                }
            }
            if failures >= STREAM_MAX_FAILURES {
                tracing::debug!(server=%self.label, "mcp: giving up on the server→client stream");
                return;
            }
            tokio::time::sleep(Duration::from_secs(1 << failures.min(4))).await;
        }
    }
}

fn is_event_stream(resp: &reqwest::Response) -> bool {
    resp.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.contains("text/event-stream"))
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send_request(
        &self,
        id: u64,
        line: String,
        limit: Duration,
    ) -> Result<JsonRpcIncoming> {
        let inner = &self.inner;
        let mut resp = inner.post(line, limit).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "mcp[{}]: HTTP {} from server: {}",
                inner.label,
                status,
                body.trim()
            ));
        }
        if !is_event_stream(&resp) {
            let body = resp
                .text()
                .await
                .with_context(|| format!("mcp[{}]: reading HTTP response body", inner.label))?;
            return serde_json::from_str(&body)
                .with_context(|| format!("mcp[{}]: decoding JSON response", inner.label));
        }
        // SSE: 応答より前に server→client リクエスト (sampling / elicitation …) が来ることがある。
        // サーバはその答えを待ってから応答を書くので、届いた順に処理しながら応答を探す
        // (本文を全部読んでから探すと、互いに待ったまま固まる)。
        let mut sse = SseParser::default();
        let mut fallback: Option<JsonRpcIncoming> = None;
        while let Some(chunk) = resp
            .chunk()
            .await
            .with_context(|| format!("mcp[{}]: reading SSE stream", inner.label))?
        {
            for event in sse.feed(&chunk) {
                let Some(inc) = classify(&event).and_then(|f| inner.handle(f)) else {
                    continue;
                };
                if inc.id == Some(id) {
                    return Ok(inc);
                }
                if fallback.is_none() && (inc.result.is_some() || inc.error.is_some()) {
                    fallback = Some(inc);
                }
            }
        }
        fallback.ok_or_else(|| anyhow!("mcp[{}]: no JSON-RPC response in SSE stream", inner.label))
    }

    async fn send_notification(&self, line: String) -> Result<()> {
        // 通知は 202 Accepted 等。本文があっても無視する。
        self.inner.post(line, REQUEST_TIMEOUT).await?;
        Ok(())
    }

    fn take_tools_changed(&self) -> bool {
        self.inner
            .tools_changed
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    fn on_initialized(&self) {
        if let Ok(mut listener) = self.listener.lock()
            && listener.is_none()
        {
            *listener = Some(tokio::spawn(Arc::clone(&self.inner).listen()));
        }
    }
}

impl Drop for HttpTransport {
    fn drop(&mut self) {
        if let Ok(mut listener) = self.listener.lock()
            && let Some(task) = listener.take()
        {
            task.abort();
        }
    }
}

/// SSE を少しずつ読んで、イベントごとの `data` を返す。`data:` 行は改行でつなぎ、空行で 1 イベント。
/// チャンクの境目は行の途中 (UTF-8 の途中) でもよい。
#[derive(Default)]
struct SseParser {
    buf: Vec<u8>,
    data: Vec<String>,
    /// 最後に見た `id:` (張り直すときの `Last-Event-ID`)。
    last_event_id: Option<String>,
}

impl SseParser {
    fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(nl) = self.buf.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(std::mem::take(&mut self.data).join("\n"));
                }
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (line, ""),
            };
            match field {
                "data" => self.data.push(value.to_string()),
                "id" => self.last_event_id = Some(value.to_string()),
                // コメント (`: keep-alive`)、`event:`、`retry:` は使わない。
                _ => {}
            }
        }
        events
    }
}

// ---------------- dispatch ----------------

/// spec の transport 種別に応じて接続し、boxed transport を返す。
/// `handler` は server→client リクエスト (roots/list 等) の応答に使う。
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
        Kind::Http { url } => Ok(Box::new(HttpTransport::connect(label, url, spec, handler)?)),
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
    fn sse_events_are_split_on_blank_lines_even_across_chunks() {
        let mut p = SseParser::default();
        // 行の途中・CRLF・コメント・複数行の data。
        let mut events = p.feed(b"event: message\r\nid: e1\r\ndata: {\"a\":");
        assert!(events.is_empty());
        events.extend(p.feed(b"1}\r\n\r\n: keep-alive\n\ndata: x\ndata: y\n\n"));
        assert_eq!(events, vec!["{\"a\":1}".to_string(), "x\ny".to_string()]);
        assert_eq!(p.last_event_id.as_deref(), Some("e1"));
        // 終わっていないイベントは出さない。
        assert!(p.feed(b"data: z\n").is_empty());
    }

    #[test]
    fn frames_with_string_ids_are_requests_not_garbage() {
        match classify(r#"{"jsonrpc":"2.0","id":"s-1","method":"roots/list"}"#) {
            Some(Frame::Request { id, method, .. }) => {
                assert_eq!(id, serde_json::json!("s-1"));
                assert_eq!(method, "roots/list");
            }
            _ => panic!("a request"),
        }
        assert!(matches!(
            classify(r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#),
            Some(Frame::Notification { .. })
        ));
        assert!(matches!(
            classify(r#"{"jsonrpc":"2.0","id":7,"result":{}}"#),
            Some(Frame::Response(r)) if r.id == Some(7)
        ));
        assert!(classify("not json").is_none());
        assert!(classify(r#"{"jsonrpc":"2.0"}"#).is_none());
    }
}
