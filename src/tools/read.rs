use async_trait::async_trait;
use std::path::PathBuf;

use super::{Tool, ToolCtx, ToolError, ToolOutput};

const MAX_LINES_DEFAULT: usize = 2000;

pub struct Read;

#[async_trait]
impl Tool for Read {
    fn name(&self) -> &str {
        "Read"
    }
    fn description(&self) -> &str {
        "Read a file by absolute path. Optional 0-based offset and line limit."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":   { "type": "string", "description": "Absolute file path" },
                "offset": { "type": "integer", "minimum": 0, "description": "0-based starting line" },
                "limit":  { "type": "integer", "minimum": 1, "description": "Maximum lines to read" }
            },
            "required": ["path"]
        })
    }

    fn is_destructive(&self) -> bool {
        // read-only: ファイルを読むだけ
        false
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
        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(MAX_LINES_DEFAULT);

        let abs = PathBuf::from(path);
        let abs = if abs.is_absolute() {
            abs
        } else {
            ctx.cwd.join(abs)
        };

        let bytes = tokio::fs::read(&abs).await?;
        let content = String::from_utf8_lossy(&bytes);
        let total_lines = content.lines().count();

        let selected: String = content
            .lines()
            .enumerate()
            .skip(offset)
            .take(limit)
            .map(|(i, line)| format!("{:>6}\t{}\n", i + 1, line))
            .collect();

        ctx.mark_read(&abs);

        let header = format!(
            "{} ({} lines, showing {}..{})\n",
            abs.display(),
            total_lines,
            offset + 1,
            (offset + limit).min(total_lines).max(offset + 1),
        );
        Ok(ToolOutput::ok(format!("{header}{selected}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn reads_with_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        let mut f = std::fs::File::create(&p).unwrap();
        for i in 1..=10 {
            writeln!(f, "line {i}").unwrap();
        }
        let ctx = ToolCtx::new(dir.path().to_path_buf());
        let out = Read
            .execute(
                serde_json::json!({ "path": p.to_str().unwrap(), "offset": 2, "limit": 3 }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.content.contains("line 3"));
        assert!(out.content.contains("line 5"));
        assert!(!out.content.contains("line 6"));
        assert!(ctx.was_read(&p));
    }
}
