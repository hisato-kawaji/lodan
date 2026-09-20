// Hook ディスパッチ: マッチした外部コマンドを順に発火し、終了コードと stdout の JSON で制御する。

use super::{HookConfig, HookOutcome, HooksCompat, Lifecycle, PermissionHint};
use anyhow::Result;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// hook サブプロセスの既定タイムアウト (MCP クライアントと揃える)。`timeout_secs` で hook ごとに変えられる。
const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(30);
/// Claude Code 互換でブロックを意味する終了コード。
const EXIT_BLOCK: i32 = 2;

/// `lc` に一致する hook を順に実行し、結果を 1 つに畳む。
///
/// - `subject`: matcher と照合する値。PreToolUse / PostToolUse なら対象ツール名、matcher を
///   持たないイベントでは `None`。
/// - `payload`: hook の stdin に渡す JSON。
///
/// いずれかの hook が止めたら、その時点で残りはスキップする。hook 自体の起動失敗と、ブロックでない
/// 失敗 (v2 の 2 以外の非 0、読めない JSON) は警告だけ出して続行する (fail-open)。
pub async fn dispatch(
    lc: Lifecycle,
    subject: Option<&str>,
    payload: &serde_json::Value,
    hooks: &[HookConfig],
    compat: HooksCompat,
) -> Result<HookOutcome> {
    let mut total = HookOutcome::default();
    for hook in hooks.iter().filter(|h| h.event == lc && h.matches(subject)) {
        let one = match run_one(hook, lc, payload, compat).await {
            Ok(one) => one,
            Err(e) => {
                warn(hook, &e.to_string());
                continue;
            }
        };
        if one.block.is_some() {
            total.block = one.block;
            return Ok(total);
        }
        total.permission = match (total.permission, one.permission) {
            (Some(PermissionHint::Ask), _) | (_, Some(PermissionHint::Ask)) => {
                Some(PermissionHint::Ask)
            }
            (a, b) => b.or(a),
        };
        if one.updated_input.is_some() {
            total.updated_input = one.updated_input;
        }
        total.context.extend(one.context);
    }
    Ok(total)
}

/// hook の出力は外部コマンドが決める文字列なので、端末へは無害化して出す。
fn warn(hook: &HookConfig, message: &str) {
    eprintln!(
        "hook[{}]: {}",
        crate::term::sanitize(&hook.command),
        crate::term::sanitize(message)
    );
}

