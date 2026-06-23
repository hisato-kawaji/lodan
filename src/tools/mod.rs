pub mod registry;

// MVP コア
pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod write;

// MVP 外（Stage A はスタブのみ。registry での登録行はコメントアウトされている）
pub mod ask_user_question;
pub mod monitor;
pub mod multi_edit;
pub mod notebook_edit;
pub mod todo_write;
pub mod web_fetch;
pub mod web_search;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(thiserror::Error, Debug)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("must read file before writing/editing: {0}")]
    NotReadYet(String),
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
    #[serde(default, alias = "activeForm", skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

pub struct ToolCtx {
    pub cwd: PathBuf,
    pub read_tracker: Arc<Mutex<HashSet<PathBuf>>>,
    pub todos: Arc<Mutex<Vec<TodoItem>>>,
}

impl ToolCtx {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            read_tracker: Arc::new(Mutex::new(HashSet::new())),
            todos: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn mark_read(&self, p: &std::path::Path) {
        if let Ok(mut s) = self.read_tracker.lock() {
            s.insert(p.to_path_buf());
        }
    }

    pub fn was_read(&self, p: &std::path::Path) -> bool {
        self.read_tracker
            .lock()
            .map(|s| s.contains(p))
            .unwrap_or(false)
    }

    pub fn replace_todos(&self, items: Vec<TodoItem>) {
        if let Ok(mut t) = self.todos.lock() {
            *t = items;
        }
    }

    pub fn render_todos(&self) -> String {
        match self.todos.lock() {
            Ok(t) => render_todo_list(&t),
            Err(_) => String::new(),
        }
    }
}

pub fn render_todo_list(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "Todos: (empty)".to_string();
    }
    let mut out = String::from("Todos:\n");
    for item in items {
        let marker = match item.status {
            TodoStatus::Pending => "[ ]",
            TodoStatus::InProgress => "[~]",
            TodoStatus::Completed => "[x]",
        };
        let label = if item.status == TodoStatus::InProgress {
            item.active_form.as_deref().unwrap_or(&item.content)
        } else {
            &item.content
        };
        out.push_str(&format!("  {marker} {} {label}\n", item.id));
    }
    out
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> serde_json::Value;
    fn is_destructive(&self) -> bool {
        false
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError>;
}
