// 単一ファイルへ複数の文字列置換を順次・原子的に適用する。
// 各 edit は Edit と同じ規約 (old は空不可・old≠new・replace_all でなければ一意一致)。
// どれか 1 つでも失敗したらファイルは書き換えず中断する
// (in-memory で全 edit を適用してから 1 度だけ write)。

use async_trait::async_trait;
use serde::Deserialize;
use std::path::PathBuf;

use super::{Tool, ToolCtx, ToolError, ToolOutput};

#[derive(Debug, Deserialize)]
struct MultiEditArgs {
    path: String,
    edits: Vec<EditOp>,
}

#[derive(Debug, Deserialize)]
struct EditOp {
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

pub struct MultiEdit;

#[async_trait]
impl Tool for MultiEdit {
    fn name(&self) -> &str {
        "MultiEdit"
    }

    fn description(&self) -> &str {
        "Apply multiple exact-string edits to a single file, in order and atomically. \
         Each edit matches like Edit (old_string must be unique unless replace_all). \
         If any edit fails, the file is left unchanged."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string":  { "type": "string" },
                            "new_string":  { "type": "string" },
                            "replace_all": { "type": "boolean", "default": false }
                        },
                        "required": ["old_string", "new_string"]
                    }
                }
            },
            "required": ["path", "edits"]
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
        let args: MultiEditArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs(format!("MultiEdit: {e}")))?;

        if args.edits.is_empty() {
            return Err(ToolError::InvalidArgs("edits must not be empty".into()));
        }

        let abs = {
            let p = PathBuf::from(&args.path);
            if p.is_absolute() { p } else { ctx.cwd.join(p) }
        };

        if !ctx.was_read(&abs) {
            return Err(ToolError::NotReadYet(abs.display().to_string()));
        }

        let original = tokio::fs::read_to_string(&abs).await?;

        // すべての edit を in-memory で順次適用。失敗したら write せず error を返す。
        let mut working = original;
        let mut total = 0usize;
        for (i, op) in args.edits.iter().enumerate() {
            if op.old_string.is_empty() {
                return Ok(ToolOutput::error(format!(
                    "edit[{i}]: old_string must not be empty"
                )));
            }
            if op.old_string == op.new_string {
                return Ok(ToolOutput::error(format!(
                    "edit[{i}]: old_string and new_string are identical"
                )));
            }
            let count = working.matches(&op.old_string).count();
            if count == 0 {
                return Ok(ToolOutput::error(format!(
                    "edit[{i}]: old_string not found (after applying earlier edits)"
                )));
            }
            if count > 1 && !op.replace_all {
                return Ok(ToolOutput::error(format!(
                    "edit[{i}]: old_string matched {count} times; pass replace_all=true or add context"
                )));
            }
            working = if op.replace_all {
                total += count;
                working.replace(&op.old_string, &op.new_string)
            } else {
                total += 1;
                working.replacen(&op.old_string, &op.new_string, 1)
            };
        }

        tokio::fs::write(&abs, &working).await?;

        Ok(ToolOutput::ok(format!(
            "applied {} edit(s) ({} replacement(s)) to {}",
            args.edits.len(),
            total,
            abs.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup(content: &str) -> (tempfile::TempDir, PathBuf, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        tokio::fs::write(&p, content).await.unwrap();
        let ctx = ToolCtx::new(dir.path().to_path_buf());
        ctx.mark_read(&p);
        (dir, p, ctx)
    }

    #[tokio::test]
    async fn applies_edits_in_order() {
        let (_d, p, ctx) = setup("hello world").await;
        let out = MultiEdit
            .execute(
                serde_json::json!({
                    "path": p.to_str().unwrap(),
                    "edits": [
                        { "old_string": "hello", "new_string": "hi" },
                        { "old_string": "world", "new_string": "lodan" }
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "hi lodan");
    }

    #[tokio::test]
    async fn later_edit_sees_earlier_result() {
        // 1 つ目で "a"→"b" した結果に 2 つ目の "b"→"c" が効く。
        let (_d, p, ctx) = setup("a").await;
        MultiEdit
            .execute(
                serde_json::json!({
                    "path": p.to_str().unwrap(),
                    "edits": [
                        { "old_string": "a", "new_string": "b" },
                        { "old_string": "b", "new_string": "c" }
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "c");
    }

    #[tokio::test]
    async fn aborts_atomically_on_failed_edit() {
        // 2 つ目が見つからない → ファイルは元のまま。
        let (_d, p, ctx) = setup("hello world").await;
        let out = MultiEdit
            .execute(
                serde_json::json!({
                    "path": p.to_str().unwrap(),
                    "edits": [
                        { "old_string": "hello", "new_string": "hi" },
                        { "old_string": "NOPE", "new_string": "x" }
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(
            tokio::fs::read_to_string(&p).await.unwrap(),
            "hello world",
            "file must be unchanged on abort"
        );
    }

    #[tokio::test]
    async fn ambiguous_match_without_replace_all_aborts() {
        let (_d, p, ctx) = setup("x x x").await;
        let out = MultiEdit
            .execute(
                serde_json::json!({
                    "path": p.to_str().unwrap(),
                    "edits": [ { "old_string": "x", "new_string": "y" } ]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "x x x");
    }

    #[tokio::test]
    async fn replace_all_within_one_edit() {
        let (_d, p, ctx) = setup("x x x").await;
        MultiEdit
            .execute(
                serde_json::json!({
                    "path": p.to_str().unwrap(),
                    "edits": [ { "old_string": "x", "new_string": "y", "replace_all": true } ]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "y y y");
    }

    #[tokio::test]
    async fn requires_read_first() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        tokio::fs::write(&p, "hello").await.unwrap();
        let ctx = ToolCtx::new(dir.path().to_path_buf());
        let res = MultiEdit
            .execute(
                serde_json::json!({
                    "path": p.to_str().unwrap(),
                    "edits": [ { "old_string": "hello", "new_string": "world" } ]
                }),
                &ctx,
            )
            .await;
        assert!(matches!(res, Err(ToolError::NotReadYet(_))));
    }
}
