//! 検証ループ (#65 D1)。`[[verify]]` に書いたコマンドを、そのターンが**ファイルを変えたとき**に
//! 最終応答の直前で走らせ、失敗したら出力をモデルへ差し戻して直させる。
//!
//! ラダーの実測 (`docs/eval/ladder/phase1-2026-10.md`) で、L2-http が満点になったのは
//! 「モデルが自分で Bash → Read → Edit の検証ループを回した 1 回だけ」だった。回すかどうかを
//! モデルの気分に任せず、ハーネスが強制する。
//!
//! hook と同じく `sh -c` で走らせる (サンドボックスは通さない)。コマンドは利用者の設定由来で、
//! 未信頼ディレクトリでは `.lodan/config.toml` 自体を読まない (#75) ので、信頼境界は hook と同じ。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// `timeout_secs` 省略時の上限。テストを 1 本走らせる程度を見込む。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// モデルへ差し戻す出力の上限 (文字)。末尾を残す — 失敗の理由は普通最後に出る。
const MAX_OUTPUT_CHARS: usize = 2000;

/// `config.toml` の `[[verify]]` 1 エントリ。
///
/// ```toml
/// [[verify]]
/// id = "pytest"            # 省略可。`disabled_verify` に書けば外せる
/// command = "pytest -q"
/// paths = ["**/*.py"]      # 省略可。このターンで変えたファイルが 1 つでも当たるときだけ走る
/// timeout_secs = 120       # 省略可 (既定 60)
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub command: String,
    /// 変更されたファイルに対する glob。空なら「このターンで何かを変えたら走る」。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl VerifyConfig {
    /// 表示用の名前 (`id` が無ければコマンドの先頭)。
    pub fn label(&self) -> String {
        self.id.clone().unwrap_or_else(|| {
            self.command
                .split_whitespace()
                .next()
                .unwrap_or("verify")
                .to_string()
        })
    }

    /// `changed` のどれかが `paths` に当たるか。
    ///
    /// `paths` が空なら「このターンで何かを変えたら走る」。`ran_shell` は Bash が成功したこと —
    /// lodan は Bash が**どのファイルを変えたか知らない**ので、`paths` 無しの検証はこれでも走らせる
    /// (`sed -i` のような書き換えを取りこぼさないため)。`paths` つきの検証は、ファイル系ツールが
    /// 触ったパスだけで判定する (#149 のレビュー)。
    pub fn applies(&self, changed: &[PathBuf], ran_shell: bool, cwd: &Path) -> bool {
        if self.paths.is_empty() {
            return !changed.is_empty() || ran_shell;
        }
        if changed.is_empty() {
            return false;
        }
        let matchers: Vec<globset::GlobMatcher> =
            self.paths.iter().filter_map(|p| glob(p)).collect();
        changed.iter().any(|f| {
            relative_inside(f, cwd).is_some_and(|rel| matchers.iter().any(|m| m.is_match(&rel)))
        })
    }
}

/// `disabled` に挙がった `id` を外し、同じ `id` が複数あれば最後のものだけを残す (hook と同じ規則)。
pub fn effective(all: &[VerifyConfig], disabled: &[String]) -> Vec<VerifyConfig> {
    all.iter()
        .enumerate()
        .filter(|(i, v)| match &v.id {
            None => true,
            Some(id) => {
                !disabled.contains(id)
                    && !all[i + 1..]
                        .iter()
                        .any(|later| later.id.as_ref() == Some(id))
            }
        })
        .map(|(_, v)| v.clone())
        .collect()
}

/// 1 本の検証の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub label: String,
    pub command: String,
    /// 成功したか (終了コード 0)。時間切れと起動失敗は失敗扱い。
    pub ok: bool,
    /// モデルへ見せる出力 (末尾 `MAX_OUTPUT_CHARS` 文字)。
    pub output: String,
    pub ms: u64,
}

