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
                // Stop hook: 停止をブロックされたら reason をユーザ入力として注入し継続する。
                // これが /goal（達成条件までターン継続）の土台になる。
                let stop_payload = serde_json::json!({
                    "hook_event_name": "Stop",
                    "last_message": resp.content,
                });
                match hooks::runner::dispatch(Lifecycle::Stop, None, &stop_payload, &self.cfg.hooks)
                    .await?
                {
                    HookOutcome::Continue => return Ok(()),
                    HookOutcome::Block(reason) => {
                        println!("{}", crate::term::dim(&format!("[stop hook] {reason}")));
                        self.history.push(Message::User { content: reason });
                        continue;
                    }
                }
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

    /// 会話履歴を圧縮する。System と直近 `KEEP_RECENT_USER_TURNS` ユーザターンを残し、
    /// それ以前を LLM 要約 1 メッセージに畳む。分割は **ユーザターン境界**
    /// (`Message::User` の直前) に限定するので、Assistant の tool_calls と対応する
    /// Tool 応答の対を跨いで切ることはない（run_turn は 1 ターンを完結させてから
    /// 次の User を積むため、境界より前は常に完結したターン列になる）。
    pub async fn compact(
        &mut self,
        llm: &dyn LlmClient,
        instruction: &str,
    ) -> Result<CompactOutcome> {
        let user_idxs: Vec<usize> = self
            .history
            .iter()
            .enumerate()
            .filter(|(_, m)| matches!(m, Message::User { .. }))
            .map(|(i, _)| i)
            .collect();
        if user_idxs.len() <= KEEP_RECENT_USER_TURNS {
            return Ok(CompactOutcome::Skipped);
        }
        let boundary = user_idxs[user_idxs.len() - KEEP_RECENT_USER_TURNS];
        // system(index 0) の直後から boundary 手前までが要約対象。
        if boundary <= 1 {
            return Ok(CompactOutcome::Skipped);
        }

        let before = self.history.len();
        let rendered = render_for_summary(&self.history[1..boundary]);
        let sys = Message::System {
            content: "You compress a coding-assistant conversation into a compact summary that \
                      preserves decisions made, file paths touched, command results, and any \
                      open tasks. Output only the summary text."
                .to_string(),
        };
        let focus = if instruction.trim().is_empty() {
            String::new()
        } else {
            format!("\n\nEmphasize: {instruction}")
        };
        let usr = Message::User {
            content: format!(
                "Summarize this earlier conversation so it can replace the raw messages while \
                 preserving continuity for the assistant.{focus}\n\n---\n{rendered}"
            ),
        };
        let resp = llm
            .chat(&[sys, usr], &[], &self.cfg.llm.active().model, Some(1024))
            .await?;
        let summary = resp.content.unwrap_or_default();
        if summary.trim().is_empty() {
            return Ok(CompactOutcome::Failed);
        }

        // 置換: [system] + [boundary..]。要約は独立 User にせず**直後の kept User
        // 本文へ前置**する。独立させると user が 2 連続になり、strict な
        // user/assistant 交互を要求するローカルモデル (llama.cpp/vLLM/ollama の
        // Mistral/Llama テンプレ) がエラーになり得るため。
        let mut kept = self.history.split_off(boundary);
        let system = self.history.remove(0);
        let block = format!("[Summary of earlier conversation]\n{summary}\n\n---\n");
        match kept.first_mut() {
            // boundary は必ず User なので通常はこちら。
            Some(Message::User { content }) => {
                *content = format!("{block}{content}");
            }
            // 想定外 (kept 先頭が User でない) 時のみ独立挿入でフォールバック。
            _ => kept.insert(0, Message::User { content: block }),
        }
        let mut new_history = Vec::with_capacity(kept.len() + 1);
        new_history.push(system);
        new_history.extend(kept);
        let after = new_history.len();
        self.history = new_history;
        Ok(CompactOutcome::Compacted { before, after })
    }
}

/// System を除き、直近何ユーザターンを生のまま残すか。
const KEEP_RECENT_USER_TURNS: usize = 2;

/// `Session::compact` の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactOutcome {
    Compacted { before: usize, after: usize },
    Skipped,
    Failed,
}

impl CompactOutcome {
    pub fn describe(&self) -> String {
        match self {
            CompactOutcome::Compacted { before, after } => {
                format!("compacted history: {before} → {after} messages")
            }
            CompactOutcome::Skipped => {
                "compact skipped: not enough history to summarize".to_string()
            }
            CompactOutcome::Failed => {
                "compact failed: summarizer returned empty output".to_string()
            }
        }
    }
}

