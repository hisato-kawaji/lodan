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
use crate::permission_rules::{RuleSet, Verdict};
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
    /// `.lodan/agents/<name>.md` で定義した子の種類 (#77)。省略は既定の調査エージェント。
    #[serde(default)]
    subagent_type: Option<String>,
}

/// 子エージェント 1 種類ぶんの設定。既定の `general-purpose` も、定義ファイル由来もこの形。
pub struct AgentProfile {
    pub llm: Arc<dyn LlmClient>,
    pub model: String,
    /// 子に許可するツール (読み取り専用)。
    pub tools: Arc<ToolRegistry>,
    pub max_iterations: usize,
    /// 定義ファイルの本文。子の system prompt の末尾に足す。
    pub instructions: String,
    pub description: String,
}

impl AgentProfile {
    /// 反復上限は親の上限と子専用上限の小さい方。
    pub fn new(
        llm: Arc<dyn LlmClient>,
        model: String,
        tools: Arc<ToolRegistry>,
        max_iterations: usize,
    ) -> Self {
        Self {
            llm,
            model,
            tools,
            max_iterations: max_iterations.min(SUBAGENT_MAX_ITERATIONS),
            instructions: String::new(),
            description: String::new(),
        }
    }

    pub fn with_instructions(mut self, instructions: String, description: String) -> Self {
        self.instructions = instructions;
        self.description = description;
        self
    }
}

/// 子エージェントを起動する `Task` ツール。
pub struct SubAgentTool {
    /// 種類ごとの設定。`DEFAULT_AGENT` は必ずある。
    profiles: std::collections::BTreeMap<String, AgentProfile>,
    /// モデルに見せる説明。定義した種類の一覧を含むので、種類を足すたびに作り直す。
    description: String,
    cwd: PathBuf,
    /// 親と同じ permission ルール。子は親の承認ゲートを通らずにツールを実行するので、
    /// ここで見ないと `deny = ["Read(**/.env)"]` を「Task に読ませる」だけですり抜けられる。
    rules: Arc<RuleSet>,
    /// SubagentStart / SubagentStop で発火させる hook (親と同じもの)。
    hooks: Vec<crate::hooks::HookConfig>,
    hooks_compat: crate::hooks::HooksCompat,
    /// 子のツール往復でも思考過程を送り返すか (`[llm.<provider>] reasoning_roundtrip`)。
    reasoning_roundtrip: bool,
}

use crate::agent::agents::DEFAULT_AGENT;

/// 説明の 1 行目 (Task の説明に載せる用)。
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

impl SubAgentTool {
    pub fn new(
        llm: Arc<dyn LlmClient>,
        model: String,
        tools: Arc<ToolRegistry>,
        cwd: PathBuf,
        max_iterations: usize,
    ) -> Self {
        let mut profiles = std::collections::BTreeMap::new();
        profiles.insert(
            DEFAULT_AGENT.to_string(),
            AgentProfile::new(llm, model, tools, max_iterations),
        );
        let mut tool = Self {
            profiles,
            description: String::new(),
            cwd,
            rules: Arc::new(RuleSet::default()),
            hooks: Vec::new(),
            hooks_compat: crate::hooks::HooksCompat::default(),
            reasoning_roundtrip: true,
        };
        tool.refresh_description();
        tool
    }

    /// 定義ファイル由来の種類を足す (#77)。
    pub fn with_profile(mut self, name: String, profile: AgentProfile) -> Self {
        self.profiles.insert(name, profile);
        self.refresh_description();
        self
    }

    /// 定義した種類の名前 (既定を除く)。
    pub fn custom_names(&self) -> Vec<&str> {
        self.profiles
            .keys()
            .filter(|n| n.as_str() != DEFAULT_AGENT)
            .map(String::as_str)
            .collect()
    }

    fn refresh_description(&mut self) {
        let mut text = String::from(
            "Spawn a read-only sub-agent to investigate a question across the codebase. \
             The sub-agent has Read / Grep / Glob and returns a single concise summary. \
             Use it to offload focused searches; it cannot modify files or run commands.",
        );
        let custom = self.custom_names();
        if !custom.is_empty() {
            text.push_str("\nsubagent_type (optional): ");
            text.push_str(DEFAULT_AGENT);
            text.push_str(" (default)");
            for name in custom {
                let p = &self.profiles[name];
                text.push_str(&format!("; {name} — {}", first_line(&p.description)));
            }
        }
        self.description = text;
    }

