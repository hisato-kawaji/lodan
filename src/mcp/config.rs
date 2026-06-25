// `.mcp.json` パース (Claude Code 互換スキーマ)。
//
// {
//   "mcpServers": {
//     "filesystem": {                                   // stdio transport
//       "command": "npx",
//       "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path"],
//       "env": { "FOO": "bar" }
//     },
//     "remote": {                                        // Streamable HTTP transport
//       "url": "https://example.com/mcp",
//       "headers": { "Authorization": "Bearer ..." }
//     }
//   }
// }

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Default, Deserialize)]
pub struct McpServersConfig {
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: BTreeMap<String, McpServerSpec>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerSpec {
    // --- stdio transport ---
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    // --- Streamable HTTP transport ---
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

impl McpServerSpec {
    /// `url` があれば HTTP、`command` があれば stdio。両方無い / 両方ある場合はエラー。
    pub fn transport(&self) -> Result<Transport<'_>> {
        match (self.command.as_deref(), self.url.as_deref()) {
            (Some(_), Some(_)) => {
                anyhow::bail!("server spec has both `command` and `url`; pick one")
            }
            (Some(cmd), None) => Ok(Transport::Stdio { command: cmd }),
            (None, Some(url)) => Ok(Transport::Http { url }),
            (None, None) => anyhow::bail!("server spec needs either `command` or `url`"),
        }
    }
}

/// 選択された transport (借用ビュー)。
pub enum Transport<'a> {
    Stdio { command: &'a str },
    Http { url: &'a str },
}

impl McpServersConfig {
    /// Read `$CWD/.mcp.json` if it exists. Returns `Ok(None)` when absent.
    pub fn load_from_cwd() -> Result<Option<Self>> {
        let cwd = std::env::current_dir().context("getting cwd")?;
        Self::load_from(&cwd.join(".mcp.json"))
    }

    pub fn load_from(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let s =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: McpServersConfig =
            serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(cfg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config() {
        let s = r#"{
            "mcpServers": {
                "fs": { "command": "npx", "args": ["-y", "x"] }
            }
        }"#;
        let cfg: McpServersConfig = serde_json::from_str(s).unwrap();
        assert_eq!(cfg.mcp_servers.len(), 1);
        let fs = &cfg.mcp_servers["fs"];
        assert_eq!(fs.command.as_deref(), Some("npx"));
        assert_eq!(fs.args, vec!["-y", "x"]);
        assert!(fs.env.is_empty());
        assert!(matches!(fs.transport().unwrap(), Transport::Stdio { .. }));
    }

    #[test]
    fn parses_http_config() {
        let s = r#"{
            "mcpServers": {
                "remote": { "url": "https://x/mcp", "headers": { "Authorization": "Bearer t" } }
            }
        }"#;
        let cfg: McpServersConfig = serde_json::from_str(s).unwrap();
        let r = &cfg.mcp_servers["remote"];
        assert_eq!(r.url.as_deref(), Some("https://x/mcp"));
        assert_eq!(r.headers.get("Authorization").unwrap(), "Bearer t");
        assert!(matches!(r.transport().unwrap(), Transport::Http { .. }));
    }

    #[test]
    fn transport_requires_exactly_one_of_command_or_url() {
        let both: McpServerSpec =
            serde_json::from_str(r#"{ "command": "c", "url": "http://x" }"#).unwrap();
        assert!(both.transport().is_err());

        let neither: McpServerSpec = serde_json::from_str(r#"{ "args": [] }"#).unwrap();
        assert!(neither.transport().is_err());
    }

    #[test]
    fn empty_object_means_no_servers() {
        let cfg: McpServersConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.mcp_servers.is_empty());
    }

    #[test]
    fn env_block_is_parsed() {
        let s = r#"{
            "mcpServers": {
                "x": { "command": "c", "env": { "K": "v" } }
            }
        }"#;
        let cfg: McpServersConfig = serde_json::from_str(s).unwrap();
        assert_eq!(cfg.mcp_servers["x"].env.get("K").unwrap(), "v");
        assert_eq!(cfg.mcp_servers["x"].command.as_deref(), Some("c"));
    }

    #[test]
    fn missing_file_returns_none() {
        let p = std::path::PathBuf::from("/nonexistent/.mcp.json.lodan-test");
        let cfg = McpServersConfig::load_from(&p).unwrap();
        assert!(cfg.is_none());
    }
}
