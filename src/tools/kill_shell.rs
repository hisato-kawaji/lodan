//! KillShell: `Bash` の run_in_background で起動したプロセスを ID 指定で終了する。
//!
//! `Monitor`（読み取り）の対。kill 合図を送るとプロセスは終了し、以降 `Monitor` は
//! `killed` を返す。実際の終了は spawn 側の wait タスクが非同期に行う。
//! 副作用（プロセス終了）を伴うため破壊的ツール扱い（承認ゲートを経る）。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct KillShell;

#[async_trait]
impl Tool for KillShell {
    fn name(&self) -> &str {
        "KillShell"
    }
    fn description(&self) -> &str {
        "Terminate a background process started by Bash with run_in_background. Args: id (e.g. bash_1)."
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
        true
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

        let prior = {
            let store = ctx
                .bg
                .lock()
                .map_err(|_| ToolError::Other("background store poisoned".into()))?;
            store.request_kill(&id)
        };

        match prior {
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
            Some(status) if !status.is_running() => Ok(ToolOutput::ok(format!(
                "background process {id} already finished (status: {}); nothing to kill",
                status.label()
            ))),
            Some(_) => Ok(ToolOutput::ok(format!(
                "sent kill signal to background process {id}. Use the Monitor tool to confirm it reports 'killed'."
            ))),
        }
    }
}
