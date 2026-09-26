//! workspace trust (#75): 信頼していないディレクトリが持ち込むものを読まない。
//!
//! プロジェクトのファイルは、開いただけで効いてしまう:
//!
//! - `.lodan/config.toml` / `.lodan/config.local.toml` — `[[hooks]]` は任意のコマンドを実行する。
//!   `[llm.*] base_url` を書き換えれば API キーを外へ送れる。`[permissions] mode = "bypass"` や
//!   `allow = ["Bash(*)"]` で承認を素通しにできる
//! - `.env` — `LODAN_BASE_URL` / `LODAN_PERMISSION_MODE=bypass` / `LODAN_TRUST=1` のように、
//!   環境変数で渡せる設定は全部ここから渡せる (だから信頼の判断より後に読む)
//! - `.mcp.json` — 任意のプロセスを起動する
//! - `.lodan/commands` / `.lodan/skills` / `LODAN.md` / `CLAUDE.md` / `AGENTS.md` — モデルへの指示を差し込む
//!
//! clone してきたリポジトリで `lodan` を起動するだけでこれらが効くのは危ない。信頼済みの
//! ディレクトリ (とその配下) でだけ読む。信頼の記録はユーザの設定ディレクトリに置くので、
//! リポジトリ側からは書き換えられない。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// このプロセスがプロジェクトのファイルを読んでよいか。`cli::dispatch` が最初に決める。
static PROJECT_TRUSTED: OnceLock<bool> = OnceLock::new();

/// プロジェクトが持ち込むファイルを読んでよいか。
///
/// 未設定のときは true。決めるのはバイナリの入口 (`cli::dispatch`) だけで、ライブラリとして
/// 個々の読み込み関数を呼ぶ側 (テストを含む) は、自分で選んだパスを渡しているので対象外。
pub fn project_trusted() -> bool {
    *PROJECT_TRUSTED.get().unwrap_or(&true)
}

/// 入口で 1 度だけ決める。2 回目以降は無視される。
pub fn set_project_trusted(trusted: bool) {
    let _ = PROJECT_TRUSTED.set(trusted);
}

/// もう決まっているか。決まっていたら尋ね直してはいけない — 2 回目の問い合わせの答えは
/// このプロセスには効かないのに、`(y)` なら記録だけは残ってしまう。
pub fn is_decided() -> bool {
    PROJECT_TRUSTED.get().is_some()
}

/// パスを端末に出す形にする。ディレクトリ名は利用者が付けたとは限らない (clone したリポジトリ、
/// 展開したアーカイブ)。制御文字や双方向テキストの上書きで警告文を書き換えさせない。
pub fn shown(path: &Path) -> String {
    crate::term::sanitize(&path.display().to_string()).into_owned()
}

/// メモリとして読まれるファイル名。`crate::memory` の探索と必ず揃えること — ここに無い名前は
/// 信頼の確認をすり抜けて system prompt に入る (#125 のレビューで `AGENTS.md` が抜けていた)。
pub const MEMORY_FILES: &[&str] = &["LODAN.md", "CLAUDE.md", "AGENTS.md"];

/// 信頼が要るファイル (表示用の名前)。cwd にあるものに加えて、メモリは cwd の祖先 (ホームまで)
/// からも読まれるので、祖先の `LODAN.md` / `CLAUDE.md` / `AGENTS.md` も含める — そうしないと、
/// 何も無いサブディレクトリから起動したときに、尋ねないまま親のメモリを読んでしまう。
pub fn project_files(cwd: &Path) -> Vec<String> {
    const CANDIDATES: &[&str] = &[
        ".env",
        ".lodan/config.toml",
        ".lodan/config.local.toml",
        ".mcp.json",
        ".lodan/commands",
        ".lodan/skills",
        "LODAN.md",
        "CLAUDE.md",
        "AGENTS.md",
    ];
    let mut found: Vec<String> = CANDIDATES
        .iter()
        .filter(|name| cwd.join(name).exists())
        .map(|name| name.to_string())
        .collect();
    let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf());
    for dir in cwd.ancestors().skip(1) {
        // メモリの探索と同じく、ホームで止める (ホーム自身の LODAN.md は利用者のもの)。
        if home.as_deref() == Some(dir) {
            break;
        }
        for name in MEMORY_FILES {
            if dir.join(name).exists() {
                found.push(shown(&dir.join(name)));
            }
        }
    }
    found
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Store {
    /// 信頼したディレクトリ (symlink 解決済みの絶対パス)。配下も信頼する。
    dirs: Vec<PathBuf>,
}

