// MVP 外: stdio/http transport の MCP クライアント。

use anyhow::Result;

pub struct McpClient;

impl McpClient {
    pub async fn connect_stdio(_command: &str, _args: &[String]) -> Result<Self> {
        unimplemented!("MCP client is out of MVP scope")
    }
    pub async fn connect_http(_url: &str) -> Result<Self> {
        unimplemented!("MCP client is out of MVP scope")
    }
    pub async fn list_tools(&self) -> Result<Vec<serde_json::Value>> {
        unimplemented!("MCP client is out of MVP scope")
    }
    pub async fn call_tool(
        &self,
        _name: &str,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value> {
        unimplemented!("MCP client is out of MVP scope")
    }
}
