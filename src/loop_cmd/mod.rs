//! `/loop` — プロンプト/ユーザ定義コマンドを一定間隔で反復実行する
//! (Claude Code の /loop 移植、MVP = フォアグラウンド固定間隔ループ)。
//!
//! 同期 REPL の lodan には cron スケジューラが無いため、REPL を占有する
//! フォアグラウンド反復として実装する。中断は呼び出し側 (REPL) が
//! `drive` の future ごと Ctrl-C でキャンセルする (/goal と同じ機構)。
//! 暴走防止として反復回数・総経過時間のハード上限を必ず持つ。

use anyhow::{Result, anyhow};
use std::time::{Duration, Instant};

use crate::agent::Session;
use crate::llm::LlmClient;
use crate::permission::PermissionGate;

/// 既定の反復回数ハード上限。
pub const DEFAULT_MAX_ITERATIONS: u32 = 100;

/// 既定の総経過時間ハード上限。
pub const DEFAULT_MAX_DURATION: Duration = Duration::from_secs(24 * 60 * 60);

/// 1 回の `/loop` 実行の仕様。上限はフィールドで持ち、テストから上書きできる。
#[derive(Debug)]
pub struct LoopSpec {
    /// 反復の間隔 (各ターン完了から次の開始まで)。
    pub interval: Duration,
    /// 毎反復そのまま再投入するプロンプト (slash は呼び出し側で展開済み)。
    pub prompt: String,
    pub max_iterations: u32,
    pub max_duration: Duration,
}

impl LoopSpec {
    pub fn new(interval: Duration, prompt: &str) -> Result<Self> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err(anyhow!("loop prompt is empty"));
        }
        Ok(Self {
            interval,
            prompt: prompt.to_string(),
            max_iterations: DEFAULT_MAX_ITERATIONS,
            max_duration: DEFAULT_MAX_DURATION,
        })
    }
}

/// `drive` の終了理由。中断 (Ctrl-C) は呼び出し側が future 破棄で扱うため
/// ここには現れない。
#[derive(Debug)]
pub enum LoopOutcome {
    /// 反復回数ハード上限に到達。
    IterationLimit { iterations: u32 },
    /// 総経過時間ハード上限に到達。
    TimeLimit { iterations: u32 },
    /// ターン失敗。同じ失敗を叩き続けないため停止する。
    TurnFailed {
        iterations: u32,
        error: anyhow::Error,
    },
}

/// 固定間隔でターンを反復する。`after_turn` は各ターン後の永続化フック。
pub async fn drive(
    spec: &LoopSpec,
    session: &mut Session,
    llm: &dyn LlmClient,
    gate: &PermissionGate,
    mut after_turn: impl FnMut(&Session),
) -> LoopOutcome {
    let started = Instant::now();
    let mut iterations = 0u32;
    loop {
        println!(
            "{}",
            crate::term::dim(&format!(
                "[loop] iteration {}/{}",
                iterations + 1,
                spec.max_iterations
            ))
        );
        if let Err(e) = session.run_turn(&spec.prompt, llm, gate).await {
            return LoopOutcome::TurnFailed {
                iterations,
                error: e,
            };
        }
        iterations += 1;
        after_turn(session);

        if iterations >= spec.max_iterations {
            return LoopOutcome::IterationLimit { iterations };
        }
        // 次の反復開始が上限を越えるならここで止める (寝てから止めない)。
        if started.elapsed() + spec.interval >= spec.max_duration {
            return LoopOutcome::TimeLimit { iterations };
        }
        println!(
            "{}",
            crate::term::dim(&format!(
                "[loop] next in {} (Ctrl-C to stop)",
                humanize(spec.interval)
            ))
        );
        tokio::time::sleep(spec.interval).await;
    }
}

/// `[number][unit]` (unit ∈ s/m/h/d) を Duration へパースする。ゼロは拒否。
pub fn parse_interval(s: &str) -> Result<Duration> {
    let s = s.trim();
    let Some(unit) = s.chars().last() else {
        return Err(anyhow!("empty interval"));
    };
    let mult: u64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 60 * 60,
        'd' => 24 * 60 * 60,
        _ => {
            return Err(anyhow!(
                "invalid interval '{s}': expected [number][s|m|h|d] like 5m"
            ));
        }
    };
    let num = &s[..s.len() - 1]; // unit は上で ASCII 確定なので安全に落とせる
    let n: u64 = num
        .parse()
        .map_err(|_| anyhow!("invalid interval '{s}': expected [number][s|m|h|d] like 5m"))?;
    if n == 0 {
        return Err(anyhow!("interval must be > 0"));
    }
    n.checked_mul(mult)
        .map(Duration::from_secs)
        .ok_or_else(|| anyhow!("interval '{s}' overflows"))
}

