use anyhow::{Result, bail};
use std::io::Write as _;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::agent::messages::Message;
use crate::config::Config;
use crate::hooks::{self, HookOutcome, Lifecycle};
use crate::llm::{ChatEvent, ChatResponse, LlmClient, Usage};
use crate::permission::PermissionGate;
use crate::prompt;
use crate::tools::registry::ToolRegistry;
use crate::tools::{ToolCtx, ToolOutput};

/// セッションの動作モード。Plan 中は破壊的ツールを LLM から不可視にし、
/// 呼ばれても実行しない (調査と計画提示のみ)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Normal,
    Plan,
}

pub struct Session {
    cfg: Config,
    registry: Arc<ToolRegistry>,
    history: Vec<Message>,
    ctx: ToolCtx,
    usage: SessionUsage,
    mode: Mode,
    /// ターン単位のファイル変更 undo 台帳 (`/undo`)。
    undo: crate::undo::UndoLog,
    /// run_turn ごとに増える通し番号 (undo 台帳のターン識別に使う)。
    turn_seq: u64,
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
            usage: SessionUsage::default(),
            mode: Mode::default(),
            undo: crate::undo::UndoLog::default(),
            turn_seq: 0,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// 永続化のための会話履歴 (system を含む全メッセージ)。
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// セッション累積のトークン使用量 (`/cost` 表示・自動圧縮の判断材料)。
    pub fn usage(&self) -> &SessionUsage {
        &self.usage
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
        self.turn_seq += 1;
        // Plan 中はモデルに「調査と計画のみ」を毎ターン明示する (system prompt は
        // モード切替で作り直さないため、入力への前置で伝える)。
        let content = match self.mode {
            Mode::Plan => format!("{PLAN_MODE_PREFIX}\n\n{user_input}"),
            Mode::Normal => user_input.to_string(),
        };
        self.history.push(Message::User { content });

        for _ in 0..self.cfg.agent.max_iterations {
            // Plan 中は read-only specs に ExitPlanMode (承認要求の擬似ツール) を
            // 加える。Normal では不可視。モードはターン途中でも切り替わり得る
            // (ExitPlanMode 承認直後) ため、毎イテレーション組み直す。
            let specs = match self.mode {
                Mode::Plan => {
                    let mut s = self.registry.read_only_tool_specs();
                    s.push(exit_plan_mode_spec());
                    s
                }
                Mode::Normal => self.registry.tool_specs(),
            };
            let resp =
                stream_once(llm, &self.history, &specs, &self.cfg.llm.active().model).await?;
            let (u, estimated) = resolve_usage(&resp, &self.history);
            self.usage.record(u, estimated);

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
                    HookOutcome::Continue => {
                        self.maybe_auto_compact(llm).await;
                        return Ok(());
                    }
                    HookOutcome::Block(reason) => {
                        println!("{}", crate::term::dim(&format!("[stop hook] {reason}")));
                        self.history.push(Message::User { content: reason });
                        continue;
                    }
                }
            }

            // 改行を入れてツール出力との視認性を確保
            println!();

