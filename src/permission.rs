use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::permission_rules::{RuleSet, Verdict};

#[derive(Debug, Default)]
pub struct SessionPolicy {
    pub always_tools: HashSet<String>,
    pub always_commands: HashSet<String>,
    /// このセッション中に `(p)` で保存した allow ルール。次回以降は設定ファイルから読まれる。
    pub saved_rules: Vec<crate::permission_rules::Rule>,
}

pub struct PermissionGate {
    auto_approve: bool,
    /// 尋ねる相手がいない (ヘッドレス実行) か、`dont-ask` モード。承認が要る呼び出しは尋ねずに拒否する。
    non_interactive: bool,
    /// `accept-edits` モード: ファイル編集ツールは尋ねずに通す。
    accept_edits: bool,
    rules: RuleSet,
    /// 相対パターンのルールを解決する基準。
    cwd: PathBuf,
    policy: Mutex<SessionPolicy>,
}

/// ゲートの結論。尋ねる必要があれば `decide` の中で尋ね終えている。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// モデルへ返す理由つき。
    Deny(String),
}

/// 検索範囲が広すぎて deny ルールに当たるか確かめきれなかったときの文面。
/// 「やり直すな」ではなく「範囲を狭めてやり直せ」— 狭めれば通り得る。
pub fn unverifiable_message(rule: &str) -> String {
    format!(
        "this search covers too much to verify against permission rule `{rule}`. \
         Retry with `path` set to a smaller directory."
    )
}

/// `accept-edits` モードで尋ねずに通すツール。
const EDIT_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit"];

const DENIED_BY_USER: &str = "user denied execution";
const DENIED_NON_INTERACTIVE: &str = "denied: this is a non-interactive run and nobody can approve \
    this action. Do not retry it. Finish with the tools that are allowed, or report what is blocked \
    (the operator can re-run with --yes to allow destructive tools).";

