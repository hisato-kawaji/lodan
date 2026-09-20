//! REPL とヘッドレス実行 (`-p`) が共有する起動処理。
//!
//! LLM クライアント・ツール登録 (built-in / MCP / Task / Skill)・セッションの新規作成と
//! 復元をここに集める。起動時の状況報告 (`skills: 2 loaded` など) は `Notices` 経由で
//! 出すので、呼び出し側が行き先を決められる — REPL は stdout、ヘッドレスは stdout を
//! 機械可読に保つため stderr。

use anyhow::Result;
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
    /// `llm` を通った全ての呼び出しの使用量と予算 (`/cost`、`-p` の結果)。
    pub ledger: Arc<llm::metered::Ledger>,
    /// `/goal` の評価器を別のモデルにする設定があるとき、そのクライアントとモデル名。
    pub goal_evaluator: Option<(Arc<dyn LlmClient>, String)>,
    pub registry: Arc<ToolRegistry>,
    /// MCP サーバが公開する prompt (`/mcp__<server>__<prompt>`)。
    pub mcp_prompts: BTreeMap<String, McpPrompt>,
    /// セッションの間 MCP クライアントを生かしておく (Drop でサブプロセスが kill される)。
    _mcp_clients: Vec<Arc<mcp::client::McpClient>>,
}

impl Runtime {
    pub async fn build(cfg: &Config, notices: Notices) -> Result<Self> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

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

        let mut registry = default_registry();
        // sampling は opt-in サーバにのみ active モデルの LLM を貸す。
        let sampling_ctx = mcp::registry::SamplingContext {
            llm: Arc::clone(&llm),
            model: cfg.llm.active().model.clone(),
        };
        let mcp_outcome = mcp::registry::load_and_register(&mut registry, Some(sampling_ctx))
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
        registry.register(Arc::new(
            agent::subagent::SubAgentTool::new(
                Arc::clone(&llm),
                cfg.llm.active().model.clone(),
                Arc::new(read_only_registry()),
                cwd.clone(),
                cfg.agent.max_iterations,
            )
            .with_rules(rules)
            .with_hooks(
                crate::hooks::effective(&cfg.hooks, &cfg.disabled_hooks),
                cfg.hooks_compat,
            ),
        ));

        // Skill ツール: モデルが名前で手順書を読み込める。skill が無ければ登録しない。
        if !user_skills.is_empty() {
            registry.register(Arc::new(crate::skills::SkillTool::new(user_skills)));
        }

        // 全ツールの登録が済んだところで、モデルに見せる範囲を絞る (#72)。
        for name in registry.apply_profile(cfg.agent.tool_profile, &cfg.agent.tools) {
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
        let spec_bytes = serde_json::to_string(&registry.tool_specs()).map_or(0, |s| s.len());
        crate::runlog::record(
            "tools",
            serde_json::json!({
                "profile": cfg.agent.tool_profile.as_str(),
                "explicit": !cfg.agent.tools.is_empty(),
                "visible": registry.names(),
                "registered": registry.registered_len(),
                "spec_bytes": spec_bytes,
            }),
        );
        if registry.len() < registry.registered_len() {
            notices.say(&format!(
                "tools: {} of {} visible to the model ({spec_bytes} bytes of tool specs)",
                registry.len(),
                registry.registered_len()
            ));
        }

        Ok(Self {
            cwd,
            llm,
            ledger,
            goal_evaluator,
            registry: Arc::new(registry),
            mcp_prompts,
            _mcp_clients: mcp_outcome.clients,
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
        // hook の payload に載せる (`session_id` / `transcript_path`)。
        session.set_hook_env(agent::HookEnv {
            session_id: recorder.as_ref().map(|r| r.id().to_string()),
            transcript_path: recorder.as_ref().map(Recorder::transcript_path),
        });
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
    let resolved = if arg == "last" {
        crate::session::latest_session_id().ok().flatten()
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
            match Recorder::open_resumed(&id, session.history()) {
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
