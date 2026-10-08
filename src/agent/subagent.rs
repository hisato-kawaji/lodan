//! サブエージェント (`Task` ツール)。
//!
//! メインエージェントが調査タスクを子エージェントへ委譲する。子は読み取り専用の
//! ツール (Read / Grep / Glob) だけを持ち、headless にツールループを回して
//! 最終テキストを 1 つの要約として返す。破壊的操作を持たないため承認ゲート不要、
//! `Task` 自身を含めないため無限再帰しない。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::agent::messages::Message;
use crate::hooks::Lifecycle;
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
    /// `worktree` なら git worktree を切ってその中で走らせる (#77)。省略は種類の既定。
    #[serde(default)]
    isolation: Option<String>,
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
    /// 既定の作業場所 (`isolation:`)。
    pub isolation: Isolation,
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
            isolation: Isolation::None,
        }
    }

    pub fn with_instructions(mut self, instructions: String, description: String) -> Self {
        self.instructions = instructions;
        self.description = description;
        self
    }

    pub fn with_isolation(mut self, isolation: Isolation) -> Self {
        self.isolation = isolation;
        self
    }
}

/// 子のために切った git worktree (#77)。`<git root>/.lodan/worktrees/<種類>-<連番>` に HEAD を
/// detached でチェックアウトする。終わったとき変更が無ければ消し、あれば残して場所を親へ伝える。
struct Worktree {
    git_root: PathBuf,
    /// worktree のルート。
    root: PathBuf,
    /// 子の作業ディレクトリ (親の cwd がリポジトリの下位なら、その相対位置を worktree の中に写す)。
    cwd: PathBuf,
    /// 切ったときの HEAD。
    base: String,
}

