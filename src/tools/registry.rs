use std::collections::BTreeMap;
use std::sync::Arc;

use crate::agent::messages::{ToolSpec, ToolSpecFunction};
use crate::tools::{
    bash, edit, glob, grep, read, write, Tool,
};

// MVP 外（import はあえて残し、登録行で利用する想定でコメントアウト）
#[allow(unused_imports)]
use crate::tools::{
    ask_user_question, monitor, multi_edit, notebook_edit, todo_write, web_fetch, web_search,
};

pub struct ToolRegistry {
    tools: BTreeMap<&'static str, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, t: Arc<dyn Tool>) {
        self.tools.insert(t.name(), t);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.tools.keys().copied().collect()
    }

    pub fn tool_specs(&self) -> Vec<ToolSpec<'_>> {
        self.tools
            .values()
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

pub fn default_registry() -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register(Arc::new(read::Read));
    r.register(Arc::new(write::Write));
    r.register(Arc::new(edit::Edit));
    r.register(Arc::new(bash::Bash));
    r.register(Arc::new(grep::Grep));
    r.register(Arc::new(glob::Glob));

    // --- MVP スコープ外: 登録は意図的に無効化 ---
    // r.register(Arc::new(todo_write::TodoWrite));
    // r.register(Arc::new(web_fetch::WebFetch));
    // r.register(Arc::new(web_search::WebSearch));
    // r.register(Arc::new(ask_user_question::AskUserQuestion));
    // r.register(Arc::new(monitor::Monitor));
    // r.register(Arc::new(notebook_edit::NotebookEdit));
    // r.register(Arc::new(multi_edit::MultiEdit));

    r
}
