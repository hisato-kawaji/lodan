//! REPL とヘッドレス実行 (`-p`) が共有する起動処理。
//!
//! LLM クライアント・ツール登録 (built-in / MCP / Task / Skill)・セッションの新規作成と
//! 復元をここに集める。起動時の状況報告 (`skills: 2 loaded` など) は `Notices` 経由で
//! 出すので、呼び出し側が行き先を決められる — REPL は stdout、ヘッドレスは stdout を
//! 機械可読に保つため stderr。

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent;
use crate::config::Config;
use crate::llm::{self, LlmClient};
use crate::mcp;
use crate::mcp::prompt::McpPrompt;
use crate::session::Recorder;
use crate::tools::registry::{ToolRegistry, default_registry, read_only_registry};

/// Plan→Execute (#65 C1) の手順を実行する子 (`plan-step`) を `Task` に足す。
/// 手順を実行できないとき (機能が off、親に渡せるツールが残らない) は何も足さない。
///
/// ツールは `plan::step_tools` が親に合わせて絞る。親より広くしてはいけない:
/// `subagent_type` の enum は助言でしかなく、モデルは種類を名指しで呼べる
/// (#154 のレビューで実証された)。親が `readonly` なのに子が書けると、
/// 「`readonly` では破壊的ツールは実行されない」が成り立たなくなる。
fn with_plan_step_profile(
    task: agent::subagent::SubAgentTool,
    cfg: &Config,
    llm: &Arc<dyn LlmClient>,
) -> agent::subagent::SubAgentTool {
    if !cfg.agent.plan_execute {
        return task;
    }
    let Some(tools) = agent::plan::step_tools(cfg) else {
        return task;
    };
    task.with_profile(
        agent::agents::PLAN_STEP_AGENT.to_string(),
        // 手順は「そのターンの仕事」なので、調査用の子の上限 (12) ではなく親と同じ予算で回す
        // (#65 C1 の実測: 12 回では手順を落とす)。
        agent::subagent::AgentProfile::for_plan_step(
            Arc::clone(llm),
            cfg.llm.active().model.clone(),
            Arc::new(tools),
            cfg.agent.max_iterations,
        )
        .with_instructions(
            String::new(),
            "runs one step of a plan made by the harness".to_string(),
        ),
    )
}

/// 起動時の状況報告の行き先。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notices {
    Stdout,
    Stderr,
}

impl Notices {
    pub fn say(self, line: &str) {
        match self {
            Notices::Stdout => println!("{line}"),
            Notices::Stderr => eprintln!("{line}"),
        }
    }
}

pub struct Runtime {
    pub cwd: PathBuf,
    pub llm: Arc<dyn LlmClient>,
    /// 承認ゲート。REPL / ヘッドレスのループと、書き込み可のサブエージェントが同じものを使う (#77)。
    pub gate: Arc<crate::permission::PermissionGate>,
    /// 親がプランモードにいるか。子エージェントが破壊的ツールを拒否する判断に使う。
    pub plan_flag: Arc<std::sync::atomic::AtomicBool>,
    /// hook の payload の共通フィールド (`session_id` / `transcript_path`)。親と子で共有する。
    pub hook_env: Arc<std::sync::Mutex<agent::HookEnv>>,
    /// `llm` を通った全ての呼び出しの使用量と予算 (`/cost`、`-p` の結果)。
    pub ledger: Arc<llm::metered::Ledger>,
    /// `/goal` の評価器を別のモデルにする設定があるとき、そのクライアントとモデル名。
    pub goal_evaluator: Option<(Arc<dyn LlmClient>, String)>,
    /// `plan_execute` の計画だけを別のモデルにする設定があるとき、そのクライアントとモデル名 (#65 C1)。
    planner: Option<(Arc<dyn LlmClient>, String)>,
    pub registry: Arc<ToolRegistry>,
    /// MCP サーバが公開する prompt (`/mcp__<server>__<prompt>`)。
    pub mcp_prompts: BTreeMap<String, McpPrompt>,
    /// セッションの間 MCP クライアントを生かしておく (Drop でサブプロセスが kill される)。
    /// 繋いだ MCP サーバ (持ち続けないと子プロセスが終わる)。セッションにも渡して
    /// `tools/list_changed` に追従させる (#83)。
    mcp_servers: Vec<mcp::registry::McpServer>,
}

