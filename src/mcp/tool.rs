// MCP の公開ツールを lodan の Tool trait に橋渡しするラッパ。
// 既定では destructive 扱い (permission gate を毎回通す)。`trustAnnotations` のサーバでだけ、
// `readOnlyHint: true` のツールを read-only として扱う (#83)。

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
    /// サーバが `readOnlyHint: true` と申告し、利用者がそのサーバの申告を信じる設定にしたもの。
    read_only: bool,
    timeout: std::time::Duration,
    max_output_bytes: usize,
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
            read_only: false,
            timeout: std::time::Duration::from_secs(crate::mcp::config::DEFAULT_TOOL_TIMEOUT_SECS),
            max_output_bytes: crate::mcp::config::DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    /// `annotations.readOnlyHint == true` なら read-only にする。呼ぶのは `trustAnnotations` の
    /// サーバに限ること (呼び出し側の責任)。
    pub fn with_annotations(mut self, annotations: Option<&Value>) -> Self {
        self.read_only = annotations
            .and_then(|a| a.get("readOnlyHint"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self
    }

    pub fn with_limits(mut self, timeout: std::time::Duration, max_output_bytes: usize) -> Self {
        self.timeout = timeout;
        self.max_output_bytes = max_output_bytes;
        self
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
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
        !self.read_only
    }

    async fn execute(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let call = self.client.call_tool(&self.upstream_name, args);
        let mut out = match tokio::time::timeout(self.timeout, call).await {
            Ok(result) => result.map_err(|e| ToolError::Other(format!("mcp call failed: {e}")))?,
            Err(_) => {
                return Err(ToolError::Other(format!(
                    "mcp call timed out after {}s (toolTimeoutSecs)",
                    self.timeout.as_secs()
                )));
            }
        };
        if out.content.len() > self.max_output_bytes {
            let total = out.content.len();
            let cut = (0..=self.max_output_bytes)
                .rev()
                .find(|&i| out.content.is_char_boundary(i))
                .unwrap_or(0);
            out.content.truncate(cut);
            out.content.push_str(&format!(
                "\n… (truncated: {total} bytes, maxOutputBytes {})",
                self.max_output_bytes
            ));
        }
        Ok(out)
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