/// 表示用の短い間隔表記 (パース済み値のみ渡される前提の粗い整形)。
fn humanize(d: Duration) -> String {
    let secs = d.as_secs();
    if secs.is_multiple_of(24 * 60 * 60) {
        format!("{}d", secs / (24 * 60 * 60))
    } else if secs.is_multiple_of(60 * 60) {
        format!("{}h", secs / (60 * 60))
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::messages::{Message, ToolSpec};
    use crate::config::Config;
    use crate::llm::{ChatEvent, ChatResponse};
    use crate::tools::registry::default_registry;
    use async_trait::async_trait;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    #[test]
    fn parse_interval_accepts_units() {
        assert_eq!(parse_interval("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_interval("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_interval("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_interval("1d").unwrap(), Duration::from_secs(86400));
        assert_eq!(parse_interval(" 10m ").unwrap(), Duration::from_secs(600));
    }

    #[test]
    fn parse_interval_rejects_invalid() {
        for bad in ["", "5", "m", "0m", "5x", "-5m", "５m", "1.5h", "5 m"] {
            assert!(parse_interval(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn loop_spec_rejects_empty_prompt() {
        assert!(LoopSpec::new(Duration::from_secs(60), "").is_err());
        assert!(LoopSpec::new(Duration::from_secs(60), "  ").is_err());
    }

    #[test]
    fn humanize_picks_coarsest_unit() {
        assert_eq!(humanize(Duration::from_secs(5)), "5s");
        assert_eq!(humanize(Duration::from_secs(300)), "5m");
        assert_eq!(humanize(Duration::from_secs(7200)), "2h");
        assert_eq!(humanize(Duration::from_secs(86400)), "1d");
        assert_eq!(humanize(Duration::from_secs(90)), "90s");
    }

    /// 毎ターン最終テキストを返すモック。fail_after 以降の呼び出しはエラー。
    struct LoopLlm {
        fail_after: Option<u32>,
        calls: std::sync::atomic::AtomicU32,
    }

    #[async_trait]
    impl LlmClient for LoopLlm {
        async fn chat(
            &self,
            _h: &[Message],
            _t: &[ToolSpec<'_>],
            _m: &str,
            _mt: Option<u32>,
        ) -> Result<ChatResponse> {
            Ok(ChatResponse {
                content: Some("ok".into()),
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
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(limit) = self.fail_after
                && n >= limit
            {
                return Err(anyhow!("mock llm failure"));
            }
            let _ = sink.send(ChatEvent::Done(ChatResponse {
                content: Some("ok".into()),
                tool_calls: vec![],
                usage: None,
            }));
            Ok(())
        }
    }

    fn test_session() -> Session {
        Session::new(Config::default(), Arc::new(default_registry()))
    }

    fn spec_with(interval: Duration, max_iterations: u32, max_duration: Duration) -> LoopSpec {
        let mut spec = LoopSpec::new(interval, "do the thing").unwrap();
        spec.max_iterations = max_iterations;
        spec.max_duration = max_duration;
        spec
    }

    /// 反復回数上限で必ず止まり、毎反復同じプロンプトが再投入される。
    #[tokio::test]
    async fn drive_stops_at_iteration_limit() {
        let llm = LoopLlm {
            fail_after: None,
            calls: 0.into(),
        };
        let mut session = test_session();
        let gate = PermissionGate::new(true);
        let spec = spec_with(Duration::from_millis(1), 3, Duration::from_secs(60));
        let mut persisted = 0;
        let out = drive(&spec, &mut session, &llm, &gate, |_| persisted += 1).await;
        match out {
            LoopOutcome::IterationLimit { iterations } => assert_eq!(iterations, 3),
            other => panic!("expected IterationLimit, got {other:?}"),
        }
        assert_eq!(persisted, 3);
        let prompts = session
            .history()
            .iter()
            .filter(|m| matches!(m, Message::User { content } if content == "do the thing"))
            .count();
        assert_eq!(prompts, 3, "same prompt re-injected each iteration");
    }

    /// 総経過時間上限 (0 なら 1 反復後に停止) でも必ず止まる。
    #[tokio::test]
    async fn drive_stops_at_time_limit() {
        let llm = LoopLlm {
            fail_after: None,
            calls: 0.into(),
        };
        let mut session = test_session();
        let gate = PermissionGate::new(true);
        let spec = spec_with(Duration::from_secs(60), 100, Duration::ZERO);
        let out = drive(&spec, &mut session, &llm, &gate, |_| {}).await;
        match out {
            LoopOutcome::TimeLimit { iterations } => assert_eq!(iterations, 1),
            other => panic!("expected TimeLimit, got {other:?}"),
        }
    }

    /// ターン失敗でループを止める (壊れたエンドポイントを叩き続けない)。
    #[tokio::test]
    async fn drive_stops_on_turn_failure() {
        let llm = LoopLlm {
            fail_after: Some(2),
            calls: 0.into(),
        };
        let mut session = test_session();
        let gate = PermissionGate::new(true);
        let spec = spec_with(Duration::from_millis(1), 100, Duration::from_secs(60));
        let out = drive(&spec, &mut session, &llm, &gate, |_| {}).await;
        match out {
            LoopOutcome::TurnFailed { iterations, .. } => assert_eq!(iterations, 2),
            other => panic!("expected TurnFailed, got {other:?}"),
        }
    }
}