/// 当てはまる検証を順に走らせる。成功したものも含めて全ての結果を返す。
pub async fn run_applicable(
    verifies: &[VerifyConfig],
    changed: &[PathBuf],
    ran_shell: bool,
    cwd: &Path,
) -> Vec<Outcome> {
    let mut out = Vec::new();
    for v in verifies
        .iter()
        .filter(|v| v.applies(changed, ran_shell, cwd))
    {
        out.push(run_one(v, cwd).await);
    }
    out
}

async fn run_one(v: &VerifyConfig, cwd: &Path) -> Outcome {
    let started = std::time::Instant::now();
    let timeout = v
        .timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TIMEOUT);
    let spawned = Command::new("sh")
        .arg("-c")
        .arg(&v.command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let (ok, text) = match spawned {
        Err(e) => (false, format!("could not start: {e}")),
        Ok(child) => match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Err(_) => (false, format!("timed out after {}s", timeout.as_secs())),
            Ok(Err(e)) => (false, format!("could not run: {e}")),
            Ok(Ok(output)) => {
                let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
                let err = String::from_utf8_lossy(&output.stderr);
                if !err.trim().is_empty() {
                    if !combined.is_empty() && !combined.ends_with('\n') {
                        combined.push('\n');
                    }
                    combined.push_str(&err);
                }
                (output.status.success(), combined)
            }
        },
    };
    Outcome {
        label: v.label(),
        command: v.command.clone(),
        ok,
        output: tail(&text, MAX_OUTPUT_CHARS),
        ms: started.elapsed().as_millis() as u64,
    }
}

/// 末尾 `max` 文字 (切ったことを頭に書く)。失敗の理由は普通最後に出るので末尾を残す。
fn tail(text: &str, max: usize) -> String {
    let n = text.chars().count();
    if n <= max {
        return text.trim_end().to_string();
    }
    let skipped = n - max;
    let kept: String = text.chars().skip(skipped).collect();
    format!("…({skipped} chars omitted)\n{}", kept.trim_end())
}

/// 失敗した検証をモデルへ差し戻す 1 通のメッセージ。
pub fn repair_note(failed: &[&Outcome]) -> String {
    let mut out = String::from(
        "[lodan] Verification failed — do not finish yet. Fix the cause, then the same command \
         will run again:\n",
    );
    for f in failed {
        out.push_str(&format!(
            "\n$ {}\n{}\n",
            crate::term::sanitize(&f.command),
            crate::term::sanitize(&f.output)
        ));
    }
    out.push_str(
        "\nRead the files involved before editing them. If the command itself is wrong (not the \
         code), say so in your final answer instead of looping.",
    );
    out
}

fn glob(pattern: &str) -> Option<globset::GlobMatcher> {
    let body = if pattern.contains('/') {
        pattern.trim_start_matches("./").to_string()
    } else {
        format!("**/{pattern}")
    };
    globset::GlobBuilder::new(&body)
        .literal_separator(true)
        .build()
        .ok()
        .map(|g| g.compile_matcher())
}

/// `file` が `cwd` の中にあれば、その相対パス。外なら None (`..` は畳んでから見る)。
fn relative_inside(file: &Path, cwd: &Path) -> Option<PathBuf> {
    let joined = if file.is_absolute() {
        file.to_path_buf()
    } else {
        cwd.join(file)
    };
    let cwd_real = std::fs::canonicalize(cwd).unwrap_or_else(|_| normalize(cwd));
    let real = std::fs::canonicalize(&joined).unwrap_or_else(|_| normalize(&joined));
    real.strip_prefix(&cwd_real).ok().map(Path::to_path_buf)
}

fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(id: Option<&str>, command: &str, paths: &[&str]) -> VerifyConfig {
        VerifyConfig {
            id: id.map(str::to_string),
            command: command.to_string(),
            paths: paths.iter().map(|s| s.to_string()).collect(),
            timeout_secs: None,
        }
    }

    #[test]
    fn applies_only_when_a_changed_file_matches() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        for f in ["src/a.py", "src/b.rs", "notes.md"] {
            std::fs::write(cwd.join(f), "x").unwrap();
        }
        let py = cfg(None, "pytest", &["**/*.py"]);
        let any = cfg(None, "make", &[]);
        let changed = |fs: &[&str]| fs.iter().map(|f| cwd.join(f)).collect::<Vec<_>>();
        assert!(py.applies(&changed(&["src/a.py"]), false, cwd));
        assert!(!py.applies(&changed(&["src/b.rs", "notes.md"]), false, cwd));
        // paths 無しは「何か変わったら走る」。何も変わっていなければ走らない。
        assert!(any.applies(&changed(&["notes.md"]), false, cwd));
        assert!(!any.applies(&[], false, cwd));
        // Bash が走ったターンは、どのファイルが変わったか分からないので paths 無しだけ走らせる。
        assert!(any.applies(&[], true, cwd));
        assert!(!py.applies(&[], true, cwd));
        // cwd の外のファイルは当たらない。
        assert!(!py.applies(&[PathBuf::from("/tmp/outside.py")], false, cwd));
        // 拡張子だけのパターンはどの階層にも当たる。
        assert!(cfg(None, "x", &["*.py"]).applies(&changed(&["src/a.py"]), false, cwd));
    }

    #[test]
    fn disabled_and_replaced_ids_follow_the_hook_rules() {
        let all = [
            cfg(Some("test"), "user-test", &[]),
            cfg(None, "anonymous", &[]),
            cfg(Some("test"), "project-test", &[]),
        ];
        let commands = |disabled: &[&str]| {
            effective(
                &all,
                &disabled.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )
            .into_iter()
            .map(|v| v.command)
            .collect::<Vec<_>>()
        };
        assert_eq!(commands(&[]), ["anonymous", "project-test"]);
        assert_eq!(commands(&["test"]), ["anonymous"]);
    }

    #[tokio::test]
    async fn a_failing_command_is_reported_with_its_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"), "x").unwrap();
        let changed = vec![dir.path().join("a.py")];
        let ok = cfg(Some("ok"), "echo fine", &[]);
        let bad = cfg(None, "echo boom >&2; exit 3", &[]);
        let outs = run_applicable(&[ok, bad], &changed, false, dir.path()).await;
        assert_eq!(outs.len(), 2);
        assert!(outs[0].ok && outs[0].output == "fine" && outs[0].label == "ok");
        assert!(!outs[1].ok && outs[1].output == "boom");
        // label は id が無ければコマンドの先頭語。
        assert_eq!(outs[1].label, "echo");
        let note = repair_note(&[&outs[1]]);
        assert!(note.starts_with("[lodan] Verification failed"), "{note}");
        assert!(note.contains("$ echo boom >&2; exit 3") && note.contains("boom"));
    }

    /// Bash が走ったターン: `paths` 無しの検証だけが走る (#149 のレビュー)。
    #[tokio::test]
    async fn a_shell_turn_runs_only_the_paths_less_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let any = cfg(Some("any"), "echo ran", &[]);
        let py = cfg(Some("py"), "echo nope", &["**/*.py"]);
        let outs = run_applicable(&[any, py], &[], true, dir.path()).await;
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].label, "any");
    }

    #[tokio::test]
    async fn a_hung_command_fails_after_its_own_timeout() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"), "x").unwrap();
        let mut slow = cfg(None, "sleep 30", &[]);
        slow.timeout_secs = Some(1);
        let outs = run_applicable(&[slow], &[dir.path().join("a.py")], false, dir.path()).await;
        assert!(!outs[0].ok && outs[0].output.contains("timed out after 1s"));
    }

    #[test]
    fn long_output_keeps_the_tail() {
        let text = (1..=100).map(|i| format!("line {i}\n")).collect::<String>();
        let cut = tail(&text, 40);
        assert!(cut.starts_with("…(") && cut.ends_with("line 100"), "{cut}");
        assert_eq!(tail("short", 40), "short");
    }
}