/// 定義ファイル 1 つぶんの子エージェント設定を組み立てる。`tools:` が無ければ読み取り専用の
/// 3 つ、あれば組み込みツールの範囲で絞る (破壊的なものも可 — 実行は親の承認ゲートを通る、#77)。
/// モデル / provider が指定されていれば専用のクライアントを作る (台帳は共有)。
fn build_agent_profile(
    def: &agent::agents::AgentDef,
    cfg: &Config,
    parent_llm: &Arc<dyn LlmClient>,
    ledger: &Arc<llm::metered::Ledger>,
) -> Result<agent::subagent::AgentProfile> {
    let tools = if def.tools.is_empty() {
        read_only_registry()
    } else {
        // Task / Skill / MCP は入らない (default_registry に無い)。子が子を呼ぶ再帰を作らない。
        // AskUserQuestion は子に渡さない — 子は利用者と直接やり取りしない (`-p` では尋ねる相手もいない)。
        if def.tools.iter().any(|t| t == "AskUserQuestion") {
            anyhow::bail!("AskUserQuestion is not available to sub-agents");
        }
        let mut tools = default_registry();
        let unknown = tools.apply_profile(crate::config::ToolProfile::Full, &def.tools);
        if !unknown.is_empty() {
            anyhow::bail!(
                "tools {} are not available to sub-agents (built-in tools only: {})",
                unknown.join(", "),
                tools.all_names().join(", ")
            );
        }
        if tools.is_empty() {
            anyhow::bail!("no usable tools");
        }
        tools
    };
    let (llm, model) = match (def.provider, &def.model) {
        (None, None) => (Arc::clone(parent_llm), cfg.llm.active().model.clone()),
        (provider, model) => {
            let mut next = cfg.clone();
            if let Some(p) = provider {
                next.llm.provider = p;
            }
            if let Some(m) = model {
                next.llm.active_mut().model = m.clone();
            }
            let client = llm::build_metered_with(&next, ledger)
                .with_context(|| format!("building the client for agent `{}`", def.name))?;
            (client, next.llm.active().model.clone())
        }
    };
    let budget = def.max_turns.unwrap_or(cfg.agent.max_iterations);
    // `.lodan/agents/plan-step.md` で手順の実行を差し替えたときも、手順ぶんの予算で回す。
    // 普通の子のコンストラクタを通すと調査用の上限 (12) に戻り、#156 で直した欠陥が再発する。
    let profile = if def.name == agent::agents::PLAN_STEP_AGENT {
        agent::subagent::AgentProfile::for_plan_step(llm, model, Arc::new(tools), budget)
    } else {
        agent::subagent::AgentProfile::new(llm, model, Arc::new(tools), budget)
    };
    Ok(profile
        .with_instructions(def.prompt.clone(), def.description.clone())
        .with_isolation(def.isolation))
}

impl Runtime {
    /// `interactive` は承認プロンプトを出せるか (REPL なら true、`-p` なら false)。
    pub async fn build(cfg: &Config, notices: Notices, interactive: bool) -> Result<Self> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let gate = Arc::new(crate::permission::PermissionGate::from_config(
            cfg,
            &cwd,
            interactive,
        )?);
        let plan_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_env = Arc::new(std::sync::Mutex::new(agent::HookEnv::default()));

        // 壊れた matcher の hook は一度も発火しない。guard のつもりで置かれたものが黙って
        // 効いていない、という状態で走り出さない。
        for hook in &cfg.hooks {
            hook.validate().map_err(|e| anyhow::anyhow!(e))?;
        }
        // 別のマシンの設定には無い hook を名指しすることもあるので、エラーにはせず知らせるだけ。
        for id in crate::hooks::unknown_disabled(&cfg.hooks, &cfg.disabled_hooks) {
            eprintln!(
                "hooks: disabled_hooks names `{}`, but no hook has that id",
                crate::term::sanitize(id)
            );
        }

