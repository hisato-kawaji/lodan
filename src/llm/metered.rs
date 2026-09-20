//! LLM 呼び出しの計上と予算 (#84)。
//!
//! 計上を呼び出し側 (エージェントループ) に置くと、ループを通らない呼び出し — サブエージェント、
//! `/goal` の評価器、コンテキスト圧縮、MCP sampling — が漏れる。実際に漏れていた。ここでは
//! `LlmClient` を包んで、**どこから呼ばれても**同じ台帳に載るようにする。
//!
//! 何のための呼び出しかは、呼び出し側が [`with_kind`] で付ける (付けなければ [`KIND_MAIN`])。
//! 予算の判定も同じ場所で行う: リクエストを送る前に確かめるので、止まるのは必ず「次の LLM
//! 呼び出しの直前」で、履歴は API に投げられる形のまま残る。

use super::{ChatEvent, ChatResponse, LlmClient, Usage};
use crate::agent::messages::{Message, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub const KIND_MAIN: &str = "main";
pub const KIND_SUBAGENT: &str = "subagent";
pub const KIND_GOAL_EVAL: &str = "goal_eval";
pub const KIND_COMPACT: &str = "compact";
pub const KIND_MCP_SAMPLING: &str = "mcp_sampling";

/// 予算の何割を使ったら、モデルに一度だけ知らせるか。
const REMINDER_PERCENT: u64 = 80;

tokio::task_local! {
    static KIND: &'static str;
    /// いま実行中の呼び出しが載っている台帳。クライアントの内側の再試行が、送り直しを
    /// 同じ台帳に申告するためのもの。
    static LEDGER: Arc<Ledger>;
}

/// 再試行で**もう 1 件送る前**に呼ぶ。送り直しもプロバイダの側では 1 リクエストなので、
/// 台帳に数え、予算が残っていなければ `false` (= 再試行しない)。計上の外で使われている
/// クライアントでは常に `true`。
pub fn admit_retry() -> bool {
    LEDGER
        .try_with(|ledger| match ledger.admit() {
            Ok(()) => true,
            Err(refused) => {
                // 呼び出しは「一時的な失敗」として返ってくる。本当の理由が予算だったことを、
                // `MeteredClient` がエラーに付け直せるように控えておく。
                ledger.state().retry_refused = Some(refused);
                false
            }
        })
        .unwrap_or(true)
}

/// `fut` の中から行われる LLM 呼び出しを `kind` として計上する。
pub async fn with_kind<F: std::future::Future>(kind: &'static str, fut: F) -> F::Output {
    KIND.scope(kind, fut).await
}

fn current_kind() -> &'static str {
    KIND.try_with(|k| *k).unwrap_or(KIND_MAIN)
}

/// `[agent] max_requests` / `max_total_tokens`。None は無制限。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Budget {
    pub max_requests: Option<u64>,
    pub max_total_tokens: Option<u64>,
}

/// `[pricing."<model>"]`。100 万トークンあたりの単価 (通貨は書いた人の単位のまま)。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelPrice {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

