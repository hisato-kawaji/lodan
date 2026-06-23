use async_trait::async_trait;
use std::path::PathBuf;

use super::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct Write;

#[async_trait]
impl Tool for Write {
    fn name(&self) -> &str {
        "Write"
    }
    fn description(&self) -> &str {
        "Create or overwrite a file. For existing files, you must Read first."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string", "description": "Absolute file path" },
                "content": { "type": "string", "description": "Full file content" }
            },
            "required": ["path", "content"]
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'path'".into()))?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'content'".into()))?;

        let abs = PathBuf::from(path);
        let abs = if abs.is_absolute() {
            abs
        } else {
            ctx.cwd.join(abs)
        };

        if abs.exists() && !ctx.was_read(&abs) {
            return Err(ToolError::NotReadYet(abs.display().to_string()));
        }

        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&abs, content).await?;
        ctx.mark_read(&abs);

        Ok(ToolOutput::ok(format!(
            "wrote {} ({} bytes)",
            abs.display(),
            content.len()
        )))
    }
}
