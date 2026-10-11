//! 編集直後の構文チェック (#65 D2)。`Write` / `Edit` / `MultiEdit` / `NotebookEdit` が成功した
//! 直後に、拡張子ごとの短いコマンドで**そのファイルだけ**を検査し、壊れていたらツール結果の
//! 後ろに注記を足す。
//!
//! **ブロックはしない**。ファイルは既に書かれているし、拒否すると「書けない」ループに入る。
//! 目的は「崩れた生成が何ターンも気付かれずに積み上がる」のを防ぐこと
//! (`docs/eval/ladder/phase1-2026-10.md` の A2 の失敗は、未完成のファイルが残って終了していた)。
//!
//! プロジェクト全体のビルド (`cargo check` / `tsc`) は [`crate::verify`] (#65 D1) の持ち場。
//! ここは単ファイルで数百 ms で終わるものだけを扱う。

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// 1 ファイルの検査に許す時間。構文チェックは普通 100ms 程度で終わる。
const TIMEOUT: Duration = Duration::from_secs(5);
/// 注記に載せる出力の上限 (文字)。構文エラーは 1〜数行。
const MAX_OUTPUT_CHARS: usize = 600;

/// 拡張子 → コマンドの既定。`{path}` は検査するファイルの絶対パス (shell quote 済み) に置き換わる。
/// 遅いもの (`tsc` / `cargo check`) と、単ファイルで完結しないものは既定に入れない。
pub fn defaults() -> BTreeMap<String, String> {
    [
        ("py", "python3 -m py_compile {path}"),
        ("js", "node --check {path}"),
        ("mjs", "node --check {path}"),
        ("cjs", "node --check {path}"),
        ("json", "python3 -m json.tool {path}"),
        ("sh", "sh -n {path}"),
        ("bash", "bash -n {path}"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// `file` に使うコマンド。設定で上書き・追加でき、空文字は「検査しない」。
/// 既定に無い拡張子も設定に書けば検査する。
pub fn command_for(file: &Path, overrides: &BTreeMap<String, String>) -> Option<String> {
    let ext = file.extension()?.to_str()?.to_ascii_lowercase();
    let template = match overrides.get(&ext) {
        Some(t) => t.clone(),
        None => defaults().get(&ext)?.clone(),
    };
    let template = template.trim();
    if template.is_empty() {
        return None;
    }
    Some(template.replace("{path}", &quote(&file.to_string_lossy())))
}

/// `sh -c` に渡す引数として安全な形。単引用符で囲み、中の `'` を閉じて繋ぐ。
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// 検査の結果。`None` は「検査しなかった」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub command: String,
    pub ok: bool,
    /// 失敗したときの出力 (末尾 `MAX_OUTPUT_CHARS` 文字)。成功なら空。
    pub output: String,
    pub ms: u64,
}

impl Outcome {
    /// ツール結果の後ろに足す注記。壊れていることと、直し方を 1 行で伝える。
    pub fn note(&self) -> String {
        format!(
            "\n\n[syntax] {} failed:\n{}\nThe file was written as given. Fix it with Edit before \
             moving on.",
            crate::term::sanitize(&self.command),
            crate::term::sanitize(&self.output)
        )
    }
}

/// `file` を検査する。対象外の拡張子・設定が空・コマンドが起動できない場合は `None`
/// (「検査できなかった」はモデルに言わない — 出力を増やすだけなので)。
pub async fn check(file: &Path, overrides: &BTreeMap<String, String>) -> Option<Outcome> {
    let command = command_for(file, overrides)?;
    let started = std::time::Instant::now();
    let child = Command::new("sh")
        .arg("-c")
        .arg(&command)
        .current_dir(file.parent().unwrap_or(Path::new(".")))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let output = tokio::time::timeout(TIMEOUT, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    let ok = output.status.success();
    let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
    if text.trim().is_empty() {
        text = String::from_utf8_lossy(&output.stdout).into_owned();
    }
    Some(Outcome {
        command,
        ok,
        output: if ok {
            String::new()
        } else {
            tail(&text, MAX_OUTPUT_CHARS)
        },
        ms: started.elapsed().as_millis() as u64,
    })
}

/// 末尾 `max` 文字。構文エラーの行番号は末尾に出る。
fn tail(text: &str, max: usize) -> String {
    let n = text.chars().count();
    if n <= max {
        return text.trim_end().to_string();
    }
    let kept: String = text.chars().skip(n - max).collect();
    format!("…\n{}", kept.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overrides(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn commands_come_from_the_defaults_and_can_be_overridden_or_disabled() {
        let none = BTreeMap::new();
        let py = command_for(Path::new("/x/a.py"), &none).unwrap();
        assert_eq!(py, "python3 -m py_compile '/x/a.py'");
        // 大文字の拡張子も既定に当たる。
        assert!(command_for(Path::new("/x/a.PY"), &none).is_some());
        // 既定に無い拡張子・拡張子なしは検査しない。
        assert!(command_for(Path::new("/x/a.rs"), &none).is_none());
        assert!(command_for(Path::new("/x/Makefile"), &none).is_none());
        // 上書き・追加・空文字で無効化。
        let ov = overrides(&[
            ("py", "ruff check {path}"),
            ("rs", "rustc --edition 2024 --emit=metadata {path}"),
            ("json", ""),
        ]);
        assert_eq!(
            command_for(Path::new("/x/a.py"), &ov).unwrap(),
            "ruff check '/x/a.py'"
        );
        assert!(
            command_for(Path::new("/x/a.rs"), &ov)
                .unwrap()
                .starts_with("rustc")
        );
        assert!(command_for(Path::new("/x/a.json"), &ov).is_none());
        // 単引用符を含むパスも壊れない。
        let odd = command_for(Path::new("/x/it's.py"), &none).unwrap();
        assert!(odd.ends_with(r"'/x/it'\''s.py'"), "{odd}");
    }

    #[tokio::test]
    async fn broken_python_is_reported_and_valid_python_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let none = BTreeMap::new();
        let good = dir.path().join("good.py");
        std::fs::write(&good, "x = 1\n").unwrap();
        let out = check(&good, &none).await.unwrap();
        assert!(out.ok && out.output.is_empty());

        let bad = dir.path().join("bad.py");
        std::fs::write(&bad, "def f(:\n").unwrap();
        let out = check(&bad, &none).await.unwrap();
        assert!(!out.ok, "{out:?}");
        let note = out.note();
        assert!(
            note.contains("[syntax]") && note.contains("Fix it with Edit"),
            "{note}"
        );
        assert!(
            note.contains("SyntaxError") || note.contains("invalid syntax"),
            "{note}"
        );

        // 対象外の拡張子は検査しない。
        let other = dir.path().join("a.rs");
        std::fs::write(&other, "fn main() {").unwrap();
        assert!(check(&other, &none).await.is_none());
        // 起動できないコマンドは「検査しなかった」扱い (ツール結果を汚さない)。
        let missing = overrides(&[("py", "definitely-not-a-real-binary {path}")]);
        let out = check(&good, &missing).await;
        assert!(out.is_some_and(|o| !o.ok), "exit != 0 として扱う");
    }

    #[test]
    fn long_output_keeps_the_tail() {
        let text = (1..=200).map(|i| format!("line {i}\n")).collect::<String>();
        let cut = tail(&text, 30);
        assert!(cut.starts_with("…\n") && cut.ends_with("line 200"), "{cut}");
    }
}
