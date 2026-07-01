use anyhow::{Result, bail};
use std::io::Write as _;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::agent::messages::Message;
use crate::config::Config;
use crate::hooks::{self, HookOutcome, Lifecycle};
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
        Self::with_prior(cfg, registry, Vec::new())
    }

    /// 保存済みセッションから復元する。`prior` の System メッセージは捨て、
    /// 現環境のツール一覧で system prompt を作り直してから残りを引き継ぐ。
    pub fn resume(cfg: Config, registry: Arc<ToolRegistry>, prior: Vec<Message>) -> Self {
        let prior: Vec<Message> = prior
            .into_iter()
            .filter(|m| !matches!(m, Message::System { .. }))
            .collect();
        Self::with_prior(cfg, registry, prior)
    }

    fn with_prior(cfg: Config, registry: Arc<ToolRegistry>, prior: Vec<Message>) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let system = prompt::build_system_prompt(&cwd, &cfg.llm.active().model, registry.as_ref());
        let mut history = vec![Message::System { content: system }];
        history.extend(prior);
        let ctx = ToolCtx::new(cwd);
        Self {
            cfg,
            registry,
            history,
            ctx,
        }
    }

    /// 永続化のための会話履歴 (system を含む全メッセージ)。
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub async fn run_turn(
        &mut self,
        user_input: &str,
        llm: &dyn LlmClient,
        gate: &PermissionGate,
    ) -> Result<()> {
        let prompt_payload = serde_json::json!({ "prompt": user_input });
        if let HookOutcome::Block(reason) = hooks::runner::dispatch(
            Lifecycle::UserPromptSubmit,
            None,
            &prompt_payload,
            &self.cfg.hooks,
        )
        .await?
        {
            println!("prompt blocked by hook: {reason}");
            return Ok(());
        }
        self.history.push(Message::User {
            content: user_input.to_string(),
        });

        for _ in 0..self.cfg.agent.max_iterations {
            let specs = self.registry.tool_specs();
            let resp =
                stream_once(llm, &self.history, &specs, &self.cfg.llm.active().model).await?;

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
                let name = call.function.name.clone();
                let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
                    .unwrap_or_else(|_| serde_json::json!({ "raw": call.function.arguments }));

                let mut output = match self.registry.get(&name) {
                    None => ToolOutput::error(format!("unknown tool: {name}")),
                    Some(tool) => {
                        let pre_payload =
                            serde_json::json!({ "tool_name": name, "tool_input": args });
                        match hooks::runner::dispatch(
                            Lifecycle::PreToolUse,
                            Some(&name),
                            &pre_payload,
                            &self.cfg.hooks,
                        )
                        .await?
                        {
                            HookOutcome::Block(reason) => {
                                ToolOutput::error(format!("blocked by hook: {reason}"))
                            }
                            HookOutcome::Continue => {
                                let approved =
                                    !tool.is_destructive() || gate.allow(tool.name(), &args);
                                if !approved {
                                    ToolOutput::error("user denied execution")
                                } else {
                                    match tool.execute(args.clone(), &self.ctx).await {
                                        Ok(o) => o,
                                        Err(e) => ToolOutput::error(format!("tool error: {e}")),
                                    }
                                }
                            }
                        }
                    }
                };

                let post_payload = serde_json::json!({
                    "tool_name": name,
                    "tool_input": args,
                    "tool_output": output.content,
                });
                if let HookOutcome::Block(reason) = hooks::runner::dispatch(
                    Lifecycle::PostToolUse,
                    Some(&name),
                    &post_payload,
                    &self.cfg.hooks,
                )
                .await?
                {
                    // 実行後なので取り消せない。理由をツール出力へ追記し、
                    // history 経由でモデルへフィードバックする。
                    println!("post-tool hook: {reason}");
                    output.content = format!("{}\n[post-tool hook] {reason}", output.content);
                }

                let tag = format!("[{name}]");
                let tag = if output.is_error {
                    crate::term::red(&tag)
                } else {
                    crate::term::cyan(&tag)
                };
                println!("{tag} {}", truncate(&output.content, 400));
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

    // 応答待ちインジケータ: 最初のトークンが来るまで dim の "…thinking" を出し、
    // 到着時に行ごと消す。tty のときだけ（パイプに制御文字を混ぜない）。
    let show_wait = crate::term::is_terminal();
    if show_wait {
        let _ = write!(stdout, "{}", crate::term::dim("…thinking"));
        let _ = stdout.flush();
    }
    let mut cleared = false;
    let mut clear_wait = |stdout: &mut std::io::Stdout| {
        if show_wait && !cleared {
            let _ = write!(stdout, "\r\x1b[2K"); // 行頭へ戻して行クリア
            let _ = stdout.flush();
            cleared = true;
        }
    };

    loop {
        tokio::select! {
            res = &mut send_fut => { res?; break; }
            ev = rx.recv() => {
                match ev {
                    Some(ChatEvent::TextDelta(s)) => {
                        clear_wait(&mut stdout);
                        let _ = stdout.write_all(s.as_bytes());
                        let _ = stdout.flush();
                    }
                    Some(ChatEvent::Done(r)) => last_done = Some(r),
                    None => break,
                }
            }
        }
    }
    // テキストが 1 つも来なかった場合もインジケータを消す。
    clear_wait(&mut stdout);
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
