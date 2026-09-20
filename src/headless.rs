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
use crate::hooks::Lifecycle;
use crate::llm::metered::BudgetExceededError;
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
// 2 は clap が引数エラーに使う (`--output-format yaml` など)。呼び出し側が区別できるよう空けておく。
/// 最終応答に至らないまま `agent.max_iterations` を使い切った。
pub const EXIT_MAX_ITERATIONS: i32 = 3;
/// `max_requests` / `max_total_tokens` を使い切り、次の LLM リクエストを送らずに打ち切った。
pub const EXIT_BUDGET: i32 = 4;
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

/// 1 ターン実行し、終了コードを返す。ターンに入る前の失敗 (stdin の読み取り・API キー無し・
/// MCP 起動など) も、指定された形式の結果として stdout に出す — 呼び出し側は「json を
/// 頼んだのに stdout が空」を解釈できない。
pub async fn run(cfg: Config, opts: Options) -> i32 {
    crate::term::route_display_to_stderr();
    let format = opts.format;
    let report = match run_turn(cfg, opts).await {
        Ok(report) => report,
        Err(e) => Report::startup_failure(&e),
    };
    report.emit(format);
    report.exit_code
}

/// `run` に入る前 (設定の読み込みなど) で失敗したときの出口。形式は `run` と同じ。
pub fn report_startup_failure(format: OutputFormat, error: &anyhow::Error) -> i32 {
    crate::term::route_display_to_stderr();
    let report = Report::startup_failure(error);
    report.emit(format);
    report.exit_code
}