/// 単一 hook を `sh -c` で起動し、payload を stdin に流して結果を読む。
async fn run_one(
    hook: &HookConfig,
    lc: Lifecycle,
    payload: &serde_json::Value,
    compat: HooksCompat,
) -> Result<HookOutcome> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&hook.command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        let bytes = serde_json::to_vec(payload)?;
        // stdin への書き込み失敗 (hook が読まずに即終了等) は致命ではない。
        let _ = stdin.write_all(&bytes).await;
        drop(stdin);
    }

    let timeout = hook
        .timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_HOOK_TIMEOUT);
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(res) => res?,
        // 時間切れは止める側に倒す。guard のつもりで置いた hook が固まったときに素通しにしない
        // (Claude Code は続行するが、ここは安全側を選んでいる)。
        Err(_) => {
            return Ok(HookOutcome::blocked(format!(
                "hook timed out after {}s: {}",
                timeout.as_secs(),
                hook.command
            )));
        }
    };

    if compat == HooksCompat::V1 {
        return Ok(if output.status.success() {
            HookOutcome::default()
        } else {
            HookOutcome::blocked(pick_reason(&output.stderr, &output.stdout))
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // PowerShell や Python の `utf-8-sig` は先頭に BOM を付ける。`trim` は BOM を落とさない。
    let stdout = stdout.trim().trim_start_matches('\u{feff}').trim_start();
    match output.status.code() {
        Some(0) => Ok(read_stdout(hook, lc, stdout)),
        // 終了コード 2 は JSON の中身に関わらず止める (JSON の `allow` でも覆せない)。理由は JSON が
        // ブロックを述べていればそれ、無ければ stderr。
        Some(EXIT_BLOCK) => {
            let from_json = parse_object(stdout)
                .and_then(|v| interpret(lc, &v).ok())
                .and_then(|o| o.block)
                .filter(|r| !r.is_empty());
            Ok(HookOutcome::blocked(
                from_json.unwrap_or_else(|| pick_reason(&output.stderr, &[])),
            ))
        }
        code => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let first = stderr.lines().next().unwrap_or("").trim();
            let code = code.map_or("a signal".to_string(), |c| format!("status {c}"));
            warn(
                hook,
                &format!("failed with non-blocking {code} (only exit 2 blocks): {first}"),
            );
            Ok(HookOutcome::default())
        }
    }
}

/// `{` で始まり `}` で終わる stdout だけを JSON として読む。
fn parse_object(stdout: &str) -> Option<serde_json::Value> {
    if !(stdout.starts_with('{') && stdout.ends_with('}')) {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(stdout)
        .ok()
        .filter(|v| v.is_object())
}

/// 判断を伝えようとした形跡のある stdout か (JSON の書き出しで始まる)。
fn looks_like_json(stdout: &str) -> bool {
    stdout.starts_with('{') || stdout.starts_with('[')
}

/// exit 0 の stdout。JSON なら判断として、UserPromptSubmit / SessionStart の素のテキストなら
/// モデルへの追加文脈として読む。それ以外のイベントの素のテキストは捨てる。
///
/// JSON らしいのに読み取れない出力を**黙って捨てない**。deny を言おうとして形を間違えた guard が
/// 素通しになるのが一番まずいので、PreToolUse ではブロックに倒し、他のイベントでは警告する。
fn read_stdout(hook: &HookConfig, lc: Lifecycle, stdout: &str) -> HookOutcome {
    if stdout.is_empty() {
        return HookOutcome::default();
    }
    let problem = match parse_object(stdout) {
        Some(value) => {
            if let Some(msg) = value.get("systemMessage").and_then(|v| v.as_str()) {
                warn(hook, msg);
            }
            match interpret(lc, &value) {
                Ok(outcome) => return outcome,
                Err(problem) => problem,
            }
        }
        None if looks_like_json(stdout) => {
            "stdout looks like JSON but is not a JSON object lodan can read".to_string()
        }
        None => {
            if matches!(lc, Lifecycle::UserPromptSubmit | Lifecycle::SessionStart) {
                return HookOutcome {
                    context: vec![stdout.to_string()],
                    ..HookOutcome::default()
                };
            }
            return HookOutcome::default();
        }
    };
    if lc == Lifecycle::PreToolUse {
        return HookOutcome::blocked(format!(
            "hook `{}` printed a decision lodan could not read ({problem}); blocking to be safe",
            hook.command
        ));
    }
    warn(hook, &format!("{problem}; ignoring it"));
    HookOutcome::default()
}

/// hook の JSON 出力 (Claude Code の形式) を読む。形は合っているのに値が読めないとき
/// (`permissionDecision` が文字列でない、知らない値) は `Err`。
fn interpret(lc: Lifecycle, value: &serde_json::Value) -> Result<HookOutcome, String> {
    let text = |v: &serde_json::Value, key: &str| {
        v.get(key)
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    };
    let mut out = HookOutcome::default();
    let specific = value.get("hookSpecificOutput");
    // 文脈は、この hook が止めるかどうかに関わらず拾う (使うかどうかは呼び出し側が決める)。
    if let Some(context) = specific.and_then(|s| text(s, "additionalContext")) {
        out.context.push(context);
    }

    // `continue: false` は「先へ進めるな」。Stop では「止まってよい」の意味になるので、ブロック
    // (= 会話を続けさせる) には読み替えない。
    if value.get("continue").and_then(|v| v.as_bool()) == Some(false) && lc != Lifecycle::Stop {
        out.block =
            Some(text(value, "stopReason").unwrap_or_else(|| "hook requested a stop".to_string()));
        return Ok(out);
    }
    if value.get("decision").and_then(|v| v.as_str()) == Some("block") {
        out.block =
            Some(text(value, "reason").unwrap_or_else(|| "hook denied (no message)".to_string()));
        return Ok(out);
    }

    let Some(specific) = specific.filter(|_| lc == Lifecycle::PreToolUse) else {
        return Ok(out);
    };
    match specific.get("permissionDecision") {
        None => {}
        Some(decision) => match decision.as_str() {
            Some("deny") => {
                out.block = Some(
                    text(specific, "permissionDecisionReason")
                        .unwrap_or_else(|| "hook denied (no message)".to_string()),
                );
                return Ok(out);
            }
            Some("allow") => out.permission = Some(PermissionHint::Allow),
            Some("ask") => out.permission = Some(PermissionHint::Ask),
            _ => return Err(format!("unknown permissionDecision {decision}")),
        },
    }
    // ツール入力は JSON オブジェクト。それ以外の形に書き換えられたものは採らない。
    match specific.get("updatedInput") {
        None => {}
        Some(input) if input.is_object() => out.updated_input = Some(input.clone()),
        Some(_) => return Err("updatedInput is not a JSON object".to_string()),
    }
    Ok(out)
}

fn pick_reason(stderr: &[u8], stdout: &[u8]) -> String {
    let err = String::from_utf8_lossy(stderr);
    let trimmed = err.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    let out = String::from_utf8_lossy(stdout);
    let trimmed = out.trim();
    if !trimmed.is_empty() {
        trimmed.to_string()
    } else {
        "hook denied (no message)".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hook(event: Lifecycle, matcher: &str, command: &str) -> HookConfig {
        HookConfig {
            event,
            matcher: matcher.to_string(),
            command: command.to_string(),
            timeout_secs: None,
        }
    }

    async fn pre_tool(tool: &str, hooks: &[HookConfig], compat: HooksCompat) -> HookOutcome {
        dispatch(
            Lifecycle::PreToolUse,
            Some(tool),
            &json!({ "tool_name": tool }),
            hooks,
            compat,
        )
        .await
        .unwrap()
    }

    /// stdout に JSON を出して exit 0 する hook。
    fn says(json: &str) -> String {
        format!("cat > /dev/null; printf '%s' '{json}'")
    }

    #[tokio::test]
    async fn no_hooks_and_exit_zero_continue() {
        assert_eq!(
            pre_tool("Bash", &[], HooksCompat::V2).await,
            HookOutcome::default()
        );
        let hooks = vec![hook(Lifecycle::PreToolUse, "Bash", "exit 0")];
        assert_eq!(
            pre_tool("Bash", &hooks, HooksCompat::V2).await,
            HookOutcome::default()
        );
    }

    #[tokio::test]
    async fn exit_two_blocks_with_stderr_and_other_codes_only_warn() {
        let block = vec![hook(
            Lifecycle::PreToolUse,
            "Bash",
            "echo nope 1>&2; exit 2",
        )];
        assert_eq!(
            pre_tool("Bash", &block, HooksCompat::V2).await.block,
            Some("nope".to_string())
        );
        // Claude Code 用に書かれた hook では、1 は「hook 自身の失敗」であってブロックではない。
        let broken = vec![hook(
            Lifecycle::PreToolUse,
            "Bash",
            "echo oops 1>&2; exit 1",
        )];
        assert_eq!(
            pre_tool("Bash", &broken, HooksCompat::V2).await,
            HookOutcome::default()
        );
    }

    #[tokio::test]
    async fn v1_compat_blocks_on_any_nonzero_and_ignores_json() {
        let broken = vec![hook(
            Lifecycle::PreToolUse,
            "Bash",
            "echo oops 1>&2; exit 1",
        )];
        assert_eq!(
            pre_tool("Bash", &broken, HooksCompat::V1).await.block,
            Some("oops".to_string())
        );
        let deny = vec![hook(
            Lifecycle::PreToolUse,
            "",
            &says(r#"{"hookSpecificOutput":{"permissionDecision":"deny"}}"#),
        )];
        assert_eq!(
            pre_tool("Bash", &deny, HooksCompat::V1).await,
            HookOutcome::default(),
            "v1 never read stdout"
        );
    }

    #[tokio::test]
    async fn exit_two_wins_over_a_json_allow() {
        let hooks = vec![hook(
            Lifecycle::PreToolUse,
            "",
            r#"printf '%s' '{"hookSpecificOutput":{"permissionDecision":"allow"}}'; echo stop 1>&2; exit 2"#,
        )];
        let out = pre_tool("Bash", &hooks, HooksCompat::V2).await;
        assert_eq!(out.block, Some("stop".to_string()));
        assert_eq!(out.permission, None);
    }

    #[tokio::test]
    async fn json_permission_decisions() {
        let deny = vec![hook(
            Lifecycle::PreToolUse,
            "",
            &says(
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"no rm"}}"#,
            ),
        )];
        assert_eq!(
            pre_tool("Bash", &deny, HooksCompat::V2).await.block,
            Some("no rm".to_string())
        );

        let allow = hook(
            Lifecycle::PreToolUse,
            "",
            &says(r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#),
        );
        let ask = hook(
            Lifecycle::PreToolUse,
            "",
            &says(r#"{"hookSpecificOutput":{"permissionDecision":"ask"}}"#),
        );
        assert_eq!(
            pre_tool("Bash", std::slice::from_ref(&allow), HooksCompat::V2)
                .await
                .permission,
            Some(PermissionHint::Allow)
        );
        // 順序に関わらず、慎重な方が勝つ。
        for hooks in [vec![allow.clone(), ask.clone()], vec![ask, allow]] {
            assert_eq!(
                pre_tool("Bash", &hooks, HooksCompat::V2).await.permission,
                Some(PermissionHint::Ask)
            );
        }
    }

    #[tokio::test]
    async fn json_updated_input_and_context() {
        let hooks = vec![hook(
            Lifecycle::PreToolUse,
            "",
            &says(
                r#"{"hookSpecificOutput":{"updatedInput":{"command":"ls -la"},"additionalContext":"be careful"}}"#,
            ),
        )];
        let out = pre_tool("Bash", &hooks, HooksCompat::V2).await;
        assert_eq!(out.updated_input, Some(json!({ "command": "ls -la" })));
        assert_eq!(out.context, vec!["be careful".to_string()]);

        // 形の違う書き換えは採らない。何をしたかったのか分からないので、実行もしない。
        let not_an_object = vec![hook(
            Lifecycle::PreToolUse,
            "",
            &says(r#"{"hookSpecificOutput":{"updatedInput":"rm -rf /"}}"#),
        )];
        let out = pre_tool("Bash", &not_an_object, HooksCompat::V2).await;
        assert_eq!(out.updated_input, None);
        assert!(out.block.unwrap().contains("updatedInput"));
    }

    #[tokio::test]
    async fn permission_fields_are_read_only_for_pre_tool_use() {
        let hooks = vec![hook(
            Lifecycle::PostToolUse,
            "",
            &says(
                r#"{"hookSpecificOutput":{"permissionDecision":"allow","updatedInput":{"a":1},"additionalContext":"lint failed"}}"#,
            ),
        )];
        let out = dispatch(
            Lifecycle::PostToolUse,
            Some("Edit"),
            &json!({}),
            &hooks,
            HooksCompat::V2,
        )
        .await
        .unwrap();
        assert_eq!(out.permission, None);
        assert_eq!(out.updated_input, None);
        assert_eq!(out.context, vec!["lint failed".to_string()]);
    }

    #[tokio::test]
    async fn decision_block_and_continue_false() {
        let block = vec![hook(
            Lifecycle::PostToolUse,
            "",
            &says(r#"{"decision":"block","reason":"tests fail"}"#),
        )];
        let out = dispatch(
            Lifecycle::PostToolUse,
            Some("Edit"),
            &json!({}),
            &block,
            HooksCompat::V2,
        )
        .await
        .unwrap();
        assert_eq!(out.block, Some("tests fail".to_string()));

        let halt = says(r#"{"continue":false,"stopReason":"budget"}"#);
        let on_prompt = vec![hook(Lifecycle::UserPromptSubmit, "", &halt)];
        let out = dispatch(
            Lifecycle::UserPromptSubmit,
            None,
            &json!({}),
            &on_prompt,
            HooksCompat::V2,
        )
        .await
        .unwrap();
        assert_eq!(out.block, Some("budget".to_string()));
        // Stop でのブロックは「会話を続けさせる」。`continue: false` をそう読んではいけない。
        let on_stop = vec![hook(Lifecycle::Stop, "", &halt)];
        let out = dispatch(Lifecycle::Stop, None, &json!({}), &on_stop, HooksCompat::V2)
            .await
            .unwrap();
        assert_eq!(out.block, None);
    }

    #[tokio::test]
    async fn plain_stdout_is_context_only_where_the_model_should_see_it() {
        let echo = "cat > /dev/null; echo 'today is friday'";
        let on_prompt = vec![hook(Lifecycle::UserPromptSubmit, "", echo)];
        let out = dispatch(
            Lifecycle::UserPromptSubmit,
            None,
            &json!({}),
            &on_prompt,
            HooksCompat::V2,
        )
        .await
        .unwrap();
        assert_eq!(out.context, vec!["today is friday".to_string()]);

        let on_tool = vec![hook(Lifecycle::PreToolUse, "", echo)];
        assert_eq!(
            pre_tool("Bash", &on_tool, HooksCompat::V2).await,
            HookOutcome::default()
        );
    }

    /// deny を言おうとして形を間違えた guard を、黙って素通しにしない (レビューで実際に通った)。
    #[tokio::test]
    async fn a_pre_tool_decision_that_cannot_be_read_blocks_instead_of_passing_silently() {
        let deny = r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"bom deny"}}"#;
        // BOM つき (PowerShell / Python の utf-8-sig) は読める。
        let bom = vec![hook(
            Lifecycle::PreToolUse,
            "",
            &format!("cat > /dev/null; printf '\\357\\273\\277%s' '{deny}'"),
        )];
        assert_eq!(
            pre_tool("Bash", &bom, HooksCompat::V2).await.block,
            Some("bom deny".to_string())
        );
        for unreadable in [
            format!("[{deny}]"),
            "{not json}".to_string(),
            r#"{"hookSpecificOutput":{"permissionDecision":{"value":"deny"}}}"#.to_string(),
            r#"{"hookSpecificOutput":{"permissionDecision":"dontAsk"}}"#.to_string(),
        ] {
            let hooks = vec![hook(Lifecycle::PreToolUse, "", &says(&unreadable))];
            let out = pre_tool("Bash", &hooks, HooksCompat::V2).await;
            assert!(
                out.block
                    .as_deref()
                    .is_some_and(|r| r.contains("could not read")),
                "{unreadable} -> {out:?}"
            );
        }
        // PreToolUse 以外では止めるものが無いので、警告して続行。
        let on_stop = vec![hook(Lifecycle::Stop, "", &says("{not json}"))];
        let out = dispatch(Lifecycle::Stop, None, &json!({}), &on_stop, HooksCompat::V2)
            .await
            .unwrap();
        assert_eq!(out, HookOutcome::default());
    }

    #[tokio::test]
    async fn context_is_kept_even_when_the_same_output_blocks_or_lets_a_stop_through() {
        let stop = vec![hook(
            Lifecycle::Stop,
            "",
            &says(
                r#"{"continue":false,"hookSpecificOutput":{"additionalContext":"remember to push"}}"#,
            ),
        )];
        let out = dispatch(Lifecycle::Stop, None, &json!({}), &stop, HooksCompat::V2)
            .await
            .unwrap();
        assert_eq!(out.block, None);
        assert_eq!(out.context, vec!["remember to push".to_string()]);
    }

    #[tokio::test]
    async fn a_block_stops_later_hooks_from_running() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let hooks = vec![
            hook(Lifecycle::PreToolUse, "", "exit 2"),
            hook(
                Lifecycle::PreToolUse,
                "",
                &format!("touch '{}'", marker.display()),
            ),
        ];
        assert!(
            pre_tool("Bash", &hooks, HooksCompat::V2)
                .await
                .block
                .is_some()
        );
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn a_hung_hook_blocks_after_its_own_timeout() {
        let mut slow = hook(Lifecycle::PreToolUse, "", "sleep 30");
        slow.timeout_secs = Some(1);
        let out = pre_tool("Bash", &[slow], HooksCompat::V2).await;
        assert!(out.block.unwrap().contains("timed out after 1s"));
    }

    #[test]
    fn matcher_exact_list_and_regex() {
        let m = |matcher: &str, tool: &str| {
            hook(Lifecycle::PreToolUse, matcher, "true").matches(Some(tool))
        };
        assert!(m("", "Bash") && m("*", "Bash") && m("Bash", "Bash"));
        assert!(!m("Write", "Bash"));
        // 英数と `|` `,` だけなら完全一致の並び。部分一致はしない。
        assert!(m("Edit|Write", "Write") && m("Edit, Write", "Edit"));
        assert!(!m("Edit|Write", "NotebookEdit"));
        assert!(!m("mcp__memory", "mcp__memory__create"));
        // それ以外は正規表現で、部分一致。
        assert!(m("Edit.*", "NotebookEdit"));
        assert!(!m("^Edit$", "NotebookEdit") && m("^Edit$", "Edit"));
        assert!(m("mcp__memory__.*", "mcp__memory__create"));
        // matcher を持たないイベントでは無視される。
        assert!(hook(Lifecycle::Stop, "^never$", "true").matches(None));
    }

    #[test]
    fn a_broken_regex_is_a_startup_error_not_a_silent_no_op() {
        let bad = hook(Lifecycle::PreToolUse, "Bash(", "guard.sh");
        assert!(!bad.matches(Some("Bash")));
        let err = bad.validate().unwrap_err();
        assert!(err.contains("Bash(") && err.contains("guard.sh"), "{err}");
        assert!(
            hook(Lifecycle::PreToolUse, "Edit|Write", "x")
                .validate()
                .is_ok()
        );
        assert!(
            hook(Lifecycle::PreToolUse, "^mcp__.*", "x")
                .validate()
                .is_ok()
        );
    }

    #[tokio::test]
    async fn payload_reaches_hook_stdin() {
        let hooks = vec![hook(
            Lifecycle::PreToolUse,
            "Bash",
            "grep -q '\"tool_name\":\"Bash\"' && exit 2 || exit 0",
        )];
        assert!(
            pre_tool("Bash", &hooks, HooksCompat::V2)
                .await
                .block
                .is_some()
        );
    }
}
