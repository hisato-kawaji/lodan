// MVP 外: サブエージェント spawn。型と入口だけ用意し、呼び出し側はコメントアウト。

use anyhow::Result;

#[derive(Debug, Clone)]
pub struct SubAgentTask {
    pub description: String,
    pub prompt: String,
}

#[derive(Debug, Clone)]
pub struct SubAgentResult {
    pub output: String,
}

pub async fn spawn(_task: &SubAgentTask) -> Result<SubAgentResult> {
    unimplemented!("sub-agent spawning is out of MVP scope")
}
