//! サブエージェント (`Task` ツール)。
//!
//! メインエージェントが調査タスクを子エージェントへ委譲する。子は読み取り専用の
//! ツール (Read / Grep / Glob) だけを持ち、headless にツールループを回して
//! 最終テキストを 1 つの要約として返す。破壊的操作を持たないため承認ゲート不要、
//! `Task` 自身を含めないため無限再帰しない。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::agent::messages::Message;
use crate::llm::LlmClient;
use crate::prompt;
use crate::tools::registry::ToolRegistry;
use crate::tools::{Tool, ToolCtx, ToolError, ToolOutput};

/// 子エージェントの反復上限。親 (`agent.max_iterations`) より小さく抑え、
/// 静かに走る子が LLM を回しすぎてコスト超過しないようにする。
const SUBAGENT_MAX_ITERATIONS: usize = 12;

#[derive(Debug, Deserialize)]
struct TaskArgs {
    description: String,
    prompt: String,
}

/// 子エージェントを起動する `Task` ツール。
pub struct SubAgentTool {
    llm: Arc<dyn LlmClient>,
    model: String,
    /// 子に許可するツール (読み取り専用)。
    tools: Arc<ToolRegistry>,
    cwd: PathBuf,
    max_iterations: usize,
}

impl SubAgentTool {
    pub fn new(
        llm: Arc<dyn LlmClient>,
        model: String,
        tools: Arc<ToolRegistry>,
        cwd: PathBuf,
        max_iterations: usize,
    ) -> Self {
        Self {
            llm,
            model,
            tools,
            cwd,
            // 親の上限と子専用上限の小さい方を採る。
            max_iterations: max_iterations.min(SUBAGENT_MAX_ITERATIONS),
        }
    }

    async fn run(&self, task: &str) -> Result<String, ToolError> {
        let system = prompt::build_system_prompt(&self.cwd, &self.model, self.tools.as_ref());
        let user = format!(
            "You are a read-only investigation sub-agent. Use the available tools to \
             complete the task, then return a concise summary as your final message \
             with no tool call.\n\nTask: {task}"
        );
        let mut history = vec![
            Message::System { content: system },
            Message::User { content: user },
        ];
        let ctx = ToolCtx::new(self.cwd.clone());

        for _ in 0..self.max_iterations {
            let specs = self.tools.tool_specs();
            let resp = self
                .llm
                .chat(&history, &specs, &self.model)
                .await
                .map_err(|e| ToolError::Other(format!("sub-agent llm error: {e}")))?;

            let tool_calls = resp.tool_calls.clone();
            history.push(Message::Assistant {
                content: resp.content.clone(),
                tool_calls: tool_calls.clone(),
            });

            if tool_calls.is_empty() {
                return Ok(resp.content.unwrap_or_default());
            }

            for call in tool_calls {
                let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
                    .unwrap_or_else(|_| serde_json::json!({ "raw": call.function.arguments }));
                let output = match self.tools.get(&call.function.name) {
                    // 読み取り専用 registry にしか無いので未知名はまず出ないが、保険。
                    None => ToolOutput::error(format!("unknown tool: {}", call.function.name)),
                    Some(tool) => match tool.execute(args, &ctx).await {
                        Ok(o) => o,
                        Err(e) => ToolOutput::error(format!("tool error: {e}")),
                    },
                };
                history.push(Message::Tool {
                    tool_call_id: call.id,
                    content: output.content,
                });
            }
        }

        Err(ToolError::Other(format!(
            "sub-agent hit max_iterations ({}) without a final answer",
            self.max_iterations
        )))
    }
}

#[async_trait]
impl Tool for SubAgentTool {
    fn name(&self) -> &str {
        "Task"
    }