async fn run_turn(cfg: Config, opts: Options) -> Result<Report> {
    let Options {
        prompt: prompt_arg,
        read_stdin,
        format: _,
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
    let gate = PermissionGate::from_config(&cfg, &runtime.cwd, false)?;
    let (mut session, mut recorder) =
        runtime.open_session(&cfg, resume.as_deref(), Notices::Stderr);
    if cfg.permissions.mode == crate::config::PermissionMode::Plan {
        session.set_mode(crate::agent::Mode::Plan);
    }

    let session_source = if resume.is_some() {
        "resume"
    } else {
        "startup"
    };
    match session
        .fire_hook(
            Lifecycle::SessionStart,
            Some(session_source),
            serde_json::json!({ "source": session_source }),
        )
        .await
    {
        Ok(started) => {
            if let Some(reason) = &started.block {
                eprintln!("session-start hook: {}", crate::term::sanitize(reason));
            }
            // plain な stdout と `additionalContext` は、最初のユーザ入力と一緒にモデルへ渡す。
            started
                .context
                .into_iter()
                .for_each(|c| session.add_context(c));
        }
        Err(e) => eprintln!(
            "session-start hook: {}",
            crate::term::sanitize(&e.to_string())
        ),
    }

    // このターンで増えた分だけを最終応答の候補にする (`--resume` では履歴の末尾が
    // 前回の応答なので、何も足されなかったターンがそれを拾ってしまう)。
    let history_before = session.history().len();
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

    let _ = session
        .fire_hook(Lifecycle::SessionEnd, None, serde_json::json!({}))
        .await;

    Ok(Report::new(
        &outcome,
        final_text(added_this_turn(session.history(), history_before)),
        recorder.as_ref().map(|r| r.id().to_string()),
        session.usage(),
    )
    .with_ledger(&runtime.ledger))
}

/// ターン開始時点より後ろの履歴。自動圧縮で履歴が開始時点より短くなっていたら、圧縮は
/// 末尾を残すので全体を見てよい。
fn added_this_turn(history: &[Message], len_before: usize) -> &[Message] {
    history.get(len_before..).unwrap_or(history)
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

fn usage_json(usage: &crate::agent::r#loop::SessionUsage) -> serde_json::Value {
    serde_json::json!({
        "llm_calls": usage.llm_calls,
        "estimated_calls": usage.estimated_calls,
        "prompt_tokens": usage.prompt_tokens,
        "completion_tokens": usage.completion_tokens,
        "total_tokens": usage.total_tokens,
    })
}

/// text モードの最終応答。stdout がパイプなら**無加工** (呼び出し側との契約)。端末に直接出るなら
/// 人が読む画面なので、モデルの書いたエスケープ列で画面を書き換えさせない (#100)。
fn text_for_stdout(text: &str, stdout_is_terminal: bool) -> std::borrow::Cow<'_, str> {
    if stdout_is_terminal {
        crate::term::sanitize(text)
    } else {
        std::borrow::Cow::Borrowed(text)
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
            // UserPromptSubmit hook がプロンプトをブロックしたか、モデルが本文の無い応答で
            // ターンを終えた。成功扱いで空を返すと、呼び出し側は「空の回答」と区別できない。
            Outcome::Done if text.is_none() => (
                EXIT_ERROR,
                None,
                Some(
                    "the turn ended without a final answer (the prompt was blocked by a hook, or the model returned no text)"
                        .to_string(),
                ),
            ),
            Outcome::Done => (EXIT_OK, text, None),
            Outcome::Interrupted => (EXIT_INTERRUPTED, None, Some("interrupted".to_string())),
            Outcome::Failed(e) => {
                let code = if e.downcast_ref::<MaxIterationsError>().is_some() {
                    EXIT_MAX_ITERATIONS
                } else if e
                    .chain()
                    .any(|c| c.downcast_ref::<BudgetExceededError>().is_some())
                {
                    EXIT_BUDGET
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
            usage: usage_json(usage),
        }
    }

    /// usage をプロセス全体の台帳で置き換える。エージェントループの外の呼び出し (サブエージェント、
    /// `/goal` の評価器、圧縮、MCP sampling) も合計に入り、`by_kind` に内訳が載る。
    fn with_ledger(mut self, ledger: &crate::llm::metered::Ledger) -> Self {
        let total = ledger.total();
        let by_kind: serde_json::Map<String, serde_json::Value> = ledger
            .by_kind()
            .into_iter()
            .map(|(kind, u)| {
                let fields = serde_json::json!({
                    "llm_calls": u.calls,
                    "total_tokens": u.total_tokens,
                });
                (kind.to_string(), fields)
            })
            .collect();
        self.usage = serde_json::json!({
            "llm_calls": total.calls,
            "requests": ledger.requests(),
            "estimated_calls": total.estimated_calls,
            "prompt_tokens": total.prompt_tokens,
            "completion_tokens": total.completion_tokens,
            "total_tokens": total.total_tokens,
            "by_kind": by_kind,
            // `[pricing]` があるときだけ数値。単価の無いモデルの分は入っていない。
            "cost": ledger.cost().map(|(cost, _)| cost),
        });
        self
    }

    fn startup_failure(error: &anyhow::Error) -> Self {
        Self {
            exit_code: EXIT_ERROR,
            result: None,
            error: Some(format!("{error:#}")),
            session_id: None,
            // 形は成功時と揃える (呼び出し側が `usage.llm_calls` を無条件に読めるように)。
            usage: usage_json(&crate::agent::r#loop::SessionUsage::default()),
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
            // エラーにはプロバイダの応答本文が入り込む。stderr は人が読む側なので無害化する
            // (json / stream-json の `error` フィールドは無加工のまま)。
            eprintln!(
                "{}",
                crate::term::red_err(&format!("error: {}", crate::term::sanitize(e)))
            );
        }
        // 形式によらず runlog には残す (`--log-jsonl` だけを付けた text / json 実行でも
        // ファイルに結果が入る)。
        crate::runlog::record("result", self.fields());
        match format {
            OutputFormat::Text => {
                if let Some(text) = &self.result {
                    println!("{}", text_for_stdout(text, crate::term::is_terminal()));
                }
            }
            OutputFormat::Json => {
                let mut obj = self.fields();
                obj["type"] = "result".into();
                println!("{obj}");
            }
            // 上の record が stdout へのエコーを兼ねる (cli::dispatch が sink を設定済み)。
            OutputFormat::StreamJson => {}
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
    fn the_text_result_is_verbatim_for_pipes_and_defused_for_a_terminal() {
        let result = "ok\x1b[2Kforged";
        assert_eq!(text_for_stdout(result, false), result);
        assert_eq!(text_for_stdout(result, true), "ok\\u{1b}[2Kforged");
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

    #[test]
    fn a_turn_that_adds_nothing_does_not_inherit_the_previous_answer() {
        let resumed = [
            Message::User {
                content: "earlier".into(),
            },
            Message::Assistant {
                content: Some("earlier answer".into()),
                tool_calls: Vec::new(),
            },
        ];
        assert_eq!(final_text(added_this_turn(&resumed, resumed.len())), None);
        assert_eq!(
            final_text(added_this_turn(&resumed, 0)).as_deref(),
            Some("earlier answer")
        );
        // 自動圧縮で開始時点より短くなった履歴は全体を見る。
        assert_eq!(
            final_text(added_this_turn(&resumed, 10)).as_deref(),
            Some("earlier answer")
        );
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

        let silent = Report::new(&Outcome::Done, None, None, &usage());
        assert_eq!(
            silent.exit_code, EXIT_ERROR,
            "no answer is not a successful empty answer"
        );

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
