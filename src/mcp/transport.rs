//! MCP transport 抽象。JSON-RPC の 1 行を送って応答 (`JsonRpcIncoming`) を受け取る
//! ワイヤ層を差し替え可能にする。高レベルの request/notify デコードは `client.rs`。
//!
//! - `StdioTransport`: 子プロセスの stdin/stdout を newline-delimited JSON で使う
//! - `HttpTransport`: Streamable HTTP。POST で JSON-RPC を送り、`application/json` か
//!   `text/event-stream` (SSE) の応答を受ける。`Mcp-Session-Id` を引き継ぐ
//!
//! どちらも server→client リクエスト (roots/list, sampling/createMessage, elicitation/create) に
//! `ServerRequestHandler` 経由で応答する。stdio は stdout から、HTTP は POST の SSE 応答と
//! GET で開く SSE ストリームから受け取り、応答は別の POST で返す (#83)。

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
    fn take_tools_changed(&self) -> bool {
        false
    }
    /// 初期化が済んだ後に呼ばれる。サーバが自分から送るメッセージを受ける口を開く
    /// (HTTP の GET ストリーム)。stdio は最初から stdout を読んでいるので何もしない。
    fn start_listening(&self) {}
}

/// server→client リクエストへの JSON-RPC 応答。
fn handler_response(id: u64, method: &str, outcome: HandlerOutcome) -> serde_json::Value {
    match outcome {
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

        // Reader task: newline-delimited JSON from stdout.
        // 三分岐: method+id = server→client リクエスト (応答する) /
        //         id のみ = 自分のリクエストへの応答 (pending へ) /
        //         method のみ = 通知 (無視)。
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
                            let outcome = handler(method.clone(), params).await;
                            let response = handler_response(id, &method, outcome);
                            let _ = out.send(response.to_string());
                        });
                    }
                    (Some(method), None) => {
                        if method == "notifications/tools/list_changed" {
                            tools_changed_for_reader
                                .store(true, std::sync::atomic::Ordering::SeqCst);
                        } else {
                            tracing::debug!(server=%label_for_reader, method, "mcp: notification ignored");
                        }
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
/// GET ストリームが切れたときの再接続の待ち時間の上限。
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);

pub struct HttpTransport {
    shared: Arc<HttpShared>,
    /// GET ストリームを読むタスク。transport を捨てたら止める。
    listener: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// POST と GET ストリームのタスクが共有する接続の状態。
struct HttpShared {
    label: String,
    url: String,
    client: reqwest::Client,
    /// GET ストリーム用。全体のタイムアウトを持たない (サーバが送ってくるまで開いたまま)。
    stream_client: reqwest::Client,
    headers: Vec<(String, String)>,
    session_id: Mutex<Option<String>>,
    handler: ServerRequestHandler,
    /// `notifications/tools/list_changed` を受けた印 (stdio と同じ扱い)。
    tools_changed: std::sync::atomic::AtomicBool,
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
            shared: Arc::new(HttpShared {
                label: label.to_string(),
                url: url.to_string(),
                client,
                stream_client,
                headers,
                session_id: Mutex::new(None),
                handler,
                tools_changed: std::sync::atomic::AtomicBool::new(false),
            }),
            listener: std::sync::Mutex::new(None),
        })
    }
}