        // プロジェクトの skill はモデルへの指示を差し込む。信頼済みのディレクトリでだけ読む (#75)。
        let user_skills = if crate::trust::project_trusted() {
            crate::skills::load_from(&cwd.join(".lodan/skills")).unwrap_or_else(|e| {
                eprintln!("skills: load failed: {e}");
                Vec::new()
            })
        } else {
            Vec::new()
        };
        if !user_skills.is_empty() {
            notices.say(&format!("skills: {} loaded", user_skills.len()));
        }

        let (llm, ledger) = llm::build_metered(cfg)?;
        let goal_evaluator = llm::build_goal_evaluator(cfg, &ledger)?;
        let planner = llm::build_planner(cfg, &ledger)?;

        let mut registry = default_registry();
        // 要件台帳 (#65) は opt-in。有効なときだけツールを見せる。
        if cfg.agent.requirements {
            registry.register(Arc::new(crate::tools::requirements::RequirementsTool));
        }
        // sampling は opt-in サーバにのみ active モデルの LLM を貸す。
        let sampling_ctx = mcp::registry::SamplingContext {
            llm: Arc::clone(&llm),
            model: cfg.llm.active().model.clone(),
        };
        let mcp_outcome =
            mcp::registry::load_and_register(&mut registry, Some(sampling_ctx), interactive)
                .await
                .unwrap_or_else(|e| {
                    eprintln!("{}", crate::term::sanitize(&format!("mcp: {e}")));
                    mcp::registry::LoadOutcome::default()
                });
        if mcp_outcome.servers > 0 {
            notices.say(&format!(
                "mcp: {} server(s), {} tool(s), {} prompt(s), {} resource(s) registered",
                mcp_outcome.servers,
                mcp_outcome.tools,
                mcp_outcome.prompts.len(),
                mcp_outcome.resources
            ));
        }
        let mcp_prompts = mcp_outcome
            .prompts
            .into_iter()
            .map(|p| (p.full_name().to_string(), p))
            .collect();