/// 要約対象メッセージを 1 本のテキストへ整形する（要約 LLM への入力用）。
fn render_for_summary(msgs: &[Message]) -> String {
    let mut out = String::new();
    for m in msgs {
        match m {
            Message::System { content } => {
                out.push_str("SYSTEM: ");
                out.push_str(content);
            }
            Message::User { content } => {
                out.push_str("USER: ");
                out.push_str(content);
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                out.push_str("ASSISTANT: ");
                if let Some(c) = content {
                    out.push_str(c);
                }
                for tc in tool_calls {
                    out.push_str(&format!(
                        " [tool_call {} {}]",
                        tc.function.name, tc.function.arguments
                    ));
                }
            }
            Message::Tool { content, .. } => {
                out.push_str("TOOL: ");
                out.push_str(content);
            }
        }
        out.push('\n');
    }
    out
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
            // 正常/異常どちらの完了でもインジケータを消してから抜ける。
            res = &mut send_fut => { clear_wait(&mut stdout); res?; break; }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::messages::ToolSpec;
    use crate::config::Config;
    use crate::hooks::HookConfig;
    use crate::llm::{ChatEvent, ChatResponse};
    use crate::permission::PermissionGate;
    use crate::tools::registry::default_registry;
    use async_trait::async_trait;

    /// 毎ターン同じ最終テキスト（tool_call 無し）を Done で返すモック。
    struct FinalTextLlm {
        text: String,
    }

    #[async_trait]
    impl LlmClient for FinalTextLlm {
        async fn chat(
            &self,
            _h: &[Message],
            _t: &[ToolSpec<'_>],
            _m: &str,
            _mt: Option<u32>,
        ) -> Result<ChatResponse> {
            Ok(ChatResponse {
                content: Some(self.text.clone()),
                tool_calls: vec![],
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
                content: Some(self.text.clone()),
                tool_calls: vec![],
            }));
            Ok(())
        }
    }

    fn session_with_stop_hook(cmd: Option<String>) -> Session {
        let mut cfg = Config::default();
        if let Some(command) = cmd {
            cfg.hooks = vec![HookConfig {
                event: Lifecycle::Stop,
                matcher: String::new(),
                command,
            }];
        }
        Session::new(cfg, Arc::new(default_registry()))
    }

    /// Stop hook 無し → Stop は Continue → 1 ターンで終わる（reason 注入なし）。
    #[tokio::test]
    async fn stop_hook_absent_ends_turn() {
        let mut session = session_with_stop_hook(None);
        let llm = FinalTextLlm {
            text: "done".into(),
        };
        let gate = PermissionGate::new(true);
        session.run_turn("hi", &llm, &gate).await.unwrap();
        let users = session
            .history()
            .iter()
            .filter(|m| matches!(m, Message::User { .. }))
            .count();
        assert_eq!(users, 1, "only the original user turn");
    }

    /// Stop hook が 1 度だけ block → reason がユーザ入力として注入され、次ターンで収束する。
    #[tokio::test]
    async fn stop_hook_block_injects_reason_then_continues() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("stop_marker");
        // 初回: marker 無 → 作成し block。2 回目: marker 有 → continue。
        let cmd = format!(
            "if [ -f '{m}' ]; then exit 0; else : > '{m}'; echo keep-going 1>&2; exit 1; fi",
            m = marker.display()
        );
        let mut session = session_with_stop_hook(Some(cmd));
        let llm = FinalTextLlm {
            text: "done".into(),
        };
        let gate = PermissionGate::new(true);

        session.run_turn("hi", &llm, &gate).await.unwrap();

        assert!(marker.exists(), "stop hook should have fired");
        let injected = session
            .history()
            .iter()
            .any(|m| matches!(m, Message::User { content } if content.contains("keep-going")));
        assert!(
            injected,
            "a blocked Stop hook should inject its reason as a user turn"
        );
    }

    /// ユーザターンが少ないうちは compact は Skipped。
    #[tokio::test]
    async fn compact_skips_when_history_short() {
        let mut session = session_with_stop_hook(None);
        let llm = FinalTextLlm {
            text: "done".into(),
        };
        let gate = PermissionGate::new(true);
        session.run_turn("first", &llm, &gate).await.unwrap();
        // 1 ユーザターンのみ → KEEP_RECENT_USER_TURNS 以下。
        let out = session.compact(&llm, "").await.unwrap();
        assert_eq!(out, CompactOutcome::Skipped);
    }

    /// 3 ターン以上で compact すると System + 要約 + 直近が残り、件数が減る。
    #[tokio::test]
    async fn compact_folds_old_turns_into_summary() {
        let mut session = session_with_stop_hook(None);
        let llm = FinalTextLlm {
            text: "SUMMARY".into(),
        };
        let gate = PermissionGate::new(true);
        for p in ["t1", "t2", "t3"] {
            session.run_turn(p, &llm, &gate).await.unwrap();
        }
        let before = session.history().len();
        let out = session.compact(&llm, "keep the file paths").await.unwrap();
        match out {
            CompactOutcome::Compacted {
                before: b,
                after: a,
            } => {
                assert_eq!(b, before);
                assert!(a < b, "compaction should shrink history ({a} !< {b})");
            }
            other => panic!("expected Compacted, got {other:?}"),
        }
        let hist = session.history();
        // 先頭は System、2 番目は要約ユーザメッセージ。
        assert!(matches!(hist[0], Message::System { .. }));
        assert!(
            matches!(&hist[1], Message::User { content } if content.contains("Summary of earlier conversation"))
        );
        // 直近ターン (t3) は生のまま残る。
        assert!(
            hist.iter()
                .any(|m| matches!(m, Message::User { content } if content == "t3"))
        );
        // 要約は独立 User にせず前置したので User が 2 連続しない
        // (strict alternation のローカルモデル対策)。
        let consecutive_users = hist
            .windows(2)
            .any(|w| matches!((&w[0], &w[1]), (Message::User { .. }, Message::User { .. })));
        assert!(
            !consecutive_users,
            "compaction must not create back-to-back user messages"
        );
    }
}
