// MCP の公開ツールを lodan の Tool trait に橋渡しするラッパ。
// すべて destructive 扱い (ユーザ合意済): permission gate を毎回通す。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::mcp::client::McpClient;
use crate::tools::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct McpTool {
    full_name: String,
    upstream_name: String,
    description: String,
    schema: Value,
    client: Arc<McpClient>,
}

impl McpTool {
    pub fn new(
        full_name: String,
        upstream_name: String,
        description: String,
        schema: Value,
        client: Arc<McpClient>,
    ) -> Self {
        Self {
            full_name,
            upstream_name,
            description,
            schema,
            client,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.full_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        // Fall back to an empty object schema so LLMs that strict-validate
        // still see a valid JSON Schema object.
        if self.schema.is_object() {
            self.schema.clone()
        } else {
            serde_json::json!({ "type": "object" })
        }
    }

    fn is_destructive(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        self.client
            .call_tool(&self.upstream_name, args)
            .await
            .map_err(|e| ToolError::Other(format!("mcp call failed: {e}")))
    }
}

pub fn namespaced(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_format() {
        assert_eq!(namespaced("fs", "read_file"), "mcp__fs__read_file");
    }
}
