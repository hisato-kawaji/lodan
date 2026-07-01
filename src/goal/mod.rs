//! `/goal` — 達成条件を満たすまでターンを自律継続する (Claude Code の /goal 移植)。
//!
//! ターン完了ごとに評価器 LLM (active モデル流用) が「条件＋直近トランスクリプト」を
//! 判定し、未達なら理由を次ターンの入力として注入して継続する。暴走防止として
//! ターン数・経過時間のハード上限を必ず持つ。評価器の出力がパース不能なときは
//! 安全側に倒して停止する (根拠のない自律継続をしない)。

use anyhow::{Result, anyhow};
use serde::Deserialize;
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
#[derive(Debug)]
pub struct Goal {
    pub condition: String,
    pub turns_used: u32,
    pub started_at: Instant,
    pub max_turns: u32,
    pub max_duration: Duration,
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
            started_at: Instant::now(),
            max_turns: DEFAULT_MAX_TURNS,
            max_duration: DEFAULT_MAX_DURATION,
        })
    }

    /// `/goal` (引数なし) の状態表示用文字列。
    pub fn describe(&self) -> String {
        format!(
            "condition: {}\nturns: {}/{}, elapsed: {}s (limit {}s)",
            self.condition,
            self.turns_used,
            self.max_turns,
            self.started_at.elapsed().as_secs(),
            self.max_duration.as_secs(),
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
    mut after_turn: impl FnMut(&Session),
) -> GoalOutcome {
    let mut input = format!(
        "Work toward this goal. When you believe it is met, say so and stop.\nGoal: {}",
        goal.condition
    );
    loop {
        if goal.turns_used >= goal.max_turns {
            return GoalOutcome::TurnLimit;
        }
        if goal.started_at.elapsed() >= goal.max_duration {
            return GoalOutcome::TimeLimit;
        }

        if let Err(e) = session.run_turn(&input, llm, gate).await {
            return GoalOutcome::TurnFailed(e);
        }
        goal.turns_used += 1;
        after_turn(session);

        match evaluate(llm, model, &goal.condition, session.history()).await {
            Err(e) => return GoalOutcome::EvaluatorFailed(e),
            Ok(v) if v.met => {
                return GoalOutcome::Achieved {
                    reason: v.reason,
                    turns: goal.turns_used,
                };
            }
            Ok(v) => {
                println!(
                    "{}",
                    crate::term::dim(&format!(
                        "[goal] turn {}/{}: not met — {}",
                        goal.turns_used, goal.max_turns, v.reason
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
    let resp = llm.chat(&[sys, usr], &[], model, Some(256)).await?;
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
            }));
            Ok(())
        }
    }

    fn test_session() -> Session {
        Session::new(Config::default(), Arc::new(default_registry()))
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
        let out = drive(&mut goal, &mut session, &llm, "m", &gate, |_| {
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
        let out = drive(&mut goal, &mut session, &llm, "m", &gate, |_| {}).await;
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
        let out = drive(&mut goal, &mut session, &llm, "m", &gate, |_| {}).await;
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
        let out = drive(&mut goal, &mut session, &llm, "m", &gate, |_| {}).await;
        assert!(
            matches!(out, GoalOutcome::EvaluatorFailed(_)),
            "got {out:?}"
        );
        assert_eq!(goal.turns_used, 1);
    }
}