    fn profile(&self, name: Option<&str>) -> Result<(&str, &AgentProfile), ToolError> {
        let name = name.unwrap_or(DEFAULT_AGENT);
        self.profiles
            .get_key_value(name)
            .map(|(k, v)| (k.as_str(), v))
            .ok_or_else(|| {
                ToolError::InvalidArgs(format!(
                    "Task: unknown subagent_type '{}' (available: {})",
                    crate::term::sanitize(name),
                    self.profiles.keys().cloned().collect::<Vec<_>>().join(", ")
                ))
            })
    }

    pub fn with_reasoning_roundtrip(mut self, enabled: bool) -> Self {
        self.reasoning_roundtrip = enabled;
        self
    }

    /// 親の hook を子の開始・終了でも発火させる。
    pub fn with_hooks(
        mut self,
        hooks: Vec<crate::hooks::HookConfig>,
        compat: crate::hooks::HooksCompat,
    ) -> Self {
        self.hooks = hooks;
        self.hooks_compat = compat;
        self
    }

    /// 通知用。子は既に走る (または走り終えた) ので、hook の結果では止めない。
    async fn notify(
        &self,
        agent_type: &str,
        lc: crate::hooks::Lifecycle,
        extra: serde_json::Value,
    ) {
        let mut payload = serde_json::json!({
            "hook_event_name": lc,
            "cwd": self.cwd,
            "agent_type": agent_type,
        });
        if let (Some(base), serde_json::Value::Object(extra)) = (payload.as_object_mut(), extra) {
            base.extend(extra);
        }
        if let Err(e) = crate::hooks::runner::dispatch(
            lc,
            Some(agent_type),
            &payload,
            &self.hooks,
            self.hooks_compat,
        )
        .await
        {
            tracing::warn!("{lc:?} hook failed: {e:#}");
        }
    }

    async fn run(&self, agent_type: &str, task: &str) -> Result<String, ToolError> {
        use crate::hooks::Lifecycle;
        self.notify(
            agent_type,
            Lifecycle::SubagentStart,
            serde_json::json!({ "prompt": task }),
        )
        .await;
        let result = self.run_inner(agent_type, task).await;
        let last = match &result {
            Ok(text) => serde_json::json!({ "last_assistant_message": text }),
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        };
        self.notify(agent_type, Lifecycle::SubagentStop, last).await;
        result
    }

    /// 親の permission ルールを子にも適用する。
    pub fn with_rules(mut self, rules: Arc<RuleSet>) -> Self {
        self.rules = rules;
        self
    }

    /// 子の中でのツール呼び出しを通すか。子は静かに走り、尋ねる相手がいないので、
    /// deny は拒否、ask も拒否 (尋ねられない)、それ以外は read-only なので通す。
    fn refusal(&self, tool: &str, args: &serde_json::Value) -> Option<String> {
        match self.rules.evaluate(tool, args, &self.cwd)? {
            Verdict::Deny(rule) => Some(format!(
                "denied by permission rule `{rule}`. Do not retry this call or work around it."
            )),
            Verdict::Unverifiable(rule) => Some(crate::permission::unverifiable_message(&rule)),
            Verdict::Ask => Some(
                "this call needs the user's approval, which a sub-agent cannot ask for. \
                 Report that it is needed instead of retrying."
                    .to_string(),
            ),
            Verdict::Allow => None,
        }
    }

