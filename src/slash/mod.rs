// ユーザ定義 slash コマンド。
// `$CWD/.lodan/commands/<name>.md` を読み、`/name [args]` で本文を
// プロンプトテンプレートとして展開し、ユーザターンとしてエージェントへ投入する。
// (REPL 組み込みの builtins: /exit /clear /tools /help は repl.rs 側)

use anyhow::Result;
use std::path::Path;

/// `.lodan/commands/<name>.md` 1 ファイル分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    /// 拡張子を除いたファイル名 = コマンド名 (`foo.md` → `foo`)。
    pub name: String,
    /// frontmatter の `description:` (省略時は空)。`/help` 表示用。
    pub description: String,
    /// frontmatter を除いた本文 (プロンプトテンプレート)。
    pub body: String,
}

/// `dir` 配下の `*.md` を読み込んでコマンド一覧を返す。
/// ディレクトリが無ければ空 Vec。読めない個別ファイルは警告して飛ばす。
/// 戻り値はコマンド名で昇順ソート済み。
pub fn load_dir(dir: &Path) -> Result<Vec<SlashCommand>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut cmds = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let (description, body) = parse_frontmatter(&content);
                cmds.push(SlashCommand {
                    name: name.to_string(),
                    description,
                    body,
                });
            }
            Err(e) => eprintln!("slash[{name}]: read failed: {e}"),
        }
    }
    cmds.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(cmds)
}

/// 先頭の `---` 〜 `---` を frontmatter として解釈し `(description, body)` を返す。
/// frontmatter が無ければ description は空、body は全文。
/// 認識するキーは `description` のみ (依存追加を避けた最小パース)。
fn parse_frontmatter(content: &str) -> (String, String) {
    let rest = match content.strip_prefix("---\n") {
        Some(r) => r,
        None => return (String::new(), content.trim_start().to_string()),
    };
    let Some(end) = rest.find("\n---") else {
        // 閉じ `---` 無し → frontmatter として扱わない。
        return (String::new(), content.trim_start().to_string());
    };
    let front = &rest[..end];
    // `\n---` の次の改行以降が本文。
    let after = &rest[end + 4..];
    let body = after.strip_prefix('\n').unwrap_or(after);

    let mut description = String::new();
    for line in front.lines() {
        if let Some(v) = line.strip_prefix("description:") {
            description = v.trim().trim_matches('"').to_string();
        }
    }
    (description, body.trim_start().to_string())
}

/// テンプレート本文に引数を差し込む。
/// - `$ARGUMENTS` → args 全体
/// - `$1`..`$9` → 空白区切りの位置引数 (該当なしは空文字)
pub fn expand(body: &str, args: &str) -> String {
    let args = args.trim();
    let mut out = body.replace("$ARGUMENTS", args);
    let positional: Vec<&str> = args.split_whitespace().collect();
    for i in 1..=9 {
        let token = format!("${i}");
        let value = positional.get(i - 1).copied().unwrap_or("");
        out = out.replace(&token, value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn frontmatter_parsed_and_stripped() {
        let content = "---\ndescription: Review the diff\n---\nRun a review on $ARGUMENTS\n";
        let (desc, body) = parse_frontmatter(content);
        assert_eq!(desc, "Review the diff");
        assert_eq!(body, "Run a review on $ARGUMENTS\n");
    }

    #[test]
    fn no_frontmatter_keeps_whole_body() {
        let (desc, body) = parse_frontmatter("just a prompt $1\n");
        assert_eq!(desc, "");
        assert_eq!(body, "just a prompt $1\n");
    }

    #[test]
    fn unterminated_frontmatter_is_not_treated_as_meta() {
        let (desc, body) = parse_frontmatter("---\ndescription: oops\nno closing");
        assert_eq!(desc, "");
        assert!(body.starts_with("---"));
    }

    #[test]
    fn expand_substitutes_arguments_and_positional() {
        let out = expand("fix $1 in $2 — full: $ARGUMENTS", "bug parser.rs");
        assert_eq!(out, "fix bug in parser.rs — full: bug parser.rs");
    }

    #[test]
    fn expand_missing_positional_is_empty() {
        assert_eq!(expand("[$1][$2]", "only"), "[only][]");
    }

    #[test]
    fn expand_no_args() {
        assert_eq!(expand("hello $ARGUMENTS", ""), "hello ");
    }

    #[test]
    fn load_dir_reads_md_sorted_and_skips_others() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("zebra.md"), "z body").unwrap();
        fs::write(
            dir.path().join("alpha.md"),
            "---\ndescription: A\n---\na body",
        )
        .unwrap();
        fs::write(dir.path().join("notes.txt"), "ignored").unwrap();

        let cmds = load_dir(dir.path()).unwrap();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].name, "alpha");
        assert_eq!(cmds[0].description, "A");
        assert_eq!(cmds[1].name, "zebra");
    }

    #[test]
    fn load_dir_missing_is_empty() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(load_dir(&missing).unwrap().is_empty());
    }
}
