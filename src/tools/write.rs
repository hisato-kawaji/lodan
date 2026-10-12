use async_trait::async_trait;
use std::path::PathBuf;

use super::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct Write;

/// 追記結果に付ける進捗。行数を数えるには全文を読むしかないので、分割生成で想定する大きさ
/// (数百行) までに留める。これを超えるファイルへの追記を繰り返すと読み直しが O(n^2) になる
/// (#152 のレビュー)。
const MAX_LINE_COUNT_BYTES: u64 = 1 << 20;

/// 追記後のファイルの大きさ。小さいうちは「何行目まで書けたか」も返す — 分割生成では
/// モデルが続きを書く位置の目印になる。
async fn append_progress(path: &std::path::Path, total: u64) -> String {
    if total > MAX_LINE_COUNT_BYTES {
        return format!(" ({total} bytes total)");
    }
    match tokio::fs::read_to_string(path).await {
        Ok(text) => format!(" ({total} bytes, {} lines total)", text.lines().count()),
        Err(_) => format!(" ({total} bytes total)"),
    }
}

#[async_trait]
impl Tool for Write {
    fn name(&self) -> &str {
        "Write"
    }
    fn description(&self) -> &str {
        "Create or overwrite a file. For existing files, you must Read first. With append: true it \
         adds to the end of the file instead (no Read needed, nothing is overwritten) — use that to \
         build a long file in pieces."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string", "description": "Absolute file path" },
                "content": { "type": "string", "description": "Full file content, or the piece to add when append is true" },
                "append": { "type": "boolean", "description": "Add to the end of the file instead of replacing it. Default false" }
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

        // 追記は既存の内容を壊さないので Read を要求しない (#65 A2 の修正: 上限に当たった後、
        // 小型モデルが `old_string` を当てられず Edit で詰むのを避けるための経路)。
        let append = args
            .get("append")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let existed = abs.exists();
        if !append && existed && !ctx.was_read(&abs) {
            return Err(ToolError::NotReadYet(abs.display().to_string()));
        }

        // #65 A2: 大きな内容を 1 引数で書かせない。小型モデルは数百行を一貫して生成できず、途中で
        // 崩れた内容がそのままファイルになる。**先頭から順に追記**させる手順に落とす。
        //
        // 以前は「骨格 → Edit で埋める」と誘導していたが、ラダーの実測
        // (`docs/eval/ladder/phase1-2026-10.md`) で 9B は自分が書いたプレースホルダを再現できず、
        // `old_string not found` を繰り返して未完成のまま終わった。追記なら文字列の一致が要らない。
        let lines = content.lines().count();
        if ctx.write_max_lines > 0 && lines > ctx.write_max_lines {
            crate::runlog::record(
                "write_split_refused",
                serde_json::json!({
                    "path": abs.display().to_string(),
                    "lines": lines,
                    "limit": ctx.write_max_lines,
                    "append": append,
                }),
            );
            let limit = ctx.write_max_lines;
            return Ok(ToolOutput::error(format!(
                "Write refused: this is {lines} lines, but one Write may have at most {limit} lines \
                 here. Nothing was written. Build the file in order instead: first Write the first \
                 {limit} lines or fewer (imports and the first definitions, complete as far as they \
                 go), then add each next piece with Write {{\"path\": …, \"append\": true, \
                 \"content\": …}} — at most {limit} lines per call, continuing exactly where the \
                 previous piece ended. Do not use placeholders and do not Edit; append in order \
                 until the file is complete, then run it to check."
            )));
        }

        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if append {
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&abs)
                .await?;
            file.write_all(content.as_bytes()).await?;
            file.flush().await?;
        } else {
            tokio::fs::write(&abs, content).await?;
        }
        // 「読んだ」に数えるのは全文を把握しているときだけ。既存ファイルへの追記でこれを立てると、
        // 一度も見ていないファイルに 1 行足してから全文上書きする、で Read 必須ガードを迂回できる
        // (#152 のレビューで実証された)。新規作成の追記は中身がこの content そのものなので数える。
        if !append || !existed {
            ctx.mark_read(&abs);
        }

