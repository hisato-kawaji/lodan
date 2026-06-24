// `.mcp.json` パース (Claude Code 互換スキーマ)。
//
// {
//   "mcpServers": {
//     "filesystem": {
//       "command": "npx",
//       "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path"],
//       "env": { "FOO": "bar" }
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
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
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
        assert_eq!(fs.command, "npx");
        assert_eq!(fs.args, vec!["-y", "x"]);
        assert!(fs.env.is_empty());
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
    }

    #[test]
    fn missing_file_returns_none() {
        let p = std::path::PathBuf::from("/nonexistent/.mcp.json.lodan-test");
        let cfg = McpServersConfig::load_from(&p).unwrap();
        assert!(cfg.is_none());
    }
}
