// MCP サーバを `.mcp.json` から読み、各サーバへ stdio で接続し、
// 公開された tools を ToolRegistry に登録する。
//
// 起動失敗 / list_tools 失敗は warning に留め、REPL は続行する。

use std::sync::Arc;

use anyhow::Result;

use crate::mcp::client::McpClient;
use crate::mcp::config::McpServersConfig;
use crate::mcp::tool::{McpTool, namespaced};
use crate::tools::registry::ToolRegistry;

/// Load `.mcp.json` from CWD, connect each server, and register their tools.
/// Returns the live clients so the caller can keep them alive (Drop on session end).
pub async fn load_and_register(reg: &mut ToolRegistry) -> Result<LoadOutcome> {
    let cfg = match McpServersConfig::load_from_cwd()? {
        Some(c) => c,
        None => return Ok(LoadOutcome::default()),
    };

    let mut outcome = LoadOutcome::default();
    for (server_name, spec) in cfg.mcp_servers {
        match McpClient::connect_stdio(&server_name, &spec).await {
            Ok(client) => {
                let client = Arc::new(client);
                match client.list_tools().await {
                    Ok(tools) => {
                        for meta in tools {
                            let full = namespaced(&server_name, &meta.name);
                            let desc = meta.description.unwrap_or_default();
                            let schema = meta.input_schema.unwrap_or(serde_json::json!({}));
                            reg.register(Arc::new(McpTool::new(
                                full,
                                meta.name,
                                desc,
                                schema,
                                Arc::clone(&client),
                            )));
                            outcome.tools += 1;
                        }
                        outcome.servers += 1;
                        outcome.clients.push(client);
                    }
                    Err(e) => {
                        eprintln!("mcp[{server_name}]: list_tools failed: {e}");
                    }
                }
            }
            Err(e) => {
                eprintln!("mcp[{server_name}]: connect failed: {e}");
            }
        }
    }
    Ok(outcome)
}

#[derive(Default)]
pub struct LoadOutcome {
    pub servers: usize,
    pub tools: usize,
    /// Kept alive by the caller; on Drop the subprocess is killed.
    pub clients: Vec<Arc<McpClient>>,
}
