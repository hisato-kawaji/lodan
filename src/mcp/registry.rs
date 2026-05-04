// MVP 外: MCP サーバー登録/起動。

use super::McpServerSpec;
use anyhow::Result;
use std::collections::HashMap;

pub async fn start_configured(_servers: &HashMap<String, McpServerSpec>) -> Result<()> {
    unimplemented!("MCP registry is out of MVP scope")
}
