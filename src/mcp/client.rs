// MCP クライアント (transport 非依存)。
// 高レベルの list/call と JSON-RPC request/notify のデコードを担い、ワイヤ送受信は
// `transport::Transport` (stdio / Streamable HTTP) に委譲する。

use std::sync::atomic::AtomicU64;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::Value;

use crate::mcp::config::McpServerSpec;
use crate::mcp::protocol::{
    CLIENT_NAME, CLIENT_VERSION, ClientCapabilities, ClientInfo, InitializeParams,
    InitializeResult, JsonRpcNotification, JsonRpcRequest, McpToolMeta, PROTOCOL_VERSION,
    PromptMeta, PromptsGetParams, PromptsGetResult, PromptsListParams, PromptsListResult,
    ResourceMeta, ResourcesListParams, ResourcesListResult, ResourcesReadParams,
    ResourcesReadResult, ToolsCallParams, ToolsCallResult, ToolsListParams, ToolsListResult,
};
use crate::mcp::transport::{self, Transport};
use crate::tools::ToolOutput;

pub struct McpClient {
    next_id: AtomicU64,
    server_label: String,
    transport: Box<dyn Transport>,
}

impl McpClient {
    pub async fn connect(label: &str, spec: &McpServerSpec) -> Result<Self> {
        let transport = transport::connect(label, spec).await?;
        let client = McpClient {
            next_id: transport::id_source(),
            server_label: label.to_string(),
            transport,
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

    pub async fn list_resources(&self) -> Result<Vec<ResourceMeta>> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = ResourcesListParams {
                cursor: cursor.as_deref(),
            };
            let resp: ResourcesListResult = self.request("resources/list", Some(&params)).await?;
            all.extend(resp.resources);
            match resp.next_cursor {
                Some(c) if !c.is_empty() => cursor = Some(c),
                _ => break,
            }
        }
        Ok(all)
    }

    pub async fn read_resource(&self, uri: &str) -> Result<ResourcesReadResult> {
        let params = ResourcesReadParams { uri };
        self.request("resources/read", Some(&params)).await
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
        let id = transport::next_id(&self.next_id);
        let payload = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        let line = serde_json::to_string(&payload).context("serializing JSON-RPC request")?;
        let inc = self.transport.send_request(id, line).await?;

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
        self.transport.send_notification(line).await
    }

    pub fn label(&self) -> &str {
        &self.server_label
    }
}
