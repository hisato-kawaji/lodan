use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::sync::Mutex;

#[derive(Debug, Default)]
pub struct SessionPolicy {
    pub always_tools: HashSet<String>,
    pub always_commands: HashSet<String>,
}

pub struct PermissionGate {
    auto_approve: bool,
    policy: Mutex<SessionPolicy>,
}

impl PermissionGate {
    pub fn new(auto_approve: bool) -> Self {
        Self {
            auto_approve,
            policy: Mutex::new(SessionPolicy::default()),
        }
    }

    pub fn allow(&self, tool_name: &str, args: &serde_json::Value) -> bool {
        if self.auto_approve {
            return true;
        }
        if let Ok(p) = self.policy.lock() {
            if p.always_tools.contains(tool_name) {
                return true;
            }
            if tool_name == "Bash"
                && let Some(cmd) = args.get("command").and_then(|v| v.as_str())
                && p.always_commands.contains(cmd)
            {
                return true;
            }
        }
        self.prompt(tool_name, args)
    }

    fn prompt(&self, tool_name: &str, args: &serde_json::Value) -> bool {
        let summary = summarize(tool_name, args);
        let stdin = io::stdin();
        let mut stdout = io::stdout().lock();
        loop {
            let _ = writeln!(
                stdout,
                "{} Allow {}: {summary}",
                crate::term::yellow("[lodan]"),
                crate::term::bold(tool_name),
            );
            if let Some(p) = preview(tool_name, args) {
                let _ = writeln!(stdout, "{p}");
            }
            let _ = writeln!(
                stdout,
                "{}",
                crate::term::dim(&format!(
                    "  (y) yes once  (n) no  (a) always allow {tool_name}  (e) always allow this exact"
                ))
            );
            let _ = write!(stdout, "{} ", crate::term::yellow(">"));
            let _ = stdout.flush();

            let mut line = String::new();
            if stdin.lock().read_line(&mut line).is_err() {
                return false;
            }
            match line.trim() {
                "y" | "Y" | "" => return true,
                "n" | "N" => return false,
                "a" | "A" => {
                    if let Ok(mut p) = self.policy.lock() {
                        p.always_tools.insert(tool_name.to_string());
                    }
                    return true;
                }
                "e" | "E" => {
                    if tool_name == "Bash"
                        && let Some(cmd) = args.get("command").and_then(|v| v.as_str())
                    {
                        if let Ok(mut p) = self.policy.lock() {
                            p.always_commands.insert(cmd.to_string());
                        }
                        return true;
                    }
                    if let Ok(mut p) = self.policy.lock() {
                        p.always_tools.insert(tool_name.to_string());
                    }
                    return true;
                }
                _ => continue,
            }
        }
    }
}

fn summarize(tool: &str, args: &serde_json::Value) -> String {
    match tool {
        "Bash" => args
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| format!("`{s}`"))
            .unwrap_or_else(|| args.to_string()),
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => args
            .get("path")
            .and_then(|v| v.as_str())
            .map(rel_path)
            .unwrap_or_else(|| args.to_string()),
        // 計画本文は直前に表示済みなので、プロンプトには要旨だけ出す。
        "ExitPlanMode" => "approve the plan above and exit plan mode".to_string(),
        _ => args.to_string(),
    }
}

/// cwd 配下のパスは相対表示にする (#42 P5)。cwd 外・取得失敗時はそのまま。
fn rel_path(path: &str) -> String {
    if let Ok(cwd) = std::env::current_dir()
        && let Ok(rel) = std::path::Path::new(path).strip_prefix(&cwd)
        && !rel.as_os_str().is_empty()
    {
        return rel.display().to_string();
    }
    path.to_string()
}

/// プレビュー 1 ブロックあたりの最大行数。
const PREVIEW_MAX_LINES: usize = 8;
/// MultiEdit で個別プレビューする最大 edit 数。
const PREVIEW_MAX_EDITS: usize = 3;

