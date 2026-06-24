use std::collections::BTreeMap;
use std::sync::Arc;

use crate::agent::messages::{ToolSpec, ToolSpecFunction};
use crate::tools::{Tool, bash, edit, glob, grep, read, todo_write, write};

// MVP 外（import はあえて残し、登録行で利用する想定でコメントアウト）
#[allow(unused_imports)]
use crate::tools::{ask_user_question, monitor, multi_edit, notebook_edit, web_fetch, web_search};

pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, t: Arc<dyn Tool>) {
        self.tools.insert(t.name().to_string(), t);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
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
    r.register(Arc::new(todo_write::TodoWrite));

    // --- MVP スコープ外: 登録は意図的に無効化 ---
    // r.register(Arc::new(web_fetch::WebFetch));
    // r.register(Arc::new(web_search::WebSearch));
    // r.register(Arc::new(ask_user_question::AskUserQuestion));
    // r.register(Arc::new(monitor::Monitor));
    // r.register(Arc::new(notebook_edit::NotebookEdit));
    // r.register(Arc::new(multi_edit::MultiEdit));

    r
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn default_registry_has_seven_builtins() {
        let r = default_registry();
        assert_eq!(r.len(), 7);
        for n in ["Read", "Write", "Edit", "Bash", "Grep", "Glob", "TodoWrite"] {
            assert!(r.get(n).is_some(), "missing {n}");
        }
    }
}