/// 残した worktree の説明 (親への結果と SubagentStop の payload)。
struct KeptWorktree {
    path: PathBuf,
    note: String,
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `cwd` を含む git リポジトリのトップ。リポジトリでなければ (git が無くても) None。
pub fn git_toplevel(cwd: &Path) -> Option<PathBuf> {
    git(cwd, &["rev-parse", "--show-toplevel"])
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// worktree を置く場所 (git root 直下)。
const WORKTREES_DIR: &str = ".lodan/worktrees";

impl Worktree {
    fn create(git_root: &Path, parent_cwd: &Path, agent_type: &str) -> Result<Self, String> {
        let base = git(git_root, &["rev-parse", "HEAD"])
            .map_err(|e| format!("{e} (the repository needs at least one commit)"))?;
        let dir = git_root.join(WORKTREES_DIR);
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        Self::exclude_from_status(git_root);
        // 同じ種類の子が同時に走っても衝突しないよう、空いている連番を探す。
        let root = (1..)
            .map(|n| dir.join(format!("{agent_type}-{n}")))
            .find(|p| !p.exists())
            .expect("unbounded range");
        let root_str = root.to_string_lossy().into_owned();
        git(
            git_root,
            &["worktree", "add", "--detach", "--quiet", &root_str, "HEAD"],
        )?;
        // 親の cwd がリポジトリの下位なら、その相対位置を worktree の中に写す (無ければルート)。
        let real_root = std::fs::canonicalize(git_root).unwrap_or_else(|_| git_root.to_path_buf());
        let real_cwd =
            std::fs::canonicalize(parent_cwd).unwrap_or_else(|_| parent_cwd.to_path_buf());
        let cwd = real_cwd
            .strip_prefix(&real_root)
            .ok()
            .map(|rel| root.join(rel))
            .filter(|p| p.is_dir())
            .unwrap_or_else(|| root.clone());
        Ok(Self {
            git_root: git_root.to_path_buf(),
            root,
            cwd,
            base,
        })
    }

    /// `.lodan/worktrees/` が親の `git status` に untracked として出ないよう、`.git/info/exclude`
    /// に 1 行足す (コミットされないローカルの除外。既にあれば何もしない)。失敗しても致命ではない。
    fn exclude_from_status(git_root: &Path) {
        let Ok(rel) = git(git_root, &["rev-parse", "--git-path", "info/exclude"]) else {
            return;
        };
        let path = git_root.join(rel);
        let line = format!("/{WORKTREES_DIR}/");
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        if current.lines().any(|l| l.trim() == line) {
            return;
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let sep = if current.is_empty() || current.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        let _ = std::fs::write(&path, format!("{current}{sep}{line}\n"));
    }

    fn short(sha: &str) -> &str {
        &sha[..sha.len().min(12)]
    }

    /// 子の system prompt に足す説明。
    fn prompt_note(&self) -> String {
        format!(
            "\nYou are working in an isolated git worktree at {} (a fresh checkout of {}; the main \
             checkout is untouched). Make your changes there; the caller will be told where the \
             worktree is.\n",
            self.cwd.display(),
            Self::short(&self.base)
        )
    }

    /// 変更が無ければ worktree を消して None。あれば残して、その説明を返す。
    fn finish(&self) -> Result<Option<KeptWorktree>, String> {
        let status = git(
            &self.root,
            &["status", "--porcelain", "--untracked-files=all"],
        )?;
        let head = git(&self.root, &["rev-parse", "HEAD"])?;
        let changed = status.lines().filter(|l| !l.trim().is_empty()).count();
        if changed == 0 && head == self.base {
            let root = self.root.to_string_lossy().into_owned();
            git(&self.git_root, &["worktree", "remove", "--force", &root])?;
            return Ok(None);
        }
        let mut what = Vec::new();
        if head != self.base {
            what.push(format!(
                "commits {}..{}",
                Self::short(&self.base),
                Self::short(&head)
            ));
        }
        if changed > 0 {
            what.push(format!("{changed} uncommitted change(s)"));
        }
        let note = format!(
            "[worktree] The sub-agent worked in an isolated git worktree at {} ({}). The main \
             checkout is untouched. Review it there (e.g. `git -C {} diff`), merge or cherry-pick \
             what you want, then remove it with `git worktree remove {}`.",
            self.root.display(),
            what.join(", "),
            self.root.display(),
            self.root.display()
        );
        Ok(Some(KeptWorktree {
            path: self.root.clone(),
            note,
        }))
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
    /// 親の承認ゲート。破壊的ツールはこれを通す (無ければ破壊的ツールは一切実行しない、#77)。
    gate: Option<Arc<crate::permission::PermissionGate>>,
    /// 親がプランモードにいるか。立っていれば破壊的ツールを拒否する。
    plan_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// 子の Bash に掛けるサンドボックス方針 (親と同じ)。
    sandbox: Option<crate::sandbox::SandboxPolicy>,
    /// hook の payload に載せる共通フィールドの素性 (親と共有。`session_id` / `transcript_path`)。
    hook_env: Option<Arc<std::sync::Mutex<crate::agent::HookEnv>>>,
    /// 親の権限モード (payload の `permission_mode`。プランモード中は `plan`)。
    permission_mode: crate::config::PermissionMode,
    /// cwd を含む git リポジトリのトップ。無ければ `isolation: worktree` は使えない (#77)。
    git_root: Option<PathBuf>,
}

use crate::agent::agents::{DEFAULT_AGENT, Isolation};

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
            gate: None,
            plan_flag: None,
            sandbox: None,
            hook_env: None,
            permission_mode: crate::config::PermissionMode::default(),
            git_root: None,
        };
        tool.refresh_description();
        tool
    }

    /// `isolation: worktree` のための git リポジトリのトップ (#77)。None なら worktree は切れない。
    pub fn with_git_root(mut self, root: Option<PathBuf>) -> Self {
        self.git_root = root;
        self
    }

    /// hook の payload に親と同じ共通フィールドを載せるための素性 (#134 のレビュー: 子の payload に
    /// `hook_event_name` などが無く、イベントで分岐する guard を素通りした)。
    pub fn with_hook_env(
        mut self,
        env: Arc<std::sync::Mutex<crate::agent::HookEnv>>,
        permission_mode: crate::config::PermissionMode,
    ) -> Self {
        self.hook_env = Some(env);
        self.permission_mode = permission_mode;
        self
    }

    /// 親の `fire_hook` と同じ共通フィールド + イベント固有の `extra`。`cwd` は子の作業ディレクトリ
    /// (worktree の中で走っていればそこ)。
    fn hook_payload(
        &self,
        lc: Lifecycle,
        extra: serde_json::Value,
        cwd: &Path,
    ) -> serde_json::Value {
        let env = self
            .hook_env
            .as_ref()
            .and_then(|e| e.lock().ok().map(|g| g.clone()))
            .unwrap_or_default();
        let in_plan = self
            .plan_flag
            .as_ref()
            .is_some_and(|f| f.load(std::sync::atomic::Ordering::SeqCst));
        let permission_mode = if in_plan {
            serde_json::json!("plan")
        } else {
            serde_json::to_value(self.permission_mode).unwrap_or(serde_json::Value::Null)
        };
        let mut payload = serde_json::json!({
            "session_id": env.session_id,
            "transcript_path": env.transcript_path,
            "cwd": cwd,
            "permission_mode": permission_mode,
            "hook_event_name": lc,
        });
        if let (Some(base), serde_json::Value::Object(extra)) = (payload.as_object_mut(), extra) {
            base.extend(extra);
        }
        payload
    }

    /// 親の承認ゲートを共有する (#77)。これが無い子は破壊的ツールを実行しない。
    pub fn with_gate(mut self, gate: Arc<crate::permission::PermissionGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    pub fn with_plan_flag(mut self, flag: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.plan_flag = Some(flag);
        self
    }

    pub fn with_sandbox(mut self, policy: crate::sandbox::SandboxPolicy) -> Self {
        self.sandbox = Some(policy);
        self
    }

    /// その種類が破壊的ツールを持つか (説明と `is_destructive` 用)。
    fn profile_writes(p: &AgentProfile) -> bool {
        p.tools
            .names()
            .into_iter()
            .any(|n| p.tools.get(n).is_some_and(|t| t.is_destructive()))
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
                let writes = if Self::profile_writes(p) {
                    " (can edit files / run commands; each such call still needs approval)"
                } else {
                    ""
                };
                let isolated = if p.isolation == Isolation::Worktree {
                    " (runs in its own git worktree)"
                } else {
                    ""
                };
                text.push_str(&format!(
                    "; {name} — {}{writes}{isolated}",
                    first_line(&p.description)
                ));
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
        let mut payload = self.hook_payload(lc, extra, &self.cwd);
        if let Some(base) = payload.as_object_mut() {
            base.insert("agent_type".into(), serde_json::json!(agent_type));
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

    async fn run(
        &self,
        agent_type: &str,
        task: &str,
        isolation: Isolation,
    ) -> Result<String, ToolError> {
        use crate::hooks::Lifecycle;
        // worktree は hook を鳴らす前に切る (切れなければ子は走らないので Start も鳴らさない)。
        let worktree = match isolation {
            Isolation::None => None,
            Isolation::Worktree => {
                let Some(git_root) = &self.git_root else {
                    return Err(ToolError::InvalidArgs(
                        "isolation: worktree needs a git repository (the working directory is \
                         not inside one)"
                            .into(),
                    ));
                };
                Some(
                    Worktree::create(git_root, &self.cwd, agent_type)
                        .map_err(|e| ToolError::Other(format!("worktree: {e}")))?,
                )
            }
        };
        let cwd = worktree
            .as_ref()
            .map_or_else(|| self.cwd.clone(), |w| w.cwd.clone());
        self.notify(
            agent_type,
            Lifecycle::SubagentStart,
            serde_json::json!({ "prompt": task, "worktree": worktree.as_ref().map(|w| &w.root) }),
        )
        .await;
        let mut result = self
            .run_inner(agent_type, task, &cwd, worktree.as_ref())
            .await;
        // 結果の成否に関わらず後始末する。残した worktree は親の結果に書き添える (後始末に
        // 失敗したときも、残っている場所は伝える)。
        let kept = match worktree.as_ref().map(Worktree::finish) {
            None => None,
            Some(Ok(kept)) => kept,
            Some(Err(e)) => {
                crate::say!("  ↳ worktree cleanup failed: {}", crate::term::sanitize(&e));
                worktree.as_ref().map(|w| KeptWorktree {
                    path: w.root.clone(),
                    note: format!(
                        "[worktree] The sub-agent's worktree at {} could not be cleaned up ({e}); \
                         it is left in place. Remove it with `git worktree remove --force {}`.",
                        w.root.display(),
                        w.root.display()
                    ),
                })
            }
        };
        if let Some(kept) = &kept {
            crate::say!("  ↳ worktree kept at {}", kept.path.display());
            match &mut result {
                Ok(text) => {
                    text.push_str("\n\n");
                    text.push_str(&kept.note);
                }
                Err(e) => {
                    *e = ToolError::Other(format!("{e}\n\n{}", kept.note));
                }
            }
        }
        let mut last = match &result {
            Ok(text) => serde_json::json!({ "last_assistant_message": text }),
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        };
        if let (Some(kept), Some(obj)) = (&kept, last.as_object_mut()) {
            obj.insert("worktree".into(), serde_json::json!(kept.path));
        }
        self.notify(agent_type, Lifecycle::SubagentStop, last).await;
        result
    }

    /// 親の permission ルールを子にも適用する。
    pub fn with_rules(mut self, rules: Arc<RuleSet>) -> Self {
        self.rules = rules;
        self
    }

    /// 子のツール呼び出しの前後で親の hook を発火する。hook が走れなかったら (読めない判断は)
    /// ブロックに倒す — 親の `fire_hook` と同じ方針。
    async fn tool_hook(
        &self,
        lc: Lifecycle,
        tool_name: &str,
        extra: serde_json::Value,
        cwd: &Path,
    ) -> crate::hooks::HookOutcome {
        let payload = self.hook_payload(lc, extra, cwd);
        match crate::hooks::runner::dispatch(
            lc,
            Some(tool_name),
            &payload,
            &self.hooks,
            self.hooks_compat,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => crate::hooks::HookOutcome::blocked(format!("{lc:?} hook failed: {e:#}")),
        }
    }

    /// 子の中でのツール呼び出しを通すか。拒否するなら、モデルへ返す理由。
    ///
    /// - 破壊的ツール: 親がプランモードなら拒否。親のゲートがあればそれに尋ねる (REPL なら利用者に
    ///   子の名前つきで yes / no のプロンプトが出る、`-p` や deny ルールなら拒否)。ゲートが無ければ
    ///   実行しない (#77)。
    /// - それ以外: 親の deny / ask ルールと、hook の `ask` の希望を見る。
    #[allow(clippy::too_many_arguments)]
    fn admit(
        &self,
        agent_type: &str,
        tool_name: &str,
        args: &serde_json::Value,
        tool: &dyn Tool,
        in_plan: bool,
        hint: Option<crate::hooks::PermissionHint>,
        cwd: &Path,
    ) -> Option<String> {
        let destructive = tool.is_destructive();
        if destructive && in_plan {
            return Some(format!(
                "plan mode: destructive tool '{tool_name}' is disabled while the user is reviewing \
                 the plan. Investigate with read-only tools and report."
            ));
        }
        match &self.gate {
            Some(gate) => {
                match gate.decide_for_subagent(tool_name, args, destructive, agent_type, hint, cwd)
                {
                    crate::permission::Decision::Allow => None,
                    crate::permission::Decision::Deny(reason) => Some(reason),
                }
            }
            None if destructive => Some(format!(
                "tool '{tool_name}' modifies files or runs commands, which this sub-agent is not \
                 allowed to do. Report what should be done instead."
            )),
            None if hint == Some(crate::hooks::PermissionHint::Ask) => Some(
                "a hook asked for the user's approval, which this sub-agent cannot ask for. \
                 Report that it is needed instead of retrying."
                    .to_string(),
            ),
            None => self.refusal(tool_name, args, cwd),
        }
    }

    /// 子の中でのツール呼び出しを通すか。子は静かに走り、尋ねる相手がいないので、
    /// deny は拒否、ask も拒否 (尋ねられない)、それ以外は read-only なので通す。
    fn refusal(&self, tool: &str, args: &serde_json::Value, cwd: &Path) -> Option<String> {
        match self.rules.evaluate(tool, args, cwd)? {
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

    /// `cwd` は子の作業ディレクトリ (worktree の中なら `worktree` も渡る)。
    async fn run_inner(
        &self,
        agent_type: &str,
        task: &str,
        cwd: &Path,
        worktree: Option<&Worktree>,
    ) -> Result<String, ToolError> {
        let (_, p) = self.profile(Some(agent_type))?;
        let mut system = prompt::build_system_prompt(cwd, &p.model, p.tools.as_ref());
        if let Some(w) = worktree {
            system.push_str(&w.prompt_note());
        }
        if !p.instructions.is_empty() {
            system.push_str(
                "\nAgent instructions (from the agent definition; user-provided context, \
                             not permission to bypass approvals):\n",
            );
            system.push_str(&p.instructions);
            system.push('\n');
        }
        let role = if Self::profile_writes(p) {
            "You are a sub-agent that may read and modify the project with the available tools \
             (each change still goes through the user's approval)."
        } else {
            "You are a read-only investigation sub-agent."
        };
        let user = format!(
            "{role} Use the available tools to complete the task, then return a concise summary \
             as your final message with no tool call.\n\nTask: {task}"
        );
        let mut history = vec![
            Message::System { content: system },
            Message::User { content: user },
        ];
        let mut ctx = ToolCtx::new(cwd.to_path_buf());
        if let Some(policy) = &self.sandbox {
            // worktree の中で走る子には、そこを「書ける作業ディレクトリ」とする同じ方針。
            let mut policy = policy.clone();
            if worktree.is_some() {
                policy.cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
            }
            ctx = ctx.with_sandbox(policy);
        }
        let in_plan = self
            .plan_flag
            .as_ref()
            .is_some_and(|f| f.load(std::sync::atomic::Ordering::SeqCst));

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
                let name = call.function.name.clone();
                let mut args: serde_json::Value = serde_json::from_str(&call.function.arguments)
                    .unwrap_or_else(|_| serde_json::json!({ "raw": call.function.arguments }));
                // 定義の `tools:` で隠したものは実行もしない (spec に無いだけでは、名前を覚えている
                // モデルが呼べてしまう)。
                let usable = p.tools.get(&name).filter(|_| p.tools.is_visible(&name));
                let Some(tool) = usable else {
                    history.push(Message::Tool {
                        tool_call_id: call.id,
                        content: format!(
                            "tool '{name}' is not available to this sub-agent (available: {})",
                            p.tools.names().join(", ")
                        ),
                    });
                    continue;
                };
                // 親と同じく PreToolUse hook を通す (#134 のレビュー: 子だけ素通りだった)。読めない
                // 判断はブロックに倒す。
                let pre_payload = serde_json::json!({
                    "tool_name": name, "tool_input": args, "agent_type": agent_type,
                });
                let pre = self
                    .tool_hook(Lifecycle::PreToolUse, &name, pre_payload, cwd)
                    .await;
                let mut executed = false;
                // worktree の中の子は、その外のファイルに触れない (`..` で親のチェックアウトに戻れる
                // ので、ルールの照合基準を worktree にするだけでは足りない)。
                let escaped = worktree.and_then(|w| {
                    crate::permission_rules::escapes_root(&name, &args, cwd, &w.root)
                });
                let mut output = if let Some(reason) = pre.block {
                    ToolOutput::error(format!("blocked by hook: {reason}"))
                } else if let Some(path) = escaped {
                    ToolOutput::error(format!(
                        "path {} is outside this sub-agent's worktree ({}); it can only read and \
                         change files inside the worktree. Do not retry with another spelling.",
                        path.display(),
                        worktree
                            .map(|w| w.root.display().to_string())
                            .unwrap_or_default()
                    ))
                } else {
                    if let Some(updated) = pre.updated_input {
                        args = updated;
                    }
                    match self.admit(
                        agent_type,
                        &name,
                        &args,
                        tool.as_ref(),
                        in_plan,
                        pre.permission,
                        cwd,
                    ) {
                        Some(refusal) => ToolOutput::error(refusal),
                        None => {
                            executed = true;
                            match tool.execute(args.clone(), &ctx).await {
                                Ok(o) => o,
                                Err(e) => ToolOutput::error(format!("tool error: {e}")),
                            }
                        }
                    }
                };
                for c in pre.context {
                    output.content.push_str("\n\n");
                    output
                        .content
                        .push_str(&crate::agent::r#loop::hook_context_block(&c));
                }
                // PostToolUse は実行したときだけ (失敗は PostToolUseFailure)。親と同じ。
                let post_event = match (self.hooks_compat, executed, output.is_error) {
                    (crate::hooks::HooksCompat::V1, _, _) | (_, true, false) => {
                        Some(Lifecycle::PostToolUse)
                    }
                    (_, true, true) => Some(Lifecycle::PostToolUseFailure),
                    (_, false, _) => None,
                };
                if let Some(event) = post_event {
                    // `tool_output` は v1 からの名前、`tool_response` は Claude Code の名前 (親と同じ)。
                    let post_payload = serde_json::json!({
                        "tool_name": name, "tool_input": args,
                        "tool_output": output.content, "tool_response": output.content,
                        "error": output.is_error.then_some(&output.content),
                        "agent_type": agent_type,
                    });
                    let post = self.tool_hook(event, &name, post_payload, cwd).await;
                    if let Some(reason) = post.block {
                        output
                            .content
                            .push_str(&format!("\n\n[post-tool hook] {reason}"));
                    }
                    for c in post.context {
                        output.content.push_str("\n\n");
                        output
                            .content
                            .push_str(&crate::agent::r#loop::hook_context_block(&c));
                    }
                }
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
        let mut schema = serde_json::json!({
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
            },
            "required": ["description", "prompt"]
        });
        // 定義が無ければ従来と同じ schema (小型モデルに余計な項目を見せない)。
        if !self.custom_names().is_empty() {
            schema["properties"]["subagent_type"] = serde_json::json!({
                "type": "string",
                "description": "Which sub-agent to run (see the tool description); omit for the default",
                "enum": self.profiles.keys().collect::<Vec<_>>()
            });
        }
        // git リポジトリの中でだけ見せる (外では使えないので)。
        if self.git_root.is_some() {
            schema["properties"]["isolation"] = serde_json::json!({
                "type": "string",
                "enum": ["worktree", "none"],
                "description": "worktree: run the sub-agent in its own git worktree (a fresh \
                                checkout of HEAD under .lodan/worktrees/) so the main checkout is \
                                untouched; the worktree is removed afterwards if it has no \
                                changes, otherwise its path is reported. Omit for the agent's \
                                default."
            });
        }
        schema
    }

    fn is_destructive(&self) -> bool {
        false
    }

    /// 子エージェントは親の履歴も他の子の結果も見ない。独立した調査を同時に走らせるのが主目的。
    /// 書き込み可の子が定義されているときは並列にしない — 複数の子が同時に承認を求めると、
    /// どの子の要求か分からなくなる (#134 のレビュー)。
    fn parallel_safe(&self) -> bool {
        !self.profiles.values().any(Self::profile_writes)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let args: TaskArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs(format!("Task: {e}")))?;
        let (agent_type, profile) = self.profile(args.subagent_type.as_deref())?;
        let isolation = match args.isolation.as_deref() {
            None => profile.isolation,
            Some(s) => Isolation::parse(s).ok_or_else(|| {
                ToolError::InvalidArgs(format!(
                    "Task: isolation must be `worktree` or `none` (got '{}')",
                    crate::term::sanitize(s)
                ))
            })?,
        };
        // 子は静かに走るので、起動を 1 行知らせて可視性を確保する。
        let label = if agent_type == DEFAULT_AGENT {
            String::new()
        } else {
            format!(" [{agent_type}]")
        };
        let where_ = if isolation == Isolation::Worktree {
            " (worktree)"
        } else {
            ""
        };
        crate::say!(
            "  ↳ Task{label}{where_}: {}",
            crate::term::sanitize(&args.description)
        );
        let summary = self.run(agent_type, &args.prompt, isolation).await?;
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
        assert!(tool.schema()["properties"]["subagent_type"]["enum"].is_array());
        let err = run(Some("nope")).await.unwrap_err().to_string();
        assert!(
            err.contains("unknown subagent_type") && err.contains("echo"),
            "{err}"
        );
        assert!(tool.description().contains("echo — echoes its prompt"));
        assert_eq!(tool.custom_names(), ["echo"]);
    }

    /// `tools:` で隠したツールは、名前を覚えているモデルが呼んでも実行されない (#128 のレビュー)。
    #[tokio::test]
    async fn a_hidden_tool_is_refused_at_execution_time_too() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "secret").unwrap();
        let mut only_read = read_only_registry();
        only_read.apply_profile(crate::config::ToolProfile::Full, &["Read".to_string()]);
        let glob = tool_call(
            "Glob",
            &format!(
                r#"{{"pattern": "*.txt", "path": "{}"}}"#,
                dir.path().display()
            ),
        );
        let tool = SubAgentTool::new(
            Arc::new(ScriptedLlm::new(vec![])),
            "mock".into(),
            Arc::new(read_only_registry()),
            dir.path().to_path_buf(),
            8,
        )
        .with_profile(
            "narrow".into(),
            AgentProfile::new(
                Arc::new(EchoToolOutputLlm { call: glob }),
                "mock".into(),
                Arc::new(only_read),
                3,
            ),
        );
        let ctx = ToolCtx::new(dir.path().to_path_buf());
        let out = tool
            .execute(
                serde_json::json!({ "description": "d", "prompt": "p", "subagent_type": "narrow" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            out.content.contains("not available to this sub-agent"),
            "{}",
            out.content
        );
        assert!(
            !out.content.contains("a.txt"),
            "the hidden Glob must not have run: {}",
            out.content
        );
    }

    /// 書き込み可の子 (#77): 破壊的ツールは親のゲートを通る。ゲート無しでは実行しない、
    /// `--yes` 相当なら実行する、非対話のゲートは拒否する、親がプランモードなら拒否する。
    #[tokio::test]
    async fn destructive_calls_in_a_sub_agent_go_through_the_parents_gate() {
        use crate::permission::PermissionGate;
        use std::sync::atomic::AtomicBool;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.txt");
        let write = |id: &str| {
            let mut c = tool_call(
                "Write",
                &format!(r#"{{"path": "{}", "content": "x"}}"#, target.display()),
            );
            c.id = id.into();
            c
        };
        let mut rw = crate::tools::registry::default_registry();
        rw.apply_profile(
            crate::config::ToolProfile::Full,
            &["Read".to_string(), "Write".to_string()],
        );
        let rw = Arc::new(rw);
        let build = |gate: Option<Arc<PermissionGate>>, flag: Option<Arc<AtomicBool>>| {
            let mut tool = SubAgentTool::new(
                Arc::new(ScriptedLlm::new(vec![])),
                "mock".into(),
                Arc::new(read_only_registry()),
                dir.path().to_path_buf(),
                8,
            )
            .with_profile(
                "writer".into(),
                AgentProfile::new(
                    Arc::new(EchoToolOutputLlm { call: write("w") }),
                    "mock".into(),
                    Arc::clone(&rw),
                    3,
                ),
            );
            if let Some(g) = gate {
                tool = tool.with_gate(g);
            }
            if let Some(f) = flag {
                tool = tool.with_plan_flag(f);
            }
            tool
        };
        let ctx = ToolCtx::new(dir.path().to_path_buf());
        let args =
            serde_json::json!({ "description": "d", "prompt": "p", "subagent_type": "writer" });
        let run = |tool: SubAgentTool| {
            let args = args.clone();
            let ctx = &ctx;
            async move { tool.execute(args, ctx).await.unwrap().content }
        };

        // ゲート無し: 実行しない。
        let out = run(build(None, None)).await;
        assert!(out.contains("not allowed to do"), "{out}");
        assert!(!target.exists());
        // 非対話のゲート (= -p): 拒否。
        let out = run(build(
            Some(Arc::new(PermissionGate::non_interactive(false))),
            None,
        ))
        .await;
        assert!(out.contains("non-interactive"), "{out}");
        assert!(!target.exists());
        // プランモード: --yes でも拒否。
        let flag = Arc::new(AtomicBool::new(true));
        let out = run(build(Some(Arc::new(PermissionGate::new(true))), Some(flag))).await;
        assert!(out.contains("plan mode"), "{out}");
        assert!(!target.exists());
        // --yes 相当: 実行される。
        let out = run(build(Some(Arc::new(PermissionGate::new(true))), None)).await;
        assert!(out.contains("wrote"), "{out}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
        assert!(build(None, None).description().contains("can edit files"));
    }

    /// 子のツール呼び出しも親の hook を通る (#134 のレビュー): PreToolUse の exit 2 はブロック、
    /// PostToolUse の additionalContext は tool_result に付く。
    #[tokio::test]
    async fn child_tool_calls_go_through_the_parents_hooks() {
        use crate::hooks::{HookConfig, HooksCompat, Lifecycle};
        use crate::permission::PermissionGate;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.txt");
        let write = tool_call(
            "Write",
            &format!(r#"{{"path": "{}", "content": "x"}}"#, target.display()),
        );
        let mut rw = crate::tools::registry::default_registry();
        rw.apply_profile(
            crate::config::ToolProfile::Full,
            &["Read".to_string(), "Write".to_string()],
        );
        let rw = Arc::new(rw);
        let build = |hooks: Vec<HookConfig>| {
            SubAgentTool::new(
                Arc::new(ScriptedLlm::new(vec![])),
                "mock".into(),
                Arc::new(read_only_registry()),
                dir.path().to_path_buf(),
                8,
            )
            .with_profile(
                "writer".into(),
                AgentProfile::new(
                    Arc::new(EchoToolOutputLlm {
                        call: write.clone(),
                    }),
                    "mock".into(),
                    Arc::clone(&rw),
                    3,
                ),
            )
            .with_gate(Arc::new(PermissionGate::new(true)))
            .with_hooks(hooks, HooksCompat::default())
        };
        let hook = |event: Lifecycle, command: &str| HookConfig {
            id: None,
            event,
            matcher: "Write".into(),
            command: command.into(),
            timeout_secs: None,
        };
        let ctx = ToolCtx::new(dir.path().to_path_buf());
        let args =
            serde_json::json!({ "description": "d", "prompt": "p", "subagent_type": "writer" });

        // PreToolUse が exit 2 でブロック → 実行されない。guard は親と同じ共通フィールドで分岐できる
        // (`hook_event_name` が無いと `exit 0` = 許可に落ちる)。
        let blocked = build(vec![hook(
            Lifecycle::PreToolUse,
            r#"p=$(cat); for k in '"hook_event_name":"PreToolUse"' '"cwd"' '"permission_mode"' '"session_id"'; do printf '%s' "$p" | grep -q "$k" || exit 0; done; echo 'no writes from children' >&2; exit 2"#,
        )]);
        let out = blocked.execute(args.clone(), &ctx).await.unwrap().content;
        assert!(
            out.contains("blocked by hook") && out.contains("no writes from children"),
            "{out}"
        );
        assert!(!target.exists(), "the hook must stop the child's Write");

        // PostToolUse の additionalContext が tool_result に付く。
        let noted = build(vec![hook(
            Lifecycle::PostToolUse,
            r#"cat > /dev/null; printf '%s' '{"hookSpecificOutput":{"additionalContext":"lint ok"}}'"#,
        )]);
        let out = noted.execute(args, &ctx).await.unwrap().content;
        assert!(target.exists());
        assert!(
            out.contains("<hook-context>") && out.contains("lint ok"),
            "{out}"
        );
    }

    /// 定義が無ければ Task の schema は従来と同じ (subagent_type を見せない)。
    #[test]
    fn without_definitions_the_schema_has_no_subagent_type() {
        let dir = tempfile::tempdir().unwrap();
        let tool = subagent(vec![], dir.path().to_path_buf());
        assert!(tool.schema()["properties"].get("subagent_type").is_none());
        assert!(tool.custom_names().is_empty());
    }

    /// 1 コミットある git リポジトリ (worktree のテスト用)。
    fn git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q"]);
        std::fs::write(dir.path().join("tracked.txt"), "tracked").unwrap();
        run(&["add", "tracked.txt"]);
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "init",
        ]);
        dir
    }

    /// `isolation: worktree` (#77): 子は HEAD の新しいチェックアウトで動く (親の未コミットの
    /// ファイルは見えない)。変更が無ければ worktree は消える。リポジトリの外では使えない。
    #[tokio::test]
    async fn a_worktree_child_runs_in_a_fresh_checkout_that_is_removed_when_clean() {
        let repo = git_repo();
        std::fs::write(repo.path().join("scratch.txt"), "SCRATCH-ONLY-IN-MAIN").unwrap();
        let peek = || {
            AgentProfile::new(
                Arc::new(EchoToolOutputLlm {
                    call: tool_call("Read", r#"{"path": "scratch.txt"}"#),
                }),
                "mock".into(),
                Arc::new(read_only_registry()),
                3,
            )
        };
        let tool = subagent(vec![], repo.path().to_path_buf())
            .with_profile("peek".into(), peek())
            .with_git_root(Some(repo.path().to_path_buf()));
        assert!(tool.schema()["properties"].get("isolation").is_some());
        let ctx = ToolCtx::new(repo.path().to_path_buf());
        let run = |isolation: &str| {
            let args = serde_json::json!({
                "description": "d", "prompt": "p", "subagent_type": "peek", "isolation": isolation,
            });
            tool.execute(args, &ctx)
        };
        // 陽性対照: 同じ cwd なら未コミットのファイルが読める。
        let same = run("none").await.unwrap().content;
        assert!(same.contains("SCRATCH-ONLY-IN-MAIN"), "{same}");
        // worktree の中には無い。
        let isolated = run("worktree").await.unwrap().content;
        assert!(!isolated.contains("SCRATCH-ONLY-IN-MAIN"), "{isolated}");
        assert!(
            !isolated.contains("[worktree]"),
            "clean worktree is not reported: {isolated}"
        );
        // 変更が無かったので消えている。
        let left: Vec<_> = std::fs::read_dir(repo.path().join(".lodan/worktrees"))
            .map(|d| d.flatten().collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "{left:?}");
        let exclude = std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
        assert!(
            exclude.lines().any(|l| l == "/.lodan/worktrees/"),
            "{exclude}"
        );

        // リポジトリの外 (git root 無し) では worktree を切れないと言う。schema にも出さない。
        let plain = tempfile::tempdir().unwrap();
        let outside =
            subagent(vec![], plain.path().to_path_buf()).with_profile("peek".into(), peek());
        assert!(outside.schema()["properties"].get("isolation").is_none());
        let err = outside
            .execute(
                serde_json::json!({ "description": "d", "prompt": "p", "subagent_type": "peek", "isolation": "worktree" }),
                &ToolCtx::new(plain.path().to_path_buf()),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs a git repository"), "{err}");
    }

    /// worktree の中の子は外のファイルに触れず、permission ルールは worktree の cwd を基準に照合される
    /// (#137 のレビュー: worktree はリポジトリの中にあるので `..` で親のチェックアウトに戻れ、ルールを
    /// 親の cwd 基準で照合すると相対パターンが一致せず deny をすり抜けた)。
    #[tokio::test]
    async fn a_worktree_child_stays_inside_its_worktree_and_deny_rules_follow_its_cwd() {
        use crate::permission::PermissionGate;
        let repo = git_repo();
        std::fs::write(repo.path().join(".env"), "SECRET-IN-MAIN").unwrap();
        let read = |path: &str| {
            AgentProfile::new(
                Arc::new(EchoToolOutputLlm {
                    call: tool_call("Read", &format!(r#"{{"path": "{path}"}}"#)),
                }),
                "mock".into(),
                Arc::new(read_only_registry()),
                3,
            )
            .with_isolation(Isolation::Worktree)
        };
        let mut rw = crate::tools::registry::default_registry();
        rw.apply_profile(
            crate::config::ToolProfile::Full,
            &["Read".to_string(), "Write".to_string()],
        );
        let rw = Arc::new(rw);
        let write = |path: &str| {
            AgentProfile::new(
                Arc::new(EchoToolOutputLlm {
                    call: tool_call("Write", &format!(r#"{{"path": "{path}", "content": "x"}}"#)),
                }),
                "mock".into(),
                Arc::clone(&rw),
                3,
            )
            .with_isolation(Isolation::Worktree)
        };
        let gate = |deny: &[&str]| {
            let mut cfg = crate::config::Config::default();
            cfg.agent.auto_approve = true;
            cfg.permissions.deny = deny.iter().map(|s| s.to_string()).collect();
            Arc::new(PermissionGate::from_config(&cfg, repo.path(), true).unwrap())
        };
        let build = |deny: &[&str]| {
            subagent(vec![], repo.path().to_path_buf())
                .with_profile("up-env".into(), read("../../../.env"))
                .with_profile(
                    "abs-env".into(),
                    read(&repo.path().join(".env").display().to_string()),
                )
                .with_profile("tracked".into(), read("tracked.txt"))
                .with_profile("up-write".into(), write("../../config.toml"))
                .with_profile("notes".into(), write("notes/a.txt"))
                .with_git_root(Some(repo.path().to_path_buf()))
                .with_gate(gate(deny))
        };
        let ctx = ToolCtx::new(repo.path().to_path_buf());
        let run = |tool: SubAgentTool, kind: &str| {
            let args =
                serde_json::json!({ "description": "d", "prompt": "p", "subagent_type": kind });
            let ctx = &ctx;
            async move { tool.execute(args, ctx).await.unwrap().content }
        };
        // 閉じ込め: 相対でも絶対でも、worktree の外は読めない・書けない (ルールが無くても)。
        let out = run(build(&[]), "up-env").await;
        assert!(
            out.contains("outside this sub-agent's worktree") && !out.contains("SECRET"),
            "{out}"
        );
        let out = run(build(&[]), "abs-env").await;
        assert!(
            out.contains("outside this sub-agent's worktree") && !out.contains("SECRET"),
            "{out}"
        );
        let out = run(build(&[]), "up-write").await;
        assert!(out.contains("outside this sub-agent's worktree"), "{out}");
        assert!(!repo.path().join(".lodan/config.toml").exists());
        // 陽性対照: 中のファイルは読める。
        let out = run(build(&[]), "tracked").await;
        assert!(out.contains("tracked"), "{out}");

        // ルールは worktree の cwd 基準: `/` を含む相対パターンが worktree の中のパスに当たる。
        let ruled = || build(&["Read(tracked.txt)", "Write(notes/**)"]);
        let out = run(ruled(), "tracked").await;
        assert!(out.contains("denied by permission rule"), "{out}");
        let out = run(ruled(), "notes").await;
        assert!(out.contains("denied by permission rule"), "{out}");
        assert!(
            std::fs::read_dir(repo.path().join(".lodan/worktrees"))
                .unwrap()
                .flatten()
                .all(|e| !e.path().join("notes/a.txt").exists()),
            "the denied write must not land in any worktree"
        );
    }

    /// 書き込み可の子が worktree の中で書くと、メインのチェックアウトは変わらず、残した worktree
    /// の場所が結果と SubagentStop の payload で親に伝わる。
    #[tokio::test]
    async fn a_writable_worktree_child_leaves_the_main_checkout_untouched_and_reports_the_worktree()
    {
        let repo = git_repo();
        let mut rw = crate::tools::registry::default_registry();
        rw.apply_profile(
            crate::config::ToolProfile::Full,
            &["Read".to_string(), "Write".to_string()],
        );
        let log = tempfile::NamedTempFile::new().unwrap();
        let tool = subagent(vec![], repo.path().to_path_buf())
            .with_profile(
                "writer".into(),
                AgentProfile::new(
                    Arc::new(EchoToolOutputLlm {
                        call: tool_call(
                            "Write",
                            r#"{"path": "note.txt", "content": "from child"}"#,
                        ),
                    }),
                    "mock".into(),
                    Arc::new(rw),
                    3,
                )
                .with_isolation(Isolation::Worktree),
            )
            .with_git_root(Some(repo.path().to_path_buf()))
            .with_gate(Arc::new(crate::permission::PermissionGate::new(true)))
            .with_hooks(
                vec![crate::hooks::HookConfig {
                    id: None,
                    event: Lifecycle::SubagentStop,
                    matcher: String::new(),
                    command: format!("cat >> {}", log.path().display()),
                    timeout_secs: None,
                }],
                crate::hooks::HooksCompat::V2,
            );
        let out = tool
            .execute(
                serde_json::json!({ "description": "d", "prompt": "p", "subagent_type": "writer" }),
                &ToolCtx::new(repo.path().to_path_buf()),
            )
            .await
            .unwrap()
            .content;
        assert!(
            !repo.path().join("note.txt").exists(),
            "main checkout must stay untouched"
        );
        let wt = repo.path().join(".lodan/worktrees/writer-1");
        assert_eq!(
            std::fs::read_to_string(wt.join("note.txt")).unwrap(),
            "from child"
        );
        assert!(
            out.contains("[worktree]") && out.contains("1 uncommitted change(s)"),
            "{out}"
        );
        assert!(out.contains(&wt.display().to_string()), "{out}");
        let payload: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(log.path()).unwrap()).unwrap();
        assert_eq!(payload["worktree"], wt.display().to_string());
        assert_eq!(payload["agent_type"], "writer");
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
            sub.run(DEFAULT_AGENT, "look around", Isolation::None)
                .await
                .unwrap(),
            "child"
        );

        let by_kind = ledger.by_kind();
        let kinds: Vec<&str> = by_kind.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, [KIND_MAIN, KIND_SUBAGENT]);
        assert_eq!(ledger.total().total_tokens, 84, "parent + child");

        // 予算は共有: 2 件を使い切ったので、次の子は 1 回も LLM を呼べずに失敗する。
        let err = sub
            .run(DEFAULT_AGENT, "again", Isolation::None)
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
            sub.run(DEFAULT_AGENT, "find the answer", Isolation::None)
                .await
                .unwrap(),
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
        let out = sub
            .run(DEFAULT_AGENT, "what is the answer", Isolation::None)
            .await
            .unwrap();
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
        let out = sub
            .run(DEFAULT_AGENT, "find the needle", Isolation::None)
            .await
            .unwrap();
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
            async move {
                sub.run(DEFAULT_AGENT, "look around", Isolation::None)
                    .await
                    .unwrap()
            }
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
        let err = sub
            .run(DEFAULT_AGENT, "loop forever", Isolation::None)
            .await
            .unwrap_err();
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
