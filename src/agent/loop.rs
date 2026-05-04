use anyhow::{Result, bail};
use std::io::Write as _;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::agent::messages::Message;
use crate::config::Config;
use crate::llm::{ChatEvent, ChatResponse, LlmClient};
use crate::permission::PermissionGate;
use crate::prompt;
use crate::tools::registry::ToolRegistry;
use crate::tools::{ToolCtx, ToolOutput};

pub struct Session {
    cfg: Config,
    registry: Arc<ToolRegistry>,
    history: Vec<Message>,
    ctx: ToolCtx,
}

impl Session {
    pub fn new(cfg: Config, registry: Arc<ToolRegistry>) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let system = prompt::build_system_prompt(&cwd, &cfg.llm.model, registry.as_ref());
        let history = vec![Message::System { content: system }];
        let ctx = ToolCtx::new(cwd);
        Self {
            cfg,
            registry,
            history,
            ctx,
        }
    }

    pub async fn run_turn(
        &mut self,
        user_input: &str,
        llm: &dyn LlmClient,
        gate: &PermissionGate,
    ) -> Result<()> {
        // hooks::runner::dispatch(Lifecycle::UserPromptSubmit, user_input).await?;  // MVP 外
        self.history.push(Message::User {
            content: user_input.to_string(),
        });

        for _ in 0..self.cfg.agent.max_iterations {
            let specs = self.registry.tool_specs();
            let resp = stream_once(llm, &self.history, &specs, &self.cfg.llm.model).await?;

            let tool_calls = resp.tool_calls.clone();
            self.history.push(Message::Assistant {
                content: resp.content.clone(),
                tool_calls: tool_calls.clone(),
            });

            if tool_calls.is_empty() {
                println!();
                return Ok(());
            }

            // 改行を入れてツール出力との視認性を確保
            println!();

            for call in tool_calls {
                // hooks::runner::dispatch(Lifecycle::PreToolUse, &call).await?;  // MVP 外
                let output = match self.registry.get(&call.function.name) {
                    None => ToolOutput::error(format!("unknown tool: {}", call.function.name)),
                    Some(tool) => {
                        let args: serde_json::Value =
                            serde_json::from_str(&call.function.arguments).unwrap_or_else(|_| {
                                serde_json::json!({ "raw": call.function.arguments })
                            });
                        let approved = !tool.is_destructive() || gate.allow(tool.name(), &args);
                        if !approved {
                            ToolOutput::error("user denied execution")
                        } else {
                            match tool.execute(args, &self.ctx).await {
                                Ok(o) => o,
                                Err(e) => ToolOutput::error(format!("tool error: {e}")),
                            }
                        }
                    }
                };
                // hooks::runner::dispatch(Lifecycle::PostToolUse, ...).await?;  // MVP 外

                println!(
                    "[{}] {}",
                    call.function.name,
                    truncate(&output.content, 400)
                );
                self.history.push(Message::Tool {
                    tool_call_id: call.id,
                    content: output.content,
                });
            }
        }

        bail!(
            "hit max_iterations ({}) without final assistant text",
            self.cfg.agent.max_iterations
        );
    }
}

async fn stream_once(
    llm: &dyn LlmClient,
    history: &[Message],
    tools: &[crate::agent::messages::ToolSpec<'_>],
    model: &str,
) -> Result<ChatResponse> {
    let (tx, mut rx) = mpsc::unbounded_channel::<ChatEvent>();
    let send_fut = llm.chat_stream(history, tools, model, tx);
    tokio::pin!(send_fut);

    let mut last_done: Option<ChatResponse> = None;
    let mut stdout = std::io::stdout();
    loop {
        tokio::select! {
            res = &mut send_fut => { res?; break; }
            ev = rx.recv() => {
                match ev {
                    Some(ChatEvent::TextDelta(s)) => {
                        let _ = stdout.write_all(s.as_bytes());
                        let _ = stdout.flush();
                    }
                    Some(ChatEvent::Done(r)) => last_done = Some(r),
                    None => break,
                }
            }
        }
    }
    // ストリーム完了後にチャネルへ残った Done を回収
    while let Ok(ev) = rx.try_recv() {
        if let ChatEvent::Done(r) = ev {
            last_done = Some(r);
        }
    }

    last_done.ok_or_else(|| anyhow::anyhow!("stream ended without Done event"))
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let head: String = s.chars().take(n).collect();
        format!("{head}…")
    }
}
