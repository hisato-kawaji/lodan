//! `Requirements`: 要件台帳 (#65 B1) をモデルが更新するツール。done / drop / add。
//! 台帳そのものはループが抽出して `ToolCtx::requirements` に持つ。読み取り専用扱い (ファイルは
//! `.lodan/requirements.json` だが、作業ツリーの成果物ではない)。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct RequirementsTool;

#[async_trait]
impl Tool for RequirementsTool {
    fn name(&self) -> &str {
        "Requirements"
    }

    fn description(&self) -> &str {
        "Update the requirements ledger shown in the system prompt. action \"done\": mark a \
         requirements verified (give ids — one or several — and evidence such as the command you \
         ran; mark several at once when one verification covers them). action \"drop\": \
         mark it out of scope (give id and reason). action \"add\": record a requirement you \
         discovered (give text). The ledger is kept by lodan and survives context compaction."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["done", "drop", "add"] },
                "ids": { "type": "array", "items": { "type": "integer" }, "description": "Requirement ids (for done / drop); `id` with a single integer also works" },
                "id": { "type": "integer", "description": "A single requirement id (for done / drop)" },
                "evidence": { "type": "string", "description": "How it was verified (done) or why it is out of scope (drop)" },
                "text": { "type": "string", "description": "The requirement (for add)" }
            },
            "required": ["action"]
        })
    }

    fn is_destructive(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("Requirements: missing `action`".into()))?;
        // `ids` (複数) か `id` (1 つ)。1 回の検証で複数を done にできるようにして往復を減らす
        // (ラダー実測: 9B は 1 件ずつ呼ぶと 1 件あたり LLM 往復が 1 回増える)。
        let ids = || -> Result<Vec<u32>, ToolError> {
            let mut out: Vec<u32> = args
                .get("ids")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_u64())
                        .map(|v| v as u32)
                        .collect()
                })
                .unwrap_or_default();
            if let Some(one) = args.get("id").and_then(|v| v.as_u64()) {
                out.push(one as u32);
            }
            out.sort_unstable();
            out.dedup();
            if out.is_empty() {
                return Err(ToolError::InvalidArgs(
                    "Requirements: `ids` (array of integers) or `id` required".into(),
                ));
            }
            Ok(out)
        };
        let list = |ids: &[u32]| {
            ids.iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let note = args
            .get("evidence")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let mut ledger = ctx
            .requirements
            .lock()
            .map_err(|_| ToolError::Other("requirements ledger poisoned".into()))?;
        let result = match action {
            "done" => {
                let ids = ids()?;
                if note.trim().is_empty() {
                    return Ok(ToolOutput::error(
                        "Requirements: `evidence` is required for done — say how you verified it",
                    ));
                }
                ids.iter()
                    .try_for_each(|id| ledger.mark_done(*id, &note))
                    .map(|_| format!("requirement(s) {} marked done", list(&ids)))
            }
            "drop" => {
                let ids = ids()?;
                if note.trim().is_empty() {
                    return Ok(ToolOutput::error(
                        "Requirements: `evidence` (the reason) is required for drop",
                    ));
                }
                ids.iter()
                    .try_for_each(|id| ledger.drop_item(*id, &note))
                    .map(|_| format!("requirement(s) {} dropped", list(&ids)))
            }
            "add" => {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| {
                        ToolError::InvalidArgs("Requirements: `text` required for add".into())
                    })?;
                let ids = ledger.add([text.to_string()]);
                Ok(format!(
                    "requirement {} added",
                    ids.first().copied().unwrap_or(0)
                ))
            }
            other => {
                return Err(ToolError::InvalidArgs(format!(
                    "Requirements: unknown action `{}`",
                    crate::term::sanitize(other)
                )));
            }
        };
        match result {
            Ok(msg) => {
                let open = ledger.unmet().len();
                Ok(ToolOutput::ok(format!("{msg}; {open} still open")))
            }
            Err(e) => Ok(ToolOutput::error(format!("Requirements: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn done_drop_and_add_update_the_shared_ledger() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        ctx.requirements
            .lock()
            .unwrap()
            .add(["a".to_string(), "b".to_string()]);
        let run = |args: serde_json::Value| RequirementsTool.execute(args, &ctx);
        let out = run(serde_json::json!({ "action": "done", "id": 1 }))
            .await
            .unwrap();
        assert!(
            out.is_error && out.content.contains("evidence"),
            "{}",
            out.content
        );
        let out = run(serde_json::json!({ "action": "done", "id": 1, "evidence": "ran it" }))
            .await
            .unwrap();
        assert_eq!(out.content, "requirement(s) 1 marked done; 1 still open");
        let out = run(serde_json::json!({ "action": "drop", "id": 2, "evidence": "not needed" }))
            .await
            .unwrap();
        assert_eq!(out.content, "requirement(s) 2 dropped; 0 still open");
        // 複数を 1 回で。
        ctx.requirements
            .lock()
            .unwrap()
            .add(["d".to_string(), "e".to_string()]);
        let out = run(serde_json::json!({ "action": "done", "ids": [3, 4], "evidence": "pytest" }))
            .await
            .unwrap();
        assert_eq!(out.content, "requirement(s) 3, 4 marked done; 0 still open");
        assert!(
            run(serde_json::json!({ "action": "done", "evidence": "x" }))
                .await
                .is_err()
        );
        let out = run(serde_json::json!({ "action": "add", "text": "c" }))
            .await
            .unwrap();
        assert_eq!(out.content, "requirement 5 added; 1 still open");
        let out = run(serde_json::json!({ "action": "done", "id": 9, "evidence": "x" }))
            .await
            .unwrap();
        assert!(out.is_error && out.content.contains("no requirement with id 9"));
        assert!(run(serde_json::json!({ "action": "nope" })).await.is_err());
    }
}
