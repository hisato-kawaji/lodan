//! `/goal` — 達成条件を満たすまでターンを自律継続する (Claude Code の /goal 移植)。
//!
//! ターン完了ごとに評価器 LLM (active モデル流用) が「条件＋直近トランスクリプト」を
//! 判定し、未達なら理由を次ターンの入力として注入して継続する。暴走防止として
//! ターン数・経過時間のハード上限を必ず持つ。評価器の出力がパース不能なときは
//! 安全側に倒して停止する (根拠のない自律継続をしない)。

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

use crate::agent::Session;
use crate::agent::messages::Message;
use crate::llm::LlmClient;
use crate::permission::PermissionGate;

/// 条件文の最大長 (Claude Code と同じ 4,000 字)。
pub const MAX_CONDITION_CHARS: usize = 4_000;

/// 既定のターン数ハード上限。
pub const DEFAULT_MAX_TURNS: u32 = 20;

/// 既定の経過時間ハード上限。
pub const DEFAULT_MAX_DURATION: Duration = Duration::from_secs(30 * 60);

/// 評価器へ渡すトランスクリプト末尾の最大文字数。
const TRANSCRIPT_MAX_CHARS: usize = 12_000;

/// アクティブな goal の状態。上限はフィールドで持ち、テストから上書きできる。
///
/// ターン数と経過時間の上限は**走らせるたびの枠**: `/goal <条件>` と `/goal resume` のたびに
/// 新しい枠が始まる (上限で止まった goal を再開できなければ、再開の意味が無い)。通算は別に持つ。
#[derive(Debug)]
pub struct Goal {
    pub condition: String,
    /// 今回の枠で使ったターン数。
    pub turns_used: u32,
    /// 今回の枠の開始時刻。走っている間だけ Some — 一時停止中の goal は時計を進めない
    /// (開いたまま放置した時間や `/goal` で眺めた時間を、作業時間に数えない)。
    pub started_at: Option<Instant>,
    /// 直近に終わった枠の長さ (一時停止中の `/goal` の表示用)。
    pub last_window: Duration,
    pub max_turns: u32,
    pub max_duration: Duration,
    /// これまでの全ての枠の合計ターン数。
    pub total_turns: u32,
    /// 以前の枠で経過した時間 (今回の枠は含まない)。
    pub prior_elapsed: Duration,
}

/// セッションと一緒に保存する goal の状態 (`goal.json`)。`--resume` で paused として戻る。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalRecord {
    pub condition: String,
    pub total_turns: u32,
    pub elapsed_secs: u64,
}

impl Goal {
    /// 条件を検証して goal を作る。空・長すぎる条件はエラー。
    pub fn new(condition: &str) -> Result<Self> {
        let condition = condition.trim();
        if condition.is_empty() {
            return Err(anyhow!("goal condition is empty"));
        }
        if condition.chars().count() > MAX_CONDITION_CHARS {
            return Err(anyhow!(
                "goal condition too long ({} chars > {MAX_CONDITION_CHARS})",
                condition.chars().count()
            ));
        }
        Ok(Self {
            condition: condition.to_string(),
            turns_used: 0,
            started_at: None,
            last_window: Duration::ZERO,
            max_turns: DEFAULT_MAX_TURNS,
            max_duration: DEFAULT_MAX_DURATION,
            total_turns: 0,
            prior_elapsed: Duration::ZERO,
        })
    }

    /// 保存された状態から、paused の goal として戻す。
    pub fn from_record(record: GoalRecord) -> Result<Self> {
        let mut goal = Self::new(&record.condition)?;
        goal.total_turns = record.total_turns;
        goal.prior_elapsed = Duration::from_secs(record.elapsed_secs);
        Ok(goal)
    }

    pub fn to_record(&self) -> GoalRecord {
        GoalRecord {
            condition: self.condition.clone(),
            total_turns: self.total_turns,
            elapsed_secs: self.total_elapsed().as_secs(),
        }
    }

    /// 今回の枠で経過した時間。一時停止中は、直近の枠の長さ。
    pub fn window_elapsed(&self) -> Duration {
        self.started_at.map_or(self.last_window, |at| at.elapsed())
    }

