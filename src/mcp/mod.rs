// MVP 外: MCP (Model Context Protocol) クライアント抽象。

pub mod client;
pub mod registry;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServerSpec {
    Stdio { command: String, args: Vec<String> },
    Http { url: String },
}
