use async_trait::async_trait;
use std::path::PathBuf;

use super::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct Write;

#[async_trait]
impl Tool for Write {
    fn name(&self) -> &str {
        "Write"
    }
    fn description(&self) -> &str {
        "Create or overwrite a file. For existing files, you must Read first."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string", "description": "Absolute file path" },
                "content": { "type": "string", "description": "Full file content" }
            },
            "required": ["path", "content"]
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
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'content'".into()))?;

        let abs = PathBuf::from(path);
        let abs = if abs.is_absolute() {
            abs
        } else {
            ctx.cwd.join(abs)
        };

        if abs.exists() && !ctx.was_read(&abs) {
            return Err(ToolError::NotReadYet(abs.display().to_string()));
        }

        // #65 A2: 大きな内容を 1 引数で書かせない。小型モデルは数百行を一貫して生成できず、途中で
        // 崩れた内容がそのままファイルになる。骨格 → Edit で部分ごとに埋める手順に落とす。
        let lines = content.lines().count();
        if ctx.write_max_lines > 0 && lines > ctx.write_max_lines {
            crate::runlog::record(
                "write_split_refused",
                serde_json::json!({ "path": abs.display().to_string(), "lines": lines, "limit": ctx.write_max_lines }),
            );
            return Ok(ToolOutput::error(format!(
                "Write refused: content is {lines} lines, but a single Write may have at most {} \
                 lines here. Nothing was written. Do it in steps: (1) Write a skeleton of at most {} \
                 lines — imports, constants, type and function signatures, with a distinct \
                 placeholder body in each (e.g. `# TODO <name>` naming the function, so every \
                 placeholder is unique) — then (2) fill each placeholder with Edit (old_string = \
                 that placeholder), one part at a time, each part at most {} lines. Read the file \
                 before each Edit if unsure of its current content.",
                ctx.write_max_lines, ctx.write_max_lines, ctx.write_max_lines
            )));
        }

        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&abs, content).await?;
        ctx.mark_read(&abs);

        Ok(ToolOutput::ok(format!(
            "wrote {} ({} bytes)",
            abs.display(),
            content.len()
        )))
    }
}

#[cfg(test)]
mod split_tests {
    use super::*;

    /// `write_max_lines` (#65 A2): 上限を超える Write は書かずに手順を返す。0 は無制限。
    #[tokio::test]
    async fn a_write_over_the_line_limit_is_refused_with_a_skeleton_then_edit_plan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.py");
        let four = "a\nb\nc\nd\n";
        let args = serde_json::json!({ "path": path, "content": four });
        let limited = ToolCtx::new(dir.path().to_path_buf()).with_write_limit(3);
        let out = Write.execute(args.clone(), &limited).await.unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("4 lines")
                && out.content.contains("at most 3")
                && out.content.contains("Edit"),
            "{}",
            out.content
        );
        assert!(!path.exists(), "nothing is written when refused");
        // 上限ちょうどは通る。
        let three = serde_json::json!({ "path": path, "content": "a\nb\nc\n" });
        assert!(!Write.execute(three, &limited).await.unwrap().is_error);
        // 0 は無制限。
        let unlimited = ToolCtx::new(dir.path().to_path_buf());
        unlimited.mark_read(&path);
        assert!(!Write.execute(args, &unlimited).await.unwrap().is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), four);
    }
}