    /// 全ての枠を通した、**走っていた**時間。
    pub fn total_elapsed(&self) -> Duration {
        self.prior_elapsed + self.started_at.map_or(Duration::ZERO, |at| at.elapsed())
    }

    /// 新しい枠を始める (`/goal <条件>` の直後と `/goal resume`)。通算は引き継ぐ。
    pub fn begin_new_window(&mut self) {
        self.pause();
        self.started_at = Some(Instant::now());
        self.turns_used = 0;
    }

    /// 時計を止める。走っていた分を通算に畳む。止まっていれば何もしない。
    pub fn pause(&mut self) {
        if let Some(at) = self.started_at.take() {
            self.last_window = at.elapsed();
            self.prior_elapsed += self.last_window;
        }
    }

    /// `/goal` (引数なし) の状態表示用文字列。
    pub fn describe(&self) -> String {
        format!(
            "condition: {}\nturns: {}/{}, elapsed: {}s (limit {}s)\nin total: {} turn(s), {}s",
            self.condition,
            self.turns_used,
            self.max_turns,
            self.window_elapsed().as_secs(),
            self.max_duration.as_secs(),
            self.total_turns,
            self.total_elapsed().as_secs(),
        )
    }
}

/// 評価器の判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub met: bool,
    pub reason: String,
}

/// `drive` の終了理由。
#[derive(Debug)]
pub enum GoalOutcome {
    /// 評価器が達成を確認した。
    Achieved { reason: String, turns: u32 },
    /// ターン数ハード上限に到達 (goal は paused として保持される)。
    TurnLimit,
    /// 経過時間ハード上限に到達 (goal は paused として保持される)。
    TimeLimit,
    /// 評価器の呼び出し失敗・出力パース不能。安全側で停止する。
    EvaluatorFailed(anyhow::Error),
    /// エージェントターン自体の失敗。
    TurnFailed(anyhow::Error),
}

/// 達成条件を満たすまでターンを回す。呼び出し側 (REPL) は outcome に応じて
/// goal の保持/解除とメッセージ表示を行う。`after_turn` は各ターン後の
/// 永続化フック (transcript 追記など)。
pub async fn drive(
    goal: &mut Goal,
    session: &mut Session,
    llm: &dyn LlmClient,
    model: &str,
    gate: &PermissionGate,
    after_turn: impl FnMut(&Session, &Goal),
) -> GoalOutcome {
    let evaluator = Evaluator { llm, model };
    drive_with(goal, session, llm, evaluator, gate, after_turn).await
}

/// [`drive`] の本体。評価器を作業側と別に渡せる。走っている間だけ goal の時計を進める。
pub async fn drive_with(
    goal: &mut Goal,
    session: &mut Session,
    llm: &dyn LlmClient,
    evaluator: Evaluator<'_>,
    gate: &PermissionGate,
    after_turn: impl FnMut(&Session, &Goal),
) -> GoalOutcome {
    if goal.started_at.is_none() {
        goal.begin_new_window();
    }
    let outcome = run_window(goal, session, llm, evaluator, gate, after_turn).await;
    goal.pause();
    outcome
}

/// 達成を判定する側。作業するモデルと同じでもよいが、別のモデルにすれば「自分の仕事を自分で
/// 合格にする」偏りを避けられる (`[goal] evaluator_provider`)。
#[derive(Clone, Copy)]
pub struct Evaluator<'a> {
    pub llm: &'a dyn LlmClient,
    pub model: &'a str,
}