pub fn store_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "lodan").map(|d| d.config_dir().join("trusted.toml"))
}

fn read_store(path: &Path) -> Result<Store> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_store(path: &Path, store: &Store) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // 記録先が symlink なら、その先を書き換えない (`append_local_allow_rule` と同じ構え)。
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        anyhow::bail!("{} is a symlink; refusing to write through it", shown(path));
    }
    let body = toml::to_string_pretty(store).context("serializing trust store")?;
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))
}

/// 比較は symlink を解決した形で行う。解決できない (存在しない) パスはそのまま。
fn real(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `dir` が信頼済みか (記録にあるディレクトリそのもの、またはその配下)。
pub fn is_recorded(store_path: &Path, dir: &Path) -> bool {
    let dir = real(dir);
    read_store(store_path)
        .map(|s| s.dirs.iter().any(|t| dir.starts_with(t)))
        .unwrap_or(false)
}

pub fn record(store_path: &Path, dir: &Path) -> Result<()> {
    let dir = real(dir);
    let mut store = read_store(store_path)?;
    if !store.dirs.iter().any(|t| dir.starts_with(t)) {
        store.dirs.push(dir);
        write_store(store_path, &store)?;
    }
    Ok(())
}

/// 記録から外す。外したら true。
pub fn forget(store_path: &Path, dir: &Path) -> Result<bool> {
    let dir = real(dir);
    let mut store = read_store(store_path)?;
    let before = store.dirs.len();
    store.dirs.retain(|t| *t != dir);
    let removed = store.dirs.len() != before;
    if removed {
        write_store(store_path, &store)?;
    }
    Ok(removed)
}

pub fn list(store_path: &Path) -> Result<Vec<PathBuf>> {
    Ok(read_store(store_path)?.dirs)
}

/// 起動時の判断の材料。
pub struct Request<'a> {
    pub cwd: &'a Path,
    pub store_path: Option<&'a Path>,
    /// `--trust` / `LODAN_TRUST`: この実行に限って信頼する。
    pub trust_flag: bool,
    /// 尋ねる相手がいるか (REPL かつ stdin が端末)。
    pub interactive: bool,
}

