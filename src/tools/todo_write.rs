// MVP 外: TodoWrite。換装のみ・呼び出しは registry 内でコメントアウト。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct TodoWrite;

#[async_trait]
impl Tool for TodoWrite {
    fn name(&self) -> &'static str {
        "TodoWrite"
    }
    fn description(&self) -> &'static str {
        "Manage a structured task list (out of MVP scope)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        unimplemented!("TodoWrite is out of MVP scope")
    }
}
