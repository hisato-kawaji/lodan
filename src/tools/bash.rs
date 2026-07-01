use async_trait::async_trait;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use tokio::sync::Notify;

use super::background::{BgStatus, SharedBuf, append_capped};
use super::{Tool, ToolCtx, ToolError, ToolOutput};

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const STDOUT_LIMIT: usize = 30_000;

pub struct Bash;

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &str {
        "Bash"
    }
    fn description(&self) -> &str {
        "Run a shell command non-interactively. Captures stdout/stderr/exit_code with a timeout."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command":      { "type": "string", "description": "Shell command line" },
                "timeout_secs": { "type": "integer", "minimum": 1, "description": "Default 30 (ignored when run_in_background)" },
                "run_in_background": { "type": "boolean", "description": "Spawn detached; returns a process id to poll with the Monitor tool. Default false" }
            },
            "required": ["command"]
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'command'".into()))?
            .to_string();
        let timeout = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        if args
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return self.spawn_background(command, ctx).await;
        }

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&command).current_dir(&ctx.cwd);

        let fut = cmd.output();
        let output = match tokio::time::timeout(Duration::from_secs(timeout), fut).await {
            Ok(r) => r?,
            Err(_) => {
                return Ok(ToolOutput::error(format!(
                    "command timed out after {timeout}s: {command}"
                )));
            }
        };

        let stdout = truncate(String::from_utf8_lossy(&output.stdout).into_owned());
        let stderr = truncate(String::from_utf8_lossy(&output.stderr).into_owned());
        let code = output.status.code().unwrap_or(-1);

        let body = format!(
            "$ {command}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}\n--- exit ---\n{code}\n"
        );
        Ok(if output.status.success() {
            ToolOutput::ok(body)
        } else {
            ToolOutput {
                content: body,
                is_error: true,
            }
        })
    }
}

impl Bash {
    /// 子プロセスを detached で spawn し、stdout/stderr を共有バッファへ流しつつ
    /// ID を即返す。`Monitor` ツールが当該 ID で増分出力と終了状態を読む。
    async fn spawn_background(
        &self,
        command: String,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&command)
            .current_dir(&ctx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutput::error(format!("failed to spawn: {e}"))),
        };

        let output: SharedBuf = Arc::new(Mutex::new(String::new()));
        let status = Arc::new(Mutex::new(BgStatus::Running));

        // stdout / stderr を同一バッファへ（チャンク順にインターリーブ）。
        if let Some(out) = child.stdout.take() {
            let buf = output.clone();
            tokio::spawn(pump(out, buf));
        }
        if let Some(err) = child.stderr.take() {
            let buf = output.clone();
            tokio::spawn(pump(err, buf));
        }

        // KillShell からの kill 合図。wait タスクと共有する。
        let kill = Arc::new(Notify::new());

        // 終了待ち → ステータス更新。child は wait タスクが所有し続けるので
        // drop で殺されない（tokio の既定は kill_on_drop=false）。kill 合図が来たら
        // child を終了させ、ステータスを Killed にする。
        let st = status.clone();
        let kill_rx = kill.clone();
        tokio::spawn(async move {
            let next = tokio::select! {
                res = child.wait() => match res {
                    Ok(s) => BgStatus::Exited(s.code().unwrap_or(-1)),
                    Err(e) => BgStatus::Failed(e.to_string()),
                },
                _ = kill_rx.notified() => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    BgStatus::Killed
                }
            };
            if let Ok(mut g) = st.lock() {
                *g = next;
            }
        });

        let id = {
            let mut store = ctx
                .bg
                .lock()
                .map_err(|_| ToolError::Other("background store poisoned".into()))?;
            store.register(command.clone(), output, status, kill)
        };

        Ok(ToolOutput::ok(format!(
            "started background process {id}: {command}\nUse the Monitor tool with id=\"{id}\" to read its output and status."
        )))
    }
}

/// 子プロセスのパイプを EOF まで読み、上限付きで共有バッファへ流す。
async fn pump<R: tokio::io::AsyncRead + Unpin>(mut reader: R, buf: SharedBuf) {
    let mut chunk = [0u8; 4096];
    // チャンク境界でマルチバイト文字が割れても化けないよう、未完バイトは carry に持ち越す。
    let mut carry: Vec<u8> = Vec::new();
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                carry.extend_from_slice(&chunk[..n]);
                let decoded = decode_utf8_prefix(&mut carry);
                if !decoded.is_empty() {
                    append_capped(&buf, &decoded);
                }
            }
        }
    }
    // EOF: 残った未完バイトは lossy に吐く。
    if !carry.is_empty() {
        append_capped(&buf, &String::from_utf8_lossy(&carry));
    }
}

