// ユーザーに選択式の質問をし、選ばれた選択肢を返す。
// REPL の stdin から読む (permission ゲートと同じ経路)。番号 or ラベルで選べる。
// 非対話 (EOF/読み取り不可) の場合はエラーを返す。副作用は無いので非破壊。

use async_trait::async_trait;
use std::io::{self, BufRead, Write};

use super::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct AskUserQuestion;

#[async_trait]
impl Tool for AskUserQuestion {
    fn name(&self) -> &str {
        "AskUserQuestion"
    }

    fn description(&self) -> &str {
        "Ask the user to pick one of several options. Prints the question and options, \
         then reads the user's choice (option number or its exact label) from stdin."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": { "type": "string" },
                "options": {
                    "type": "array",
                    "minItems": 1,
                    "items": { "type": "string" }
                }
            },
            "required": ["question", "options"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let question = args
            .get("question")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ToolError::InvalidArgs("AskUserQuestion: non-empty `question` required".into())
            })?
            .to_string();

        let options: Vec<String> = args
            .get("options")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        if options.is_empty() {
            return Err(ToolError::InvalidArgs(
                "AskUserQuestion: `options` must be a non-empty array of strings".into(),
            ));
        }

        // stdin は同期ブロッキングなので blocking スレッドで読む。
        let chosen = tokio::task::spawn_blocking(move || prompt_loop(&question, &options))
            .await
            .map_err(|e| ToolError::Other(format!("AskUserQuestion: join error: {e}")))?;

        match chosen {
            Some(label) => Ok(ToolOutput::ok(format!("User selected: {label}"))),
            None => Ok(ToolOutput::error(
                "AskUserQuestion: no selection (non-interactive input or EOF)",
            )),
        }
    }
}

/// 質問＋番号付き選択肢を表示する文字列。
fn render(question: &str, options: &[String]) -> String {
    let mut out = format!("{question}\n");
    for (i, opt) in options.iter().enumerate() {
        out.push_str(&format!("  {}) {}\n", i + 1, opt));
    }
    out
}

/// 入力行を選択肢の index に対応づける。1 始まりの番号、または選択肢ラベルとの
/// 完全一致 (大小無視)。該当なしは None。
fn parse_choice(line: &str, options: &[String]) -> Option<usize> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(n) = trimmed.parse::<usize>()
        && n >= 1
        && n <= options.len()
    {
        return Some(n - 1);
    }
    options.iter().position(|o| o.eq_ignore_ascii_case(trimmed))
}

/// 有効な選択が得られるまで再表示しつつ読む。EOF / 読み取り不可なら None。
fn prompt_loop(question: &str, options: &[String]) -> Option<String> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    loop {
        let _ = write!(stdout, "{}", render(question, options));
        let _ = write!(stdout, "> ");
        let _ = stdout.flush();

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => return None, // EOF / エラー
            Ok(_) => {}
        }
        if let Some(i) = parse_choice(&line, options) {
            return Some(options[i].clone());
        }
        // 無効な入力 → 再表示してループ。
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Vec<String> {
        vec!["Yes".into(), "No".into(), "Maybe".into()]
    }

    #[test]
    fn parse_choice_by_number() {
        let o = opts();
        assert_eq!(parse_choice("1", &o), Some(0));
        assert_eq!(parse_choice(" 3 \n", &o), Some(2));
        assert_eq!(parse_choice("0", &o), None);
        assert_eq!(parse_choice("4", &o), None);
    }

    #[test]
    fn parse_choice_by_label_case_insensitive() {
        let o = opts();
        assert_eq!(parse_choice("Yes", &o), Some(0));
        assert_eq!(parse_choice("no", &o), Some(1));
        assert_eq!(parse_choice("MAYBE", &o), Some(2));
        assert_eq!(parse_choice("nope", &o), None);
        assert_eq!(parse_choice("", &o), None);
    }

    #[test]
    fn render_lists_numbered_options() {
        let out = render("Pick one", &opts());
        assert!(out.starts_with("Pick one\n"));
        assert!(out.contains("1) Yes"));
        assert!(out.contains("3) Maybe"));
    }

    #[tokio::test]
    async fn missing_question_is_invalid_args() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let res = AskUserQuestion
            .execute(serde_json::json!({ "options": ["a"] }), &ctx)
            .await;
        assert!(matches!(res, Err(ToolError::InvalidArgs(_))));
    }

    #[tokio::test]
    async fn empty_options_is_invalid_args() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let res = AskUserQuestion
            .execute(serde_json::json!({ "question": "q?", "options": [] }), &ctx)
            .await;
        assert!(matches!(res, Err(ToolError::InvalidArgs(_))));
    }
}
