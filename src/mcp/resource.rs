// MCP サーバが公開する resources を「読み取りツール」として lodan に橋渡しする。
// サーバごとに `mcp__<server>__read_resource` を 1 つ登録し、uri を渡すと
// resources/read の内容をテキスト化して返す。read-only なので非破壊扱い
// (tools/call と違い permission gate を経ない)。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::mcp::client::McpClient;
use crate::mcp::protocol::ResourceMeta;
use crate::tools::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct McpResourceTool {
    full_name: String,
    description: String,
    /// 公開 uri 一覧 (schema の enum 用)。
    uris: Vec<String>,
    client: Arc<McpClient>,
}

impl McpResourceTool {
    pub fn new(server: &str, resources: &[ResourceMeta], client: Arc<McpClient>) -> Self {
        let (description, uris) = build_listing(server, resources);
        Self {
            full_name: format!("mcp__{server}__read_resource"),
            description,
            uris,
            client,
        }
    }
}

/// ツール説明文 (resource 一覧) と schema 用の uri 一覧を作る。client 非依存なので
/// 単体テスト可能。
fn build_listing(server: &str, resources: &[ResourceMeta]) -> (String, Vec<String>) {
    let mut description = format!(
        "Read a resource exposed by the `{server}` MCP server. \
         Pass the resource `uri`. Available resources:\n"
    );
    let mut uris = Vec::with_capacity(resources.len());
    for r in resources {
        let label = r.name.as_deref().unwrap_or(&r.uri);
        match &r.description {
            Some(d) => description.push_str(&format!("- {} ({}): {d}\n", r.uri, label)),
            None => description.push_str(&format!("- {} ({})\n", r.uri, label)),
        }
        uris.push(r.uri.clone());
    }
    (description, uris)
}

#[async_trait]
impl Tool for McpResourceTool {
    fn name(&self) -> &str {
        &self.full_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "uri": {
                    "type": "string",
                    "description": "The resource URI to read.",
                    "enum": self.uris,
                }
            },
            "required": ["uri"]
        })
    }

    fn is_destructive(&self) -> bool {
        false
    }

    async fn execute(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let uri = args
            .get("uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("read_resource: missing `uri`".into()))?;
        // schema の uri enum は advisory。クライアントは uri を制限せず、認可境界は
        // サーバ側に委ねる (enum 外 uri はサーバが error を返し ToolOutput::error になる)。
        match self.client.read_resource(uri).await {
            Ok(result) => Ok(ToolOutput::ok(result.flatten_text())),
            Err(e) => Ok(ToolOutput::error(format!("resources/read failed: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(uri: &str, name: &str) -> ResourceMeta {
        ResourceMeta {
            uri: uri.into(),
            name: Some(name.into()),
            description: Some("desc".into()),
            mime_type: None,
        }
    }

    #[test]
    fn listing_enumerates_uris_and_descriptions() {
        let resources = [meta("file:///a", "a"), meta("res://b", "b")];
        let (desc, uris) = build_listing("docs", &resources);
        assert_eq!(uris, vec!["file:///a".to_string(), "res://b".to_string()]);
        assert!(desc.contains("`docs` MCP server"));
        assert!(desc.contains("- file:///a (a): desc"));
        assert!(desc.contains("- res://b (b): desc"));
    }

    #[test]
    fn listing_falls_back_to_uri_when_name_absent() {
        let r = ResourceMeta {
            uri: "res://x".into(),
            name: None,
            description: None,
            mime_type: None,
        };
        let (desc, _) = build_listing("s", &[r]);
        assert!(desc.contains("- res://x (res://x)\n"));
    }
}