    fn description(&self) -> &str {
        "Spawn a read-only sub-agent to investigate a question across the codebase. \
         The sub-agent has Read / Grep / Glob and returns a single concise summary. \
         Use it to offload focused searches; it cannot modify files or run commands."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "A short (3-5 word) description of the task"
                },
                "prompt": {
                    "type": "string",
                    "description": "The investigation task. Be specific and self-contained; \
                                    the sub-agent does not see this conversation."
                }
            },
            "required": ["description", "prompt"]
        })
    }

    fn is_destructive(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let args: TaskArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs(format!("Task: {e}")))?;
        // 子は静かに走るので、起動を 1 行知らせて可視性を確保する。
        println!("  ↳ Task: {}", args.description);
        let summary = self.run(&args.prompt).await?;
        Ok(ToolOutput::ok(summary))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::Result;
    use tokio::sync::mpsc;

    use crate::agent::messages::{ToolCall, ToolCallFunction, ToolSpec};
    use crate::llm::{ChatEvent, ChatResponse};
    use crate::tools::registry::read_only_registry;

    // スクリプト化した応答を順に返す擬似 LLM。
    struct ScriptedLlm {
        steps: Vec<ChatResponse>,
        idx: AtomicUsize,
    }

    impl ScriptedLlm {
        fn new(steps: Vec<ChatResponse>) -> Self {
            Self {
                steps,
                idx: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl LlmClient for ScriptedLlm {
        async fn chat(
            &self,
            _history: &[Message],
            _tools: &[ToolSpec<'_>],
            _model: &str,
        ) -> Result<ChatResponse> {
            let i = self.idx.fetch_add(1, Ordering::SeqCst);
            Ok(self.steps.get(i).cloned().unwrap_or(ChatResponse {
                content: Some("(no more script)".into()),
                tool_calls: vec![],
            }))
        }

        async fn chat_stream(
            &self,
            _history: &[Message],
            _tools: &[ToolSpec<'_>],
            _model: &str,
            _sink: mpsc::UnboundedSender<ChatEvent>,
        ) -> Result<()> {
            Ok(())
        }
    }

    fn tool_call(name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: name.into(),
                arguments: args.into(),
            },
        }
    }

    fn subagent(steps: Vec<ChatResponse>, cwd: PathBuf) -> SubAgentTool {
        SubAgentTool::new(
            Arc::new(ScriptedLlm::new(steps)),
            "mock".into(),
            Arc::new(read_only_registry()),
            cwd,
            8,
        )
    }

    #[tokio::test]
    async fn returns_final_text_without_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = subagent(
            vec![ChatResponse {
                content: Some("the answer is 42".into()),
                tool_calls: vec![],
            }],
            tmp.path().to_path_buf(),
        );
        let out = sub.run("what is the answer").await.unwrap();
        assert_eq!(out, "the answer is 42");
    }

    #[tokio::test]
    async fn executes_tool_then_summarizes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "needle here").unwrap();
        let steps = vec![
            ChatResponse {
                content: None,
                tool_calls: vec![tool_call(
                    "Grep",
                    &serde_json::json!({ "pattern": "needle", "path": tmp.path() }).to_string(),
                )],
            },
            ChatResponse {
                content: Some("found the needle".into()),
                tool_calls: vec![],
            },
        ];
        let sub = subagent(steps, tmp.path().to_path_buf());
        let out = sub.run("find the needle").await.unwrap();
        assert_eq!(out, "found the needle");
    }

    #[tokio::test]
    async fn max_iterations_guard_errors() {
        let tmp = tempfile::tempdir().unwrap();
        // 常に tool_call を返し続ける → 上限で打ち切り。
        let looping = (0..20)
            .map(|_| ChatResponse {
                content: None,
                tool_calls: vec![tool_call("Grep", r#"{"pattern":"x","path":"."}"#)],
            })
            .collect();
        let sub = subagent(looping, tmp.path().to_path_buf());
        let err = sub.run("loop forever").await.unwrap_err();
        assert!(format!("{err}").contains("max_iterations"));
    }

    #[test]
    fn read_only_registry_excludes_task_and_destructive() {
        let r = read_only_registry();
        let names = r.names();
        assert!(names.contains(&"Read"));
        assert!(names.contains(&"Grep"));
        assert!(names.contains(&"Glob"));
        // 破壊的ツールと Task 自身は含めない (無限再帰・無確認破壊の防止)。
        assert!(!names.contains(&"Write"));
        assert!(!names.contains(&"Edit"));
        assert!(!names.contains(&"Bash"));
        assert!(!names.contains(&"Task"));
    }

    #[test]
    fn read_only_registry_has_no_destructive_tools() {
        // 名前ベースの allowlist に頼らず、子に渡る全ツールが非破壊であることを
        // 本質で固定する。将来 default_registry に破壊的ツールが増えても、
        // 誤って read_only_registry に混入すればここで落ちる。
        let r = read_only_registry();
        for name in r.names() {
            let tool = r.get(name).expect("registered");
            assert!(
                !tool.is_destructive(),
                "sub-agent tool {name} must be non-destructive"
            );
        }
    }
}
