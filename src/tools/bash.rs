use async_trait::async_trait;
use std::time::Duration;
use tokio::process::Command;

use super::{Tool, ToolCtx, ToolError, ToolOutput};

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const STDOUT_LIMIT: usize = 30_000;

pub struct Bash;

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &'static str {
        "Bash"
    }
    fn description(&self) -> &'static str {
        "Run a shell command non-interactively. Captures stdout/stderr/exit_code with a timeout."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command":      { "type": "string", "description": "Shell command line" },
                "timeout_secs": { "type": "integer", "minimum": 1, "description": "Default 30" }
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

fn truncate(mut s: String) -> String {
    if s.len() > STDOUT_LIMIT {
        s.truncate(STDOUT_LIMIT);
        s.push_str("\n...[truncated]...");
    }
    s
}