/// `carry` の先頭から完全な UTF-8 プレフィックスを取り出して返し、残り（未完の
/// マルチバイト等）を `carry` に残す。不正バイト列は U+FFFD に置換して消費する。
fn decode_utf8_prefix(carry: &mut Vec<u8>) -> String {
    match std::str::from_utf8(carry) {
        Ok(s) => {
            let out = s.to_string();
            carry.clear();
            out
        }
        Err(e) => {
            let valid = e.valid_up_to();
            let mut out = String::from_utf8_lossy(&carry[..valid]).into_owned();
            match e.error_len() {
                // error_len() == Some なら境界跨ぎでなく真に不正なバイト列。
                // 置換文字を出して該当バイトを捨て、carry が詰まらないようにする。
                Some(bad) => {
                    out.push('\u{FFFD}');
                    carry.drain(..valid + bad);
                }
                // 未完（more bytes 待ち）: valid 分だけ消費して残りを持ち越す。
                None => {
                    carry.drain(..valid);
                }
            }
            out
        }
    }
}

fn truncate(mut s: String) -> String {
    if s.len() > STDOUT_LIMIT {
        s.truncate(STDOUT_LIMIT);
        s.push_str("\n...[truncated]...");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// マルチバイト文字がチャンク境界で割れても carry 経由で正しく復元される。
    #[test]
    fn decode_utf8_prefix_handles_split_multibyte() {
        // "あ" は E3 81 82。先頭 2 バイトだけ来た状態。
        let mut carry = vec![0xE3, 0x81];
        let out = decode_utf8_prefix(&mut carry);
        assert_eq!(out, ""); // まだ確定文字なし
        assert_eq!(carry, vec![0xE3, 0x81]); // 持ち越し
        // 残り 1 バイト到着 → "あ" が確定。
        carry.push(0x82);
        let out2 = decode_utf8_prefix(&mut carry);
        assert_eq!(out2, "あ");
        assert!(carry.is_empty());
    }

    #[test]
    fn decode_utf8_prefix_emits_replacement_for_invalid() {
        // 0xFF は不正な先頭バイト。U+FFFD を出して消費する（無限ループしない）。
        let mut carry = vec![0xFF, b'a'];
        let out = decode_utf8_prefix(&mut carry);
        assert!(out.starts_with('\u{FFFD}'));
        assert_eq!(carry, vec![b'a']);
    }

    /// バックグラウンド実行 → Monitor で終了まで読み切れることを確認する。
    #[tokio::test]
    async fn background_run_is_monitorable() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let started = Bash
            .execute(
                serde_json::json!({
                    "command": "printf 'hello\\n'; printf 'err\\n' 1>&2",
                    "run_in_background": true
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!started.is_error);
        assert!(started.content.contains("bash_1"), "{}", started.content);

        // 終了するまで Monitor をポーリングし、全出力を蓄積する。
        let monitor = crate::tools::monitor::Monitor;
        let mut collected = String::new();
        let mut exited = false;
        for _ in 0..100 {
            let out = monitor
                .execute(serde_json::json!({ "id": "bash_1" }), &ctx)
                .await
                .unwrap();
            collected.push_str(&out.content);
            if out.content.contains("exited(0)") {
                exited = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(exited, "process did not exit; collected: {collected}");
        assert!(collected.contains("hello"), "missing stdout: {collected}");
        assert!(collected.contains("err"), "missing stderr: {collected}");
    }

    #[tokio::test]
    async fn monitor_unknown_id_is_error() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = crate::tools::monitor::Monitor
            .execute(serde_json::json!({ "id": "bash_404" }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("bash_404"));
    }

    /// 長寿命プロセスを起動 → KillShell → Monitor が killed を報告する。
    #[tokio::test]
    async fn background_run_is_killable() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        Bash.execute(
            serde_json::json!({ "command": "sleep 60", "run_in_background": true }),
            &ctx,
        )
        .await
        .unwrap();

        let killed = crate::tools::kill_shell::KillShell
            .execute(serde_json::json!({ "id": "bash_1" }), &ctx)
            .await
            .unwrap();
        assert!(!killed.is_error, "{}", killed.content);
        assert!(killed.content.contains("kill signal"), "{}", killed.content);

        // wait タスクが Killed を書き込むまでポーリング。
        let monitor = crate::tools::monitor::Monitor;
        let mut killed_seen = false;
        for _ in 0..100 {
            let out = monitor
                .execute(serde_json::json!({ "id": "bash_1" }), &ctx)
                .await
                .unwrap();
            if out.content.contains("killed") {
                killed_seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(killed_seen, "process was not reported killed");
    }

    #[tokio::test]
    async fn kill_unknown_id_is_error() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = crate::tools::kill_shell::KillShell
            .execute(serde_json::json!({ "id": "bash_404" }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("bash_404"));
    }
}
