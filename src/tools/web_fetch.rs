// MVP 外: WebFetch。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct WebFetch;

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &'static str {
        "WebFetch"
    }
    fn description(&self) -> &'static str {
        "Fetch a URL and summarize (out of MVP scope)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        unimplemented!("WebFetch is out of MVP scope")
    }
}
