// Jupyter notebook (.ipynb) のセルを編集する。
// JSON を Value として読み、`cells` 配列を index 指定で置換 / 挿入 / 削除する。
// 未知フィールドは保持し、書き戻しは pretty JSON + 末尾改行。
// read-before-edit 規律 (Edit / MultiEdit と同様) を課す。

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;

use super::{Tool, ToolCtx, ToolError, ToolOutput};

#[derive(Debug, Deserialize)]
struct NotebookEditArgs {
    path: String,
    /// 対象セルの 0 始まり index。replace/delete では既存セル、insert では挿入位置。
    cell_index: usize,
    #[serde(default)]
    new_source: String,
    /// replace (既定) / insert / delete。
    #[serde(default)]
    edit_mode: EditMode,
    /// insert 時のセル種別 (既定 code)。replace で指定すると種別も変更する。
    #[serde(default)]
    cell_type: Option<CellType>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum EditMode {
    #[default]
    Replace,
    Insert,
    Delete,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum CellType {
    Code,
    Markdown,
}

impl CellType {
    fn as_str(self) -> &'static str {
        match self {
            CellType::Code => "code",
            CellType::Markdown => "markdown",
        }
    }
}

pub struct NotebookEdit;

#[async_trait]
impl Tool for NotebookEdit {
    fn name(&self) -> &str {
        "NotebookEdit"
    }

    fn description(&self) -> &str {
        "Edit a Jupyter notebook (.ipynb) cell by 0-based index. edit_mode is \
         replace (default) / insert / delete. For insert, cell_type selects code or \
         markdown (default code). Replacing a code cell clears its outputs."
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "cell_index": { "type": "integer", "minimum": 0 },
                "new_source": { "type": "string" },
                "edit_mode": {
                    "type": "string",
                    "enum": ["replace", "insert", "delete"],
                    "default": "replace"
                },
                "cell_type": { "type": "string", "enum": ["code", "markdown"] }
            },
            "required": ["path", "cell_index"]
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
        let args: NotebookEditArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs(format!("NotebookEdit: {e}")))?;

        let abs = {
            let p = PathBuf::from(&args.path);
            if p.is_absolute() { p } else { ctx.cwd.join(p) }
        };

        if !ctx.was_read(&abs) {
            return Err(ToolError::NotReadYet(abs.display().to_string()));
        }

        let raw = tokio::fs::read_to_string(&abs).await?;
        let mut nb: Value = serde_json::from_str(&raw)
            .map_err(|e| ToolError::Other(format!("{} is not valid JSON: {e}", abs.display())))?;

        let cells = nb
            .get_mut("cells")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| ToolError::Other(format!("{}: missing `cells` array", abs.display())))?;

        let summary = match args.edit_mode {
            EditMode::Replace => {
                let len = cells.len();
                let cell = cells
                    .get_mut(args.cell_index)
                    .ok_or_else(|| out_of_range(args.cell_index, len, "replace"))?;
                apply_replace(cell, &args.new_source, args.cell_type);
                format!("replaced cell {}", args.cell_index)
            }
            EditMode::Insert => {
                if args.cell_index > cells.len() {
                    return Err(out_of_range(args.cell_index, cells.len(), "insert"));
                }
                let kind = args.cell_type.unwrap_or(CellType::Code);
                cells.insert(args.cell_index, new_cell(kind, &args.new_source));
                format!("inserted {} cell at {}", kind.as_str(), args.cell_index)
            }
            EditMode::Delete => {
                if args.cell_index >= cells.len() {
                    return Err(out_of_range(args.cell_index, cells.len(), "delete"));
                }
                cells.remove(args.cell_index);
                format!("deleted cell {}", args.cell_index)
            }
        };

        let mut serialized = serde_json::to_string_pretty(&nb)
            .map_err(|e| ToolError::Other(format!("serializing notebook: {e}")))?;
        serialized.push('\n');
        tokio::fs::write(&abs, serialized).await?;

        Ok(ToolOutput::ok(format!("{} in {}", summary, abs.display())))
    }
}

fn out_of_range(index: usize, len: usize, mode: &str) -> ToolError {
    ToolError::InvalidArgs(format!(
        "cell_index {index} out of range for {mode} (notebook has {len} cell(s))"
    ))
}

/// `source` を Jupyter 慣習のリスト (各行が改行込み、最終行は改行なし可) へ変換する。
/// 空文字列は空配列。
fn to_source_lines(s: &str) -> Vec<Value> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split_inclusive('\n').map(|l| json!(l)).collect()
}

/// 既存セルの source を差し替える。code セルなら outputs/execution_count をリセット。
/// `cell_type` 指定があれば種別も変更する (code↔markdown でフィールドを整える)。
fn apply_replace(cell: &mut Value, new_source: &str, new_type: Option<CellType>) {
    if let Some(kind) = new_type {
        cell["cell_type"] = json!(kind.as_str());
        normalize_for_type(cell, kind);
    }
    cell["source"] = Value::Array(to_source_lines(new_source));
    if cell.get("cell_type").and_then(Value::as_str) == Some("code") {
        cell["outputs"] = json!([]);
        cell["execution_count"] = Value::Null;
    }
}