        // サブエージェント (Task): 読み取り専用ツールで調査を委譲する。
        // LLM クライアントが要るため default_registry ではなくここで登録する。
        // 子は親の承認ゲートを通らないので、同じルールを持たせる (#74)。
        let p = &cfg.permissions;
        let rules = Arc::new(crate::permission_rules::RuleSet::parse(
            &p.allow, &p.deny, &p.ask,
        )?);
        let mut task = agent::subagent::SubAgentTool::new(
            Arc::clone(&llm),
            cfg.llm.active().model.clone(),
            Arc::new(read_only_registry()),
            cwd.clone(),
            cfg.agent.max_iterations,
        )
        .with_rules(rules)
        .with_reasoning_roundtrip(cfg.llm.active().reasoning_roundtrip)
        .with_hooks(
            crate::hooks::effective(&cfg.hooks, &cfg.disabled_hooks),
            cfg.hooks_compat,
        )
        // 書き込み可の子 (#77): 破壊的ツールは親と同じゲートを通し、親のプランモードを見て、
        // 親と同じサンドボックス方針で Bash を走らせる。
        .with_gate(Arc::clone(&gate))
        .with_plan_flag(Arc::clone(&plan_flag))
        .with_hook_env(Arc::clone(&hook_env), cfg.permissions.mode)
        .with_sandbox(crate::sandbox::SandboxPolicy::new(&cfg.sandbox, &cwd))
        .with_write_limit(cfg.agent.write_max_lines)
        // `isolation: worktree` (#77) は git リポジトリの中でだけ。
        .with_git_root(agent::subagent::git_toplevel(&cwd))
        // バックグラウンド実行 (#77) は REPL だけ (`-p` では結果を知らせる次のターンが無い)。
        .with_background(interactive);
        // カスタムエージェント定義 (`.lodan/agents/*.md`, #77)。プロジェクトのものは信頼済みのときだけ。
        let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf());
        let (defs, warnings) =
            agent::agents::load_agent_defs(&cwd, home.as_deref(), crate::trust::project_trusted());
        for w in warnings {
            eprintln!("agents: {}", crate::term::sanitize(&w));
        }
        // Plan→Execute (#65 C1) の手順を実行する子。モデルに見せる種類の一覧には出さないが
        // (`custom_names` が除く)、**schema の enum は助言でしかなく、モデルは種類を名指しで
        // 呼べる** (#154 のレビューで実証された)。だから「親より強い子」を作らない:
        // ツールは**親に見えている範囲と同じ**に絞る (`--tool-profile readonly` なら子も
        // 読み取り専用のまま = `readonly` の意味を壊さない)。破壊的ツールは親と同じ承認ゲート・
        // サンドボックス・プランモードを通る (#77 / #134 と同じ扱い)。
        // 定義ファイルより先に入れてあるので、`.lodan/agents/plan-step.md` があればそちらが勝つ。
        task = with_plan_step_profile(task, cfg, &llm);
        let mut agent_names = Vec::new();
        for def in defs {
            match build_agent_profile(&def, cfg, &llm, &ledger) {
                Ok(profile) => {
                    agent_names.push(def.name.clone());
                    task = task.with_profile(def.name, profile);
                }
                Err(e) => eprintln!(
                    "agents: {} skipped: {}",
                    crate::term::sanitize(&def.name),
                    crate::term::sanitize(&format!("{e:#}"))
                ),
            }
        }
        if !agent_names.is_empty() {
            notices.say(&format!("agents: {}", agent_names.join(", ")));
        }
        registry.register(Arc::new(task));

        // Skill ツール: モデルが名前で手順書を読み込める。skill が無ければ登録しない。
        if !user_skills.is_empty() {
            registry.register(Arc::new(crate::skills::SkillTool::new(user_skills)));
        }

        // 全ツールの登録が済んだところで、モデルに見せる範囲を絞る (#72)。
        for name in registry.apply_profile_with_search(
            cfg.agent.tool_profile,
            &cfg.agent.tools,
            cfg.agent.tool_search,
        ) {
            eprintln!("tools: '{name}' is listed in agent.tools but no such tool is registered");
        }
        // 綴り間違いで全ツールが消えた実行は、ツールなしのまま LLM を呼んで終わるだけで何も
        // 測れない。走らせる前に止める。
        if registry.is_empty() {
            anyhow::bail!(
                "agent.tools / --tools matches no registered tool (registered: {})",
                registry.all_names().join(", ")
            );
        }
        // 毎リクエスト送る定義の大きさ。ToolSearch を見せるならその分も入る (遅延ロードの固定費)。
        let mut specs = registry.tool_specs();
        specs.extend(registry.tool_search_spec());
        let spec_bytes = serde_json::to_string(&specs).map_or(0, |s| s.len());
        let tool_search_bytes = registry
            .tool_search_spec()
            .and_then(|s| serde_json::to_string(&s).ok())
            .map_or(0, |s| s.len());
        crate::runlog::record(
            "tools",
            serde_json::json!({
                "profile": cfg.agent.tool_profile.as_str(),
                "explicit": !cfg.agent.tools.is_empty(),
                "visible": registry.names(),
                "deferred": registry.deferred_names(),
                "registered": registry.registered_len(),
                "spec_bytes": spec_bytes,
                "tool_search_bytes": tool_search_bytes,
            }),
        );
        if registry.len() < registry.registered_len() {
            let deferred = registry.deferred_names().len();
            notices.say(&format!(
                "tools: {} of {} visible to the model ({spec_bytes} bytes of tool specs{})",
                registry.len(),
                registry.registered_len(),
                if deferred > 0 {
                    format!(", {deferred} loadable with ToolSearch")
                } else {
                    String::new()
                }
            ));
        }

        Ok(Self {
            cwd,
            llm,
            gate,
            plan_flag,
            hook_env,
            ledger,
            goal_evaluator,
            planner,
            registry: Arc::new(registry),
            mcp_prompts,
            mcp_servers: mcp_outcome.clients,
        })
    }

    /// `resume` があれば復元、無ければ新規。復元に失敗したら警告して新規にフォールバックする。
    pub fn open_session(
        &self,
        cfg: &Config,
        resume: Option<&str>,
        notices: Notices,
    ) -> (agent::Session, Option<Recorder>) {
        let (mut session, recorder) = match resume {
            Some(arg) => resume_session(arg, &self.cwd, cfg, &self.registry, notices),
            None => new_session(&self.cwd, cfg, &self.registry, notices),
        };
        // 予算が残り少なくなったことを、ループがモデルに伝えられるように。
        session.set_ledger(Arc::clone(&self.ledger));
        if let Some(planner) = &self.planner {
            session.set_planner(planner.clone());
        }
        session.set_mcp_servers(self.mcp_servers.clone());
        // 子エージェントが親のプランモードを見られるように (#77)。
        session.set_plan_flag(Arc::clone(&self.plan_flag));
        // hook の payload に載せる (`session_id` / `transcript_path`)。
        let env = agent::HookEnv {
            session_id: recorder.as_ref().map(|r| r.id().to_string()),
            transcript_path: recorder.as_ref().map(Recorder::transcript_path),
        };
        if let Ok(mut shared) = self.hook_env.lock() {
            *shared = env.clone();
        }
        session.set_hook_env(env);
        (session, recorder)
    }
}

