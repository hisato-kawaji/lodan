// Hook ディスパッチ: マッチした外部コマンドを順に発火し、exit code で制御する。

use super::{HookConfig, HookOutcome, Lifecycle};
use anyhow::Result;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// hook サブプロセスのタイムアウト (MCP クライアントと揃える)。
const HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// `lc` に一致する hook を順に実行する。
///
/// - `tool_name`: PreToolUse / PostToolUse のとき対象ツール名。matcher 照合に使う。
///   それ以外のイベントでは `None`。
/// - `payload`: hook の stdin に渡す JSON。
///
/// いずれかの hook が非 0 で終了したら、その時点で `Block` を返して残りはスキップする
/// (Claude Code の deny セマンティクスに合わせる)。全て成功なら `Continue`。
pub async fn dispatch(
    lc: Lifecycle,
    tool_name: Option<&str>,
    payload: &serde_json::Value,
    hooks: &[HookConfig],
) -> Result<HookOutcome> {
    for hook in hooks
        .iter()
        .filter(|h| h.event == lc && h.matches(tool_name))
    {
        match run_one(hook, payload).await {
            Ok(HookOutcome::Continue) => {}
            Ok(block @ HookOutcome::Block(_)) => return Ok(block),
            // hook 自体の起動失敗はループを止めず警告のみ (fail-open)。
            Err(e) => {
                eprintln!("hook[{}]: {e}", hook.command);
            }
        }
    }
    Ok(HookOutcome::Continue)
}

/// 単一 hook を `sh -c` で起動し、payload を stdin に流して終了コードを判定する。
async fn run_one(hook: &HookConfig, payload: &serde_json::Value) -> Result<HookOutcome> {
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

    let output = match tokio::time::timeout(HOOK_TIMEOUT, child.wait_with_output()).await {
        Ok(res) => res?,
        Err(_) => {
            return Ok(HookOutcome::Block(format!(
                "hook timed out after {}s: {}",
                HOOK_TIMEOUT.as_secs(),
                hook.command
            )));
        }
    };

    if output.status.success() {
        Ok(HookOutcome::Continue)
    } else {
        // ブロック理由は stderr 優先、無ければ stdout。
        let reason = pick_reason(&output.stderr, &output.stdout);
        Ok(HookOutcome::Block(reason))
    }
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
        }
    }

    #[tokio::test]
    async fn no_hooks_continues() {
        let out = dispatch(Lifecycle::PreToolUse, Some("Bash"), &json!({}), &[])
            .await
            .unwrap();
        assert_eq!(out, HookOutcome::Continue);
    }

    #[tokio::test]
    async fn exit_zero_continues() {
        let hooks = vec![hook(Lifecycle::PreToolUse, "Bash", "exit 0")];
        let out = dispatch(Lifecycle::PreToolUse, Some("Bash"), &json!({}), &hooks)
            .await
            .unwrap();
        assert_eq!(out, HookOutcome::Continue);
    }

    #[tokio::test]
    async fn nonzero_blocks_with_stderr() {
        let hooks = vec![hook(
            Lifecycle::PreToolUse,
            "Bash",
            "echo nope 1>&2; exit 1",
        )];
        let out = dispatch(Lifecycle::PreToolUse, Some("Bash"), &json!({}), &hooks)
            .await
            .unwrap();
        assert_eq!(out, HookOutcome::Block("nope".to_string()));
    }

    #[tokio::test]
    async fn matcher_filters_by_tool_name() {
        // matcher = "Write" なので Bash 呼び出しには発火しない。
        let hooks = vec![hook(Lifecycle::PreToolUse, "Write", "exit 1")];
        let out = dispatch(Lifecycle::PreToolUse, Some("Bash"), &json!({}), &hooks)
            .await
            .unwrap();
        assert_eq!(out, HookOutcome::Continue);
    }

    #[tokio::test]
    async fn wildcard_matcher_runs_for_any_tool() {
        let hooks = vec![hook(Lifecycle::PreToolUse, "*", "exit 1")];
        let out = dispatch(Lifecycle::PreToolUse, Some("AnyTool"), &json!({}), &hooks)
            .await
            .unwrap();
        assert!(matches!(out, HookOutcome::Block(_)));
    }

    #[tokio::test]
    async fn event_filter_skips_other_lifecycles() {
        let hooks = vec![hook(Lifecycle::PostToolUse, "", "exit 1")];
        // PreToolUse をディスパッチしても PostToolUse hook は無視される。
        let out = dispatch(Lifecycle::PreToolUse, Some("Bash"), &json!({}), &hooks)
            .await
            .unwrap();
        assert_eq!(out, HookOutcome::Continue);
    }

    #[tokio::test]
    async fn payload_reaches_hook_stdin() {
        // stdin の JSON に "Bash" が含まれれば exit 1 でブロック、含まなければ exit 0。
        let hooks = vec![hook(
            Lifecycle::PreToolUse,
            "Bash",
            "grep -q Bash && exit 3 || exit 0",
        )];
        let payload = json!({ "tool_name": "Bash" });
        let out = dispatch(Lifecycle::PreToolUse, Some("Bash"), &payload, &hooks)
            .await
            .unwrap();
        assert!(matches!(out, HookOutcome::Block(_)));
    }
}
