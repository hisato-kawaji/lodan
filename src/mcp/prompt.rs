// MCP サーバが公開する prompt を lodan の slash コマンドへ橋渡しする。
// `/mcp__<server>__<prompt> 引数...` で呼ぶと prompts/get を実行し、返ってきた
// メッセージをテキスト化してユーザターンとしてエージェントへ投入する。

use std::sync::Arc;

use anyhow::Result;

use crate::mcp::client::McpClient;

/// 登録済みの MCP prompt。
pub struct McpPrompt {
    /// `mcp__<server>__<prompt>` (slash 名)。
    full_name: String,
    /// サーバ側の prompt 名 (prompts/get に渡す)。
    upstream_name: String,
    description: String,
    /// 宣言された引数名 (順序つき)。位置引数をこの順で対応づける。
    arg_names: Vec<String>,
    client: Arc<McpClient>,
}

impl McpPrompt {
    pub fn new(
        full_name: String,
        upstream_name: String,
        description: String,
        arg_names: Vec<String>,
        client: Arc<McpClient>,
    ) -> Self {
        Self {
            full_name,
            upstream_name,
            description,
            arg_names,
            client,
        }
    }

    pub fn full_name(&self) -> &str {
        &self.full_name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    /// 空白区切りの位置引数を宣言順に名前付き引数へ対応づけ、prompts/get を実行して
    /// 返ってきたメッセージを 1 つのテキストへ畳んで返す。
    pub async fn render(&self, positional: &[&str]) -> Result<String> {
        let mut map = serde_json::Map::new();
        for (name, value) in self.arg_names.iter().zip(positional.iter()) {
            map.insert(name.clone(), serde_json::Value::String(value.to_string()));
        }
        let result = self
            .client
            .get_prompt(&self.upstream_name, serde_json::Value::Object(map))
            .await?;
        Ok(result.render())
    }
}

/// slash 名: tool と同じ `mcp__<server>__<prompt>` 規約。
pub fn namespaced(server: &str, prompt: &str) -> String {
    format!("mcp__{server}__{prompt}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_format() {
        assert_eq!(namespaced("docs", "review"), "mcp__docs__review");
    }
}