            // ExitPlanMode 承認でモードが Plan → Normal に変わった後、同一バッチの
            // 残り tool_call を実行すると plan ガードを素通りしてしまうため、
            // 残りは実行せずスキップ応答を返す (tool_call_id の対は維持)。
            let mut plan_just_approved = false;
            for call in tool_calls {
                let name = call.function.name.clone();
                let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
                    .unwrap_or_else(|_| serde_json::json!({ "raw": call.function.arguments }));

                let mut output = if plan_just_approved {
                    ToolOutput::error(format!(
                        "skipped '{name}': the plan was approved earlier in this same response. \
                         Re-issue this tool call in your next response now that plan mode is exited."
                    ))
                } else if name == EXIT_PLAN_MODE {
                    // registry 外の擬似ツール。計画提示→ユーザ承認→モード遷移を
                    // ここで完結させる (PreToolUse hook は通さない)。
                    let out = self.handle_exit_plan_mode(&args, gate);
                    if !out.is_error {
                        plan_just_approved = true;
                    }
                    out
                } else {
                    match self.registry.get(&name) {
                        None => ToolOutput::error(format!("unknown tool: {name}")),
                        // specs から隠していても呼ばれ得るので実行側でも防ぐ (多層防御)。
                        Some(tool) if self.mode == Mode::Plan && tool.is_destructive() => {
                            ToolOutput::error(format!(
                                "plan mode: destructive tool '{name}' is disabled. Investigate with \
                             read-only tools and present a plan; the user approves it with /accept."
                            ))
                        }
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
                                        // 実行が確定してから変更前を退避する (/undo 用)。
                                        self.snapshot_for_undo(&name, &args);
                                        match tool.execute(args.clone(), &self.ctx).await {
                                            Ok(o) => o,
                                            Err(e) => ToolOutput::error(format!("tool error: {e}")),
                                        }
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

    /// 直近のコンテキストサイズがしきい値 (context_window の
    /// `AUTO_COMPACT_THRESHOLD_PERCENT`%) に達したか。`context_window = 0` は無効。
    pub fn should_auto_compact(&self) -> bool {
        let window = self.cfg.llm.active().context_window;
        window > 0
            && self.usage.last_context_tokens * 100 >= window * AUTO_COMPACT_THRESHOLD_PERCENT
    }

    /// しきい値超過時の自動圧縮。ターン終端で呼ぶ。圧縮に失敗しても
    /// ターン自体は成功扱いにする (次ターン終端で再試行される)。
    async fn maybe_auto_compact(&mut self, llm: &dyn LlmClient) {
        if !self.should_auto_compact() {
            return;
        }
        let window = self.cfg.llm.active().context_window;
        println!(
            "{}",
            crate::term::dim(&format!(
                "[auto-compact] context ~{} tokens ≥ {}% of {} window",
                self.usage.last_context_tokens, AUTO_COMPACT_THRESHOLD_PERCENT, window
            ))
        );
        // compact() 内の要約呼び出しで last_context_tokens は要約プロンプト分に
        // 上書きされるが、次ターンの stream_once が本来の値で再上書きするため
        // 連続発火にはならない。
        match self.compact(llm, "").await {
            Ok(outcome) => println!("{}", crate::term::dim(&outcome.describe())),
            Err(e) => println!(
                "{}",
                crate::term::red(&format!("auto-compact failed: {e:#}"))
            ),
        }
    }

    /// 中断 (Ctrl-C で run_turn の future を破棄) した後に履歴の整合性を直す。
    /// 詳細は `repair_interrupted_history` を参照。
    pub fn interrupt_repair(&mut self) {
        repair_interrupted_history(&mut self.history);
    }

    /// ファイル系ツールの実行直前に変更前スナップショットを取る (`/undo` 用)。
    /// path はツール本体 (write.rs 等) と同じ規則で解決する: 絶対ならそのまま、
    /// 相対なら ctx.cwd 基準。実行が失敗してもスナップショットは台帳に残るが、
    /// 変更前と同じ内容を書き戻すだけなので undo しても無害。
    fn snapshot_for_undo(&mut self, tool_name: &str, args: &serde_json::Value) {
        if !UNDOABLE_FILE_TOOLS.contains(&tool_name) {
            return;
        }
        let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
            return;
        };
        let abs = std::path::PathBuf::from(path);
        let abs = if abs.is_absolute() {
            abs
        } else {
            self.ctx.cwd.join(abs)
        };
        self.undo.record_before(self.turn_seq, &abs);
    }

    /// 直近ターンのファイル変更を巻き戻す (`/undo`)。記録が無ければ None。
    /// Bash など非可逆な副作用は対象外 (undo 台帳に載らない)。
    pub fn undo_last_turn(&mut self) -> Option<crate::undo::UndoReport> {
        self.undo.undo_last()
    }

    /// ExitPlanMode 擬似ツールの処理。計画を表示してユーザ承認を取り、
    /// 承認なら Normal へ遷移して実行続行を、拒否なら Plan 維持で修正を
    /// モデルに指示する。承認は既存の PermissionGate を使う (`--yes` /
    /// auto_approve なら自動承認、"always" 応答で以後の計画も自動承認)。
    fn handle_exit_plan_mode(
        &mut self,
        args: &serde_json::Value,
        gate: &PermissionGate,
    ) -> ToolOutput {
        if self.mode != Mode::Plan {
            return ToolOutput::error(
                "ExitPlanMode is only available in plan mode (the session is in normal mode)",
            );
        }
        let plan = args.get("plan").and_then(|v| v.as_str()).unwrap_or("");
        if plan.trim().is_empty() {
            return ToolOutput::error("ExitPlanMode requires a non-empty 'plan' argument");
        }

        println!("{}", crate::term::bold("--- proposed plan ---"));
        println!("{plan}");
        println!("{}", crate::term::bold("---------------------"));

        if gate.allow(EXIT_PLAN_MODE, args) {
            self.mode = Mode::Normal;
            ToolOutput::ok(
                "Plan approved by the user. Plan mode exited — all tools are available again; \
                 proceed to execute the plan.",
            )
        } else {
            ToolOutput::error(
                "The user rejected the plan. Stay in plan mode: ask what should change or \
                 revise the plan, then call ExitPlanMode again.",
            )
        }
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
        let summary_input = [sys, usr];
        let resp = llm
            .chat(
                &summary_input,
                &[],
                &self.cfg.llm.active().model,
                Some(1024),
            )
            .await?;
        let (u, estimated) = resolve_usage(&resp, &summary_input);
        self.usage.record(u, estimated);
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

/// 自動圧縮を発火するコンテキスト使用率 (context_window に対する %)。
const AUTO_COMPACT_THRESHOLD_PERCENT: u64 = 80;

/// usage 概算フォールバックの 1 トークンあたり文字数。英語 ~4 文字/トークン、
/// 日本語 ~1-2 文字/トークンの間を取った粗い近似 (桁が合えば十分)。
const ESTIMATE_CHARS_PER_TOKEN: u64 = 3;

/// セッション累積のトークン使用量。`/cost` 表示と自動圧縮 (しきい値) の基盤。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionUsage {
    pub llm_calls: u64,
    /// サーバが usage を返さず文字数概算にフォールバックした呼び出し数。
    pub estimated_calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// 直近呼び出しの prompt_tokens (= 現在のコンテキストサイズの近似)。
    pub last_context_tokens: u64,
}

impl SessionUsage {
    fn record(&mut self, u: Usage, estimated: bool) {
        self.llm_calls += 1;
        if estimated {
            self.estimated_calls += 1;
        }
        self.prompt_tokens += u.prompt_tokens;
        self.completion_tokens += u.completion_tokens;
        self.total_tokens += u.total_tokens;
        self.last_context_tokens = u.prompt_tokens;
    }

    /// `/cost` 用の表示文字列。料金はローカル/Sakana では出せないためトークン数のみ。
    pub fn describe(&self) -> String {
        if self.llm_calls == 0 {
            return "no LLM calls yet".to_string();
        }
        let mut out = format!(
            "tokens: {} total (prompt {} + completion {}) across {} LLM call(s)\nlast context: {} prompt tokens",
            self.total_tokens,
            self.prompt_tokens,
            self.completion_tokens,
            self.llm_calls,
            self.last_context_tokens,
        );
        if self.estimated_calls > 0 {
            out.push_str(&format!(
                "\nnote: {} call(s) lacked server usage; counted via ~{} chars/token estimate",
                self.estimated_calls, ESTIMATE_CHARS_PER_TOKEN
            ));
        }
        out
    }
}

/// 応答の usage を返す。サーバが usage を返さなかった場合、または空の
/// `usage: {}` (全ゼロ) を返した場合は、送信メッセージ列と応答本文から
/// 文字数ベースで概算する (bool は概算フラグ)。
fn resolve_usage(resp: &ChatResponse, prompt_messages: &[Message]) -> (Usage, bool) {
    match resp.usage {
        // normalized 済みなので total == 0 は全フィールドゼロ = 実質未報告。
        Some(u) if u.total_tokens > 0 => (u, false),
        _ => (estimate_usage(prompt_messages, resp), true),
    }
}

/// 文字数ベースの粗いトークン概算。トークナイザ非依存で桁を合わせるのが目的。
fn estimate_usage(prompt_messages: &[Message], resp: &ChatResponse) -> Usage {
    let prompt_chars: u64 = prompt_messages.iter().map(message_chars).sum();
    let mut completion_chars: u64 = resp.content.as_deref().map_or(0, |c| c.chars().count()) as u64;
    for tc in &resp.tool_calls {
        completion_chars +=
            (tc.function.name.chars().count() + tc.function.arguments.chars().count()) as u64;
    }
    let prompt_tokens = prompt_chars.div_ceil(ESTIMATE_CHARS_PER_TOKEN);
    let completion_tokens = completion_chars.div_ceil(ESTIMATE_CHARS_PER_TOKEN);
    Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens + completion_tokens,
    }
}

fn message_chars(m: &Message) -> u64 {
    let n = match m {
        Message::System { content } | Message::User { content } | Message::Tool { content, .. } => {
            content.chars().count()
        }
        Message::Assistant {
            content,
            tool_calls,
        } => {
            content.as_deref().map_or(0, |c| c.chars().count())
                + tool_calls
                    .iter()
                    .map(|tc| {
                        tc.function.name.chars().count() + tc.function.arguments.chars().count()
                    })
                    .sum::<usize>()
        }
    };
    n as u64
}

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

/// 中断で補填する応答の本文。モデルに「途中で切られた」ことを伝える。
const INTERRUPT_NOTE: &str = "[interrupted by user before completion]";

/// Plan モード中に毎ユーザ入力へ前置する指示。
const PLAN_MODE_PREFIX: &str = "[plan mode] You are in plan mode: investigate with the available \
    read-only tools and produce a concrete step-by-step plan. Do NOT attempt to modify files or \
    run commands — destructive tools are disabled. When the plan is complete, call the \
    ExitPlanMode tool with the plan to request the user's approval (they can also approve \
    manually with /accept).";

/// Plan モード中のみ specs へ加える承認要求の擬似ツール名。registry には登録しない。
pub(crate) const EXIT_PLAN_MODE: &str = "ExitPlanMode";

/// `/undo` の巻き戻し対象 (args の `path` を変更前退避するファイル系ツール)。
/// Bash 等の副作用は巻き戻せないため対象外。
const UNDOABLE_FILE_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit"];

/// ExitPlanMode の spec (Plan モード中のみ LLM へ提示)。
fn exit_plan_mode_spec() -> crate::agent::messages::ToolSpec<'static> {
    crate::agent::messages::ToolSpec {
        kind: "function",
        function: crate::agent::messages::ToolSpecFunction {
            name: EXIT_PLAN_MODE,
            description: "Present the finished plan to the user and request approval to exit \
                          plan mode and start executing. Call only when the plan is complete.",
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "string",
                        "description": "The complete step-by-step plan (markdown)"
                    }
                },
                "required": ["plan"]
            }),
        },
    }
}

