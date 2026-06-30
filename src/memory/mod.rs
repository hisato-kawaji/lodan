//! プロジェクト/ユーザのメモリファイルを読み込み、system prompt へ注入する。
//!
//! Claude Code の `CLAUDE.md` 階層に相当。cwd から上方向（home まで、無ければ root まで）に
//! 各ディレクトリの `LODAN.md`（無ければ `CLAUDE.md`）を集め、加えてユーザ全体
//! `~/.lodan/LODAN.md` を読む。外側（汎用）→ 内側（具体）の順に連結し、合計サイズ上限を課す。
//!
//! ⚠️ 信頼前提: これらのファイルは CWD 階層からそのままプロンプトへ注入される。
//! 信頼できないリポジトリの memory は prompt injection ベクタになり得る
//! （hooks / skills / `.mcp.json` と同じ CWD 信頼前提）。

use std::path::{Path, PathBuf};

/// メモリ全体のサイズ上限（バイト）。超過分は char 境界で打ち切る。
pub const MEMORY_CAP: usize = 32 * 1024;

/// 各ディレクトリで優先的に探すファイル名（先にヒットしたものを採用）。
const PROJECT_FILES: &[&str] = &["LODAN.md", "CLAUDE.md"];

/// cwd 階層＋ユーザ全体のメモリを連結して返す。何も無ければ空文字列。
pub fn load_memory(cwd: &Path) -> String {
    load_memory_from(cwd, home_dir().as_deref())
}

/// `home` を明示で受ける本体（テスト用に分離）。
fn load_memory_from(cwd: &Path, home: Option<&Path>) -> String {
    let mut sources: Vec<(PathBuf, String)> = Vec::new();

    // ユーザ全体（最も汎用なので先頭）。
    if let Some(home) = home {
        if let Some(hit) = read_first(&home.join(".lodan"), &["LODAN.md"]) {
            sources.push(hit);
        }
    }

    // cwd → 上方向。home がパス上にあればそこで打ち切る（その上の system 領域は読まない）。
    let mut dirs: Vec<PathBuf> = Vec::new();
    for anc in cwd.ancestors() {
        dirs.push(anc.to_path_buf());
        if Some(anc) == home {
            break;
        }
    }
    // 外側（root/home 寄り）が先に来るよう逆順。
    dirs.reverse();
    for dir in &dirs {
        if let Some(hit) = read_first(dir, PROJECT_FILES) {
            sources.push(hit);
        }
    }

    if sources.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    for (path, content) in sources {
        if out.len() >= MEMORY_CAP {
            break;
        }
        let header = format!("\n# Memory: {}\n", path.display());
        out.push_str(&header);
        let remaining = MEMORY_CAP.saturating_sub(out.len());
        if content.len() <= remaining {
            out.push_str(&content);
        } else {
            let cut = floor_char_boundary(&content, remaining);
            out.push_str(&content[..cut]);
            out.push_str("\n...[memory truncated]...");
        }
    }
    out
}

/// `dir` 直下で `names` の先頭ヒット（中身が非空）を読む。読めなければ次へ。
fn read_first(dir: &Path, names: &[&str]) -> Option<(PathBuf, String)> {
    for name in names {
        let p = dir.join(name);
        if let Ok(c) = std::fs::read_to_string(&p) {
            if !c.trim().is_empty() {
                return Some((p, c));
            }
        }
    }
    None
}

/// `$HOME`（無ければ Windows の `USERPROFILE`）を返す。
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// `n` 以下で最大の char 境界を返す。
fn floor_char_boundary(s: &str, mut n: usize) -> usize {
    if n >= s.len() {
        return s.len();
    }
    while n > 0 && !s.is_char_boundary(n) {
        n -= 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn empty_when_no_memory_files() {
        let dir = tempdir().unwrap();
        assert_eq!(load_memory_from(dir.path(), None), "");
    }

    #[test]
    fn reads_lodan_md_in_cwd() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("LODAN.md"), "project rules here").unwrap();
        let out = load_memory_from(dir.path(), None);
        assert!(out.contains("project rules here"));
        assert!(out.contains("# Memory:"));
    }

    #[test]
    fn lodan_md_preferred_over_claude_md() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("LODAN.md"), "from-lodan").unwrap();
        fs::write(dir.path().join("CLAUDE.md"), "from-claude").unwrap();
        let out = load_memory_from(dir.path(), None);
        assert!(out.contains("from-lodan"));
        assert!(!out.contains("from-claude"));
    }

    #[test]
    fn falls_back_to_claude_md() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("CLAUDE.md"), "from-claude").unwrap();
        let out = load_memory_from(dir.path(), None);
        assert!(out.contains("from-claude"));
    }

    #[test]
    fn outer_dirs_come_before_inner() {
        let root = tempdir().unwrap();
        let child = root.path().join("sub");
        fs::create_dir(&child).unwrap();
        fs::write(root.path().join("LODAN.md"), "OUTER").unwrap();
        fs::write(child.join("LODAN.md"), "INNER").unwrap();
        // home を root に切り、root より上は読まない。
        let out = load_memory_from(&child, Some(root.path()));
        let outer = out.find("OUTER").unwrap();
        let inner = out.find("INNER").unwrap();
        assert!(outer < inner, "outer memory should precede inner: {out}");
    }

    #[test]
    fn blank_file_is_skipped() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("LODAN.md"), "   \n\t  ").unwrap();
        assert_eq!(load_memory_from(dir.path(), None), "");
    }

    #[test]
    fn caps_total_size_on_char_boundary() {
        let dir = tempdir().unwrap();
        // マルチバイト文字で上限超過させ、境界割れで panic しないことを確認。
        let big = "あ".repeat(MEMORY_CAP); // 3 bytes/char → 上限を大きく超える
        fs::write(dir.path().join("LODAN.md"), &big).unwrap();
        let out = load_memory_from(dir.path(), None);
        assert!(out.len() <= MEMORY_CAP + 64); // header + marker 分の余白
        assert!(out.contains("[memory truncated]"));
        // 妥当な UTF-8 のまま（切り出しが境界を割っていない）。
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }
}
