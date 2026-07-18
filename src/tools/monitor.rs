//! Monitor: `Bash` の run_in_background が起動したプロセスの増分出力と終了状態を読む。
//!
//! 前回 Monitor を呼んだ位置以降の新規出力のみ返す（cursor はストア側が保持）。
//! 読み取り専用なのでパーミッションゲートは経ない。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct Monitor;

#[async_trait]
impl Tool for Monitor {
    fn name(&self) -> &str {
        "Monitor"
    }
    fn description(&self) -> &str {
        "Read newly-produced output and the running/exited status of a background process \
         started by Bash with run_in_background. Returns only output since the last Monitor call."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Background process id, e.g. bash_1" }
            },
            "required": ["id"]
        })
    }

    fn is_destructive(&self) -> bool {
        // read-only: バックグラウンド出力の閲覧のみ
        false
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let id = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'id'".into()))?
            .to_string();

        let read = {
            let mut store = ctx
                .bg
                .lock()
                .map_err(|_| ToolError::Other("background store poisoned".into()))?;
            store.read(&id)
        };

        match read {
            None => {
                let ids = ctx.bg.lock().map(|s| s.ids()).unwrap_or_default();
                let known = if ids.is_empty() {
                    "(none)".to_string()
                } else {
                    ids.join(", ")
                };
                Ok(ToolOutput::error(format!(
                    "no background process with id \"{id}\". known ids: {known}"
                )))
            }
            Some(r) => {
                let new_output = if r.new_output.is_empty() {
                    "(no new output)\n".to_string()
                } else {
                    r.new_output
                };
                Ok(ToolOutput::ok(format!(
                    "[{id}] {} | status: {}\n--- new output ---\n{new_output}",
                    r.command,
                    r.status.label()
                )))
            }
        }
    }
}
