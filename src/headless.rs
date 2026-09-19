//! ヘッドレス実行 (`lodan -p "<prompt>"`)。1 ターンだけ走らせて終了する。
//!
//! stdout は呼び出し側 (スクリプト・CI・評価ハーネス) との契約なので、人間向けの表示
//! (ストリーム本文・ツール出力の要約・起動時の通知) は全て stderr へ回す。stdout に出るのは:
//!
//! - `text`: 最終応答の本文だけ
//! - `json`: 結果オブジェクト 1 行
//! - `stream-json`: runlog と同じイベント列 (`src/runlog.rs` の表) + 最後に `result`
//!
//! 承認を求める相手がいないので、`--yes` 無しでは破壊的ツールは尋ねずに拒否される。

use anyhow::{Context, Result};
use std::io::{IsTerminal, Read};

use crate::agent::r#loop::MaxIterationsError;
use crate::agent::messages::Message;
use crate::config::Config;
use crate::hooks::{self, HookOutcome, Lifecycle};
use crate::permission::PermissionGate;
use crate::runtime::{Notices, Runtime};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    StreamJson,
}

pub const EXIT_OK: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
/// 最終応答に至らないまま `agent.max_iterations` を使い切った。
pub const EXIT_MAX_ITERATIONS: i32 = 2;
/// SIGINT (128 + 2)。
pub const EXIT_INTERRUPTED: i32 = 130;

/// パイプされた stdin の読み取り上限。プロンプトとして LLM に送るものなので、誤って
/// 巨大なファイルを流し込まれても際限なく抱えない。
const STDIN_LIMIT_BYTES: u64 = 10 * 1024 * 1024;

pub struct Options {
    /// `-p` の引数 (値なしなら空文字列)。
    pub prompt: String,
    /// `--stdin`: 引数に加えて stdin も読む。
    pub read_stdin: bool,
    pub format: OutputFormat,
    pub resume: Option<String>,
}

/// 1 ターン実行し、終了コードを返す。
pub async fn run(cfg: Config, opts: Options) -> Result<i32> {
    crate::term::route_display_to_stderr();
    let Options {
        prompt: prompt_arg,
        read_stdin,
        format,
        resume,
    } = opts;

    let piped = if wants_stdin(&prompt_arg, read_stdin) {
        read_stdin_to_end()?
    } else {
        None
    };
    let prompt = assemble_prompt(&prompt_arg, piped.as_deref());
    if prompt.is_empty() {
        anyhow::bail!("-p needs a prompt: pass it as the argument, or pipe it on stdin");
    }

    let runtime = Runtime::build(&cfg, Notices::Stderr).await?;
    let gate = PermissionGate::non_interactive(cfg.agent.auto_approve);
    let (mut session, mut recorder) =
        runtime.open_session(&cfg, resume.as_deref(), Notices::Stderr);

    let start_payload = serde_json::json!({
        "hook_event_name": "SessionStart",
        "cwd": runtime.cwd.display().to_string(),
    });
    if let Ok(HookOutcome::Block(reason)) =
        hooks::runner::dispatch(Lifecycle::SessionStart, None, &start_payload, &cfg.hooks).await
    {
        eprintln!("session-start hook: {reason}");
    }

    let outcome = {
        let turn = session.run_turn(&prompt, runtime.llm.as_ref(), &gate);
        tokio::pin!(turn);
        tokio::select! {
            res = &mut turn => match res {
                Ok(()) => Outcome::Done,
                Err(e) => Outcome::Failed(e),
            },
            _ = tokio::signal::ctrl_c() => Outcome::Interrupted,
        }
    };
    if matches!(outcome, Outcome::Interrupted) {
        session.interrupt_repair();
    }
    if let Some(rec) = recorder.as_mut()
        && let Err(e) = rec.sync(session.history())
    {
        eprintln!("session: save failed: {e}");
    }

    let end_payload = serde_json::json!({ "hook_event_name": "SessionEnd" });
    let _ = hooks::runner::dispatch(Lifecycle::SessionEnd, None, &end_payload, &cfg.hooks).await;

    let report = Report::new(
        &outcome,
        final_text(session.history()),
        recorder.as_ref().map(|r| r.id().to_string()),
        session.usage(),
    );
    report.emit(format);
    Ok(report.exit_code)
}

enum Outcome {
    Done,
    Failed(anyhow::Error),
    Interrupted,
}

/// 引数と stdin を 1 つのプロンプトにまとめる。両方あれば「指示 + 対象データ」の並び
/// (`cat log.txt | lodan -p "要約して"`)。
fn assemble_prompt(arg: &str, piped: Option<&str>) -> String {
    let arg = arg.trim();
    let piped = piped.map(str::trim).unwrap_or("");
    match (arg.is_empty(), piped.is_empty()) {
        (false, false) => format!("{arg}\n\n{piped}"),
        (false, true) => arg.to_string(),
        (true, _) => piped.to_string(),
    }
}

/// stdin を読むか。「端末でなければ読む」にはしない — CI や親プロセスから継承した stdin は
/// 端末ではないのに閉じられないことがあり、引数でプロンプトを渡しているのに EOF を待って
/// 永久に固まる。読むのは、引数が無い (stdin がプロンプト本体) か、`--stdin` で明示されたときだけ。
fn wants_stdin(prompt_arg: &str, read_stdin_flag: bool) -> bool {
    read_stdin_flag || prompt_arg.trim().is_empty()
}

