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
    // --- capabilities ---
    /// server→client の sampling/createMessage を許可するか。
    /// 許可するとサーバがこちらの LLM (モデル/トークン) を駆動できるため、
    /// 既定は false。信頼するサーバのみ明示的に opt-in する。
    #[serde(default, rename = "allowSampling")]
    pub allow_sampling: bool,
    /// ツールの `annotations.readOnlyHint` を信じて、承認ゲートを通さずに実行するか (#83)。
    /// ヒントはサーバの自己申告なので既定 false。信頼するサーバだけ opt-in する。
    #[serde(default, rename = "trustAnnotations")]
    pub trust_annotations: bool,
    /// 取り込むツール名 (upstream の名前)。空なら全部。
    #[serde(default, rename = "enabledTools")]
    pub enabled_tools: Vec<String>,
    /// 取り込まないツール名。`enabledTools` より優先。
    #[serde(default, rename = "disabledTools")]
    pub disabled_tools: Vec<String>,
    /// ツール 1 回の呼び出しの上限秒。既定 `DEFAULT_TOOL_TIMEOUT_SECS`。
    #[serde(default, rename = "toolTimeoutSecs")]
    pub tool_timeout_secs: Option<u64>,
    /// ツール結果をモデルに渡す上限バイト。既定 `DEFAULT_MAX_OUTPUT_BYTES`。超えた分は切る。
    #[serde(default, rename = "maxOutputBytes")]
    pub max_output_bytes: Option<usize>,
}

/// MCP ツール 1 回の呼び出しの既定上限。
pub const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 60;
/// MCP ツール結果の既定上限 (バイト)。ツール出力はそのままコンテキストを食うので、無制限にしない。
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;

impl McpServerSpec {
    /// このサーバのツール `name` を取り込むか。
    pub fn tool_enabled(&self, name: &str) -> bool {
        if self.disabled_tools.iter().any(|d| d == name) {
            return false;
        }
        self.enabled_tools.is_empty() || self.enabled_tools.iter().any(|e| e == name)
    }

    pub fn tool_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.tool_timeout_secs.unwrap_or(DEFAULT_TOOL_TIMEOUT_SECS))
    }

    pub fn max_output_bytes(&self) -> usize {
        self.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES)
    }
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

/// どのスコープの設定ファイルか (#83)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `<config_dir>/mcp.json` (ユーザー全体。`config.toml` と同じ場所)。
    User,
    /// `<cwd>/.mcp.json` (プロジェクト。信頼済みのときだけ読む)。
    Project,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
        }
    }
}

/// ユーザー全体の `mcp.json` の場所。
pub fn user_mcp_path() -> Option<std::path::PathBuf> {
    directories::ProjectDirs::from("", "", "lodan").map(|d| d.config_dir().join("mcp.json"))
}

impl McpServersConfig {
    /// Read `$CWD/.mcp.json` if it exists. Returns `Ok(None)` when absent.
    pub fn load_from_cwd() -> Result<Option<Self>> {
        // `.mcp.json` は任意のプロセスを起動する。信頼していないディレクトリのものは読まない (#75)。
        if !crate::trust::project_trusted() {
            return Ok(None);
        }
        let cwd = std::env::current_dir().context("getting cwd")?;
        Self::load_from(&cwd.join(".mcp.json"))
    }