impl PermissionGate {
    pub fn new(auto_approve: bool) -> Self {
        Self {
            auto_approve,
            non_interactive: false,
            accept_edits: false,
            rules: RuleSet::default(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            policy: Mutex::new(SessionPolicy::default()),
        }
    }

    /// 設定からゲートを組む。`interactive` は尋ねる相手がいるか (REPL = true、`-p` = false)。
    /// ルールが 1 つでも解釈できなければエラー — 権限の設定を黙って読み飛ばさない。
    pub fn from_config(
        cfg: &crate::config::Config,
        cwd: &Path,
        interactive: bool,
    ) -> anyhow::Result<Self> {
        use crate::config::PermissionMode;
        let p = &cfg.permissions;
        let mode = p.mode;
        Ok(Self {
            auto_approve: cfg.agent.auto_approve || mode == PermissionMode::Bypass,
            non_interactive: !interactive || mode == PermissionMode::DontAsk,
            accept_edits: mode == PermissionMode::AcceptEdits,
            rules: RuleSet::parse(&p.allow, &p.deny, &p.ask)?,
            cwd: cwd.to_path_buf(),
            policy: Mutex::new(SessionPolicy::default()),
        })
    }

    /// プロンプトを出さないゲート。stdin はプロンプト本文に使われ得るので、承認待ちで
    /// 読みに行くとハングするか、入力の続きを承認の答えと取り違える。
    pub fn non_interactive(auto_approve: bool) -> Self {
        Self {
            non_interactive: true,
            ..Self::new(auto_approve)
        }
    }

    /// 破壊的な呼び出しとして承認を求める (ExitPlanMode など、ツール以外の承認用)。
    pub fn allow(&self, tool_name: &str, args: &serde_json::Value) -> bool {
        self.decide(tool_name, args, true) == Decision::Allow
    }

    /// 呼び出しを通すか決める。必要なら尋ねる。
    pub fn decide(&self, tool_name: &str, args: &serde_json::Value, destructive: bool) -> Decision {
        if let Some(decision) = self.decide_quietly(tool_name, args, destructive) {
            return decision;
        }
        if self.non_interactive {
            return Decision::Deny(DENIED_NON_INTERACTIVE.to_string());
        }
        if self.prompt(tool_name, args) {
            Decision::Allow
        } else {
            Decision::Deny(DENIED_BY_USER.to_string())
        }
    }

    /// 尋ねずに決まるならその結論、尋ねる必要があるなら `None`。
    /// 並列の先行実行はこれが `Allow` の呼び出しだけを対象にする (尋ねながら並列にはできないし、
    /// deny ルールに当たる Read を先に読んでしまってもいけない)。
    ///
    /// 順序: **deny ルール** → bypass → ask ルール → allow ルール → セッション中の「常に許可」→
    /// 既定 (read-only は通す / accept-edits の編集は通す / それ以外は尋ねる)。
    pub fn decide_quietly(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        destructive: bool,
    ) -> Option<Decision> {
        let verdict = self.rules.evaluate(tool_name, args, &self.cwd);
        // deny は `--yes` にも勝つ。「全部通す」と「これだけは絶対に通さない」を両立させるため。
        if let Some(Verdict::Deny(rule)) = &verdict {
            return Some(Decision::Deny(format!(
                "denied by permission rule `{rule}`. Do not retry this call or work around it; \
                 use another approach or report that it is blocked."
            )));
        }
        if let Some(Verdict::Unverifiable(rule)) = &verdict {
            return Some(Decision::Deny(unverifiable_message(rule)));
        }
        if self.auto_approve {
            return Some(Decision::Allow);
        }
        match verdict {
            Some(Verdict::Ask) => return None,
            Some(Verdict::Allow) => return Some(Decision::Allow),
            Some(Verdict::Deny(_) | Verdict::Unverifiable(_)) | None => {}
        }
        if let Ok(p) = self.policy.lock() {
            if p.always_tools.contains(tool_name)
                || p.saved_rules
                    .iter()
                    .any(|r| r.allows(tool_name, args, &self.cwd))
            {
                return Some(Decision::Allow);
            }
            if tool_name == "Bash"
                && let Some(cmd) = args.get("command").and_then(|v| v.as_str())
                && p.always_commands.contains(cmd)
            {
                return Some(Decision::Allow);
            }
        }
        if !destructive || (self.accept_edits && EDIT_TOOLS.contains(&tool_name)) {
            return Some(Decision::Allow);
        }
        None
    }

    fn prompt(&self, tool_name: &str, args: &serde_json::Value) -> bool {
        use std::io::IsTerminal;
        let stdin = io::stdin();
        // Enter だけで yes になるのは、人が端末で答えているときだけ。パイプされた入力の
        // 空行は答えではない (`printf 'do it\n\n' | lodan` が無承認で通ってしまう)。
        let enter_means_yes = stdin.is_terminal();
        self.prompt_with(
            tool_name,
            args,
            &mut stdin.lock(),
            &mut io::stdout().lock(),
            enter_means_yes,
        )
    }

    /// `prompt` の本体。入出力を差し替えられるようにしてある (テスト用)。
    fn prompt_with(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        input: &mut dyn BufRead,
        stdout: &mut dyn Write,
        enter_means_yes: bool,
    ) -> bool {
        let summary = summarize(tool_name, args);
        // 保存しても意図どおりに効かない呼び出しには (p) を出さない。
        // ask ルールに当たる呼び出しは、allow を保存しても毎回尋ねられる (ask が優先)。
        // 「保存した」と言って効かないものは出さない。
        let asked_by_rule = self.rules.evaluate(tool_name, args, &self.cwd) == Some(Verdict::Ask);
        let persistable = (!asked_by_rule)
            .then(|| crate::permission_rules::persistable_allow_rule(tool_name, args, &self.cwd))
            .flatten();
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
                crate::term::dim(&match &persistable {
                    Some(rule) => format!(
                        "  (y) yes once  (n) no  (a) always allow {tool_name}  (e) always allow this exact  \
                         (p) always allow `{rule}` in this project"
                    ),
                    None => format!(
                        "  (y) yes once  (n) no  (a) always allow {tool_name}  (e) always allow this exact"
                    ),
                })
            );
            let _ = write!(stdout, "{} ", crate::term::yellow(">"));
            let _ = stdout.flush();

            let mut line = String::new();
            if nobody_answered(&input.read_line(&mut line)) {
                let _ = writeln!(stdout, "(no input — denied)");
                return false;
            }
            match line.trim() {
                "" if !enter_means_yes => continue,
                "y" | "Y" | "" => return true,
                "n" | "N" => return false,
                "a" | "A" => {
                    if let Ok(mut p) = self.policy.lock() {
                        p.always_tools.insert(tool_name.to_string());
                    }
                    return true;
                }
                "p" | "P" if persistable.is_some() => {
                    let rule = persistable.as_deref().unwrap_or_default();
                    match crate::config::append_local_allow_rule(&self.cwd, rule) {
                        Ok(path) => {
                            let _ = writeln!(
                                stdout,
                                "{}",
                                crate::term::dim(&format!(
                                    "  saved `{rule}` to {} (keep this file out of version control)",
                                    path.display()
                                ))
                            );
                        }
                        // 保存できなくても今回の承認は有効。次回また尋ねることになるだけ。
                        Err(e) => {
                            let _ = writeln!(stdout, "  could not save the rule: {e:#}");
                        }
                    }
                    // このセッションでも以後は尋ねない。保存したのと**同じ広さ**で効かせる
                    // (`Edit(src/**)` を保存したのに、セッション中は Edit 全体が通る、にしない)。
                    if let Ok(mut p) = self.policy.lock()
                        && let Ok(parsed) = crate::permission_rules::Rule::parse(rule)
                    {
                        p.saved_rules.push(parsed);
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

/// 読み取りが「答えなし」だったか。EOF は 0 バイトの成功として返るので、エラーだけを
/// 見ていると空行 (= Enter = yes) と区別できず、プロンプトをパイプで渡した実行で
/// 破壊的ツールが無承認で通ってしまう。
fn nobody_answered(read: &io::Result<usize>) -> bool {
    matches!(read, Ok(0) | Err(_))
}

fn summarize(tool: &str, args: &serde_json::Value) -> String {
    match tool {
        "Bash" => args
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| format!("`{}`", visible(s)))
            .unwrap_or_else(|| args.to_string()),
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => args
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| visible(&rel_path(p)))
            .unwrap_or_else(|| args.to_string()),
        // 計画本文は直前に表示済みなので、プロンプトには要旨だけ出す。
        "ExitPlanMode" => "approve the plan above and exit plan mode".to_string(),
        // serde は C0 の制御文字はエスケープするが、U+202E のような書式文字は素通しする。
        _ => visible(&args.to_string()),
    }
}

/// 制御文字を `\u{1b}` のような見える形にする。承認プロンプトに出すのはモデルが渡した文字列で、
/// ANSI エスケープや CR をそのまま端末へ流すと、行を消したり上書きしたりして「承認しようと
/// しているもの」を偽れる。
fn visible(text: &str) -> String {
    text.chars()
        .map(|c| {
            if crate::permission_rules::is_invisible(c) {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// プレビュー用。コードにはタブが普通にあるので、タブだけはそのまま出す。
fn visible_line(line: &str) -> String {
    line.split('\t').map(visible).collect::<Vec<_>>().join("\t")
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
                out.push_str(&crate::term::green(&format!("  + {}", visible_line(line))));
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
            out.push_str(&color(&format!("  {sign} {}", visible_line(line))));
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

    fn gate_with(
        mode: crate::config::PermissionMode,
        allow: &[&str],
        deny: &[&str],
        ask: &[&str],
    ) -> PermissionGate {
        let mut cfg = crate::config::Config::default();
        cfg.permissions.mode = mode;
        cfg.permissions.allow = allow.iter().map(|s| s.to_string()).collect();
        cfg.permissions.deny = deny.iter().map(|s| s.to_string()).collect();
        cfg.permissions.ask = ask.iter().map(|s| s.to_string()).collect();
        PermissionGate::from_config(&cfg, Path::new("/work"), true).unwrap()
    }

    fn bash(cmd: &str) -> serde_json::Value {
        serde_json::json!({ "command": cmd })
    }

    fn gate_in(cwd: &Path) -> PermissionGate {
        PermissionGate::from_config(&crate::config::Config::default(), cwd, true).unwrap()
    }

    #[test]
    fn p_saves_a_project_rule_and_stops_asking_for_the_rest_of_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let gate = gate_in(dir.path());
        let args = bash("cargo test --lib");
        let mut out = Vec::new();
        assert!(gate.prompt_with("Bash", &args, &mut "p\n".as_bytes(), &mut out, true));
        let shown = String::from_utf8(out).unwrap();
        assert!(
            shown.contains("(p) always allow `Bash(cargo test --lib)` in this project"),
            "{shown}"
        );

        let saved = std::fs::read_to_string(dir.path().join(".lodan/config.local.toml")).unwrap();
        let cfg: crate::config::Config = toml::from_str(&saved).unwrap();
        assert_eq!(cfg.permissions.allow, ["Bash(cargo test --lib)"]);
        // このセッションではもう尋ねない。
        assert_eq!(
            gate.decide_quietly("Bash", &args, true),
            Some(Decision::Allow)
        );
        // 次のセッション (設定から組み直したゲート) でも尋ねない。
        let next = PermissionGate::from_config(&cfg, dir.path(), true).unwrap();
        assert_eq!(
            next.decide_quietly("Bash", &args, true),
            Some(Decision::Allow)
        );
        assert_eq!(
            next.decide_quietly("Bash", &bash("cargo publish"), true),
            None
        );
    }

    #[test]
    fn p_on_a_file_edit_is_scoped_to_its_directory_now_and_later() {
        let dir = tempfile::tempdir().unwrap();
        let gate = gate_in(dir.path());
        let edit = |p: &str| serde_json::json!({ "path": p, "old_string": "a", "new_string": "b" });
        let mut out = Vec::new();
        assert!(gate.prompt_with(
            "Edit",
            &edit("src/agent/loop.rs"),
            &mut "p\n".as_bytes(),
            &mut out,
            true
        ));
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("`Edit(src/agent/**)`")
        );
        // セッション中も、保存したのと同じ広さでしか通らない。
        assert_eq!(
            gate.decide_quietly("Edit", &edit("src/agent/subagent.rs"), true),
            Some(Decision::Allow)
        );
        assert_eq!(gate.decide_quietly("Edit", &edit("Cargo.toml"), true), None);
        assert_eq!(
            gate.decide_quietly("Write", &edit("src/agent/x.rs"), true),
            None
        );
    }

    #[test]
    fn p_is_not_offered_for_a_call_an_ask_rule_will_keep_asking_about() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.permissions.ask = vec!["Bash(cargo *)".into()];
        let gate = PermissionGate::from_config(&cfg, dir.path(), true).unwrap();
        let mut out = Vec::new();
        assert!(gate.prompt_with(
            "Bash",
            &bash("cargo test --lib"),
            &mut "p\ny\n".as_bytes(),
            &mut out,
            true
        ));
        assert!(!String::from_utf8(out).unwrap().contains("(p)"));
        assert!(!dir.path().join(".lodan/config.local.toml").exists());
    }

    #[test]
    fn p_is_not_offered_when_the_saved_rule_would_not_mean_the_same_thing() {
        let dir = tempfile::tempdir().unwrap();
        let gate = gate_in(dir.path());
        let mut out = Vec::new();
        // (p) は無視されて再プロンプト、次の n で拒否。
        let allowed = gate.prompt_with(
            "Bash",
            &bash("ls && rm -rf build"),
            &mut "p\nn\n".as_bytes(),
            &mut out,
            true,
        );
        assert!(!allowed);
        assert!(!String::from_utf8(out).unwrap().contains("(p)"));
        assert!(!dir.path().join(".lodan/config.local.toml").exists());
    }

    #[test]
    fn paths_and_previews_are_escaped_too() {
        let esc = "\x1b[2K\x1b[1G";
        let edit = serde_json::json!({
            "path": format!("src/{esc}harmless.rs"),
            "old_string": format!("a{esc}b"),
            "new_string": "fn x() {\n\tlet y = 1;\n}",
        });
        let write =
            serde_json::json!({ "path": "x", "content": format!("\x1b[2A{esc}looks fine") });
        for shown in [
            summarize("Edit", &edit),
            preview("Edit", &edit).unwrap(),
            preview("Write", &write).unwrap(),
        ] {
            // 着色のための SGR (`\x1b[..m`) 以外のエスケープが端末へ出てはいけない。
            let stripped = shown.replace("\x1b[0m", "");
            let raw_escapes = stripped.matches('\x1b').filter(|_| true).count();
            let sgr = stripped.matches("\x1b[3").count() + stripped.matches("\x1b[2m").count();
            assert_eq!(raw_escapes, sgr, "unescaped control sequence in {shown:?}");
        }
        // タブはコードに普通にあるので、プレビューではそのまま。
        assert!(preview("Edit", &edit).unwrap().contains("\tlet y = 1;"));
        // 不可視文字入りのパスには (p) を出さない。
        assert_eq!(
            crate::permission_rules::persistable_allow_rule("Edit", &edit, Path::new("/work")),
            None
        );
    }

    #[test]
    fn the_prompt_shows_control_characters_instead_of_obeying_them() {
        let sneaky = "rm -rf ~\x1b[2K\x1b[1Gls";
        let shown = summarize("Bash", &bash(sneaky));
        assert!(
            !shown.contains('\x1b'),
            "raw escape reached the terminal: {shown:?}"
        );
        assert!(
            shown.contains("rm -rf ~") && shown.contains("\\u{1b}"),
            "{shown}"
        );
        // そして、そういうコマンドには (p) を出さない。
        assert_eq!(
            crate::permission_rules::persistable_allow_rule("Bash", &bash(sneaky), Path::new("/")),
            None
        );
    }

    #[test]
    fn defaults_let_read_only_through_and_ask_about_the_rest() {
        use crate::config::PermissionMode::Default;
        let gate = gate_with(Default, &[], &[], &[]);
        let read = serde_json::json!({ "path": "src/lib.rs" });
        assert_eq!(
            gate.decide_quietly("Read", &read, false),
            Some(Decision::Allow)
        );
        assert_eq!(
            gate.decide_quietly("Bash", &bash("ls"), true),
            None,
            "would prompt"
        );
    }

    #[test]
    fn a_deny_rule_beats_everything_including_bypass_and_read_only() {
        use crate::config::PermissionMode::Bypass;
        let gate = gate_with(
            Bypass,
            &["Bash(*)"],
            &["Bash(git push *)", "Read(.env)"],
            &[],
        );
        assert_eq!(
            gate.decide_quietly("Bash", &bash("ls"), true),
            Some(Decision::Allow)
        );
        let pushed = gate.decide_quietly("Bash", &bash("git push origin main"), true);
        assert!(
            matches!(&pushed, Some(Decision::Deny(why)) if why.contains("Bash(git push *)")),
            "{pushed:?}"
        );
        let env = gate.decide_quietly("Read", &serde_json::json!({ "path": "config/.env" }), false);
        assert!(
            matches!(env, Some(Decision::Deny(_))),
            "deny applies to read-only tools too"
        );
    }

    #[test]
    fn allow_rules_skip_the_prompt_and_ask_rules_force_it() {
        use crate::config::PermissionMode::Default;
        let gate = gate_with(
            Default,
            &["Bash(cargo *)", "Read"],
            &[],
            &["Bash(cargo publish*)", "Read(secrets/**)"],
        );
        assert_eq!(
            gate.decide_quietly("Bash", &bash("cargo test"), true),
            Some(Decision::Allow)
        );
        assert_eq!(
            gate.decide_quietly("Bash", &bash("cargo publish"), true),
            None
        );
        assert_eq!(
            gate.decide_quietly("Bash", &bash("cargo test && rm -rf target"), true),
            None
        );
        // ask は read-only にも効く。
        let secret = serde_json::json!({ "path": "secrets/key.pem" });
        assert_eq!(gate.decide_quietly("Read", &secret, false), None);
    }

    #[test]
    fn accept_edits_covers_file_edits_but_not_bash() {
        use crate::config::PermissionMode::AcceptEdits;
        let gate = gate_with(AcceptEdits, &[], &["Edit(Cargo.lock)"], &[]);
        let edit = |p: &str| serde_json::json!({ "path": p });
        assert_eq!(
            gate.decide_quietly("Edit", &edit("src/lib.rs"), true),
            Some(Decision::Allow)
        );
        assert_eq!(
            gate.decide_quietly("Write", &edit("notes.md"), true),
            Some(Decision::Allow)
        );
        assert!(matches!(
            gate.decide_quietly("Edit", &edit("Cargo.lock"), true),
            Some(Decision::Deny(_))
        ));
        assert_eq!(gate.decide_quietly("Bash", &bash("ls"), true), None);
    }

    #[test]
    fn dont_ask_denies_what_would_have_prompted_but_honours_allow_rules() {
        use crate::config::PermissionMode::DontAsk;
        let gate = gate_with(DontAsk, &["Bash(git status)"], &[], &[]);
        assert_eq!(
            gate.decide("Bash", &bash("git status"), true),
            Decision::Allow
        );
        let denied = gate.decide("Bash", &bash("rm -rf build"), true);
        assert!(
            matches!(&denied, Decision::Deny(why) if why.contains("non-interactive")),
            "{denied:?}"
        );
    }

    #[test]
    fn an_unparseable_rule_is_a_startup_error_not_a_silent_no_op() {
        let mut cfg = crate::config::Config::default();
        cfg.permissions.deny = vec!["Bash(rm *".into()];
        let err = PermissionGate::from_config(&cfg, Path::new("/work"), true)
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("Bash(rm *"), "{err:#}");
    }

    fn answer(input: &str, enter_means_yes: bool) -> bool {
        let gate = PermissionGate::new(false);
        let args = serde_json::json!({ "command": "rm -rf build" });
        let mut out = Vec::new();
        gate.prompt_with(
            "Bash",
            &args,
            &mut input.as_bytes(),
            &mut out,
            enter_means_yes,
        )
    }

    #[test]
    fn a_prompt_nobody_answers_is_denied() {
        // プロンプトをパイプで渡した実行: 承認の時点で stdin は EOF。
        assert!(!answer("", true));
        assert!(!answer("", false));
    }

    #[test]
    fn enter_is_yes_only_at_a_terminal() {
        assert!(answer("\n", true));
        // パイプの空行は答えではない。読み飛ばして EOF に達したら拒否。
        assert!(!answer("\n\n", false));
        // 空行の後に明示的な答えがあればそれに従う。
        assert!(answer("\ny\n", false));
        assert!(!answer("\nn\n", false));
    }

    #[test]
    fn explicit_answers_work_from_a_pipe() {
        assert!(answer("y\n", false));
        assert!(!answer("n\n", false));
        assert!(
            answer("garbage\ny\n", false),
            "unrecognised input re-prompts"
        );
    }

    #[test]
    fn eof_is_not_an_answer_but_an_empty_line_is() {
        // EOF: read_line は Ok(0)。誰も答えていないので拒否する。
        assert!(nobody_answered(&Ok(0)));
        assert!(nobody_answered(&Err(io::Error::other("closed"))));
        // Enter だけの行は "\n" の 1 バイト。従来どおり yes として扱う。
        assert!(!nobody_answered(&Ok(1)));
    }

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
