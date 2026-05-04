// MVP 外: Hooks (PreToolUse, PostToolUse, SessionStart 等)。
// 換装のみ。agent loop からの呼び出しはコメントアウト。

pub mod runner;

use async_trait::async_trait;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
}

#[async_trait]
pub trait Hook: Send + Sync {
    fn name(&self) -> &str;
    async fn run(&self, lc: Lifecycle, payload: &serde_json::Value)
        -> anyhow::Result<HookOutcome>;
}

#[derive(Debug, Clone)]
pub enum HookOutcome {
    Continue,
    Block(String),
    Replace(serde_json::Value),
}