/// 予算を使い切ったので、次のリクエストを送らなかった。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("budget exhausted: {used} of {limit} {what} used; not sending another LLM request")]
pub struct BudgetExceededError {
    pub what: &'static str,
    pub used: u64,
    pub limit: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KindUsage {
    pub calls: u64,
    /// サーバが usage を返さず、文字数からの概算で数えた呼び出し。
    pub estimated_calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

impl KindUsage {
    fn add(&mut self, usage: Usage, estimated: bool) {
        self.calls += 1;
        self.estimated_calls += u64::from(estimated);
        self.prompt_tokens += usage.prompt_tokens;
        self.completion_tokens += usage.completion_tokens;
        self.total_tokens += usage.total_tokens;
    }

    fn plus(mut self, other: &KindUsage) -> Self {
        self.calls += other.calls;
        self.estimated_calls += other.estimated_calls;
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
        self
    }
}

#[derive(Debug, Default)]
struct LedgerState {
    /// 送ったリクエストの数。失敗したものも含む (無料枠はリクエスト数で数えられる)。
    requests: u64,
    by_kind: BTreeMap<&'static str, KindUsage>,
    /// モデル名ごとの使用量 (単価はモデルごとに違う。fallback や `/goal` の評価器は別モデル)。
    by_model: BTreeMap<String, KindUsage>,
    reminded: bool,
    /// 予算が無くて見送った再試行 (あれば)。
    retry_refused: Option<BudgetExceededError>,
}

/// プロセス全体の LLM 使用量の台帳。
#[derive(Debug, Default)]
pub struct Ledger {
    budget: Budget,
    pricing: BTreeMap<String, ModelPrice>,
    state: Mutex<LedgerState>,
}

impl Ledger {
    pub fn new(budget: Budget) -> Self {
        Self {
            budget,
            pricing: BTreeMap::new(),
            state: Mutex::default(),
        }
    }

    /// `/cost` に金額を出すための単価表。
    pub fn with_pricing(mut self, pricing: BTreeMap<String, ModelPrice>) -> Self {
        self.pricing = pricing;
        self
    }

    /// 単価の分かっているモデルの使用量から見積もった金額。単価表が空なら None。
    /// 第 2 要素は、使ったのに単価が無くて金額に入っていないモデル。
    pub fn cost(&self) -> Option<(f64, Vec<String>)> {
        if self.pricing.is_empty() {
            return None;
        }
        let state = self.state();
        let mut total = 0.0;
        let mut unpriced = Vec::new();
        for (model, usage) in &state.by_model {
            match self.pricing.get(model) {
                Some(price) => {
                    total += usage.prompt_tokens as f64 / 1e6 * price.input_per_mtok
                        + usage.completion_tokens as f64 / 1e6 * price.output_per_mtok;
                }
                None => unpriced.push(model.clone()),
            }
        }
        Some((total, unpriced))
    }

    pub fn budget(&self) -> Budget {
        self.budget
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LedgerState> {
        // 計上の途中で panic したスレッドがあっても、数字そのものは壊れていない。
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 次のリクエストを送ってよいか確かめ、よければ 1 件として数える。
    fn admit(&self) -> std::result::Result<(), BudgetExceededError> {
        let mut state = self.state();
        if let Some(limit) = self.budget.max_requests
            && state.requests >= limit
        {
            return Err(BudgetExceededError {
                what: "requests",
                used: state.requests,
                limit,
            });
        }
        let tokens = total(&state.by_kind).total_tokens;
        if let Some(limit) = self.budget.max_total_tokens
            && tokens >= limit
        {
            return Err(BudgetExceededError {
                what: "tokens",
                used: tokens,
                limit,
            });
        }
        state.requests += 1;
        Ok(())
    }

    fn record(&self, kind: &'static str, model: &str, usage: Usage, estimated: bool) {
        let mut state = self.state();
        state.by_kind.entry(kind).or_default().add(usage, estimated);
        state
            .by_model
            .entry(model.to_string())
            .or_default()
            .add(usage, estimated);
    }

    pub fn requests(&self) -> u64 {
        self.state().requests
    }

    pub fn total(&self) -> KindUsage {
        total(&self.state().by_kind)
    }

    pub fn by_kind(&self) -> Vec<(&'static str, KindUsage)> {
        self.state().by_kind.iter().map(|(k, v)| (*k, *v)).collect()
    }

    /// 予算の 8 割を超えた最初の 1 回だけ、モデルに渡す注意書きを返す。
    pub fn take_reminder(&self) -> Option<String> {
        let mut state = self.state();
        if state.reminded {
            return None;
        }
        let tokens = total(&state.by_kind).total_tokens;
        // もう 1 件も送れないなら、知らせても畳む機会が無い (履歴に宙に浮いた注意書きが残るだけ)。
        let spent = |used: u64, limit: Option<u64>| limit.is_some_and(|l| used >= l);
        if spent(state.requests, self.budget.max_requests)
            || spent(tokens, self.budget.max_total_tokens)
        {
            return None;
        }
        // u64 のまま掛けると、巨大な上限 (`--max-requests 18446744073709551615`) であふれる。
        let near = |used: u64, limit: Option<u64>| {
            limit.is_some_and(|l| {
                l > 0 && u128::from(used) * 100 >= u128::from(l) * u128::from(REMINDER_PERCENT)
            })
        };
        let mut parts = Vec::new();
        if near(state.requests, self.budget.max_requests) {
            parts.push(format!(
                "{} of {} LLM requests",
                state.requests,
                self.budget.max_requests.unwrap_or_default()
            ));
        }
        if near(tokens, self.budget.max_total_tokens) {
            parts.push(format!(
                "{tokens} of {} tokens",
                self.budget.max_total_tokens.unwrap_or_default()
            ));
        }
        if parts.is_empty() {
            return None;
        }
        state.reminded = true;
        Some(format!(
            "[budget] This run has used {}. It will be cut off when the budget runs out. Wrap up: \
             finish the most important remaining step and report what is done and what is not.",
            parts.join(" and ")
        ))
    }

    /// `/cost` 用の表示。
    pub fn describe(&self) -> String {
        let state = self.state();
        let all = total(&state.by_kind);
        let limit = |used: u64, limit: Option<u64>, what: &str| {
            limit.map(|l| format!("\nbudget: {used} / {l} {what}"))
        };
        if state.requests == 0 {
            // まだ何も送っていなくても、設定した予算は見せる。
            let mut out = "no LLM calls yet".to_string();
            out.extend(limit(0, self.budget.max_requests, "requests"));
            out.extend(limit(0, self.budget.max_total_tokens, "tokens"));
            return out;
        }
        let mut out = format!(
            "tokens: {} total (prompt {} + completion {}) across {} LLM request(s)",
            all.total_tokens, all.prompt_tokens, all.completion_tokens, state.requests
        );
        if state.by_kind.len() > 1 || !state.by_kind.contains_key(KIND_MAIN) {
            for (kind, u) in &state.by_kind {
                out.push_str(&format!(
                    "\n  {kind}: {} tokens in {} call(s)",
                    u.total_tokens, u.calls
                ));
            }
        }
        let failed = state.requests.saturating_sub(all.calls);
        if failed > 0 {
            out.push_str(&format!(
                "\nnote: {failed} request(s) returned no response (errors count against max_requests)"
            ));
        }
        if all.estimated_calls > 0 {
            out.push_str(&format!(
                "\nnote: {} call(s) lacked server usage; counted via ~{} chars/token estimate",
                all.estimated_calls,
                crate::agent::r#loop::ESTIMATE_CHARS_PER_TOKEN
            ));
        }
        drop(state);
        if let Some((cost, unpriced)) = self.cost() {
            out.push_str(&format!("\ncost: ~{cost:.4} (from [pricing])"));
            if !unpriced.is_empty() {
                out.push_str(&format!("; no price for {}", unpriced.join(", ")));
            }
        }
        let state = self.state();
        out.extend(limit(state.requests, self.budget.max_requests, "requests"));
        out.extend(limit(
            all.total_tokens,
            self.budget.max_total_tokens,
            "tokens",
        ));
        out
    }
}

fn total(by_kind: &BTreeMap<&'static str, KindUsage>) -> KindUsage {
    by_kind
        .values()
        .fold(KindUsage::default(), |acc, u| acc.plus(u))
}

/// 内側のクライアントの呼び出しを台帳に載せ、予算を超えていたら送らない。
pub struct MeteredClient {
    inner: Arc<dyn LlmClient>,
    ledger: Arc<Ledger>,
}

impl MeteredClient {
    pub fn new(inner: Arc<dyn LlmClient>, ledger: Arc<Ledger>) -> Self {
        Self { inner, ledger }
    }

    /// 再試行を予算で見送った末の失敗なら、予算切れとして返す (`-p` の終了コードが 4 になる)。
    /// 元の失敗の内容は文脈として残す。
    fn explain(&self, error: anyhow::Error) -> anyhow::Error {
        match self.ledger.state().retry_refused.take() {
            Some(refused) => anyhow::Error::new(refused).context(format!(
                "the request failed and the budget left no room to retry it: {error:#}"
            )),
            None => error,
        }
    }

    fn record(&self, model: &str, history: &[Message], resp: &ChatResponse) {
        let (usage, estimated) = match resp.usage {
            Some(u) if u.prompt_tokens + u.completion_tokens + u.total_tokens > 0 => {
                (u.normalized(), false)
            }
            _ => (crate::agent::r#loop::estimate_usage(history, resp), true),
        };
        self.ledger.record(current_kind(), model, usage, estimated);
    }
}

#[async_trait]
impl LlmClient for MeteredClient {
    async fn chat(
        &self,
        history: &[Message],
        tools: &[ToolSpec<'_>],
        model: &str,
        max_tokens: Option<u32>,
    ) -> Result<ChatResponse> {
        self.ledger.admit()?;
        let resp = LEDGER
            .scope(
                self.ledger.clone(),
                self.inner.chat(history, tools, model, max_tokens),
            )
            .await
            .map_err(|e| self.explain(e))?;
        self.record(model, history, &resp);
        Ok(resp)
    }

    async fn chat_stream(
        &self,
        history: &[Message],
        tools: &[ToolSpec<'_>],
        model: &str,
        sink: mpsc::UnboundedSender<ChatEvent>,
    ) -> Result<()> {
        self.ledger.admit()?;
        // 最終イベントの usage を見るために、イベントを中継する。
        let (tx, mut rx) = mpsc::unbounded_channel();
        let forward = async {
            while let Some(event) = rx.recv().await {
                if let ChatEvent::Done(resp) = &event {
                    self.record(model, history, resp);
                }
                // 受け手が先にいなくなっても (Ctrl-C)、内側の送信は最後まで吸い出す。
                let _ = sink.send(event);
            }
        };
        let inner = LEDGER.scope(
            self.ledger.clone(),
            self.inner.chat_stream(history, tools, model, tx),
        );
        let (result, ()) = tokio::join!(inner, forward);
        result.map_err(|e| self.explain(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 決まった usage を返すだけのクライアント。`fail` なら応答せずにエラー。
    struct Fixed {
        usage: Option<Usage>,
        fail: bool,
    }

    fn resp(usage: Option<Usage>) -> ChatResponse {
        ChatResponse {
            content: Some("ok".into()),
            tool_calls: vec![],
            usage,
        }
    }

    #[async_trait]
    impl LlmClient for Fixed {
        async fn chat(
            &self,
            _h: &[Message],
            _t: &[ToolSpec<'_>],
            _m: &str,
            _mt: Option<u32>,
        ) -> Result<ChatResponse> {
            if self.fail {
                anyhow::bail!("boom");
            }
            Ok(resp(self.usage))
        }

        async fn chat_stream(
            &self,
            _h: &[Message],
            _t: &[ToolSpec<'_>],
            _m: &str,
            sink: mpsc::UnboundedSender<ChatEvent>,
        ) -> Result<()> {
            if self.fail {
                anyhow::bail!("boom");
            }
            let _ = sink.send(ChatEvent::TextDelta("o".into()));
            let _ = sink.send(ChatEvent::Done(resp(self.usage)));
            Ok(())
        }
    }

    fn usage(prompt: u64, completion: u64) -> Option<Usage> {
        Some(Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
        })
    }

    fn metered(usage: Option<Usage>, budget: Budget) -> (MeteredClient, Arc<Ledger>) {
        let ledger = Arc::new(Ledger::new(budget));
        let client = MeteredClient::new(Arc::new(Fixed { usage, fail: false }), ledger.clone());
        (client, ledger)
    }

    #[tokio::test]
    async fn calls_are_counted_by_kind_wherever_they_come_from() {
        let (client, ledger) = metered(usage(100, 10), Budget::default());
        client.chat(&[], &[], "m", None).await.unwrap();
        with_kind(KIND_SUBAGENT, client.chat(&[], &[], "m", None))
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        with_kind(KIND_GOAL_EVAL, client.chat_stream(&[], &[], "m", tx))
            .await
            .unwrap();
        // ストリームは中継しても、受け手に同じ順で届く。
        assert!(matches!(rx.recv().await, Some(ChatEvent::TextDelta(_))));
        assert!(matches!(rx.recv().await, Some(ChatEvent::Done(_))));

        let kinds: Vec<&str> = ledger.by_kind().iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, [KIND_GOAL_EVAL, KIND_MAIN, KIND_SUBAGENT]);
        assert_eq!(ledger.total().total_tokens, 330);
        assert_eq!(ledger.requests(), 3);
        let shown = ledger.describe();
        assert!(
            shown.contains("330 total") && shown.contains("subagent: 110 tokens"),
            "{shown}"
        );
    }

    #[tokio::test]
    async fn cost_is_estimated_per_model_from_the_pricing_table() {
        let pricing = BTreeMap::from([(
            "big".to_string(),
            ModelPrice {
                input_per_mtok: 2.0,
                output_per_mtok: 10.0,
            },
        )]);
        let ledger = Arc::new(Ledger::new(Budget::default()).with_pricing(pricing));
        let client = MeteredClient::new(
            Arc::new(Fixed {
                usage: usage(1_000_000, 100_000),
                fail: false,
            }),
            ledger.clone(),
        );
        client.chat(&[], &[], "big", None).await.unwrap();
        let (cost, unpriced) = ledger.cost().unwrap();
        assert!((cost - 3.0).abs() < 1e-9, "2.0 in + 1.0 out = {cost}");
        assert!(unpriced.is_empty());

        // 単価の無いモデル (fallback など) は金額に入れず、入っていないことを言う。
        client.chat(&[], &[], "small-local", None).await.unwrap();
        let (cost, unpriced) = ledger.cost().unwrap();
        assert!((cost - 3.0).abs() < 1e-9);
        assert_eq!(unpriced, ["small-local"]);
        let shown = ledger.describe();
        assert!(
            shown.contains("cost: ~3.0000") && shown.contains("no price for small-local"),
            "{shown}"
        );

        // 単価表が無ければ、金額の行そのものを出さない。
        let (plain, plain_ledger) = metered(usage(10, 10), Budget::default());
        plain.chat(&[], &[], "big", None).await.unwrap();
        assert!(plain_ledger.cost().is_none() && !plain_ledger.describe().contains("cost:"));
    }

    #[tokio::test]
    async fn a_missing_usage_is_estimated_and_flagged() {
        let (client, ledger) = metered(None, Budget::default());
        let history = [Message::User {
            content: "123456".into(),
        }];
        client.chat(&history, &[], "m", None).await.unwrap();
        let total = ledger.total();
        assert_eq!(total.estimated_calls, 1);
        assert!(
            total.prompt_tokens >= 2 && total.completion_tokens >= 1,
            "{total:?}"
        );
        assert!(ledger.describe().contains("lacked server usage"));
    }

    #[tokio::test]
    async fn the_request_budget_stops_the_next_request_and_counts_failures() {
        let ledger = Arc::new(Ledger::new(Budget {
            max_requests: Some(2),
            max_total_tokens: None,
        }));
        let failing = MeteredClient::new(
            Arc::new(Fixed {
                usage: None,
                fail: true,
            }),
            ledger.clone(),
        );
        // 失敗したリクエストも、プロバイダの側では 1 件として数えられている。
        assert!(failing.chat(&[], &[], "m", None).await.is_err());
        let ok = MeteredClient::new(
            Arc::new(Fixed {
                usage: usage(5, 5),
                fail: false,
            }),
            ledger.clone(),
        );
        ok.chat(&[], &[], "m", None).await.unwrap();

        let err = ok.chat(&[], &[], "m", None).await.unwrap_err();
        assert_eq!(
            err.downcast_ref::<BudgetExceededError>(),
            Some(&BudgetExceededError {
                what: "requests",
                used: 2,
                limit: 2
            })
        );
        assert_eq!(ledger.requests(), 2, "the refused request was never sent");
        let (tx, _rx) = mpsc::unbounded_channel();
        assert!(ok.chat_stream(&[], &[], "m", tx).await.is_err());
    }

    #[tokio::test]
    async fn the_token_budget_is_checked_before_each_request() {
        let (client, ledger) = metered(
            usage(60, 0),
            Budget {
                max_requests: None,
                max_total_tokens: Some(100),
            },
        );
        client.chat(&[], &[], "m", None).await.unwrap();
        // 60 < 100 なので 2 回目は送る。上限は「次を送らない」線で、超過ぶんは 1 回の呼び出しまで。
        client.chat(&[], &[], "m", None).await.unwrap();
        let err = client.chat(&[], &[], "m", None).await.unwrap_err();
        assert!(err.to_string().contains("120 of 100 tokens"), "{err}");
        assert_eq!(ledger.requests(), 2);
    }

    #[tokio::test]
    async fn the_model_is_reminded_once_when_the_budget_is_nearly_gone() {
        let (client, ledger) = metered(
            usage(10, 0),
            Budget {
                max_requests: Some(5),
                max_total_tokens: None,
            },
        );
        for _ in 0..3 {
            client.chat(&[], &[], "m", None).await.unwrap();
            assert_eq!(ledger.take_reminder(), None, "3 of 5 is under 80%");
        }
        client.chat(&[], &[], "m", None).await.unwrap();
        let reminder = ledger.take_reminder().expect("4 of 5 is 80%");
        assert!(reminder.contains("4 of 5 LLM requests"), "{reminder}");
        assert_eq!(ledger.take_reminder(), None, "only once");

        // 使い切ってからでは遅い: 次のリクエストは送られないので、知らせない。
        let (client, ledger) = metered(
            usage(10, 0),
            Budget {
                max_requests: Some(1),
                max_total_tokens: None,
            },
        );
        client.chat(&[], &[], "m", None).await.unwrap();
        assert_eq!(ledger.take_reminder(), None);

        let unlimited = Ledger::new(Budget::default());
        assert_eq!(unlimited.take_reminder(), None);

        // 巨大な上限でも掛け算があふれない。
        let huge = Ledger::new(Budget {
            max_requests: Some(u64::MAX),
            max_total_tokens: Some(u64::MAX),
        });
        huge.admit().unwrap();
        assert_eq!(huge.take_reminder(), None);
    }
}