/// run_turn の future を途中で破棄すると履歴は次のどちらかの不整合で終わり得る:
/// (a) User を積んだ直後 (ストリーム中) — 末尾が User のままで、次のターンで
///     user が 2 連続になり strict alternation のローカルモデルが落ちる。
/// (b) tool_calls つき Assistant を積んでツール実行中 — 対応する Tool 応答が
///     欠けて tool_call_id の対が壊れる。
/// これを (b) 未応答 tool_call への Tool 補填 → (a) 末尾 User への中断
/// Assistant 補填、の順で修復する。完結した履歴には何もしない。
pub(crate) fn repair_interrupted_history(history: &mut Vec<Message>) {
    if let Some(i) = history
        .iter()
        .rposition(|m| matches!(m, Message::Assistant { .. }))
    {
        let ids: Vec<String> = match &history[i] {
            Message::Assistant { tool_calls, .. } => {
                tool_calls.iter().map(|tc| tc.id.clone()).collect()
            }
            _ => unreachable!("rposition matched Assistant"),
        };
        let answered: std::collections::HashSet<&str> = history[i + 1..]
            .iter()
            .filter_map(|m| match m {
                Message::Tool { tool_call_id, .. } => Some(tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        let missing: Vec<String> = ids
            .into_iter()
            .filter(|id| !answered.contains(id.as_str()))
            .collect();
        for id in missing {
            history.push(Message::Tool {
                tool_call_id: id,
                content: INTERRUPT_NOTE.to_string(),
            });
        }
    }

    if matches!(history.last(), Some(Message::User { .. })) {
        history.push(Message::Assistant {
            content: Some(INTERRUPT_NOTE.to_string()),
            tool_calls: vec![],
        });
    }
}

/// 要約対象メッセージを 1 本のテキストへ整形する（要約・goal 評価器 LLM への入力用）。
pub(crate) fn render_for_summary(msgs: &[Message]) -> String {
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
        usage: Option<Usage>,
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
                usage: self.usage,
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
                usage: self.usage,
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
            usage: None,
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
            usage: None,
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
            usage: None,
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
            usage: None,
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

    fn session_with_window(window: u64) -> Session {
        let mut cfg = Config::default();
        // 既定 provider は Local。
        cfg.llm.local.context_window = window;
        Session::new(cfg, Arc::new(default_registry()))
    }

    fn llm_with_prompt_tokens(prompt_tokens: u64) -> FinalTextLlm {
        FinalTextLlm {
            text: "SUMMARY".into(),
            usage: Some(Usage {
                prompt_tokens,
                completion_tokens: 5,
                total_tokens: prompt_tokens + 5,
            }),
        }
    }

    /// context_window = 0 は自動圧縮無効。どれだけ使っても発火しない。
    #[tokio::test]
    async fn auto_compact_disabled_when_window_zero() {
        let mut session = session_with_window(0);
        let llm = llm_with_prompt_tokens(1_000_000);
        let gate = PermissionGate::new(true);
        session.run_turn("hi", &llm, &gate).await.unwrap();
        assert!(!session.should_auto_compact());
    }

    /// しきい値はちょうど 80% で発火 (>=)、その手前では発火しない。
    #[tokio::test]
    async fn auto_compact_threshold_boundary() {
        let gate = PermissionGate::new(true);

        let mut at = session_with_window(100);
        at.run_turn("hi", &llm_with_prompt_tokens(80), &gate)
            .await
            .unwrap();
        assert!(at.should_auto_compact(), "80/100 must trigger");

        let mut below = session_with_window(100);
        below
            .run_turn("hi", &llm_with_prompt_tokens(79), &gate)
            .await
            .unwrap();
        assert!(!below.should_auto_compact(), "79/100 must not trigger");
    }

    /// しきい値超過中にターンを重ねると、ターン終端の自動圧縮で要約に畳まれる。
    #[tokio::test]
    async fn auto_compact_fires_at_turn_end() {
        let mut session = session_with_window(100);
        let llm = llm_with_prompt_tokens(90);
        let gate = PermissionGate::new(true);
        // 1-2 ターン目はしきい値超過でも履歴不足で Skipped。3 ターン目で圧縮される。
        for p in ["t1", "t2", "t3"] {
            session.run_turn(p, &llm, &gate).await.unwrap();
        }
        assert!(
            session.history().iter().any(|m| matches!(
                m,
                Message::User { content } if content.contains("Summary of earlier conversation")
            )),
            "turn end above threshold should auto-compact history"
        );
    }

    /// LLM に渡ったツール名リストをターンごとに記録するモック。
    struct SpecRecordingLlm {
        seen: std::sync::Mutex<Vec<Vec<String>>>,
    }

    #[async_trait]
    impl LlmClient for SpecRecordingLlm {
        async fn chat(
            &self,
            _h: &[Message],
            _t: &[ToolSpec<'_>],
            _m: &str,
            _mt: Option<u32>,
        ) -> Result<ChatResponse> {
            unreachable!("not used")
        }

        async fn chat_stream(
            &self,
            _h: &[Message],
            t: &[ToolSpec<'_>],
            _m: &str,
            sink: mpsc::UnboundedSender<ChatEvent>,
        ) -> Result<()> {
            self.seen
                .lock()
                .unwrap()
                .push(t.iter().map(|s| s.function.name.to_string()).collect());
            let _ = sink.send(ChatEvent::Done(ChatResponse {
                content: Some("done".into()),
                tool_calls: vec![],
                usage: None,
            }));
            Ok(())
        }
    }

    /// 初回だけ指定の tool_call 群を返し、以後は最終テキストを返すモック。
    struct CallThenDoneLlm {
        calls: Vec<crate::agent::messages::ToolCall>,
        called: std::sync::atomic::AtomicBool,
    }

    impl CallThenDoneLlm {
        fn one(call: crate::agent::messages::ToolCall) -> Self {
            Self {
                calls: vec![call],
                called: false.into(),
            }
        }
    }

    #[async_trait]
    impl LlmClient for CallThenDoneLlm {
        async fn chat(
            &self,
            _h: &[Message],
            _t: &[ToolSpec<'_>],
            _m: &str,
            _mt: Option<u32>,
        ) -> Result<ChatResponse> {
            unreachable!("not used")
        }

        async fn chat_stream(
            &self,
            _h: &[Message],
            _t: &[ToolSpec<'_>],
            _m: &str,
            sink: mpsc::UnboundedSender<ChatEvent>,
        ) -> Result<()> {
            let first = !self.called.swap(true, std::sync::atomic::Ordering::SeqCst);
            let resp = if first {
                ChatResponse {
                    content: None,
                    tool_calls: self.calls.clone(),
                    usage: None,
                }
            } else {
                ChatResponse {
                    content: Some("done".into()),
                    tool_calls: vec![],
                    usage: None,
                }
            };
            let _ = sink.send(ChatEvent::Done(resp));
            Ok(())
        }
    }

    /// ExitPlanMode は Plan 中のみ specs に現れる。
    #[tokio::test]
    async fn exit_plan_mode_spec_visible_only_in_plan() {
        let mut session = session_with_stop_hook(None);
        let llm = SpecRecordingLlm {
            seen: std::sync::Mutex::new(Vec::new()),
        };
        let gate = PermissionGate::new(true);

        session.set_mode(Mode::Plan);
        session.run_turn("plan it", &llm, &gate).await.unwrap();
        session.set_mode(Mode::Normal);
        session.run_turn("do it", &llm, &gate).await.unwrap();

        let seen = llm.seen.lock().unwrap();
        assert!(seen[0].contains(&EXIT_PLAN_MODE.to_string()));
        assert!(!seen[1].contains(&EXIT_PLAN_MODE.to_string()));
    }

    /// 承認 (auto_approve) されると Normal へ遷移し、実行続行の指示が返る。
    #[tokio::test]
    async fn exit_plan_mode_approved_switches_to_normal() {
        let mut session = session_with_stop_hook(None);
        let llm = CallThenDoneLlm::one(tool_call_with_args(
            "p1",
            EXIT_PLAN_MODE,
            r#"{"plan": "1. do X\n2. do Y"}"#,
        ));
        let gate = PermissionGate::new(true);
        session.set_mode(Mode::Plan);
        session.run_turn("plan ready", &llm, &gate).await.unwrap();

        assert_eq!(session.mode(), Mode::Normal, "approval must exit plan mode");
        assert!(session.history().iter().any(|m| matches!(
            m,
            Message::Tool { content, .. } if content.contains("approved")
        )));
    }

    /// 同一バッチ [ExitPlanMode, Write] では、承認後の残り tool_call を実行せず
    /// スキップ応答にする (plan ガード素通り防止)。
    #[tokio::test]
    async fn exit_plan_mode_skips_rest_of_batch_after_approval() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("should_not_exist.txt");
        let write_args = format!(r#"{{"path": "{}", "content": "x"}}"#, target.display());
        let mut session = session_with_stop_hook(None);
        let llm = CallThenDoneLlm {
            calls: vec![
                tool_call_with_args("p1", EXIT_PLAN_MODE, r#"{"plan": "1. write file"}"#),
                tool_call_with_args("w1", "Write", &write_args),
            ],
            called: false.into(),
        };
        let gate = PermissionGate::new(true);
        session.set_mode(Mode::Plan);
        session.run_turn("plan ready", &llm, &gate).await.unwrap();

        assert_eq!(session.mode(), Mode::Normal);
        assert!(
            !target.exists(),
            "Write in the same batch as the approval must not execute"
        );
        // Write への応答は「スキップ・再発行せよ」で tool_call_id 対は維持される。
        assert!(session.history().iter().any(|m| matches!(
            m,
            Message::Tool { tool_call_id, content }
                if tool_call_id == "w1" && content.contains("Re-issue")
        )));
    }

    /// Normal 中に呼ばれた ExitPlanMode はエラー応答でモードも変わらない。
    #[tokio::test]
    async fn exit_plan_mode_outside_plan_errors() {
        let mut session = session_with_stop_hook(None);
        let llm = CallThenDoneLlm::one(tool_call_with_args(
            "p1",
            EXIT_PLAN_MODE,
            r#"{"plan": "whatever"}"#,
        ));
        let gate = PermissionGate::new(true);
        session.run_turn("hi", &llm, &gate).await.unwrap();

        assert_eq!(session.mode(), Mode::Normal);
        assert!(session.history().iter().any(|m| matches!(
            m,
            Message::Tool { content, .. } if content.contains("only available in plan mode")
        )));
    }

    /// plan 引数が空ならエラーで Plan のまま。
    #[tokio::test]
    async fn exit_plan_mode_requires_plan_argument() {
        let mut session = session_with_stop_hook(None);
        let llm = CallThenDoneLlm::one(tool_call_with_args("p1", EXIT_PLAN_MODE, "{}"));
        let gate = PermissionGate::new(true);
        session.set_mode(Mode::Plan);
        session.run_turn("plan ready", &llm, &gate).await.unwrap();

        assert_eq!(session.mode(), Mode::Plan, "missing plan must not exit");
        assert!(session.history().iter().any(|m| matches!(
            m,
            Message::Tool { content, .. } if content.contains("non-empty 'plan'")
        )));
    }

    /// Plan 中は破壊的ツールが LLM の specs から消え、Normal へ戻すと復活する。
    #[tokio::test]
    async fn plan_mode_hides_destructive_specs() {
        let mut session = session_with_stop_hook(None);
        let llm = SpecRecordingLlm {
            seen: std::sync::Mutex::new(Vec::new()),
        };
        let gate = PermissionGate::new(true);

        session.set_mode(Mode::Plan);
        session.run_turn("plan it", &llm, &gate).await.unwrap();
        session.set_mode(Mode::Normal);
        session.run_turn("do it", &llm, &gate).await.unwrap();

        let seen = llm.seen.lock().unwrap();
        let plan_specs = &seen[0];
        let normal_specs = &seen[1];
        for destructive in ["Write", "Edit", "Bash"] {
            assert!(
                !plan_specs.contains(&destructive.to_string()),
                "plan specs must hide {destructive}: {plan_specs:?}"
            );
            assert!(
                normal_specs.contains(&destructive.to_string()),
                "normal specs must include {destructive}: {normal_specs:?}"
            );
        }
        assert!(plan_specs.contains(&"Read".to_string()));
    }

    /// specs から隠していても呼ばれた破壊的ツールは実行されずエラー応答になる。
    #[tokio::test]
    async fn plan_mode_blocks_destructive_execution() {
        let mut session = session_with_stop_hook(None);
        let llm = CallThenDoneLlm::one(tool_call_named("w1", "Write"));
        // auto-approve でもモードチェックが先に効くことを確認する。
        let gate = PermissionGate::new(true);
        session.set_mode(Mode::Plan);
        session
            .run_turn("write something", &llm, &gate)
            .await
            .unwrap();

        let blocked = session.history().iter().any(|m| {
            matches!(m, Message::Tool { content, .. } if content.contains("plan mode") && content.contains("Write"))
        });
        assert!(blocked, "Write should be rejected with a plan-mode error");
    }

    /// Plan 中のユーザ入力には plan 指示が前置され、Normal では素のまま。
    #[tokio::test]
    async fn plan_mode_wraps_user_input() {
        let mut session = session_with_stop_hook(None);
        let llm = FinalTextLlm {
            text: "done".into(),
            usage: None,
        };
        let gate = PermissionGate::new(true);

        session.set_mode(Mode::Plan);
        session.run_turn("investigate", &llm, &gate).await.unwrap();
        session.set_mode(Mode::Normal);
        session.run_turn("execute", &llm, &gate).await.unwrap();

        let users: Vec<&str> = session
            .history()
            .iter()
            .filter_map(|m| match m {
                Message::User { content } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert!(users[0].starts_with("[plan mode]") && users[0].contains("investigate"));
        assert_eq!(users[1], "execute");
    }

    /// run_turn の Write が undo 台帳に載り、undo_last_turn で巻き戻せる。
    #[tokio::test]
    async fn undo_reverts_write_from_turn() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("made.txt");
        let args = format!(r#"{{"path": "{}", "content": "hello"}}"#, target.display());
        let mut session = session_with_stop_hook(None);
        let llm = CallThenDoneLlm::one(tool_call_with_args("w1", "Write", &args));
        let gate = PermissionGate::new(true);
        session.run_turn("write it", &llm, &gate).await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");

        let report = session.undo_last_turn().unwrap();
        assert_eq!(report.removed.len(), 1);
        assert!(!target.exists(), "undo must remove the created file");
        assert!(session.undo_last_turn().is_none(), "log is consumed");
    }

    /// Bash の副作用は undo 対象外 (台帳に載らない)。
    #[tokio::test]
    async fn undo_ignores_bash_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("via_bash.txt");
        let args = format!(r#"{{"command": ": > '{}'"}}"#, target.display());
        let mut session = session_with_stop_hook(None);
        let llm = CallThenDoneLlm::one(tool_call_with_args("b1", "Bash", &args));
        let gate = PermissionGate::new(true);
        session.run_turn("touch it", &llm, &gate).await.unwrap();
        assert!(target.exists(), "bash should have created the file");
        assert!(
            session.undo_last_turn().is_none(),
            "bash side effects must not be recorded as undoable"
        );
    }

    /// サーバが usage を返すとき: そのまま累積され、estimated は増えない。
    #[tokio::test]
    async fn usage_from_server_accumulates() {
        let mut session = session_with_stop_hook(None);
        let llm = FinalTextLlm {
            text: "done".into(),
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 20,
                total_tokens: 120,
            }),
        };
        let gate = PermissionGate::new(true);
        session.run_turn("one", &llm, &gate).await.unwrap();
        session.run_turn("two", &llm, &gate).await.unwrap();

        let u = session.usage();
        assert_eq!(u.llm_calls, 2);
        assert_eq!(u.estimated_calls, 0);
        assert_eq!(u.prompt_tokens, 200);
        assert_eq!(u.completion_tokens, 40);
        assert_eq!(u.total_tokens, 240);
        assert_eq!(u.last_context_tokens, 100);
    }

    /// usage 非対応サーバ (None): 文字数概算フォールバックで 0 より大きく累積される。
    #[tokio::test]
    async fn usage_fallback_estimates_when_server_omits() {
        let mut session = session_with_stop_hook(None);
        let llm = FinalTextLlm {
            text: "done".into(),
            usage: None,
        };
        let gate = PermissionGate::new(true);
        session
            .run_turn("hello estimate", &llm, &gate)
            .await
            .unwrap();

        let u = session.usage();
        assert_eq!(u.llm_calls, 1);
        assert_eq!(u.estimated_calls, 1);
        assert!(u.prompt_tokens > 0, "system prompt should yield tokens");
        assert!(u.completion_tokens > 0);
        assert_eq!(u.total_tokens, u.prompt_tokens + u.completion_tokens);
    }

    /// 空の `usage: {}` (全ゼロ) を返すサーバも未報告とみなし概算にフォールバックする。
    #[tokio::test]
    async fn usage_all_zero_treated_as_missing() {
        let mut session = session_with_stop_hook(None);
        let llm = FinalTextLlm {
            text: "done".into(),
            usage: Some(Usage::default()),
        };
        let gate = PermissionGate::new(true);
        session.run_turn("hello zero", &llm, &gate).await.unwrap();

        let u = session.usage();
        assert_eq!(u.estimated_calls, 1, "all-zero usage should be estimated");
        assert!(u.total_tokens > 0, "estimate should replace zero usage");
    }

    /// estimate_usage 単体: 文字数 / ESTIMATE_CHARS_PER_TOKEN (切り上げ)。
    #[test]
    fn estimate_usage_counts_chars() {
        let history = [
            Message::System {
                content: "abcdef".into(), // 6 chars → 2 tokens
            },
            Message::User {
                content: "abc".into(), // 3 chars → まとめて 9 chars = 3 tokens
            },
        ];
        let resp = ChatResponse {
            content: Some("abcd".into()), // 4 chars → 2 tokens (切り上げ)
            tool_calls: vec![],
            usage: None,
        };
        let u = estimate_usage(&history, &resp);
        assert_eq!(u.prompt_tokens, 3);
        assert_eq!(u.completion_tokens, 2);
        assert_eq!(u.total_tokens, 5);
    }

    fn tool_call(id: &str) -> crate::agent::messages::ToolCall {
        tool_call_named(id, "Read")
    }

    fn tool_call_named(id: &str, name: &str) -> crate::agent::messages::ToolCall {
        tool_call_with_args(id, name, "{}")
    }

    fn tool_call_with_args(
        id: &str,
        name: &str,
        arguments: &str,
    ) -> crate::agent::messages::ToolCall {
        crate::agent::messages::ToolCall {
            id: id.to_string(),
            kind: "function".to_string(),
            function: crate::agent::messages::ToolCallFunction {
                name: name.to_string(),
                arguments: arguments.to_string(),
            },
        }
    }

    /// 末尾が User (ストリーム中の中断) → 中断 Assistant を補い user 2 連続を防ぐ。
    #[test]
    fn repair_appends_assistant_after_trailing_user() {
        let mut h = vec![
            Message::System {
                content: "s".into(),
            },
            Message::User {
                content: "u1".into(),
            },
        ];
        repair_interrupted_history(&mut h);
        assert_eq!(h.len(), 3);
        assert!(
            matches!(&h[2], Message::Assistant { content: Some(c), .. } if c.contains("interrupted"))
        );
    }

    /// ツール実行中の中断 → 未応答 tool_call だけ Tool 応答が補填される。
    #[test]
    fn repair_fills_missing_tool_responses() {
        let mut h = vec![
            Message::System {
                content: "s".into(),
            },
            Message::User {
                content: "u".into(),
            },
            Message::Assistant {
                content: None,
                tool_calls: vec![tool_call("a"), tool_call("b")],
            },
            Message::Tool {
                tool_call_id: "a".into(),
                content: "done".into(),
            },
        ];
        repair_interrupted_history(&mut h);
        assert_eq!(h.len(), 5);
        assert!(
            matches!(&h[4], Message::Tool { tool_call_id, content } if tool_call_id == "b" && content.contains("interrupted"))
        );
        // 応答済みの "a" は重複補填されない。
        let a_count = h
            .iter()
            .filter(|m| matches!(m, Message::Tool { tool_call_id, .. } if tool_call_id == "a"))
            .count();
        assert_eq!(a_count, 1);
    }

    /// 完結した履歴には何もしない。
    #[test]
    fn repair_leaves_complete_history_untouched() {
        let mut h = vec![
            Message::System {
                content: "s".into(),
            },
            Message::User {
                content: "u".into(),
            },
            Message::Assistant {
                content: Some("done".into()),
                tool_calls: vec![],
            },
        ];
        repair_interrupted_history(&mut h);
        assert_eq!(h.len(), 3);
    }

    /// /compact の要約呼び出しも usage に計上される。
    #[tokio::test]
    async fn compact_records_usage() {
        let mut session = session_with_stop_hook(None);
        let llm = FinalTextLlm {
            text: "SUMMARY".into(),
            usage: None,
        };
        let gate = PermissionGate::new(true);
        for p in ["t1", "t2", "t3"] {
            session.run_turn(p, &llm, &gate).await.unwrap();
        }
        let calls_before = session.usage().llm_calls;
        session.compact(&llm, "").await.unwrap();
        assert_eq!(session.usage().llm_calls, calls_before + 1);
    }
}