/// セル種別に必要なフィールドを揃える。code には outputs/execution_count、
/// markdown ではそれらを取り除く。
fn normalize_for_type(cell: &mut Value, kind: CellType) {
    let obj = match cell.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    match kind {
        CellType::Code => {
            obj.entry("outputs").or_insert_with(|| json!([]));
            obj.entry("execution_count").or_insert(Value::Null);
        }
        CellType::Markdown => {
            obj.remove("outputs");
            obj.remove("execution_count");
        }
    }
}

fn new_cell(kind: CellType, source: &str) -> Value {
    let mut cell = json!({
        "cell_type": kind.as_str(),
        "metadata": {},
        "source": to_source_lines(source),
    });
    if kind == CellType::Code {
        cell["outputs"] = json!([]);
        cell["execution_count"] = Value::Null;
    }
    cell
}

#[cfg(test)]
mod tests {
    use super::*;

    const NB: &str = r##"{
        "cells": [
            {"cell_type": "code", "metadata": {}, "execution_count": 3,
             "outputs": [{"output_type": "stream", "text": "old"}],
             "source": ["print('a')\n"]},
            {"cell_type": "markdown", "metadata": {}, "source": ["# title\n"]}
        ],
        "metadata": {"kernelspec": {"name": "python3"}},
        "nbformat": 4,
        "nbformat_minor": 5
    }"##;

    async fn setup() -> (tempfile::TempDir, PathBuf, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nb.ipynb");
        tokio::fs::write(&p, NB).await.unwrap();
        let ctx = ToolCtx::new(dir.path().to_path_buf());
        ctx.mark_read(&p);
        (dir, p, ctx)
    }

    async fn read_nb(p: &PathBuf) -> Value {
        serde_json::from_str(&tokio::fs::read_to_string(p).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn replace_code_cell_clears_outputs() {
        let (_d, p, ctx) = setup().await;
        let out = NotebookEdit
            .execute(
                json!({ "path": p.to_str().unwrap(), "cell_index": 0,
                        "new_source": "print('b')\n" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let nb = read_nb(&p).await;
        let cell = &nb["cells"][0];
        assert_eq!(cell["source"], json!(["print('b')\n"]));
        assert_eq!(cell["outputs"], json!([]));
        assert!(cell["execution_count"].is_null());
        // 他のセルや top-level メタは保持される。
        assert_eq!(nb["nbformat"], 4);
        assert_eq!(nb["cells"][1]["source"], json!(["# title\n"]));
    }

    #[tokio::test]
    async fn insert_markdown_cell_shifts_others() {
        let (_d, p, ctx) = setup().await;
        NotebookEdit
            .execute(
                json!({ "path": p.to_str().unwrap(), "cell_index": 1,
                        "edit_mode": "insert", "cell_type": "markdown",
                        "new_source": "## note\nbody" }),
                &ctx,
            )
            .await
            .unwrap();
        let nb = read_nb(&p).await;
        assert_eq!(nb["cells"].as_array().unwrap().len(), 3);
        let inserted = &nb["cells"][1];
        assert_eq!(inserted["cell_type"], "markdown");
        // 複数行は改行込みでリスト化される。
        assert_eq!(inserted["source"], json!(["## note\n", "body"]));
        assert!(inserted.get("outputs").is_none());
        // 元の index 1 (markdown) は 2 へずれる。
        assert_eq!(nb["cells"][2]["source"], json!(["# title\n"]));
    }

    #[tokio::test]
    async fn delete_cell() {
        let (_d, p, ctx) = setup().await;
        NotebookEdit
            .execute(
                json!({ "path": p.to_str().unwrap(), "cell_index": 0,
                        "edit_mode": "delete" }),
                &ctx,
            )
            .await
            .unwrap();
        let nb = read_nb(&p).await;
        let cells = nb["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0]["cell_type"], "markdown");
    }

    #[tokio::test]
    async fn out_of_range_index_is_invalid_args() {
        let (_d, p, ctx) = setup().await;
        let res = NotebookEdit
            .execute(
                json!({ "path": p.to_str().unwrap(), "cell_index": 9,
                        "new_source": "x" }),
                &ctx,
            )
            .await;
        assert!(matches!(res, Err(ToolError::InvalidArgs(_))));
    }

    #[tokio::test]
    async fn requires_read_first() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nb.ipynb");
        tokio::fs::write(&p, NB).await.unwrap();
        let ctx = ToolCtx::new(dir.path().to_path_buf());
        let res = NotebookEdit
            .execute(
                json!({ "path": p.to_str().unwrap(), "cell_index": 0, "new_source": "x" }),
                &ctx,
            )
            .await;
        assert!(matches!(res, Err(ToolError::NotReadYet(_))));
    }

    #[tokio::test]
    async fn rejects_non_notebook_json() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bad.ipynb");
        tokio::fs::write(&p, "{\"foo\": 1}").await.unwrap();
        let ctx = ToolCtx::new(dir.path().to_path_buf());
        ctx.mark_read(&p);
        let res = NotebookEdit
            .execute(
                json!({ "path": p.to_str().unwrap(), "cell_index": 0, "new_source": "x" }),
                &ctx,
            )
            .await;
        assert!(matches!(res, Err(ToolError::Other(_))));
    }
}