/// stdin を EOF まで読む。端末なら読まない (入力待ちで固まるため)。
fn read_stdin_to_end() -> Result<Option<String>> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Ok(None);
    }
    let mut buf = String::new();
    stdin
        .lock()
        .take(STDIN_LIMIT_BYTES + 1)
        .read_to_string(&mut buf)
        .context("reading prompt from stdin")?;
    if buf.len() as u64 > STDIN_LIMIT_BYTES {
        anyhow::bail!("stdin is larger than {STDIN_LIMIT_BYTES} bytes");
    }
    Ok(Some(buf))
}

/// このターンの最終応答。履歴の末尾が本文つきの assistant メッセージのときだけ返す
/// (失敗・中断したターンでは、末尾は tool 応答や中断の補填になっている)。
fn final_text(history: &[Message]) -> Option<String> {
    match history.last()? {
        Message::Assistant {
            content: Some(text),
            tool_calls,
        } if tool_calls.is_empty() => Some(text.clone()),
        _ => None,
    }
}

struct Report {
    exit_code: i32,
    result: Option<String>,
    error: Option<String>,
    session_id: Option<String>,
    usage: serde_json::Value,
}

impl Report {
    fn new(
        outcome: &Outcome,
        text: Option<String>,
        session_id: Option<String>,
        usage: &crate::agent::r#loop::SessionUsage,
    ) -> Self {
        let (exit_code, result, error) = match outcome {
            Outcome::Done => (EXIT_OK, text, None),
            Outcome::Interrupted => (EXIT_INTERRUPTED, None, Some("interrupted".to_string())),
            Outcome::Failed(e) => {
                let code = if e.downcast_ref::<MaxIterationsError>().is_some() {
                    EXIT_MAX_ITERATIONS
                } else {
                    EXIT_ERROR
                };
                (code, None, Some(format!("{e:#}")))
            }
        };
        Self {
            exit_code,
            result,
            error,
            session_id,
            usage: serde_json::json!({
                "llm_calls": usage.llm_calls,
                "estimated_calls": usage.estimated_calls,
                "prompt_tokens": usage.prompt_tokens,
                "completion_tokens": usage.completion_tokens,
                "total_tokens": usage.total_tokens,
            }),
        }
    }

    fn fields(&self) -> serde_json::Value {
        serde_json::json!({
            "is_error": self.exit_code != EXIT_OK,
            "exit_code": self.exit_code,
            "result": self.result,
            "error": self.error,
            "session_id": self.session_id,
            "usage": self.usage,
        })
    }

    fn emit(&self, format: OutputFormat) {
        if let Some(e) = &self.error {
            eprintln!("{}", crate::term::red_err(&format!("error: {e}")));
        }
        match format {
            OutputFormat::Text => {
                if let Some(text) = &self.result {
                    println!("{text}");
                }
            }
            OutputFormat::Json => {
                let mut obj = self.fields();
                obj["type"] = "result".into();
                println!("{obj}");
            }
            // sink が stdout へエコーする (cli::dispatch が init_with で設定済み)。
            OutputFormat::StreamJson => crate::runlog::record("result", self.fields()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_is_read_only_when_it_is_the_prompt_or_asked_for() {
        assert!(
            !wants_stdin("do it", false),
            "an inherited, never-closed stdin must not hang -p"
        );
        assert!(wants_stdin("do it", true));
        assert!(wants_stdin("", false));
        assert!(wants_stdin("  ", false));
    }

    #[test]
    fn prompt_is_argument_then_piped_data() {
        assert_eq!(
            assemble_prompt("summarize", Some("log line\n")),
            "summarize\n\nlog line"
        );
        assert_eq!(assemble_prompt("just this", None), "just this");
        assert_eq!(assemble_prompt("", Some("from stdin\n")), "from stdin");
        assert_eq!(assemble_prompt("  ", Some("  ")), "");
    }

    #[test]
    fn final_text_is_only_a_trailing_plain_assistant_message() {
        let done = [
            Message::User {
                content: "q".into(),
            },
            Message::Assistant {
                content: Some("answer".into()),
                tool_calls: Vec::new(),
            },
        ];
        assert_eq!(final_text(&done).as_deref(), Some("answer"));

        let mid_tool = [Message::Tool {
            tool_call_id: "1".into(),
            content: "out".into(),
        }];
        assert_eq!(final_text(&mid_tool), None);
        assert_eq!(final_text(&[]), None);
    }

    fn usage() -> crate::agent::r#loop::SessionUsage {
        crate::agent::r#loop::SessionUsage::default()
    }

    #[test]
    fn exit_codes_distinguish_max_iterations_from_other_failures() {
        let max = Outcome::Failed(MaxIterationsError(25).into());
        assert_eq!(
            Report::new(&max, None, None, &usage()).exit_code,
            EXIT_MAX_ITERATIONS
        );

        let other = Outcome::Failed(anyhow::anyhow!("LLM HTTP 500"));
        let report = Report::new(&other, Some("stale".into()), None, &usage());
        assert_eq!(report.exit_code, EXIT_ERROR);
        assert_eq!(
            report.result, None,
            "a failed turn has no result, even if text exists"
        );
        assert_eq!(report.fields()["is_error"], true);

        let cut = Report::new(&Outcome::Interrupted, None, None, &usage());
        assert_eq!(cut.exit_code, EXIT_INTERRUPTED);

        let ok = Report::new(
            &Outcome::Done,
            Some("hi".into()),
            Some("s1".into()),
            &usage(),
        );
        assert_eq!(ok.exit_code, EXIT_OK);
        assert_eq!(ok.fields()["result"], "hi");
        assert_eq!(ok.fields()["session_id"], "s1");
    }
}
