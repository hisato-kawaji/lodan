use async_trait::async_trait;
use grep_regex::RegexMatcher;
use grep_searcher::{Searcher, Sink, SinkMatch};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

use super::{Tool, ToolCtx, ToolError, ToolOutput};

const DEFAULT_MAX: usize = 100;

pub struct Grep;

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "Grep"
    }
    fn description(&self) -> &str {
        "Regex search across files (gitignore-aware). Returns up to `max` matches."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex pattern" },
                "path":    { "type": "string", "description": "Directory (default: cwd)" },
                "glob":    { "type": "string", "description": "Optional path filter, e.g. **/*.rs" },
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
        let glob_pat = args
            .get("glob")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let max = args
            .get("max")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_MAX);

        let matcher = RegexMatcher::new(&pattern)
            .map_err(|e| ToolError::InvalidArgs(format!("bad regex: {e}")))?;

        let glob = match glob_pat {
            Some(p) => Some(
                globset::Glob::new(&p)
                    .map_err(|e| ToolError::InvalidArgs(format!("bad glob: {e}")))?
                    .compile_matcher(),
            ),
            None => None,
        };

        // ripgrep-style walker, run in blocking pool because it is sync.
        let result = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<String>> {
            let mut out: Vec<String> = Vec::new();
            let walker = WalkBuilder::new(&root).hidden(false).build();
            for entry in walker {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                let path: &Path = entry.path();
                if let Some(g) = &glob {
                    if !g.is_match(path) {
                        continue;
                    }
                }
                let mut hits: Vec<(u64, String)> = Vec::new();
                let mut sink = MatchSink {
                    out: &mut hits,
                    remaining: max.saturating_sub(out.len()),
                };
                if sink.remaining == 0 {
                    break;
                }
                if Searcher::new().search_path(&matcher, path, &mut sink).is_err() {
                    continue;
                }
                for (lineno, line) in hits {
                    out.push(format!("{}:{}: {}", path.display(), lineno, line.trim_end()));
                    if out.len() >= max {
                        return Ok(out);
                    }
                }
            }
            Ok(out)
        })
        .await
        .map_err(|e| ToolError::Other(e.to_string()))??;

        let body = if result.is_empty() {
            format!("no matches for /{pattern}/")
        } else {
            format!(
                "{} match(es) for /{}/:\n{}",
                result.len(),
                pattern,
                result.join("\n")
            )
        };
        Ok(ToolOutput::ok(body))
    }
}

struct MatchSink<'a> {
    out: &'a mut Vec<(u64, String)>,
    remaining: usize,
}

impl<'a> Sink for MatchSink<'a> {
    type Error = std::io::Error;
    fn matched(&mut self, _s: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self.remaining == 0 {
            return Ok(false);
        }
        let line = String::from_utf8_lossy(mat.bytes()).into_owned();
        let lineno = mat.line_number().unwrap_or(0);
        self.out.push((lineno, line));
        self.remaining -= 1;
        Ok(self.remaining > 0)
    }
}
