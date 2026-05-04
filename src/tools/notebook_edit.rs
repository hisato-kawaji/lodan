// MVP 外: NotebookEdit。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct NotebookEdit;

#[async_trait]
impl Tool for NotebookEdit {
    fn name(&self) -> &'static str {
        "NotebookEdit"
    }
    fn description(&self) -> &'static str {
        "Edit Jupyter notebook cells (out of MVP scope)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        unimplemented!("NotebookEdit is out of MVP scope")
    }
}
