use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::agent::messages::{ToolSpec, ToolSpecFunction};
use crate::tools::{Tool, bash, edit, glob, grep, multi_edit, read, todo_write, write};
use crate::tools::{ask_user_question, kill_shell, monitor, notebook_edit, web_fetch, web_search};

pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    /// モデルに見せるツール名。`None` は全部。隠したツールも登録は残すので、呼ばれたときに
    /// 「存在しない」ではなく「このプロファイルでは無効」と答えられる。
    visible: Option<BTreeSet<String>>,
    /// 定義は送らず、`ToolSearch` で読み込ませてから使わせるツール (#72)。`visible` とは重ならない。
    deferred: BTreeSet<String>,
    /// `ToolSearch` の説明文。遅延ツールの名前と 1 行説明を載せるので、`apply_profile` で作る。
    tool_search_description: String,
}

/// `tool_profile = "core"` でモデルに見せるツール。
pub const CORE_TOOLS: &[&str] = &["Read", "Write", "Edit", "Bash", "Grep", "Glob"];

/// 遅延ツールを読み込む擬似ツール (#72)。registry には登録せず、ループが横取りする。
pub const TOOL_SEARCH: &str = "ToolSearch";
/// `ToolSearch` が 1 回に返す定義の上限。見境なく全部を返すと遅延させた意味がない。
pub const TOOL_SEARCH_MAX_RESULTS: usize = 5;
/// `ToolSearch` の説明に載せる 1 行説明の長さ。
const ONE_LINE_CHARS: usize = 100;
/// MCP ツールの名前の接頭辞 (`mcp__<server>__<tool>`)。
const MCP_PREFIX: &str = "mcp__";

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
            visible: None,
            deferred: BTreeSet::new(),
            tool_search_description: String::new(),
        }
    }

    /// 設定に従ってモデルに見せるツールを絞る。全ツールの登録が済んでから呼ぶこと。
    /// 明示リストにある未登録の名前は無視して返す (呼び出し側が警告できるように)。
    pub fn apply_profile(
        &mut self,
        profile: crate::config::ToolProfile,
        explicit: &[String],
    ) -> Vec<String> {
        self.apply_profile_with_search(profile, explicit, false)
    }

    /// `apply_profile` に加えて、`tool_search` なら「プロファイルで隠したツール」と「MCP ツール」を
    /// 遅延ツールにする: 定義は送らず、`ToolSearch` の説明に名前と 1 行説明だけを載せ、
    /// 読み込まれたら次のリクエストから定義を送る (#72)。
    pub fn apply_profile_with_search(
        &mut self,
        profile: crate::config::ToolProfile,
        explicit: &[String],
        tool_search: bool,
    ) -> Vec<String> {
        let unknown = self.restrict(profile, explicit);
        self.deferred.clear();
        if tool_search {
            // `readonly` が隠すのは破壊的だからで、読み込ませてよいものではない。遅延にもしない
            // (「readonly では破壊的ツールは実行されない」を tool_search が覆さない)。
            let readonly = profile == crate::config::ToolProfile::Readonly && explicit.is_empty();
            let hidden_or_mcp: Vec<String> = self
                .tools
                .iter()
                .filter(|(n, t)| {
                    (!self.is_visible(n) || n.starts_with(MCP_PREFIX))
                        && !(readonly && t.is_destructive())
                })
                .map(|(n, _)| n.clone())
                .collect();
            if let Some(visible) = &mut self.visible {
                visible.retain(|n| !n.starts_with(MCP_PREFIX));
            } else if hidden_or_mcp.iter().any(|n| n.starts_with(MCP_PREFIX)) {
                // `full` は「全部」(None) で表しているので、MCP を外すには実体の集合にする。
                self.visible = Some(
                    self.tools
                        .keys()
                        .filter(|n| !n.starts_with(MCP_PREFIX))
                        .cloned()
                        .collect(),
                );
            }
            self.deferred = hidden_or_mcp.into_iter().collect();
        }
        self.tool_search_description = self.build_tool_search_description();
        unknown
    }

    fn restrict(
        &mut self,
        profile: crate::config::ToolProfile,
        explicit: &[String],
    ) -> Vec<String> {
        use crate::config::ToolProfile;
        let mut unknown = Vec::new();
        self.visible = if !explicit.is_empty() {
            let (known, missing): (Vec<_>, Vec<_>) = explicit
                .iter()
                .cloned()
                .partition(|n| self.tools.contains_key(n));
            unknown = missing;
            Some(known.into_iter().collect())
        } else {
            match profile {
                ToolProfile::Full => None,
                ToolProfile::Core => Some(
                    CORE_TOOLS
                        .iter()
                        .filter(|n| self.tools.contains_key(**n))
                        .map(|n| n.to_string())
                        .collect(),
                ),
                ToolProfile::Readonly => Some(
                    self.tools
                        .values()
                        .filter(|t| !t.is_destructive())
                        .map(|t| t.name().to_string())
                        .collect(),
                ),
            }
        };
        unknown
    }

    /// モデルに見せているか。未登録の名前は false。遅延ツールは読み込まれるまで false。
    pub fn is_visible(&self, name: &str) -> bool {
        self.tools.contains_key(name)
            && !self.deferred.contains(name)
            && self.visible.as_ref().is_none_or(|v| v.contains(name))
    }

    /// `ToolSearch` で読み込める遅延ツールか。
    pub fn is_deferred(&self, name: &str) -> bool {
        self.deferred.contains(name)
    }

    /// 遅延ツールの名前。
    pub fn deferred_names(&self) -> Vec<&str> {
        self.deferred.iter().map(|s| s.as_str()).collect()
    }

    /// 遅延ツールがあるときだけ、`ToolSearch` の定義を返す。
    pub fn tool_search_spec(&self) -> Option<ToolSpec<'_>> {
        (!self.deferred.is_empty()).then(|| ToolSpec {
            kind: "function",
            function: ToolSpecFunction {
                name: TOOL_SEARCH,
                description: &self.tool_search_description,
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "\"select:Name1,Name2\" to load tools by exact name, or keywords to search their names and descriptions"
                        }
                    },
                    "required": ["query"]
                }),
            },
        })
    }

    fn build_tool_search_description(&self) -> String {
        let mut text = String::from(
            "Load tools that are not in your current tool list. Loaded tools appear in the list \
             from your next response; call them only after loading. Tools you can load \
             (name: purpose):\n",
        );
        for name in &self.deferred {
            let purpose = self.tools.get(name).map_or("", |t| t.description());
            text.push_str(&format!("- {name}: {}\n", one_line(purpose)));
        }
        text.push_str(
            "Query with \"select:Name1,Name2\" for exact names, or with keywords to search.",
        );
        text
    }

    /// `ToolSearch` の検索。`select:A,B` は名前の完全一致、それ以外は名前と説明の部分一致
    /// (大文字小文字は区別しない)。返すのは遅延ツールだけ、最大 `TOOL_SEARCH_MAX_RESULTS` 個。
    pub fn search_deferred(&self, query: &str) -> Vec<Arc<dyn Tool>> {
        let query = query.trim();
        let mut hits: Vec<Arc<dyn Tool>> = if let Some(names) = query.strip_prefix("select:") {
            // 名前は大文字小文字を区別しない (小型モデルは `todowrite` と書きがち)。
            names
                .split(',')
                .map(str::trim)
                .filter_map(|want| {
                    self.deferred
                        .iter()
                        .find(|n| n.eq_ignore_ascii_case(want))
                        .and_then(|n| self.tools.get(n).cloned())
                })
                .collect()
        } else {
            let needles: Vec<String> = query.split_whitespace().map(|w| w.to_lowercase()).collect();
            self.deferred
                .iter()
                .filter_map(|n| self.tools.get(n))
                .filter(|t| {
                    let hay = format!("{} {}", t.name(), t.description()).to_lowercase();
                    !needles.is_empty() && needles.iter().any(|w| hay.contains(w))
                })
                .cloned()
                .collect()
        };
        hits.truncate(TOOL_SEARCH_MAX_RESULTS);
        hits
    }

    /// 登録済みの全ツール名 (隠したものを含む)。
    pub fn all_names(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }

    /// 登録済みの全ツール数 (隠したものを含む)。
    pub fn registered_len(&self) -> usize {
        self.tools.len()
    }

    pub fn register(&mut self, t: Arc<dyn Tool>) {
        self.tools.insert(t.name().to_string(), t);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// モデルに見せているツール名。
    pub fn names(&self) -> Vec<&str> {
        self.tools
            .keys()
            .map(|s| s.as_str())
            .filter(|n| self.is_visible(n))
            .collect()
    }

    /// モデルに見せているツール数。
    pub fn len(&self) -> usize {
        self.names().len()
    }

    /// モデルに見せているツールが 1 つも無いか (`len() == 0` と同じ意味)。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn tool_specs(&self) -> Vec<ToolSpec<'_>> {
        self.tool_specs_with(&BTreeSet::new())
    }

    /// 破壊的ツールを除いた specs (プランモードで LLM から不可視にする用)。
    pub fn read_only_tool_specs(&self) -> Vec<ToolSpec<'_>> {
        self.read_only_tool_specs_with(&BTreeSet::new())
    }

    /// 見せているツールに、`ToolSearch` で読み込んだ遅延ツールを足した specs。
    pub fn tool_specs_with(&self, loaded: &BTreeSet<String>) -> Vec<ToolSpec<'_>> {
        self.specs(loaded, |_| true)
    }

    pub fn read_only_tool_specs_with(&self, loaded: &BTreeSet<String>) -> Vec<ToolSpec<'_>> {
        self.specs(loaded, |t| !t.is_destructive())
    }

    /// 呼び出しに応じてよいか: 見せているか、遅延ツールが読み込み済みか。
    pub fn is_usable(&self, name: &str, loaded: &BTreeSet<String>) -> bool {
        self.is_visible(name) || (self.is_deferred(name) && loaded.contains(name))
    }

    fn specs(
        &self,
        loaded: &BTreeSet<String>,
        keep: impl Fn(&dyn Tool) -> bool,
    ) -> Vec<ToolSpec<'_>> {
        self.tools
            .values()
            .filter(|t| self.is_usable(t.name(), loaded) && keep(t.as_ref()))
            .map(|t| ToolSpec {
                kind: "function",
                function: ToolSpecFunction {
                    name: t.name(),
                    description: t.description(),
                    parameters: t.schema(),
                },
            })
            .collect()
    }
}