impl HttpShared {
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
        self.remember_session(&resp).await;
        Ok(resp)
    }

    /// サーバが割り当てたセッション ID を引き継ぐ。
    async fn remember_session(&self, resp: &reqwest::Response) {
        if let Some(sid) = resp.headers().get(SESSION_HEADER)
            && let Ok(s) = sid.to_str()
        {
            *self.session_id.lock().await = Some(s.to_string());
        }
    }

    /// サーバが自分から送るメッセージ用の GET ストリームを開く。サーバが提供しない (405 など)
    /// ときは `Ok(None)` — 再接続しない。
    async fn open_stream(&self) -> Result<Option<reqwest::Response>> {
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
        let resp = req
            .send()
            .await
            .with_context(|| format!("mcp[{}]: HTTP GET to {}", self.label, self.url))?;
        let is_sse = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.contains("text/event-stream"));
        if !resp.status().is_success() || !is_sse {
            tracing::debug!(server=%self.label, status=%resp.status(), "mcp: no server stream");
            return Ok(None);
        }
        Ok(Some(resp))
    }

    /// SSE を順に読む。サーバからのリクエストと通知はその場で処理し、`want` の id に一致する
    /// 応答が来たらそれを返す (残りは読まない)。一致が無いまま終われば、最初に見つかった
    /// result / error を持つ応答を返す。`seen` は読んだメッセージの数を足していく。
    async fn read_sse<S, B, E>(
        self: &Arc<Self>,
        body: S,
        want: Option<u64>,
        seen: &mut usize,
    ) -> Option<JsonRpcIncoming>
    where
        S: futures_util::Stream<Item = std::result::Result<B, E>> + Unpin,
        B: AsRef<[u8]>,
        E: std::fmt::Display,
    {
        use eventsource_stream::Eventsource;
        use futures_util::StreamExt;
        let mut stream = body.eventsource();
        let mut fallback: Option<JsonRpcIncoming> = None;
        while let Some(event) = stream.next().await {
            let event = match event {
                Ok(event) => event,
                Err(e) => {
                    tracing::debug!(server=%self.label, %e, "mcp: SSE stream error");
                    break;
                }
            };
            let Ok(inc) = serde_json::from_str::<JsonRpcIncoming>(event.data.trim()) else {
                continue;
            };
            *seen += 1;
            if inc.method.is_some() {
                self.dispatch(inc);
                continue;
            }
            if want.is_some() && inc.id == want {
                return Some(inc);
            }
            if fallback.is_none() && (inc.result.is_some() || inc.error.is_some()) {
                fallback = Some(inc);
            }
        }
        fallback
    }

    /// サーバからのリクエスト・通知を処理する。リクエストへの応答は別の POST で返す
    /// (応答を待たずに次の SSE を読めるよう、タスクに切り出す — elicitation は利用者を待つ)。
    fn dispatch(self: &Arc<Self>, inc: JsonRpcIncoming) {
        let Some(method) = inc.method else {
            return;
        };
        match inc.id {
            Some(id) => {
                let shared = Arc::clone(self);
                tokio::spawn(async move {
                    let outcome = (shared.handler)(method.clone(), inc.params).await;
                    let response = handler_response(id, &method, outcome);
                    if let Err(e) = shared.post(response.to_string(), REQUEST_TIMEOUT).await {
                        tracing::debug!(server=%shared.label, %e, "mcp: could not send the response");
                    }
                });
            }
            None if method == "notifications/tools/list_changed" => {
                self.tools_changed
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            None => tracing::debug!(server=%self.label, method, "mcp: notification ignored"),
        }
    }

    /// GET ストリームを読み続ける。切れたら待ってから開き直す (待ち時間は倍々、上限 30 秒。
    /// 何か届いたら 1 秒に戻す)。サーバが提供しないと分かったら終える。
    async fn listen(self: Arc<Self>) {
        let mut delay = Duration::from_secs(1);
        loop {
            match self.open_stream().await {
                Ok(Some(resp)) => {
                    let mut seen = 0;
                    self.read_sse(resp.bytes_stream(), None, &mut seen).await;
                    if seen > 0 {
                        delay = Duration::from_secs(1);
                    }
                }
                Ok(None) => return,
                Err(e) => tracing::debug!(server=%self.label, %e, "mcp: server stream failed"),
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(MAX_RECONNECT_DELAY);
        }
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send_request(
        &self,
        id: u64,
        line: String,
        limit: Duration,
    ) -> Result<JsonRpcIncoming> {
        let shared = &self.shared;
        let resp = shared.post(line, limit).await?;
        let status = resp.status();
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
                shared.label,
                status,
                body.trim()
            ));
        }
        if ctype.contains("text/event-stream") {
            // 応答の前に、サーバからのリクエスト (elicitation など) が同じストリームに来ることがある。
            return shared
                .read_sse(resp.bytes_stream(), Some(id), &mut 0)
                .await
                .ok_or_else(|| {
                    anyhow!("mcp[{}]: no JSON-RPC response in SSE stream", shared.label)
                });
        }
        let body = resp
            .text()
            .await
            .with_context(|| format!("mcp[{}]: reading HTTP response body", shared.label))?;
        serde_json::from_str(&body)
            .with_context(|| format!("mcp[{}]: decoding JSON response", shared.label))
    }

    async fn send_notification(&self, line: String) -> Result<()> {
        // 通知は 202 Accepted 等。本文があっても無視する。
        self.shared.post(line, REQUEST_TIMEOUT).await?;
        Ok(())
    }

    fn take_tools_changed(&self) -> bool {
        self.shared
            .tools_changed
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    fn start_listening(&self) {
        let Ok(mut listener) = self.listener.lock() else {
            return;
        };
        if listener.is_none() {
            *listener = Some(tokio::spawn(Arc::clone(&self.shared).listen()));
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

    fn shared() -> Arc<HttpShared> {
        let spec: McpServerSpec = serde_json::from_value(serde_json::json!({
            "url": "http://127.0.0.1:9/"
        }))
        .unwrap();
        let handler: ServerRequestHandler =
            Arc::new(|_, _| Box::pin(async { HandlerOutcome::Unhandled }) as _);
        let transport = HttpTransport::connect("t", "http://127.0.0.1:9/", &spec, handler).unwrap();
        Arc::clone(&transport.shared)
    }

    async fn read(body: &str, want: Option<u64>) -> (Option<JsonRpcIncoming>, usize) {
        let chunks: Vec<std::result::Result<Vec<u8>, std::io::Error>> =
            vec![Ok(body.as_bytes().to_vec())];
        let mut seen = 0;
        let inc = shared()
            .read_sse(futures_util::stream::iter(chunks), want, &mut seen)
            .await;
        (inc, seen)
    }

    #[tokio::test]
    async fn sse_extracts_matching_id() {
        let body = "event: message\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n";
        let (inc, _) = read(body, Some(7)).await;
        let inc = inc.unwrap();
        assert_eq!(inc.id, Some(7));
        assert!(inc.result.is_some());
    }

    #[tokio::test]
    async fn sse_falls_back_to_first_result_when_id_absent() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"a\":1}}\n\n";
        // request id 99 has no exact match → fallback to the id:1 result.
        let (inc, _) = read(body, Some(99)).await;
        assert_eq!(inc.unwrap().id, Some(1));
    }

    #[tokio::test]
    async fn sse_ignores_non_data_and_unparseable() {
        let body = ": comment\n\nevent: ping\ndata: not-json\n\n";
        let (inc, seen) = read(body, Some(1)).await;
        assert!(inc.is_none());
        assert_eq!(seen, 0);
    }

    /// 応答の前に来たサーバからの通知は、読み飛ばさずに処理する (#83)。
    #[tokio::test]
    async fn sse_handles_notifications_that_come_before_the_response() {
        let shared = shared();
        let body = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{}}\n\n";
        let chunks: Vec<std::result::Result<Vec<u8>, std::io::Error>> =
            vec![Ok(body.as_bytes().to_vec())];
        let mut seen = 0;
        let inc = shared
            .read_sse(futures_util::stream::iter(chunks), Some(3), &mut seen)
            .await;
        assert_eq!(inc.unwrap().id, Some(3));
        assert_eq!(seen, 2);
        assert!(
            shared
                .tools_changed
                .load(std::sync::atomic::Ordering::SeqCst)
        );
    }
}