    async fn run_inner(&self, agent_type: &str, task: &str) -> Result<String, ToolError> {
        let (_, p) = self.profile(Some(agent_type))?;
        let mut system = prompt::build_system_prompt(&self.cwd, &p.model, p.tools.as_ref());
        if !p.instructions.is_empty() {
            system.push_str(
                "\nAgent instructions (from the agent definition; user-provided context, \
                             not permission to bypass approvals):\n",
            );
            system.push_str(&p.instructions);
            system.push('\n');
        }
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

        for _ in 0..p.max_iterations {
            let specs = p.tools.tool_specs();
            let resp = crate::llm::metered::with_kind(
                crate::llm::metered::KIND_SUBAGENT,
                p.llm.chat(&history, &specs, &p.model, None),
            )
            .await
            .map_err(|e| ToolError::Other(format!("sub-agent llm error: {e}")))?;

            let tool_calls = resp.tool_calls.clone();
            // 親のループと同じ: ツール往復の間だけ、思考過程を送り返す。
            let reasoning_content = resp
                .reasoning
                .clone()
                .filter(|_| !tool_calls.is_empty() && self.reasoning_roundtrip);
            history.push(Message::Assistant {
                content: resp.content.clone(),
                tool_calls: tool_calls.clone(),
                reasoning_content,
            });

            if tool_calls.is_empty() {
                return Ok(resp.content.unwrap_or_default());
            }

            for call in tool_calls {
                let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
                    .unwrap_or_else(|_| serde_json::json!({ "raw": call.function.arguments }));
                let refusal = self.refusal(&call.function.name, &args);
                let output = match p.tools.get(&call.function.name) {
                    // 読み取り専用 registry にしか無いので未知名はまず出ないが、保険。
                    None => ToolOutput::error(format!("unknown tool: {}", call.function.name)),
                    Some(_) if refusal.is_some() => ToolOutput::error(refusal.unwrap_or_default()),
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
            p.max_iterations
        )))
    }
}

#[async_trait]
impl Tool for SubAgentTool {
    fn name(&self) -> &str {
        "Task"
    }