    /// ユーザー全体 + プロジェクト (信頼済みのときだけ) を合成する。同名はプロジェクトが勝つ。
    /// 戻り値の第 2 要素は、各サーバがどのスコープから来たか。
    pub fn load_effective() -> Result<(Self, BTreeMap<String, Scope>)> {
        let mut merged = McpServersConfig::default();
        let mut scopes = BTreeMap::new();
        if let Some(path) = user_mcp_path()
            && let Some(user) = Self::load_from(&path)?
        {
            for (name, spec) in user.mcp_servers {
                scopes.insert(name.clone(), Scope::User);
                merged.mcp_servers.insert(name, spec);
            }
        }
        if let Some(project) = Self::load_from_cwd()? {
            for (name, spec) in project.mcp_servers {
                scopes.insert(name.clone(), Scope::Project);
                merged.mcp_servers.insert(name, spec);
            }
        }
        Ok((merged, scopes))
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

/// `lodan mcp add` / `remove` が書く 1 件ぶんの設定 (JSON のまま扱い、他のキーは保つ)。
pub fn upsert_server(path: &Path, name: &str, spec: serde_json::Value) -> Result<()> {
    let mut root = read_json_or_empty(path)?;
    let servers = root
        .as_object_mut()
        .context("mcp.json is not a JSON object")?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    servers
        .as_object_mut()
        .context("`mcpServers` is not a JSON object")?
        .insert(name.to_string(), spec);
    write_json(path, &root)
}

/// `remove`: 無ければ `Ok(false)`。
pub fn remove_server(path: &Path, name: &str) -> Result<bool> {
    let mut root = read_json_or_empty(path)?;
    let removed = root
        .get_mut("mcpServers")
        .and_then(|s| s.as_object_mut())
        .and_then(|s| s.remove(name))
        .is_some();
    if removed {
        write_json(path, &root)?;
    }
    Ok(removed)
}

fn read_json_or_empty(path: &Path) -> Result<serde_json::Value> {
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let s = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display()))
}

fn write_json(path: &Path, root: &serde_json::Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(root)?;
    std::fs::write(path, text + "\n").with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_server_knobs_parse_and_default() {
        let s = r#"{ "mcpServers": { "a": {
            "command": "x", "trustAnnotations": true,
            "enabledTools": ["read", "list"], "disabledTools": ["list"],
            "toolTimeoutSecs": 5, "maxOutputBytes": 100 } } }"#;
        let cfg: McpServersConfig = serde_json::from_str(s).unwrap();
        let a = &cfg.mcp_servers["a"];
        assert!(a.trust_annotations);
        assert!(a.tool_enabled("read"));
        assert!(!a.tool_enabled("list"), "disabled wins over enabled");
        assert!(!a.tool_enabled("other"), "not in enabledTools");
        assert_eq!(a.tool_timeout().as_secs(), 5);
        assert_eq!(a.max_output_bytes(), 100);
        let plain: McpServersConfig =
            serde_json::from_str(r#"{ "mcpServers": { "b": { "command": "x" } } }"#).unwrap();
        let b = &plain.mcp_servers["b"];
        assert!(!b.trust_annotations && b.tool_enabled("anything"));
        assert_eq!(b.tool_timeout().as_secs(), DEFAULT_TOOL_TIMEOUT_SECS);
        assert_eq!(b.max_output_bytes(), DEFAULT_MAX_OUTPUT_BYTES);
    }

    #[test]
    fn upsert_and_remove_keep_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/mcp.json");
        upsert_server(
            &path,
            "fs",
            serde_json::json!({ "command": "npx", "args": ["x"] }),
        )
        .unwrap();
        std::fs::write(
            &path,
            r#"{ "note": "keep me", "mcpServers": { "fs": { "command": "npx", "args": ["x"] } } }"#,
        )
        .unwrap();
        upsert_server(&path, "web", serde_json::json!({ "url": "http://h/mcp" })).unwrap();
        let cfg = McpServersConfig::load_from(&path).unwrap().unwrap();
        assert_eq!(cfg.mcp_servers.len(), 2);
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["note"], "keep me");
        assert!(remove_server(&path, "fs").unwrap());
        assert!(!remove_server(&path, "fs").unwrap());
        let cfg = McpServersConfig::load_from(&path).unwrap().unwrap();
        assert_eq!(cfg.mcp_servers.keys().collect::<Vec<_>>(), ["web"]);
    }

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
    fn allow_sampling_defaults_false_and_parses_when_set() {
        let off: McpServerSpec = serde_json::from_str(r#"{ "command": "c" }"#).unwrap();
        assert!(!off.allow_sampling);
        let on: McpServerSpec =
            serde_json::from_str(r#"{ "command": "c", "allowSampling": true }"#).unwrap();
        assert!(on.allow_sampling);
    }

    #[test]
    fn missing_file_returns_none() {
        let p = std::path::PathBuf::from("/nonexistent/.mcp.json.lodan-test");
        let cfg = McpServersConfig::load_from(&p).unwrap();
        assert!(cfg.is_none());
    }
}
