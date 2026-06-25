// stdio transport MCP クライアント。
// - 子プロセスを spawn して stdin/stdout を双方向の JSON-RPC チャネルとして使う
// - request は id 払い出し → oneshot で応答待ち
// - stderr は読み捨て (将来 tracing 転送)
// - Drop で子プロセスを kill

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::timeout;

use crate::mcp::config::McpServerSpec;
use crate::mcp::protocol::{
    CLIENT_NAME, CLIENT_VERSION, ClientCapabilities, ClientInfo, ContentBlock, InitializeParams,
    InitializeResult, JsonRpcIncoming, JsonRpcNotification, JsonRpcRequest, McpToolMeta,
    PROTOCOL_VERSION, PromptMeta, PromptsGetParams, PromptsGetResult, PromptsListParams,
    PromptsListResult, ToolsCallParams, ToolsCallResult, ToolsListParams, ToolsListResult,
};
use crate::tools::ToolOutput;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcIncoming>>>>;

pub struct McpClient {
    next_id: AtomicU64,
    pending: Pending,
    /// Single-producer channel into the writer task.
    outbound: mpsc::UnboundedSender<String>,
    /// Owned so Drop kills the subprocess. Behind Mutex for kill().
    child: Mutex<Option<Child>>,
    server_label: String,
}

impl McpClient {
    pub async fn connect_stdio(label: &str, spec: &McpServerSpec) -> Result<Self> {
        let mut cmd = Command::new(&spec.command);
        cmd.args(&spec.args)
            .envs(spec.env.iter())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning MCP server `{}`", spec.command))?;

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
                if stdin.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if stdin.write_all(b"\n").await.is_err() {
                    break;
                }
                if stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        // Reader task: newline-delimited JSON from stdout → dispatch by id.
        let pending_clone = Arc::clone(&pending);
        let label_for_reader = label.to_string();
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
                if let Some(id) = inc.id {
                    let sender = pending_clone.lock().await.remove(&id);
                    if let Some(tx) = sender {
                        let _ = tx.send(inc);
                    } else {
                        tracing::debug!(server=%label_for_reader, id, "mcp: response with no waiter");
                    }
                } else {
                    // Notification — currently ignored.
                    tracing::debug!(
                        server=%label_for_reader,
                        method=?inc.method,
                        "mcp: notification ignored"
                    );
                }
            }
        });

        // Stderr drain (avoid filling the pipe buffer; capture for trace logging).
        let label_for_stderr = label.to_string();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                tracing::debug!(server=%label_for_stderr, "mcp[stderr] {}", line);
            }
        });

        let client = McpClient {
            next_id: AtomicU64::new(1),
            pending,
            outbound: outbound_tx,
            child: Mutex::new(Some(child)),
            server_label: label.to_string(),
        };

        client.handshake().await?;
        Ok(client)
    }

    async fn handshake(&self) -> Result<()> {
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            capabilities: ClientCapabilities::default(),
            client_info: ClientInfo {
                name: CLIENT_NAME,
                version: CLIENT_VERSION,
            },
        };
        let _: InitializeResult = self.request("initialize", Some(&params)).await?;
        // notifications/initialized — no response expected.
        self.notify::<()>("notifications/initialized", None).await?;
        Ok(())
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolMeta>> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = ToolsListParams {
                cursor: cursor.as_deref(),
            };
            let resp: ToolsListResult = self.request("tools/list", Some(&params)).await?;
            all.extend(resp.tools);
            match resp.next_cursor {
                Some(c) if !c.is_empty() => cursor = Some(c),
                _ => break,
            }
        }
        Ok(all)
    }

    pub async fn list_prompts(&self) -> Result<Vec<PromptMeta>> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = PromptsListParams {
                cursor: cursor.as_deref(),
            };
            let resp: PromptsListResult = self.request("prompts/list", Some(&params)).await?;
            all.extend(resp.prompts);
            match resp.next_cursor {
                Some(c) if !c.is_empty() => cursor = Some(c),
                _ => break,
            }
        }
        Ok(all)
    }

    pub async fn get_prompt(&self, name: &str, arguments: Value) -> Result<PromptsGetResult> {
        let params = PromptsGetParams { name, arguments };
        self.request("prompts/get", Some(&params)).await
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolOutput> {
        let params = ToolsCallParams { name, arguments };
        let result: ToolsCallResult = self.request("tools/call", Some(&params)).await?;
        let text = result.flatten_text();
        let content = if text.is_empty() && !result.is_error {
            "(no content)".to_string()
        } else {
            text
        };
        Ok(if result.is_error {
            ToolOutput::error(content)
        } else {
            ToolOutput::ok(content)
        })
    }

    async fn request<P: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Option<&P>,
    ) -> Result<R> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let payload = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        let line = serde_json::to_string(&payload).context("serializing JSON-RPC request")?;
        self.outbound
            .send(line)
            .map_err(|_| anyhow!("mcp[{}]: writer channel closed", self.server_label))?;

        let inc = match timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => {
                return Err(anyhow!(
                    "mcp[{}]: response channel dropped for {} (server died?)",
                    self.server_label,
                    method
                ));
            }
            Err(_) => {
                // Drop our pending slot so a late response doesn't leak.
                self.pending.lock().await.remove(&id);
                return Err(anyhow!(
                    "mcp[{}]: {} timed out after {:?}",
                    self.server_label,
                    method,
                    REQUEST_TIMEOUT
                ));
            }
        };

        if let Some(err) = inc.error {
            return Err(anyhow!(
                "mcp[{}]: {} → JSON-RPC error {}: {}",
                self.server_label,
                method,
                err.code,
                err.message
            ));
        }
        let result = inc.result.unwrap_or(Value::Null);
        serde_json::from_value(result)
            .with_context(|| format!("mcp[{}]: decoding {} result", self.server_label, method))
    }

    async fn notify<P: Serialize>(&self, method: &str, params: Option<&P>) -> Result<()> {
        let payload = JsonRpcNotification {
            jsonrpc: "2.0",
            method,
            params,
        };
        let line = serde_json::to_string(&payload).context("serializing JSON-RPC notification")?;
        self.outbound
            .send(line)
            .map_err(|_| anyhow!("mcp[{}]: writer channel closed", self.server_label))?;
        Ok(())
    }

    pub fn label(&self) -> &str {
        &self.server_label
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Best effort: take the child synchronously and let kill_on_drop finish it.
        if let Ok(mut guard) = self.child.try_lock()
            && let Some(child) = guard.as_mut()
        {
            let _ = child.start_kill();
        }
    }
}

// expose ContentBlock for downstream/tests if needed
#[allow(dead_code)]
pub(crate) type _ContentBlockAlias = ContentBlock;
