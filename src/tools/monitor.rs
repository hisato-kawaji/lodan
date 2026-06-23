// MVP 外: Monitor (バックグラウンド出力監視)。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct Monitor;

#[async_trait]
impl Tool for Monitor {
    fn name(&self) -> &str {
        "Monitor"
    }
    fn description(&self) -> &str {
        "Monitor a background process's output (out of MVP scope)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        unimplemented!("Monitor is out of MVP scope")
    }
}
