use async_trait::async_trait;

use super::{TodoItem, TodoStatus, Tool, ToolCtx, ToolError, ToolOutput};

pub struct TodoWrite;

#[async_trait]
impl Tool for TodoWrite {
    fn name(&self) -> &str {
        "TodoWrite"
    }
    fn description(&self) -> &str {
        "Replace the session todo list. Use at the start of multi-step work and after each item finishes. Pass the full list every call; one item at most may be in_progress."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id":         { "type": "string" },
                            "content":    { "type": "string" },
                            "status":     { "type": "string", "enum": ["pending", "in_progress", "completed"] },
                            "activeForm": { "type": "string", "description": "Present-tense form shown while the item is in_progress" }
                        },
                        "required": ["id", "content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let raw = args
            .get("todos")
            .ok_or_else(|| ToolError::InvalidArgs("missing 'todos' array".into()))?
            .clone();

        let items: Vec<TodoItem> = serde_json::from_value(raw)
            .map_err(|e| ToolError::InvalidArgs(format!("bad 'todos' shape: {e}")))?;

        let in_progress = items
            .iter()
            .filter(|i| i.status == TodoStatus::InProgress)
            .count();
        if in_progress > 1 {
            return Err(ToolError::InvalidArgs(format!(
                "at most one todo may be in_progress, got {in_progress}"
            )));
        }

        ctx.replace_todos(items);
        Ok(ToolOutput::ok(ctx.render_todos()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> ToolCtx {
        ToolCtx::new(PathBuf::from("/tmp"))
    }

    #[tokio::test]
    async fn replaces_list_and_renders() {
        let ctx = ctx();
        let out = TodoWrite
            .execute(
                serde_json::json!({
                    "todos": [
                        {"id":"1","content":"Read repl.rs","status":"completed"},
                        {"id":"2","content":"Add test","status":"in_progress","activeForm":"Adding test"},
                        {"id":"3","content":"Open PR","status":"pending"}
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("[x] 1"));
        assert!(out.content.contains("[~] 2 Adding test"));
        assert!(out.content.contains("[ ] 3 Open PR"));

        let stored = ctx.todos.lock().unwrap();
        assert_eq!(stored.len(), 3);
    }

    #[tokio::test]
    async fn rejects_multiple_in_progress() {
        let ctx = ctx();
        let res = TodoWrite
            .execute(
                serde_json::json!({
                    "todos": [
                        {"id":"1","content":"a","status":"in_progress"},
                        {"id":"2","content":"b","status":"in_progress"}
                    ]
                }),
                &ctx,
            )
            .await;
        assert!(matches!(res, Err(ToolError::InvalidArgs(_))));
    }

    #[tokio::test]
    async fn rejects_missing_todos_key() {
        let ctx = ctx();
        let res = TodoWrite.execute(serde_json::json!({}), &ctx).await;
        assert!(matches!(res, Err(ToolError::InvalidArgs(_))));
    }

    #[tokio::test]
    async fn empty_list_is_valid_and_clears_state() {
        let ctx = ctx();
        // seed
        TodoWrite
            .execute(
                serde_json::json!({"todos":[{"id":"1","content":"x","status":"pending"}]}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(ctx.todos.lock().unwrap().len(), 1);

        // clear
        let out = TodoWrite
            .execute(serde_json::json!({"todos":[]}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("(empty)"));
        assert_eq!(ctx.todos.lock().unwrap().len(), 0);
    }
}