async fn run_window(
    goal: &mut Goal,
    session: &mut Session,
    llm: &dyn LlmClient,
    evaluator: Evaluator<'_>,
    gate: &PermissionGate,
    mut after_turn: impl FnMut(&Session, &Goal),
) -> GoalOutcome {
    // 再開 (通算ターンがある) なら、やり直しではなく続きであることを伝える。
    let mut input = if goal.total_turns == 0 {
        format!(
            "Work toward this goal. When you believe it is met, say so and stop.\nGoal: {}",
            goal.condition
        )
    } else {
        format!(
            "Resume working toward this goal (you already spent {} turn(s) on it; build on what \
             is done instead of starting over). When you believe it is met, say so and stop.\n\
             Goal: {}",
            goal.total_turns, goal.condition
        )
    };
    loop {
        if goal.turns_used >= goal.max_turns {
            return GoalOutcome::TurnLimit;
        }
        if goal.window_elapsed() >= goal.max_duration {
            return GoalOutcome::TimeLimit;
        }

        if let Err(e) = session.run_turn(&input, llm, gate).await {
            return GoalOutcome::TurnFailed(e);
        }
        goal.turns_used += 1;
        goal.total_turns += 1;
        after_turn(session, goal);

        match evaluate(
            evaluator.llm,
            evaluator.model,
            &goal.condition,
            session.history(),
        )
        .await
        {
            Err(e) => return GoalOutcome::EvaluatorFailed(e),
            Ok(v) if v.met => {
                return GoalOutcome::Achieved {
                    reason: v.reason,
                    turns: goal.total_turns,
                };
            }
            Ok(v) => {
                println!(
                    "{}",
                    crate::term::dim(&format!(
                        // 評価器 (LLM) の自由記述。
                        "[goal] turn {}/{}: not met — {}",
                        goal.turns_used,
                        goal.max_turns,
                        crate::term::sanitize(&v.reason)
                    ))
                );
                input = format!(
                    "The goal is not met yet. Evaluator feedback: {}\n\
                     Continue working toward the goal: {}",
                    v.reason, goal.condition
                );
            }
        }
    }
}

/// 評価器 LLM を 1 回呼び、条件が満たされたか判定する。
/// トランスクリプトのみを見せ、ツールは渡さない (Claude Code と同じ)。
pub async fn evaluate(
    llm: &dyn LlmClient,
    model: &str,
    condition: &str,
    history: &[Message],
) -> Result<Verdict> {
    // System プロンプト (ツール一覧等) は判定に不要なので除く。
    let body: Vec<Message> = history
        .iter()
        .filter(|m| !matches!(m, Message::System { .. }))
        .cloned()
        .collect();
    let transcript = tail_chars(
        &crate::agent::render_for_summary(&body),
        TRANSCRIPT_MAX_CHARS,
    );

    let sys = Message::System {
        content: "You are a strict goal evaluator for a coding agent. Judge ONLY from the \
                  transcript whether the goal condition is met. Respond with a single JSON \
                  object and nothing else: {\"met\": true|false, \"reason\": \"short \
                  explanation\"}. If unsure, answer met=false with what is missing."
            .to_string(),
    };
    let usr = Message::User {
        content: format!(
            "Goal condition:\n{condition}\n\nConversation transcript (most recent \
             last):\n---\n{transcript}\n---\n\nIs the goal condition met?"
        ),
    };
    let resp = crate::llm::metered::with_kind(
        crate::llm::metered::KIND_GOAL_EVAL,
        llm.chat(&[sys, usr], &[], model, Some(256)),
    )
    .await?;
    let text = resp.content.unwrap_or_default();
    parse_verdict(&text).ok_or_else(|| {
        anyhow!(
            "evaluator returned unparsable verdict: {}",
            tail_chars(&text, 200)
        )
    })
}

