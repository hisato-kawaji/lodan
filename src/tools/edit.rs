use async_trait::async_trait;
use std::path::PathBuf;

use super::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct Edit;

#[async_trait]
impl Tool for Edit {
    fn name(&self) -> &str {
        "Edit"
    }
    fn description(&self) -> &str {
        "Replace an exact string in a file. The match must be unique unless replace_all is true."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":        { "type": "string" },
                "old_string":  { "type": "string" },
                "new_string":  { "type": "string" },
                "replace_all": { "type": "boolean", "default": false }
            },
            "required": ["path", "old_string", "new_string"]
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
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'path'".into()))?;
        let old = args
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'old_string'".into()))?;
        let new = args
            .get("new_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'new_string'".into()))?;
        let replace_all = args
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if old.is_empty() {
            return Err(ToolError::InvalidArgs(
                "old_string must not be empty".into(),
            ));
        }
        if old == new {
            return Err(ToolError::InvalidArgs(
                "old_string and new_string are identical".into(),
            ));
        }

        let abs = PathBuf::from(path);
        let abs = if abs.is_absolute() {
            abs
        } else {
            ctx.cwd.join(abs)
        };

        if !ctx.was_read(&abs) {
            return Err(ToolError::NotReadYet(abs.display().to_string()));
        }

        let original = tokio::fs::read_to_string(&abs).await?;
        let count = original.matches(old).count();
        if count == 0 {
            return Ok(ToolOutput::error(format!(
                "old_string not found in {}",
                abs.display()
            )));
        }
        if count > 1 && !replace_all {
            return Ok(ToolOutput::error(format!(
                "old_string matched {count} times in {}; pass replace_all=true or supply more context",
                abs.display()
            )));
        }

        let updated = if replace_all {
            original.replace(old, new)
        } else {
            original.replacen(old, new, 1)
        };
        tokio::fs::write(&abs, &updated).await?;

        Ok(ToolOutput::ok(format!(
            "edited {} ({} replacement{})",
            abs.display(),
            if replace_all { count } else { 1 },
            if replace_all && count != 1 { "s" } else { "" }
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn errors_on_ambiguous_match() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        tokio::fs::write(&p, "x x x").await.unwrap();

        let ctx = ToolCtx::new(dir.path().to_path_buf());
        ctx.mark_read(&p);

        let out = Edit
            .execute(
                serde_json::json!({
                    "path": p.to_str().unwrap(),
                    "old_string": "x",
                    "new_string": "y"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "x x x");
    }

    #[tokio::test]
    async fn requires_read_first() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        tokio::fs::write(&p, "hello").await.unwrap();

        let ctx = ToolCtx::new(dir.path().to_path_buf());
        let res = Edit
            .execute(
                serde_json::json!({
                    "path": p.to_str().unwrap(),
                    "old_string": "hello",
                    "new_string": "world"
                }),
                &ctx,
            )
            .await;
        assert!(matches!(res, Err(ToolError::NotReadYet(_))));
    }

    #[tokio::test]
    async fn replaces_unique_match() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        tokio::fs::write(&p, "hello world").await.unwrap();

        let ctx = ToolCtx::new(dir.path().to_path_buf());
        ctx.mark_read(&p);

        Edit.execute(
            serde_json::json!({
                "path": p.to_str().unwrap(),
                "old_string": "world",
                "new_string": "lodan"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "hello lodan");
    }
}