/// 新規セッションを作り、永続化レコーダを用意する。
/// レコーダ作成に失敗してもセッションは続行する (永続化なしの ephemeral)。
fn new_session(
    cwd: &Path,
    cfg: &Config,
    registry: &Arc<ToolRegistry>,
    notices: Notices,
) -> (agent::Session, Option<Recorder>) {
    let session = agent::Session::new(cfg.clone(), Arc::clone(registry));
    let recorder = match Recorder::create(cwd, cfg.llm.provider.as_str(), &cfg.llm.active().model) {
        Ok(r) => {
            notices.say(&format!("session: {}", r.id()));
            Some(r)
        }
        Err(e) => {
            eprintln!("session: persistence disabled ({e})");
            None
        }
    };
    (session, recorder)
}

/// 保存済みセッションを復元する。失敗時は警告して新規セッションにフォールバックする。
fn resume_session(
    arg: &str,
    cwd: &Path,
    cfg: &Config,
    registry: &Arc<ToolRegistry>,
    notices: Notices,
) -> (agent::Session, Option<Recorder>) {
    // `last` は cli が cwd スコープで id に解決してから渡す。ここに残るのは保険で、同じ cwd スコープ。
    let resolved = if arg == "last" {
        crate::session::latest_session_id_in(Some(cwd))
            .ok()
            .flatten()
    } else {
        Some(arg.to_string())
    };

    let Some(id) = resolved else {
        eprintln!("session: no session to resume");
        return new_session(cwd, cfg, registry, notices);
    };

    match crate::session::load_transcript(&id) {
        Ok(prior) => {
            let n = prior.len();
            let session = agent::Session::resume(cfg.clone(), Arc::clone(registry), prior);
            // recorder は復元後の history を基準に「保存済み」位置を決める。
            match Recorder::open_resumed(&id, session.history(), session.compacted_away()) {
                Ok(recorder) => {
                    notices.say(&format!("session: resumed {id} ({n} messages)"));
                    (session, Some(recorder))
                }
                Err(e) => {
                    eprintln!("session: resumed {id} but persistence disabled ({e})");
                    (session, None)
                }
            }
        }
        Err(e) => {
            eprintln!("session: cannot resume {id}: {e}");
            new_session(cwd, cfg, registry, notices)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agents::PLAN_STEP_AGENT;
    use crate::agent::messages::ToolSpec;
    use crate::agent::plan;
    use crate::config::ToolProfile;
    use crate::llm::{ChatEvent, ChatResponse};
    use async_trait::async_trait;
    use tokio::sync::mpsc;

    /// 呼ばれない LLM (プロファイルの組み立てだけを見るテスト用)。
    struct UnusedLlm;

    #[async_trait]
    impl LlmClient for UnusedLlm {
        async fn chat(
            &self,
            _h: &[crate::agent::messages::Message],
            _t: &[ToolSpec<'_>],
            _m: &str,
            _mt: Option<u32>,
        ) -> anyhow::Result<ChatResponse> {
            anyhow::bail!("not called")
        }
        async fn chat_stream(
            &self,
            _h: &[crate::agent::messages::Message],
            _t: &[ToolSpec<'_>],
            _m: &str,
            _sink: mpsc::UnboundedSender<ChatEvent>,
        ) -> anyhow::Result<()> {
            anyhow::bail!("not called")
        }
    }

    fn task_for(cfg: &Config) -> agent::subagent::SubAgentTool {
        let llm: Arc<dyn LlmClient> = Arc::new(UnusedLlm);
        let task = agent::subagent::SubAgentTool::new(
            Arc::clone(&llm),
            "mock".into(),
            Arc::new(read_only_registry()),
            PathBuf::from("."),
            cfg.agent.max_iterations,
        );
        with_plan_step_profile(task, cfg, &llm)
    }

    /// 手順を実行する子は親より強くならない。`readonly` の親の下では子も読み取り専用
    /// (#154 のレビュー: モデルは `subagent_type` を名指しで呼べるので、ここが権限の境界)。
    /// **`Task` に実際に載った**プロファイルを見る — 絞り方だけ正しくて配線が違うのが
    /// `30c1159` の欠陥だった。
    #[test]
    fn the_plan_step_child_never_gets_more_tools_than_the_parent() {
        let tools_of = |cfg: &Config| -> Option<Vec<String>> {
            let task = task_for(cfg);
            let p = task.profile_of(PLAN_STEP_AGENT)?;
            Some(p.tools.names().into_iter().map(str::to_string).collect())
        };
        let writes = |cfg: &Config| -> bool {
            let task = task_for(cfg);
            task.profile_of(PLAN_STEP_AGENT)
                .is_some_and(agent::subagent::SubAgentTool::profile_writes)
        };
        let mut cfg = Config::default();
        // 機能が off なら種類ごと登録しない。
        assert!(tools_of(&cfg).is_none());

        cfg.agent.plan_execute = true;
        // 手順は親と同じ反復予算で回す (調査用の子の上限 12 では手順を落とす。#65 C1 の実測)。
        {
            let task = task_for(&cfg);
            let p = task.profile_of(PLAN_STEP_AGENT).expect("registered");
            assert_eq!(p.max_iterations, cfg.agent.max_iterations);
            assert!(
                cfg.agent.max_iterations > 12,
                "既定が子の上限より大きい前提"
            );
        }
        // 既定 (full): 書ける。利用者と話すツールは渡さない。
        let full = tools_of(&cfg).expect("full has tools");
        assert!(writes(&cfg));
        assert!(full.contains(&"Write".to_string()));
        assert!(!full.contains(&"AskUserQuestion".to_string()));

        // readonly: 子も読み取り専用。
        cfg.agent.tool_profile = ToolProfile::Readonly;
        assert!(!writes(&cfg), "{:?}", tools_of(&cfg));

        // 明示リスト: その範囲だけ。
        cfg.agent.tool_profile = ToolProfile::Full;
        cfg.agent.tools = vec!["Read".into(), "Grep".into()];
        assert_eq!(tools_of(&cfg).unwrap(), vec!["Grep", "Read"]);

        // 子に渡せるものが残らない構成では登録しない (空リスト = 絞らない、にしない)。
        cfg.agent.tools = vec!["AskUserQuestion".into()];
        assert!(tools_of(&cfg).is_none());

        // 分解の可否も同じ判定を見る (計画の LLM 呼び出しを無駄にしない)。
        assert!(!plan::can_run_steps(&cfg));
        cfg.agent.tools.clear();
        assert!(plan::can_run_steps(&cfg));

        // `.lodan/agents/plan-step.md` で差し替えたときも手順ぶんの予算で回す
        // (普通の子のコンストラクタを通すと調査用の上限 12 に戻る。#156 のレビュー)。
        let def = |name: &str| agent::agents::AgentDef {
            name: name.to_string(),
            description: "d".into(),
            tools: vec!["Read".into(), "Write".into()],
            provider: None,
            model: None,
            max_turns: None,
            isolation: agent::agents::Isolation::None,
            prompt: String::new(),
            path: PathBuf::from("plan-step.md"),
        };
        let llm: Arc<dyn LlmClient> = Arc::new(UnusedLlm);
        let ledger = Arc::new(llm::metered::Ledger::new(Default::default()));
        let built = |name: &str| {
            build_agent_profile(&def(name), &cfg, &llm, &ledger)
                .unwrap()
                .max_iterations
        };
        assert_eq!(built(PLAN_STEP_AGENT), cfg.agent.max_iterations);
        // 普通の子は今までどおり子専用の上限で切る。
        assert!(built("digger") < cfg.agent.max_iterations);
    }
}