/// 説明の 1 行目を `ONE_LINE_CHARS` 文字までに詰める。
fn one_line(description: &str) -> String {
    let first = description.lines().next().unwrap_or("").trim();
    let mut out: String = first.chars().take(ONE_LINE_CHARS).collect();
    if first.chars().count() > ONE_LINE_CHARS {
        out.push('…');
    }
    out
}

pub fn default_registry() -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register(Arc::new(read::Read));
    r.register(Arc::new(write::Write));
    r.register(Arc::new(edit::Edit));
    r.register(Arc::new(bash::Bash));
    r.register(Arc::new(grep::Grep));
    r.register(Arc::new(glob::Glob));
    r.register(Arc::new(todo_write::TodoWrite));
    r.register(Arc::new(multi_edit::MultiEdit));
    r.register(Arc::new(notebook_edit::NotebookEdit));
    r.register(Arc::new(web_fetch::WebFetch));
    r.register(Arc::new(web_search::WebSearch));
    r.register(Arc::new(ask_user_question::AskUserQuestion));
    r.register(Arc::new(monitor::Monitor));
    r.register(Arc::new(kill_shell::KillShell));

    r
}

/// サブエージェント (`Task`) に渡す読み取り専用ツールのみの registry。
/// 破壊的ツール (Write/Edit/Bash) と Task 自身は含めないため、headless 実行でも
/// 承認ゲート不要・無限再帰なし。
pub fn read_only_registry() -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register(Arc::new(read::Read));
    r.register(Arc::new(grep::Grep));
    r.register(Arc::new(glob::Glob));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_names(r: &ToolRegistry) -> Vec<String> {
        r.tool_specs()
            .iter()
            .map(|s| s.function.name.to_string())
            .collect()
    }

    #[test]
    fn only_the_listed_tools_opt_into_parallel_execution() {
        // 並列可の宣言は個別の判断。増やすときはこの一覧も理由つきで更新すること。
        let r = default_registry();
        let mut safe: Vec<&str> = r
            .all_names()
            .into_iter()
            .filter(|n| r.get(n).unwrap().parallel_safe())
            .collect();
        safe.sort();
        assert_eq!(safe, ["Glob", "Grep", "Read", "WebFetch", "WebSearch"]);
        for name in safe {
            assert!(!r.get(name).unwrap().is_destructive(), "{name}");
        }
    }

    #[test]
    fn full_profile_shows_everything() {
        let mut r = default_registry();
        assert!(
            r.apply_profile(crate::config::ToolProfile::Full, &[])
                .is_empty()
        );
        assert_eq!(r.len(), r.registered_len());
    }

    #[test]
    fn core_profile_shows_exactly_the_six_core_tools() {
        let mut r = default_registry();
        r.apply_profile(crate::config::ToolProfile::Core, &[]);
        let mut expected: Vec<String> = CORE_TOOLS.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(spec_names(&r), expected);
        assert_eq!(
            r.names(),
            expected.iter().map(String::as_str).collect::<Vec<_>>()
        );
        // 隠しただけで、登録は残っている (呼ばれたら「無効」と答えるため)。
        assert!(r.get("WebFetch").is_some());
        assert!(!r.is_visible("WebFetch"));
        assert_eq!(r.registered_len(), default_registry().registered_len());
    }

    #[test]
    fn readonly_profile_hides_every_destructive_tool() {
        let mut r = default_registry();
        r.apply_profile(crate::config::ToolProfile::Readonly, &[]);
        for name in r.names() {
            assert!(!r.get(name).unwrap().is_destructive(), "{name}");
        }
        assert!(r.is_visible("Read") && !r.is_visible("Bash"));
    }

    #[test]
    fn an_explicit_list_wins_over_the_profile_and_reports_unknown_names() {
        let mut r = default_registry();
        let unknown = r.apply_profile(
            crate::config::ToolProfile::Core,
            &["Read".into(), "TodoWrite".into(), "NoSuchTool".into()],
        );
        assert_eq!(unknown, ["NoSuchTool"]);
        assert_eq!(spec_names(&r), ["Read", "TodoWrite"]);
        assert!(!r.is_empty());

        let mut none = default_registry();
        none.apply_profile(crate::config::ToolProfile::Full, &["NoSuchTool".into()]);
        assert!(none.is_empty(), "nothing visible");
        assert!(
            none.names().is_empty(),
            "is_empty must agree with what the model sees"
        );
    }

    #[test]
    fn plan_mode_specs_respect_the_profile_too() {
        let mut r = default_registry();
        r.apply_profile(crate::config::ToolProfile::Core, &[]);
        let names: Vec<String> = r
            .read_only_tool_specs()
            .iter()
            .map(|s| s.function.name.to_string())
            .collect();
        assert_eq!(names, ["Glob", "Grep", "Read"]);
    }

    #[test]
    fn core_profile_at_least_halves_the_tool_spec_payload() {
        // #72 の受け入れ条件。毎リクエスト送る JSON の大きさで比べる。
        let bytes = |r: &ToolRegistry| serde_json::to_string(&r.tool_specs()).unwrap().len();
        let full = bytes(&default_registry());
        let mut core = default_registry();
        core.apply_profile(crate::config::ToolProfile::Core, &[]);
        let core = bytes(&core);
        assert!(core * 2 <= full, "core = {core} bytes, full = {full} bytes");
    }
    use async_trait::async_trait;
    use serde_json::json;

    use crate::tools::{ToolCtx, ToolError, ToolOutput};

    struct Dyn {
        n: String,
        d: String,
    }
    #[async_trait]
    impl Tool for Dyn {
        fn name(&self) -> &str {
            &self.n
        }
        fn description(&self) -> &str {
            &self.d
        }
        fn schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCtx,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok("ok"))
        }
    }

    fn dyn_tool(name: &str, description: &str) -> Arc<dyn Tool> {
        Arc::new(Dyn {
            n: name.into(),
            d: description.into(),
        })
    }

    #[test]
    fn tool_search_defers_the_tools_the_profile_hides() {
        let mut r = default_registry();
        r.apply_profile_with_search(crate::config::ToolProfile::Core, &[], true);
        assert!(r.is_visible("Read") && !r.is_visible("TodoWrite"));
        assert!(r.is_deferred("TodoWrite") && !r.is_deferred("Read"));
        // 定義は送らないが、ToolSearch の説明には名前と 1 行説明が載る。
        let names: BTreeSet<&str> = r.tool_specs().iter().map(|s| s.function.name).collect();
        assert_eq!(names, CORE_TOOLS.iter().copied().collect::<BTreeSet<_>>());
        let search = r.tool_search_spec().expect("deferred tools exist");
        assert!(search.function.description.contains("- TodoWrite:"));
        // 読み込んだら次の specs に入り、呼び出しにも応じる。
        let loaded: BTreeSet<String> = ["TodoWrite".to_string()].into();
        assert!(r.is_usable("TodoWrite", &loaded));
        assert!(!r.is_usable("TodoWrite", &BTreeSet::new()));
        assert!(
            r.tool_specs_with(&loaded)
                .iter()
                .any(|s| s.function.name == "TodoWrite")
        );
    }

    #[test]
    fn without_tool_search_nothing_is_deferred_and_there_is_no_search_spec() {
        let mut r = default_registry();
        r.apply_profile(crate::config::ToolProfile::Core, &[]);
        assert!(r.deferred_names().is_empty());
        assert!(r.tool_search_spec().is_none());
    }

    /// `readonly` が隠した破壊的ツールは、`tool_search` でも読み込めない (#120 のレビュー)。
    #[test]
    fn readonly_profile_never_defers_destructive_tools() {
        let mut r = default_registry();
        r.register(dyn_tool("mcp__fs__write", "Write a file over MCP"));
        r.apply_profile_with_search(crate::config::ToolProfile::Readonly, &[], true);
        assert!(!r.is_visible("Write") && !r.is_deferred("Write"));
        assert!(
            !r.is_deferred("mcp__fs__write"),
            "MCP tools are destructive"
        );
        assert!(r.search_deferred("select:Write").is_empty());
        assert!(
            r.tool_search_spec().is_none(),
            "nothing to load, so no ToolSearch: {:?}",
            r.deferred_names()
        );
    }

    #[test]
    fn select_ignores_case() {
        let mut r = default_registry();
        r.apply_profile_with_search(crate::config::ToolProfile::Core, &[], true);
        let hits = r.search_deferred("select:todowrite");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name(), "TodoWrite");
    }

    #[test]
    fn full_profile_with_tool_search_defers_only_mcp_tools() {
        let mut r = default_registry();
        r.register(dyn_tool("mcp__fs__read", "Read a file over MCP"));
        r.apply_profile_with_search(crate::config::ToolProfile::Full, &[], true);
        assert!(r.is_visible("Read") && r.is_visible("TodoWrite"));
        assert!(!r.is_visible("mcp__fs__read") && r.is_deferred("mcp__fs__read"));
        assert_eq!(r.deferred_names(), ["mcp__fs__read"]);

        // tool_search 無しなら MCP ツールもこれまでどおり見せる。
        let mut plain = default_registry();
        plain.register(dyn_tool("mcp__fs__read", "Read a file over MCP"));
        plain.apply_profile(crate::config::ToolProfile::Full, &[]);
        assert!(plain.is_visible("mcp__fs__read"));
    }

    #[test]
    fn search_deferred_matches_by_exact_name_or_keyword_and_caps_the_results() {
        let mut r = default_registry();
        r.register(dyn_tool("mcp__fs__read", "Read a file over MCP"));
        r.apply_profile_with_search(crate::config::ToolProfile::Core, &[], true);
        let names = |hits: Vec<Arc<dyn Tool>>| -> Vec<String> {
            hits.iter().map(|t| t.name().to_string()).collect()
        };
        assert_eq!(
            names(r.search_deferred("select:TodoWrite, mcp__fs__read, Read")),
            ["TodoWrite", "mcp__fs__read"],
            "exact names only, and never a tool that is already visible"
        );
        assert_eq!(names(r.search_deferred("MCP")), ["mcp__fs__read"]);
        assert!(names(r.search_deferred("")).is_empty());
        assert!(names(r.search_deferred("nothing-like-this")).is_empty());
        assert!(r.search_deferred("a e i o u").len() <= TOOL_SEARCH_MAX_RESULTS);
    }

    #[test]
    fn one_line_keeps_the_first_line_and_marks_a_cut() {
        assert_eq!(one_line("first\nsecond"), "first");
        let long = "x".repeat(150);
        let cut = one_line(&long);
        assert_eq!(cut.chars().count(), ONE_LINE_CHARS + 1);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn dynamic_name_registers_and_resolves() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(Dyn {
            n: "mcp__fs__read".into(),
            d: "dyn".into(),
        }));
        assert!(r.get("mcp__fs__read").is_some());
        let names = r.names();
        assert!(names.contains(&"mcp__fs__read"));
        let specs = r.tool_specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].function.name, "mcp__fs__read");
    }

    #[test]
    fn default_registry_has_builtins() {
        let r = default_registry();
        assert_eq!(r.len(), 14);
        for n in [
            "Read",
            "Write",
            "Edit",
            "Bash",
            "Grep",
            "Glob",
            "TodoWrite",
            "MultiEdit",
            "NotebookEdit",
            "WebFetch",
            "WebSearch",
            "AskUserQuestion",
            "Monitor",
            "KillShell",
        ] {
            assert!(r.get(n).is_some(), "missing {n}");
        }
    }

    /// 全 built-in の destructive 分類を明示的に固定する (#52)。
    /// 既定が true になったため、分類が変わると承認要否と plan モード可視性が
    /// 変わる。新規ツール追加時はこの表を意図して更新すること。
    #[test]
    fn destructive_classification_is_pinned() {
        let r = default_registry();
        let mut destructive: Vec<&str> = Vec::new();
        let mut read_only: Vec<&str> = Vec::new();
        for name in r.names() {
            if r.get(name).unwrap().is_destructive() {
                destructive.push(name);
            } else {
                read_only.push(name);
            }
        }
        assert_eq!(
            destructive,
            vec![
                "Bash",
                "Edit",
                "KillShell",
                "MultiEdit",
                "NotebookEdit",
                "Write"
            ]
        );
        assert_eq!(
            read_only,
            vec![
                "AskUserQuestion",
                "Glob",
                "Grep",
                "Monitor",
                "Read",
                "TodoWrite",
                "WebFetch",
                "WebSearch"
            ]
        );
    }

    /// 既定 (オーバーライドなし) は安全側 = destructive 扱い (#52)。
    #[test]
    fn default_is_destructive_for_unclassified_tools() {
        let d = Dyn {
            n: "unclassified".into(),
            d: "no is_destructive override".into(),
        };
        assert!(d.is_destructive(), "unclassified tools must be safe-side");
    }

    /// read_only_tool_specs は破壊的ツールを除き、読み取り系は残す。
    #[test]
    fn read_only_specs_exclude_destructive() {
        let r = default_registry();
        let names: Vec<&str> = r
            .read_only_tool_specs()
            .iter()
            .map(|s| s.function.name)
            .collect();
        for destructive in ["Write", "Edit", "Bash", "MultiEdit", "NotebookEdit"] {
            assert!(
                !names.contains(&destructive),
                "{destructive} must be hidden"
            );
        }
        for read_only in ["Read", "Grep", "Glob"] {
            assert!(names.contains(&read_only), "{read_only} must remain");
        }
        assert!(names.len() < r.tool_specs().len());
    }
}