/// 評価器出力から `{"met": bool, "reason": string}` を取り出す。ローカル LLM は
/// 前置き・コードフェンス付きで返しがちなので、全文パース → 最初の `{` から
/// 最後の `}` までの再パース、の順で試す。
pub fn parse_verdict(text: &str) -> Option<Verdict> {
    #[derive(Deserialize)]
    struct Raw {
        met: bool,
        #[serde(default)]
        reason: String,
    }
    let attempt = |s: &str| -> Option<Verdict> {
        serde_json::from_str::<Raw>(s.trim()).ok().map(|r| Verdict {
            met: r.met,
            reason: r.reason,
        })
    };
    if let Some(v) = attempt(text) {
        return Some(v);
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if start < end {
        return attempt(&text[start..=end]);
    }
    None
}

/// 末尾 `max` 文字 (文字境界) を返す。トランスクリプトは新しい方が重要。
fn tail_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        s.chars().skip(count - max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::messages::ToolSpec;
    use crate::config::Config;
    use crate::llm::{ChatEvent, ChatResponse};
    use crate::tools::registry::default_registry;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    #[test]
    fn parse_verdict_accepts_plain_json() {
        let v = parse_verdict(r#"{"met": true, "reason": "tests pass"}"#).unwrap();
        assert!(v.met);
        assert_eq!(v.reason, "tests pass");
    }

    #[test]
    fn parse_verdict_accepts_wrapped_json() {
        let text = "Sure! Here is my judgement:\n```json\n{\"met\": false, \"reason\": \"no test run yet\"}\n```\nDone.";
        let v = parse_verdict(text).unwrap();
        assert!(!v.met);
        assert_eq!(v.reason, "no test run yet");
    }

    #[test]
    fn parse_verdict_defaults_missing_reason() {
        let v = parse_verdict(r#"{"met": true}"#).unwrap();
        assert!(v.met);
        assert!(v.reason.is_empty());
    }

    #[test]
    fn parse_verdict_rejects_garbage() {
        assert!(parse_verdict("yes, the goal is met").is_none());
        assert!(parse_verdict("").is_none());
        assert!(parse_verdict("{not json}").is_none());
    }

    #[test]
    fn goal_new_validates_condition() {
        assert!(Goal::new("").is_err());
        assert!(Goal::new("   ").is_err());
        let long: String = "あ".repeat(MAX_CONDITION_CHARS + 1);
        assert!(Goal::new(&long).is_err());
        let ok = Goal::new("cargo test exits 0").unwrap();
        assert_eq!(ok.turns_used, 0);
        assert_eq!(ok.max_turns, DEFAULT_MAX_TURNS);
    }

    #[test]
    fn tail_chars_keeps_recent_end() {
        assert_eq!(tail_chars("abcdef", 3), "def");
        assert_eq!(tail_chars("abc", 10), "abc");
        // 多バイト文字でも境界を壊さない。
        assert_eq!(tail_chars("あいうえお", 2), "えお");
    }

    /// ターンは chat_stream、評価は chat と経路が分かれることを利用したモック。
    /// 評価器は `verdicts` を先頭から順に返す (尽きたら最後を繰り返す)。
    struct GoalLlm {
        verdicts: Vec<&'static str>,
        eval_calls: AtomicUsize,
    }

    #[async_trait]
    impl LlmClient for GoalLlm {
        async fn chat(
            &self,
            _h: &[Message],
            _t: &[ToolSpec<'_>],
            _m: &str,
            _mt: Option<u32>,
        ) -> Result<ChatResponse> {
            let i = self.eval_calls.fetch_add(1, Ordering::SeqCst);
            let v = self.verdicts[i.min(self.verdicts.len() - 1)];
            Ok(ChatResponse {
                content: Some(v.to_string()),
                tool_calls: vec![],
                usage: None,
                reasoning: None,
            })
        }

        async fn chat_stream(
            &self,
            _h: &[Message],
            _t: &[ToolSpec<'_>],
            _m: &str,
            sink: mpsc::UnboundedSender<ChatEvent>,
        ) -> Result<()> {
            let _ = sink.send(ChatEvent::Done(ChatResponse {
                content: Some("working on it".to_string()),
                tool_calls: vec![],
                usage: None,
                reasoning: None,
            }));
            Ok(())
        }
    }

    fn test_session() -> Session {
        Session::new(Config::default(), Arc::new(default_registry()))
    }

    /// 上限で止まった goal を再開すると、新しい枠で続きから走る (通算は引き継ぐ)。保存形式を
    /// 経由しても同じ。
    #[tokio::test]
    async fn a_goal_stopped_by_its_limit_resumes_with_a_fresh_window() {
        let llm = GoalLlm {
            verdicts: vec![
                r#"{"met": false, "reason": "not yet"}"#,
                r#"{"met": false, "reason": "not yet"}"#,
                r#"{"met": true, "reason": "done"}"#,
            ],
            eval_calls: AtomicUsize::new(0),
        };
        let mut session = test_session();
        let gate = PermissionGate::new(true);
        let mut goal = Goal::new("ship it").unwrap();
        goal.max_turns = 2;
        let out = drive(&mut goal, &mut session, &llm, "m", &gate, |_, _| {}).await;
        assert!(matches!(out, GoalOutcome::TurnLimit), "{out:?}");

        // セッションに保存され、別のプロセスで読み戻された、という経路。
        let record = goal.to_record();
        assert_eq!(
            (record.total_turns, record.condition.as_str()),
            (2, "ship it")
        );
        let json = serde_json::to_string(&record).unwrap();
        let mut goal = Goal::from_record(serde_json::from_str(&json).unwrap()).unwrap();
        goal.max_turns = 2;
        assert_eq!((goal.turns_used, goal.total_turns), (0, 2));

        // `/goal resume` は新しい枠を始める (読み戻した直後でも、同じプロセスで止まった後でも同じ)。
        goal.begin_new_window();
        let out = drive(&mut goal, &mut session, &llm, "m", &gate, |_, _| {}).await;
        match out {
            GoalOutcome::Achieved { turns, .. } => assert_eq!(turns, 3, "turns are cumulative"),
            other => panic!("{other:?}"),
        }
        // 再開のターンでは「続きから」と伝えている。
        let resumed_prompt = session
            .history()
            .iter()
            .filter_map(|m| match m {
                Message::User { content } => Some(content.as_str()),
                _ => None,
            })
            .find(|c| c.starts_with("Resume working toward this goal"))
            .expect("the resume prompt");
        assert!(
            resumed_prompt.contains("already spent 2 turn(s)"),
            "{resumed_prompt}"
        );
    }

    /// 一時停止中の goal は時計を進めない。`/goal` で眺めただけで経過時間が増えたり、開いたまま
    /// 放置した時間が作業時間に数えられたりしない。
    #[tokio::test]
    async fn a_paused_goal_does_not_age() {
        let mut goal = Goal::from_record(GoalRecord {
            condition: "ship it".into(),
            total_turns: 3,
            elapsed_secs: 90,
        })
        .unwrap();
        assert!(goal.started_at.is_none(), "restored as paused");
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(goal.total_elapsed(), Duration::from_secs(90));
        assert_eq!(goal.to_record().elapsed_secs, 90);

        // 走らせると進み、止めるとまた止まる。
        goal.begin_new_window();
        tokio::time::sleep(Duration::from_millis(30)).await;
        goal.pause();
        let after_run = goal.total_elapsed();
        assert!(
            after_run > Duration::from_secs(90)
                && goal.window_elapsed() >= Duration::from_millis(30)
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(goal.total_elapsed(), after_run);
    }

    /// `drive` を抜けたら、どの終わり方でも時計は止まっている。
    #[tokio::test]
    async fn the_clock_stops_when_the_run_ends() {
        let llm = GoalLlm {
            verdicts: vec![r#"{"met": false, "reason": "no"}"#],
            eval_calls: AtomicUsize::new(0),
        };
        let mut session = test_session();
        let mut goal = Goal::new("x").unwrap();
        goal.max_turns = 1;
        let out = drive(
            &mut goal,
            &mut session,
            &llm,
            "m",
            &PermissionGate::new(true),
            |_, g| {
                assert!(
                    g.started_at.is_some(),
                    "running while turns are being taken"
                );
            },
        )
        .await;
        assert!(matches!(out, GoalOutcome::TurnLimit));
        assert!(goal.started_at.is_none());
    }

    /// 評価器を別のクライアントにすると、判定はそちらに尋ねる (作業側には尋ねない)。
    #[tokio::test]
    async fn a_separate_evaluator_is_the_one_that_judges() {
        let worker = GoalLlm {
            verdicts: vec![r#"{"met": true, "reason": "I say I am done"}"#],
            eval_calls: AtomicUsize::new(0),
        };
        let judge = GoalLlm {
            verdicts: vec![
                r#"{"met": false, "reason": "no tests were run"}"#,
                r#"{"met": true, "reason": "tests pass"}"#,
            ],
            eval_calls: AtomicUsize::new(0),
        };
        let mut session = test_session();
        let gate = PermissionGate::new(true);
        let mut goal = Goal::new("run the tests").unwrap();
        let evaluator = Evaluator {
            llm: &judge,
            model: "judge-model",
        };
        let out = drive_with(
            &mut goal,
            &mut session,
            &worker,
            evaluator,
            &gate,
            |_, _| {},
        )
        .await;
        match out {
            GoalOutcome::Achieved { turns, reason } => {
                assert_eq!((turns, reason.as_str()), (2, "tests pass"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(worker.eval_calls.load(Ordering::SeqCst), 0);
        assert_eq!(judge.eval_calls.load(Ordering::SeqCst), 2);
    }

    /// 未達 → 達成の順で評価されると、2 ターンで Achieved になる。
    #[tokio::test]
    async fn drive_continues_until_achieved() {
        let llm = GoalLlm {
            verdicts: vec![
                r#"{"met": false, "reason": "tests not run"}"#,
                r#"{"met": true, "reason": "tests pass"}"#,
            ],
            eval_calls: AtomicUsize::new(0),
        };
        let mut session = test_session();
        let gate = PermissionGate::new(true);
        let mut goal = Goal::new("run the tests").unwrap();
        let mut persisted = 0;
        let out = drive(&mut goal, &mut session, &llm, "m", &gate, |_, _| {
            persisted += 1;
        })
        .await;
        match out {
            GoalOutcome::Achieved { turns, reason } => {
                assert_eq!(turns, 2);
                assert_eq!(reason, "tests pass");
            }
            other => panic!("expected Achieved, got {other:?}"),
        }
        assert_eq!(persisted, 2, "after_turn fires once per turn");
        // 未達時の評価器 reason が次ターンの入力として履歴に入る。
        assert!(
            session.history().iter().any(
                |m| matches!(m, Message::User { content } if content.contains("tests not run"))
            )
        );
    }

    /// 評価器が常に未達でも max_turns で必ず止まる。
    #[tokio::test]
    async fn drive_stops_at_turn_limit() {
        let llm = GoalLlm {
            verdicts: vec![r#"{"met": false, "reason": "keep going"}"#],
            eval_calls: AtomicUsize::new(0),
        };
        let mut session = test_session();
        let gate = PermissionGate::new(true);
        let mut goal = Goal::new("never satisfied").unwrap();
        goal.max_turns = 3;
        let out = drive(&mut goal, &mut session, &llm, "m", &gate, |_, _| {}).await;
        assert!(matches!(out, GoalOutcome::TurnLimit), "got {out:?}");
        assert_eq!(goal.turns_used, 3);
    }

    /// 経過時間上限でも止まる (max_duration = 0 は即時到達)。
    #[tokio::test]
    async fn drive_stops_at_time_limit() {
        let llm = GoalLlm {
            verdicts: vec![r#"{"met": false, "reason": "keep going"}"#],
            eval_calls: AtomicUsize::new(0),
        };
        let mut session = test_session();
        let gate = PermissionGate::new(true);
        let mut goal = Goal::new("whatever").unwrap();
        goal.max_duration = Duration::ZERO;
        let out = drive(&mut goal, &mut session, &llm, "m", &gate, |_, _| {}).await;
        assert!(matches!(out, GoalOutcome::TimeLimit), "got {out:?}");
        assert_eq!(goal.turns_used, 0, "no turn should run past the deadline");
    }

    /// 評価器出力がパース不能なら安全側で停止する。
    #[tokio::test]
    async fn drive_stops_when_evaluator_unparsable() {
        let llm = GoalLlm {
            verdicts: vec!["I think it looks good!"],
            eval_calls: AtomicUsize::new(0),
        };
        let mut session = test_session();
        let gate = PermissionGate::new(true);
        let mut goal = Goal::new("anything").unwrap();
        let out = drive(&mut goal, &mut session, &llm, "m", &gate, |_, _| {}).await;
        assert!(
            matches!(out, GoalOutcome::EvaluatorFailed(_)),
            "got {out:?}"
        );
        assert_eq!(goal.turns_used, 1);
    }
}