        let total = tokio::fs::metadata(&abs)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        Ok(ToolOutput::ok(if append {
            format!(
                "appended {} bytes to {}{}",
                content.len(),
                abs.display(),
                append_progress(&abs, total).await
            )
        } else {
            format!("wrote {} ({} bytes)", abs.display(), content.len())
        }))
    }
}

#[cfg(test)]
mod split_tests {
    use super::*;

    /// `append: true` (#65 A2 の修正): 末尾に足す。Read は要らず、既存の内容は消えない。
    /// 上限は 1 回ごとに掛かるので、上限以内の追記を繰り返して長いファイルを作れる。
    #[tokio::test]
    async fn append_adds_to_the_end_without_a_read_and_respects_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grow.py");
        let ctx = ToolCtx::new(dir.path().to_path_buf()).with_write_limit(2);
        let put = |content: &str, append: bool| {
            let args = serde_json::json!({ "path": path, "content": content, "append": append });
            Write.execute(args, &ctx)
        };
        // 1 回目は新規 (上限ちょうど)。
        assert!(!put("a\nb\n", false).await.unwrap().is_error);
        // 追記は Read 無しで通り、内容は足される。
        let out = put("c\nd\n", true).await.unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("appended") && out.content.contains("4 lines total"),
            "{}",
            out.content
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\nc\nd\n");
        // 上限は追記にも掛かる。拒否されても中身は変わらない。
        let out = put("e\nf\ng\n", true).await.unwrap();
        assert!(
            out.is_error && out.content.contains("append"),
            "{}",
            out.content
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\nc\nd\n");
        // 既存ファイルの上書きは従来どおり Read が必要。
        let fresh = ToolCtx::new(dir.path().to_path_buf());
        let err = Write
            .execute(
                serde_json::json!({ "path": path, "content": "x\n" }),
                &fresh,
            )
            .await;
        assert!(matches!(err, Err(ToolError::NotReadYet(_))));
        // 追記なら Read 無しでよい。
        assert!(
            !Write
                .execute(
                    serde_json::json!({ "path": path, "content": "x\n", "append": true }),
                    &fresh
                )
                .await
                .unwrap()
                .is_error
        );
    }

    /// 既存ファイルへの追記は「読んだ」ことにしない。これを立てると、一度も見ていないファイルに
    /// 1 行足して Read 必須ガードを外し、そのまま全文上書きできてしまう (#152 のレビュー)。
    #[tokio::test]
    async fn appending_to_an_unread_file_does_not_unlock_overwriting_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.env");
        std::fs::write(&path, "KEY=original\n").unwrap();
        let ctx = ToolCtx::new(dir.path().to_path_buf());
        let put = |content: &str, append: bool| {
            let args = serde_json::json!({ "path": path, "content": content, "append": append });
            Write.execute(args, &ctx)
        };
        // 見ないままの追記は通る (中身は消えない)。
        assert!(!put("# note\n", true).await.unwrap().is_error);
        // その直後の全文上書きは、依然として Read を要求する。
        assert!(matches!(
            put("KEY=stolen\n", false).await,
            Err(ToolError::NotReadYet(_))
        ));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "KEY=original\n# note\n"
        );
        // 自分が作ったファイルなら全文を把握しているので、追記のあとも上書きできる。
        let made = dir.path().join("grown.py");
        assert!(
            !Write
                .execute(
                    serde_json::json!({ "path": made, "content": "a\n", "append": true }),
                    &ctx
                )
                .await
                .unwrap()
                .is_error
        );
        assert!(
            !Write
                .execute(serde_json::json!({ "path": made, "content": "b\n" }), &ctx)
                .await
                .unwrap()
                .is_error
        );
    }

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
