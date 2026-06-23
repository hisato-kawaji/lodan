use async_trait::async_trait;
use globset::Glob as GlobsetGlob;
use ignore::WalkBuilder;
use std::path::PathBuf;

use super::{Tool, ToolCtx, ToolError, ToolOutput};

const DEFAULT_MAX: usize = 200;

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "Glob"
    }
    fn description(&self) -> &str {
        "Find files by glob pattern (gitignore-aware)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob, e.g. **/*.rs" },
                "path":    { "type": "string", "description": "Root dir (default: cwd)" },
                "max":     { "type": "integer", "minimum": 1 }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing 'pattern'".into()))?
            .to_string();
        let root = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx.cwd.clone());
        let max = args
            .get("max")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_MAX);

        let matcher = GlobsetGlob::new(&pattern)
            .map_err(|e| ToolError::InvalidArgs(format!("bad glob: {e}")))?
            .compile_matcher();
        let root_for_strip = root.clone();

        let hits = tokio::task::spawn_blocking(move || {
            let mut out: Vec<PathBuf> = Vec::new();
            for entry in WalkBuilder::new(&root).hidden(false).build().flatten() {
                if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                let path = entry.path();
                let rel = path.strip_prefix(&root_for_strip).unwrap_or(path);
                if matcher.is_match(rel) || matcher.is_match(path) {
                    out.push(path.to_path_buf());
                    if out.len() >= max {
                        break;
                    }
                }
            }
            out
        })
        .await
        .map_err(|e| ToolError::Other(e.to_string()))?;

        let body = if hits.is_empty() {
            format!("no files match {pattern}")
        } else {
            hits.iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(ToolOutput::ok(body))
    }
}