/// 承認プロンプトの下に出す変更内容プレビュー (#42 P5)。
/// Edit/MultiEdit は old→new の差分風表示、Write は書き込む内容の先頭。
/// 対象外のツールは None。
fn preview(tool: &str, args: &serde_json::Value) -> Option<String> {
    let as_str = |key: &str| args.get(key).and_then(|v| v.as_str());
    match tool {
        "Edit" => Some(diff_block(
            as_str("old_string").unwrap_or(""),
            as_str("new_string").unwrap_or(""),
        )),
        "MultiEdit" => {
            let edits = args.get("edits")?.as_array()?;
            let mut out = String::new();
            for (i, e) in edits.iter().take(PREVIEW_MAX_EDITS).enumerate() {
                if i > 0 {
                    out.push('\n');
                }
                let g = |k: &str| e.get(k).and_then(|v| v.as_str()).unwrap_or("");
                out.push_str(&diff_block(g("old_string"), g("new_string")));
            }
            if edits.len() > PREVIEW_MAX_EDITS {
                out.push_str(&crate::term::dim(&format!(
                    "\n  … (+{} more edits)",
                    edits.len() - PREVIEW_MAX_EDITS
                )));
            }
            Some(out)
        }
        "Write" => {
            let content = as_str("content")?;
            let mut out = String::new();
            for (i, line) in content.lines().take(PREVIEW_MAX_LINES).enumerate() {
                if i > 0 {
                    out.push('\n');
                }
                out.push_str(&crate::term::green(&format!("  + {line}")));
            }
            let total = content.lines().count();
            if total > PREVIEW_MAX_LINES {
                out.push_str(&crate::term::dim(&format!(
                    "\n  … (+{} more lines, {} bytes)",
                    total - PREVIEW_MAX_LINES,
                    content.len()
                )));
            }
            Some(out)
        }
        _ => None,
    }
}

/// old→new の差分風ブロック (`- old` 赤 / `+ new` 緑、各ブロック行数上限つき)。
fn diff_block(old: &str, new: &str) -> String {
    let mut out = String::new();
    let mut push_side = |text: &str, sign: char, color: fn(&str) -> String| {
        let total = text.lines().count();
        for line in text.lines().take(PREVIEW_MAX_LINES) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&color(&format!("  {sign} {line}")));
        }
        if total > PREVIEW_MAX_LINES {
            out.push_str(&crate::term::dim(&format!(
                "\n  … (+{} more {sign} lines)",
                total - PREVIEW_MAX_LINES
            )));
        }
    };
    push_side(old, '-', crate::term::red);
    push_side(new, '+', crate::term::green);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_approve_short_circuits() {
        let g = PermissionGate::new(true);
        assert!(g.allow("Bash", &serde_json::json!({"command":"ls"})));
    }

    #[test]
    fn always_tool_membership() {
        let g = PermissionGate::new(false);
        g.policy
            .lock()
            .unwrap()
            .always_tools
            .insert("Write".to_string());
        assert!(g.allow("Write", &serde_json::json!({"path":"/x"})));
    }

    #[test]
    fn always_command_membership() {
        let g = PermissionGate::new(false);
        g.policy
            .lock()
            .unwrap()
            .always_commands
            .insert("ls -la".to_string());
        assert!(g.allow("Bash", &serde_json::json!({"command":"ls -la"})));
    }

    #[test]
    fn edit_preview_shows_diff() {
        let p = preview(
            "Edit",
            &serde_json::json!({"path": "/x", "old_string": "a\nb", "new_string": "c"}),
        )
        .unwrap();
        assert!(p.contains("- a") && p.contains("- b"), "{p}");
        assert!(p.contains("+ c"), "{p}");
    }

    #[test]
    fn write_preview_shows_head_and_caps() {
        let content: String = (0..20).map(|i| format!("l{i}\n")).collect();
        let p = preview(
            "Write",
            &serde_json::json!({"path": "/x", "content": content}),
        )
        .unwrap();
        assert!(p.contains("+ l0") && p.contains("+ l7"), "{p}");
        assert!(!p.contains("+ l8"), "{p}");
        assert!(p.contains("+12 more lines"), "{p}");
    }

    #[test]
    fn multi_edit_preview_caps_edits() {
        let edits: Vec<serde_json::Value> = (0..5)
            .map(|i| {
                serde_json::json!({"old_string": format!("o{i}"), "new_string": format!("n{i}")})
            })
            .collect();
        let p = preview(
            "MultiEdit",
            &serde_json::json!({"path": "/x", "edits": edits}),
        )
        .unwrap();
        assert!(p.contains("- o0") && p.contains("+ n2"), "{p}");
        assert!(!p.contains("o3"), "only PREVIEW_MAX_EDITS edits shown: {p}");
        assert!(p.contains("+2 more edits"), "{p}");
    }

    #[test]
    fn bash_has_no_preview() {
        assert!(preview("Bash", &serde_json::json!({"command": "ls"})).is_none());
    }

    /// cwd 配下は相対表示、cwd 外は絶対のまま。
    #[test]
    fn summarize_relativizes_cwd_paths() {
        let cwd = std::env::current_dir().unwrap();
        let abs = cwd.join("src/main.rs");
        let s = summarize("Edit", &serde_json::json!({"path": abs.to_str().unwrap()}));
        assert_eq!(s, "src/main.rs");
        let s2 = summarize("Write", &serde_json::json!({"path": "/etc/hosts"}));
        assert_eq!(s2, "/etc/hosts");
    }
}
