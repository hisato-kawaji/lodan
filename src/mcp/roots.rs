// MCP roots: クライアントが作業ディレクトリ (root) をサーバへ公開する。
// サーバから来る `roots/list` リクエストに応答するハンドラを提供する。
// (server→client リクエストは stdio transport のみ対応)

use std::path::Path;

use serde_json::{Value, json};

/// 公開する root の集合。現状は cwd 1 つ。
#[derive(Debug, Clone)]
pub struct RootsProvider {
    roots: Vec<Root>,
}

#[derive(Debug, Clone)]
struct Root {
    uri: String,
    name: String,
}

impl RootsProvider {
    /// cwd を唯一の root として公開する。
    pub fn from_cwd(cwd: &Path) -> Self {
        let name = cwd
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("workspace")
            .to_string();
        let uri = format!("file://{}", cwd.display());
        Self {
            roots: vec![Root { uri, name }],
        }
    }

    /// server→client リクエストを処理する。対応メソッドは `roots/list` のみ。
    /// 対応する場合は `Some(result)`、未対応なら `None` (呼び出し側で method-not-found)。
    pub fn handle(&self, method: &str) -> Option<Value> {
        if method != "roots/list" {
            return None;
        }
        let roots: Vec<Value> = self
            .roots
            .iter()
            .map(|r| json!({ "uri": r.uri, "name": r.name }))
            .collect();
        Some(json!({ "roots": roots }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_cwd_exposes_single_file_uri_root() {
        let p = Path::new("/tmp/lodan-demo");
        let provider = RootsProvider::from_cwd(p);
        let result = provider.handle("roots/list").unwrap();
        let roots = result["roots"].as_array().unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0]["uri"], "file:///tmp/lodan-demo");
        assert_eq!(roots[0]["name"], "lodan-demo");
    }

    #[test]
    fn unknown_method_is_none() {
        let provider = RootsProvider::from_cwd(Path::new("/x"));
        assert!(provider.handle("sampling/createMessage").is_none());
    }
}
