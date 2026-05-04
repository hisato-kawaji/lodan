// MVP 外: AskUserQuestion。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct AskUserQuestion;

#[async_trait]
impl Tool for AskUserQuestion {
    fn name(&self) -> &'static str {
        "AskUserQuestion"
    }
    fn description(&self) -> &'static str {
        "Ask the user a structured question (out of MVP scope)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        unimplemented!("AskUserQuestion is out of MVP scope")
    }
}