/// プロジェクトのファイルを読んでよいかを決める。尋ねる必要があれば `input` / `out` で尋ねる。
///
/// - 読むべきファイルが 1 つも無ければ、尋ねずに true (決めるものが無い)
/// - `--trust`、または記録済みなら true
/// - 尋ねる相手がいなければ false (`-p` / パイプ)。何を読まなかったかは `out` に書く
/// - それ以外は尋ねる: (y) 信頼して記録 / (o) 今回だけ / (n) 読まずに起動
pub fn decide(req: &Request<'_>, input: &mut dyn BufRead, out: &mut dyn Write) -> bool {
    let files = project_files(req.cwd);
    if files.is_empty() || req.trust_flag {
        return true;
    }
    if let Some(store) = req.store_path
        && is_recorded(store, req.cwd)
    {
        return true;
    }
    if !req.interactive {
        let _ = writeln!(
            out,
            "lodan: {} is not a trusted directory; ignoring {}. Run `lodan trust` here, or pass --trust.",
            shown(req.cwd),
            files.join(", ")
        );
        return false;
    }

    let _ = writeln!(
        out,
        "{} This directory has lodan project settings that have not been trusted yet:",
        crate::term::yellow("[lodan]")
    );
    for f in &files {
        let _ = writeln!(out, "    {f}");
    }
    let _ = writeln!(
        out,
        "  They can run commands (hooks, MCP servers), change where your API key is sent, and \
         turn off approval prompts.\n  Only trust directories whose contents you have reviewed."
    );
    loop {
        let _ = writeln!(
            out,
            "{}",
            crate::term::dim("  (y) trust this directory  (o) trust once  (n) start without them")
        );
        let _ = write!(out, "{} ", crate::term::yellow(">"));
        let _ = out.flush();
        let mut line = String::new();
        // 答えが無い (EOF / エラー) は信頼しない。
        if !matches!(input.read_line(&mut line), Ok(n) if n > 0) {
            let _ = writeln!(out, "(no input — starting without them)");
            return false;
        }
        match line.trim() {
            "y" | "Y" => {
                if let Some(store) = req.store_path
                    && let Err(e) = record(store, req.cwd)
                {
                    let _ = writeln!(out, "  could not save the decision: {e:#} (trusting once)");
                }
                return true;
            }
            "o" | "O" => return true,
            "n" | "N" => return false,
            // Enter だけでは信頼しない。明示的に選ばせる。
            _ => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn the_store_is_not_written_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim.toml");
        std::fs::write(&victim, "keep = true\n").unwrap();
        let store = dir.path().join("trusted.toml");
        std::os::unix::fs::symlink(&victim, &store).unwrap();
        assert!(record(&store, dir.path()).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep = true\n");
    }

    fn project(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for f in files {
            let p = dir.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        }
        dir
    }

    fn ask(
        dir: &Path,
        store: &Path,
        interactive: bool,
        flag: bool,
        answer: &str,
    ) -> (bool, String) {
        let mut out = Vec::new();
        let trusted = decide(
            &Request {
                cwd: dir,
                store_path: Some(store),
                trust_flag: flag,
                interactive,
            },
            &mut answer.as_bytes(),
            &mut out,
        );
        (trusted, String::from_utf8(out).unwrap())
    }

    #[test]
    fn a_directory_with_nothing_to_trust_is_never_asked_about() {
        let dir = project(&["src/main.rs"]);
        let store = dir.path().join("store.toml");
        let (trusted, shown) = ask(dir.path(), &store, true, false, "");
        assert!(trusted);
        assert!(shown.is_empty());
    }

    /// メモリの探索と trust の候補が揃っていること。片方に名前を足して片方を忘れると、
    /// その名前のファイルだけのリポジトリが尋ねられずに信頼される。
    #[test]
    fn every_memory_file_name_needs_trust() {
        for name in crate::memory::PROJECT_FILES {
            assert!(
                MEMORY_FILES.contains(name),
                "{name} is read as memory but not trust-gated"
            );
        }
        let dir = project(&["AGENTS.md"]);
        assert_eq!(project_files(dir.path()), ["AGENTS.md"]);
    }

    #[test]
    fn an_untrusted_project_asks_and_lists_what_it_found() {
        let dir = project(&[".lodan/config.toml", ".mcp.json", "CLAUDE.md"]);
        let store = dir.path().join("store.toml");
        let (trusted, shown) = ask(dir.path(), &store, true, false, "n\n");
        assert!(!trusted);
        for f in [".lodan/config.toml", ".mcp.json", "CLAUDE.md"] {
            assert!(shown.contains(f), "{shown}");
        }
        assert!(!store.exists(), "declining records nothing");
    }

    #[test]
    fn y_is_remembered_and_covers_subdirectories_o_is_not() {
        let dir = project(&[".lodan/config.toml", "sub/.mcp.json"]);
        let store = dir.path().join("store.toml");

        let (once, _) = ask(dir.path(), &store, true, false, "o\n");
        assert!(once);
        assert!(!is_recorded(&store, dir.path()), "(o) must not persist");

        let (yes, _) = ask(dir.path(), &store, true, false, "y\n");
        assert!(yes);
        assert!(is_recorded(&store, dir.path()));
        // 次回は尋ねない。配下も同じ。
        let (again, shown) = ask(dir.path(), &store, true, false, "");
        assert!(again && shown.is_empty());
        let (sub, shown) = ask(&dir.path().join("sub"), &store, true, false, "");
        assert!(sub && shown.is_empty());

        assert!(forget(&store, dir.path()).unwrap());
        assert!(!is_recorded(&store, dir.path()));
    }

    #[test]
    fn a_dotenv_file_and_a_parents_memory_both_need_trust() {
        let dir = project(&["repo/.env", "CLAUDE.md", "repo/sub/code.rs"]);
        let store = dir.path().join("store.toml");
        assert_eq!(
            project_files(&dir.path().join("repo"))
                .first()
                .map(String::as_str),
            Some(".env")
        );
        // 何も無いサブディレクトリから起動しても、親のメモリを尋ねないまま読んだりしない。
        let (trusted, shown) = ask(&dir.path().join("repo/sub"), &store, false, false, "");
        assert!(!trusted);
        assert!(shown.contains("CLAUDE.md"), "{shown}");
    }

    #[test]
    fn a_hostile_directory_name_cannot_rewrite_the_notice() {
        let dir = tempfile::tempdir().unwrap();
        let evil = dir.path().join("repo\x1b[2K\x1b[1Gtrusted \u{202E}");
        std::fs::create_dir_all(evil.join(".lodan")).unwrap();
        std::fs::write(evil.join(".lodan/config.toml"), "x").unwrap();
        let (trusted, shown) = ask(&evil, &dir.path().join("store.toml"), false, false, "");
        assert!(!trusted);
        assert!(
            !shown.contains('\x1b') && !shown.contains('\u{202E}'),
            "{shown:?}"
        );
    }

    #[test]
    fn enter_garbage_and_eof_never_grant_trust() {
        let dir = project(&[".lodan/config.toml"]);
        let store = dir.path().join("store.toml");
        assert!(!ask(dir.path(), &store, true, false, "").0, "EOF");
        assert!(
            !ask(dir.path(), &store, true, false, "\n\n").0,
            "Enter is not yes"
        );
        assert!(!ask(dir.path(), &store, true, false, "yes please\nn\n").0);
    }

    #[test]
    fn non_interactive_runs_never_prompt_and_say_what_they_skipped() {
        let dir = project(&[".lodan/config.toml", ".mcp.json"]);
        let store = dir.path().join("store.toml");
        let (trusted, shown) = ask(dir.path(), &store, false, false, "y\n");
        assert!(
            !trusted,
            "an answer on stdin is not consulted when nobody can be asked"
        );
        assert!(
            shown.contains("not a trusted directory") && shown.contains(".mcp.json"),
            "{shown}"
        );
        // --trust は、その実行に限って信頼する (記録しない)。
        let (flagged, shown) = ask(dir.path(), &store, false, true, "");
        assert!(flagged && shown.is_empty());
        assert!(!is_recorded(&store, dir.path()));
    }

    #[cfg(unix)]
    #[test]
    fn trust_is_compared_on_resolved_paths() {
        let dir = project(&["real/.lodan/config.toml"]);
        let store = dir.path().join("store.toml");
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("alias")).unwrap();
        record(&store, &dir.path().join("alias")).unwrap();
        assert!(is_recorded(&store, &dir.path().join("real")));
        // 名前が前方一致するだけの別ディレクトリは含めない。
        std::fs::create_dir_all(dir.path().join("real-evil")).unwrap();
        assert!(!is_recorded(&store, &dir.path().join("real-evil")));
    }
}
