//! 共有 frontmatter パーサ。
//!
//! `---\n ... \n---\n<body>` 形式の先頭メタブロックを分離する。slash コマンド
//! (`crate::slash`) と skills (`crate::skills`) が同じ分割ロジックを使う。
//!
//! 既知ギャップ: `---\r\n` (CRLF) は frontmatter として認識しない。

/// frontmatter を本文から分離する。
/// frontmatter があれば `(Some(front), body)`、無ければ `(None, body)`。
/// いずれの場合も `body` は先頭の改行・空白を除いて返す。
pub fn split(content: &str) -> (Option<&str>, &str) {
    if let Some(rest) = content.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---")
    {
        let front = &rest[..end];
        // `\n---` の後の行（閉じ `---` の残り）から最初の改行以降が本文。
        let after = &rest[end + 4..];
        let body = after.strip_prefix('\n').unwrap_or(after).trim_start();
        return (Some(front), body);
    }
    (None, content.trim_start())
}

/// frontmatter ブロックから `key: value` を 1 つ取り出す。
/// 値は trim し、両端の `"` を除く。複数行あれば最後の一致を採用。無ければ `None`。
pub fn field(front: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    let mut found = None;
    for line in front.lines() {
        if let Some(v) = line.strip_prefix(prefix.as_str()) {
            found = Some(v.trim().trim_matches('"').to_string());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_extracts_frontmatter_and_body() {
        let (front, body) = split("---\nname: x\ndescription: y\n---\nbody here\n");
        assert_eq!(front, Some("name: x\ndescription: y"));
        assert_eq!(body, "body here\n");
    }

    #[test]
    fn split_no_frontmatter_returns_trimmed_body() {
        let (front, body) = split("  just a body\n");
        assert_eq!(front, None);
        assert_eq!(body, "just a body\n");
    }

    #[test]
    fn split_unterminated_is_not_frontmatter() {
        let (front, body) = split("---\nname: x\nno closing");
        assert_eq!(front, None);
        assert!(body.starts_with("---"));
    }

    #[test]
    fn field_reads_trims_and_dequotes() {
        let front = "name: deploy\ndescription: \"ship it\"";
        assert_eq!(field(front, "name").as_deref(), Some("deploy"));
        assert_eq!(field(front, "description").as_deref(), Some("ship it"));
        assert_eq!(field(front, "missing"), None);
    }

    #[test]
    fn field_last_occurrence_wins() {
        let front = "k: first\nk: second";
        assert_eq!(field(front, "k").as_deref(), Some("second"));
    }
}