    fn description(&self) -> &str {
        &self.description
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
                },
                "subagent_type": {
                    "type": "string",
                    "description": "Which sub-agent to run (see the tool description); omit for the default",
                    "enum": self.profiles.keys().collect::<Vec<_>>()
                }
            },
            "required": ["description", "prompt"]
        })
    }

    fn is_destructive(&self) -> bool {
        false
    }

    /// 子エージェントは親の履歴も他の子の結果も見ない。独立した調査を同時に走らせるのが主目的。
    fn parallel_safe(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let args: TaskArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs(format!("Task: {e}")))?;
        let (agent_type, _) = self.profile(args.subagent_type.as_deref())?;
        // 子は静かに走るので、起動を 1 行知らせて可視性を確保する。
        let label = if agent_type == DEFAULT_AGENT {
            String::new()
        } else {
            format!(" [{agent_type}]")
        };
        crate::say!(
            "  ↳ Task{label}: {}",
            crate::term::sanitize(&args.description)
        );
        let summary = self.run(agent_type, &args.prompt).await?;
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
            _max_tokens: Option<u32>,
        ) -> Result<ChatResponse> {
            let i = self.idx.fetch_add(1, Ordering::SeqCst);
            Ok(self.steps.get(i).cloned().unwrap_or(ChatResponse {
                content: Some("(no more script)".into()),
                tool_calls: vec![],
                usage: None,
                reasoning: None,
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

    /// 受け取った system prompt をそのまま最終応答にする LLM。子の system prompt を観測する。
    struct EchoSystemLlm;

    #[async_trait]
    impl LlmClient for EchoSystemLlm {
        async fn chat(
            &self,
            history: &[Message],
            _tools: &[ToolSpec<'_>],
            _model: &str,
            _max_tokens: Option<u32>,
        ) -> Result<ChatResponse> {
            let system = history
                .iter()
                .find_map(|m| match m {
                    Message::System { content } => Some(content.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            Ok(ChatResponse {
                content: Some(system),
                tool_calls: vec![],
                usage: None,
                reasoning: None,
            })
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

    /// `subagent_type` で定義した種類を選ぶと、その種類のクライアント・ツール・指示で走る (#77)。
    #[tokio::test]
    async fn a_custom_profile_routes_to_its_own_client_tools_and_instructions() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(read_only_registry());
        let mut only_read = read_only_registry();
        only_read.apply_profile(crate::config::ToolProfile::Full, &["Read".to_string()]);
        let tool = SubAgentTool::new(
            Arc::new(ScriptedLlm::new(vec![ChatResponse {
                content: Some("from default".into()),
                tool_calls: vec![],
                usage: None,
                reasoning: None,
            }])),
            "mock".into(),
            Arc::clone(&registry),
            dir.path().to_path_buf(),
            8,
        )
        .with_profile(
            "echo".into(),
            AgentProfile::new(
                Arc::new(EchoSystemLlm),
                "other-model".into(),
                Arc::new(only_read),
                3,
            )
            .with_instructions("BE TERSE".into(), "echoes its prompt".into()),
        );
        let ctx = ToolCtx::new(dir.path().to_path_buf());
        let run = |sub: Option<&str>| {
            let mut args = serde_json::json!({ "description": "d", "prompt": "p" });
            if let Some(s) = sub {
                args["subagent_type"] = serde_json::Value::String(s.into());
            }
            tool.execute(args, &ctx)
        };
        assert_eq!(run(None).await.unwrap().content, "from default");
        let echoed = run(Some("echo")).await.unwrap().content;
        assert!(echoed.contains("model: other-model"), "{echoed}");
        assert!(echoed.contains("Agent instructions") && echoed.contains("BE TERSE"));
        assert!(
            echoed.contains("- Read") && !echoed.contains("- Grep"),
            "tools narrowed: {echoed}"
        );
        let err = run(Some("nope")).await.unwrap_err().to_string();
        assert!(
            err.contains("unknown subagent_type") && err.contains("echo"),
            "{err}"
        );
        assert!(tool.description().contains("echo — echoes its prompt"));
        assert_eq!(tool.custom_names(), ["echo"]);
    }

    /// 子の system prompt は素の `build_system_prompt` のまま。親の `--append-system-prompt`
    /// (`Session::with_prior` が足すもの) は子に渡らない (#71)。将来、子の起動を Session 経由に
    /// 変えたときに黙って漏れないよう固定する。
    #[tokio::test]
    async fn the_sub_agents_system_prompt_has_no_appended_instructions() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(read_only_registry());
        let tool = SubAgentTool::new(
            Arc::new(EchoSystemLlm),
            "mock".into(),
            Arc::clone(&registry),
            dir.path().to_path_buf(),
            8,
        );
        let ctx = ToolCtx::new(dir.path().to_path_buf());
        let out = tool
            .execute(
                serde_json::json!({ "description": "d", "prompt": "p" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            out.content,
            prompt::build_system_prompt(dir.path(), "mock", registry.as_ref())
        );
        assert!(
            !out.content
                .contains("Additional instructions from the user")
        );
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

    /// 1 回目は指定のツールを呼び、2 回目は**受け取ったツール出力をそのまま最終応答にする** LLM。
    /// 子の中で読めてしまった内容が親へ漏れるかを観測するためのもの。
    struct EchoToolOutputLlm {
        call: ToolCall,
    }

    #[async_trait]
    impl LlmClient for EchoToolOutputLlm {
        async fn chat(
            &self,
            history: &[Message],
            _tools: &[ToolSpec<'_>],
            _model: &str,
            _max_tokens: Option<u32>,
        ) -> Result<ChatResponse> {
            let last_tool_output = history.iter().rev().find_map(|m| match m {
                Message::Tool { content, .. } => Some(content.clone()),
                _ => None,
            });
            Ok(match last_tool_output {
                None => ChatResponse {
                    content: None,
                    tool_calls: vec![self.call.clone()],
                    usage: None,
                    reasoning: None,
                },
                Some(output) => ChatResponse {
                    content: Some(output),
                    tool_calls: Vec::new(),
                    usage: None,
                    reasoning: None,
                },
            })
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

    async fn task_reads_env(rules: RuleSet) -> String {
        let tmp = tempfile::tempdir().unwrap();
        let env_path = tmp.path().join(".env");
        std::fs::write(&env_path, "API_KEY=hunter2").unwrap();
        let read_args = serde_json::json!({ "path": env_path.display().to_string() }).to_string();
        let tool = SubAgentTool::new(
            Arc::new(EchoToolOutputLlm {
                call: tool_call("Read", &read_args),
            }),
            "mock".into(),
            Arc::new(read_only_registry()),
            tmp.path().to_path_buf(),
            8,
        )
        .with_rules(Arc::new(rules));
        crate::tools::Tool::execute(
            &tool,
            serde_json::json!({ "description": "peek", "prompt": "read .env" }),
            &ToolCtx::new(tmp.path().to_path_buf()),
        )
        .await
        .unwrap()
        .content
    }

    #[tokio::test]
    async fn the_parents_deny_rules_also_bind_the_sub_agent() {
        // ルールが無ければ子は読めて、その内容が親へ返る (テストの観測方法が効いていることの確認)。
        assert!(task_reads_env(RuleSet::default()).await.contains("hunter2"));

        let deny = RuleSet::parse(&[], &["Read(.env)".to_string()], &[]).unwrap();
        let out = task_reads_env(deny).await;
        assert!(
            !out.contains("hunter2"),
            "the secret reached the parent: {out}"
        );
        assert!(
            out.contains("denied by permission rule `Read(.env)`"),
            "{out}"
        );

        // ask も子の中では尋ねられないので通さない。
        let ask = RuleSet::parse(&[], &[], &["Read(.env)".to_string()]).unwrap();
        let out = task_reads_env(ask).await;
        assert!(
            !out.contains("hunter2") && out.contains("approval"),
            "{out}"
        );
    }

    #[test]
    fn task_opts_into_parallel_execution_and_is_not_destructive() {
        // Task は runtime.rs で登録されるため、registry.rs の一覧テストには現れない。
        // 並列可の宣言のうち影響が最も大きい (子の LLM ループが同時に走る) ので、ここで固定する。
        // 同時数の上限は agent::loop の MAX_PARALLEL_TOOL_CALLS。
        let tool = subagent(Vec::new(), PathBuf::from("."));
        assert!(crate::tools::Tool::parallel_safe(&tool));
        assert!(!crate::tools::Tool::is_destructive(&tool));
    }

    /// #84: 子エージェントの LLM 呼び出しは親のループを通らないが、同じ台帳に `subagent` として載り、
    /// 同じ予算から引かれる。
    #[tokio::test]
    async fn sub_agent_calls_are_metered_and_draw_on_the_same_budget() {
        use crate::llm::metered::{Budget, KIND_MAIN, KIND_SUBAGENT, Ledger, MeteredClient};
        let usage = Some(crate::llm::Usage {
            prompt_tokens: 40,
            completion_tokens: 2,
            total_tokens: 42,
        });
        let reply = |text: &str| ChatResponse {
            content: Some(text.into()),
            tool_calls: vec![],
            usage,
            reasoning: None,
        };
        let ledger = Arc::new(Ledger::new(Budget {
            max_requests: Some(2),
            max_total_tokens: None,
        }));
        let llm: Arc<dyn LlmClient> = Arc::new(MeteredClient::new(
            Arc::new(ScriptedLlm::new(vec![reply("parent"), reply("child")])),
            ledger.clone(),
        ));
        // 親のターンに相当する呼び出しが 1 回、続いて子が 1 回。
        llm.chat(&[], &[], "mock", None).await.unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let sub = SubAgentTool::new(
            llm.clone(),
            "mock".into(),
            Arc::new(read_only_registry()),
            tmp.path().to_path_buf(),
            8,
        );
        assert_eq!(
            sub.run(DEFAULT_AGENT, "look around").await.unwrap(),
            "child"
        );

        let by_kind = ledger.by_kind();
        let kinds: Vec<&str> = by_kind.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, [KIND_MAIN, KIND_SUBAGENT]);
        assert_eq!(ledger.total().total_tokens, 84, "parent + child");

        // 予算は共有: 2 件を使い切ったので、次の子は 1 回も LLM を呼べずに失敗する。
        let err = sub
            .run(DEFAULT_AGENT, "again")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("budget exhausted"), "{err}");
        assert_eq!(ledger.requests(), 2);
    }

    #[tokio::test]
    async fn start_and_stop_hooks_see_the_task_and_its_answer() {
        use crate::hooks::{HookConfig, HooksCompat, Lifecycle};
        let tmp = tempfile::tempdir().unwrap();
        let seen = tmp.path().join("payloads.jsonl");
        let record = |event| HookConfig {
            id: None,
            event,
            // matcher は子エージェントの種類。
            matcher: "general-purpose".into(),
            command: format!("cat >> '{0}'; echo >> '{0}'", seen.display()),
            timeout_secs: None,
        };
        let sub = subagent(
            vec![ChatResponse {
                content: Some("the answer is 42".into()),
                tool_calls: vec![],
                usage: None,
                reasoning: None,
            }],
            tmp.path().to_path_buf(),
        )
        .with_hooks(
            vec![
                record(Lifecycle::SubagentStart),
                record(Lifecycle::SubagentStop),
            ],
            HooksCompat::V2,
        );
        assert_eq!(
            sub.run(DEFAULT_AGENT, "find the answer").await.unwrap(),
            "the answer is 42"
        );

        let payloads: Vec<serde_json::Value> = std::fs::read_to_string(&seen)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(payloads.len(), 2, "{payloads:?}");
        assert_eq!(payloads[0]["hook_event_name"], "SubagentStart");
        assert_eq!(payloads[0]["prompt"], "find the answer");
        assert_eq!(payloads[0]["agent_type"], "general-purpose");
        assert_eq!(payloads[1]["hook_event_name"], "SubagentStop");
        assert_eq!(payloads[1]["last_assistant_message"], "the answer is 42");
    }

    #[tokio::test]
    async fn returns_final_text_without_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = subagent(
            vec![ChatResponse {
                content: Some("the answer is 42".into()),
                tool_calls: vec![],
                usage: None,
                reasoning: None,
            }],
            tmp.path().to_path_buf(),
        );
        let out = sub.run(DEFAULT_AGENT, "what is the answer").await.unwrap();
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
                usage: None,
                reasoning: None,
            },
            ChatResponse {
                content: Some("found the needle".into()),
                tool_calls: vec![],
                usage: None,
                reasoning: None,
            },
        ];
        let sub = subagent(steps, tmp.path().to_path_buf());
        let out = sub.run(DEFAULT_AGENT, "find the needle").await.unwrap();
        assert_eq!(out, "found the needle");
    }

    /// 1 回目は思考つきでツールを呼び、2 回目は「受け取った履歴に思考が付いていたか」を答える LLM。
    struct ThinkingProbeLlm {
        call: ToolCall,
    }

    #[async_trait]
    impl LlmClient for ThinkingProbeLlm {
        async fn chat(
            &self,
            history: &[Message],
            _tools: &[ToolSpec<'_>],
            _model: &str,
            _max_tokens: Option<u32>,
        ) -> Result<ChatResponse> {
            let came_back = history.iter().any(|m| {
                matches!(m, Message::Assistant { reasoning_content: Some(r), .. } if r == "look first")
            });
            let asked_already = history.iter().any(|m| matches!(m, Message::Tool { .. }));
            Ok(if asked_already {
                ChatResponse {
                    content: Some(format!("reasoning came back: {came_back}")),
                    tool_calls: vec![],
                    usage: None,
                    reasoning: None,
                }
            } else {
                ChatResponse {
                    content: None,
                    tool_calls: vec![self.call.clone()],
                    usage: None,
                    reasoning: Some("look first".into()),
                }
            })
        }

        async fn chat_stream(
            &self,
            _h: &[Message],
            _t: &[ToolSpec<'_>],
            _m: &str,
            _sink: mpsc::UnboundedSender<ChatEvent>,
        ) -> Result<()> {
            unreachable!("the sub-agent does not stream")
        }
    }

    /// 子のツール往復でも、親と同じく思考過程を送り返す (設定で止められる)。
    #[tokio::test]
    async fn the_sub_agent_round_trips_reasoning_inside_its_own_tool_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let run = |roundtrip: bool| {
            let llm = ThinkingProbeLlm {
                call: tool_call(
                    "Glob",
                    &serde_json::json!({ "pattern": "*.none", "path": tmp.path() }).to_string(),
                ),
            };
            let sub = SubAgentTool::new(
                Arc::new(llm),
                "mock".into(),
                Arc::new(read_only_registry()),
                tmp.path().to_path_buf(),
                8,
            )
            .with_reasoning_roundtrip(roundtrip);
            async move { sub.run(DEFAULT_AGENT, "look around").await.unwrap() }
        };
        assert_eq!(run(true).await, "reasoning came back: true");
        assert_eq!(run(false).await, "reasoning came back: false");
    }

    #[tokio::test]
    async fn max_iterations_guard_errors() {
        let tmp = tempfile::tempdir().unwrap();
        // 常に tool_call を返し続ける → 上限で打ち切り。
        let looping = (0..20)
            .map(|_| ChatResponse {
                content: None,
                tool_calls: vec![tool_call("Grep", r#"{"pattern":"x","path":"."}"#)],
                usage: None,
                reasoning: None,
            })
            .collect();
        let sub = subagent(looping, tmp.path().to_path_buf());
        let err = sub.run(DEFAULT_AGENT, "loop forever").await.unwrap_err();
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
