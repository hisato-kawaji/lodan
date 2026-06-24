// Hooks: ライフサイクルイベント (UserPromptSubmit / PreToolUse / PostToolUse 等) で
// 外部コマンドを発火し、exit code でエージェントループを制御する。
// Claude Code の hook モデルに準拠した最小実装。

pub mod runner;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lifecycle {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
}

/// `config.toml` の `[[hooks]]` 1 エントリ。
///
/// ```toml
/// [[hooks]]
/// event = "PreToolUse"
/// matcher = "Bash"            # 省略可: ツール名一致 (PreToolUse / PostToolUse のみ)。空 / "*" は全て
/// command = "./scripts/guard.sh"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    pub event: Lifecycle,
    #[serde(default)]
    pub matcher: String,
    pub command: String,
}

impl HookConfig {
    /// `tool_name` がこの hook の matcher に一致するか。
    /// matcher 未指定 (空) または `"*"` は常に一致。`tool_name` が None の
    /// イベント (UserPromptSubmit 等) では matcher を無視して一致扱い。
    pub fn matches(&self, tool_name: Option<&str>) -> bool {
        match tool_name {
            None => true,
            Some(name) => {
                self.matcher.is_empty() || self.matcher == "*" || self.matcher == name
            }
        }
    }
}

#[async_trait]
pub trait Hook: Send + Sync {
    fn name(&self) -> &str;
    async fn run(&self, lc: Lifecycle, payload: &serde_json::Value)
    -> anyhow::Result<HookOutcome>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// 全 hook が exit 0。処理を継続する。
    Continue,
    /// いずれかの hook が非 0 で終了。理由付きでブロックする。
    Block(String),
}
