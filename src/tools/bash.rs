use async_trait::async_trait;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

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

        // 終了待ち → ステータス更新。child は wait タスクが所有し続けるので
        // drop で殺されない（tokio の既定は kill_on_drop=false）。
        let st = status.clone();
        tokio::spawn(async move {
            let next = match child.wait().await {
                Ok(s) => BgStatus::Exited(s.code().unwrap_or(-1)),
                Err(e) => BgStatus::Failed(e.to_string()),
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
            store.register(command.clone(), output, status)
        };

        Ok(ToolOutput::ok(format!(
            "started background process {id}: {command}\nUse the Monitor tool with id=\"{id}\" to read its output and status."
        )))
    }
}

/// 子プロセスのパイプを EOF まで読み、上限付きで共有バッファへ流す。
async fn pump<R: tokio::io::AsyncRead + Unpin>(mut reader: R, buf: SharedBuf) {
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => append_capped(&buf, &String::from_utf8_lossy(&chunk[..n])),
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
}
