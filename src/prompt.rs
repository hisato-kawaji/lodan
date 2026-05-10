use crate::tools::registry::ToolRegistry;
use std::path::Path;

pub fn build_system_prompt(cwd: &Path, model: &str, registry: &ToolRegistry) -> String {
    let tools = registry
        .names()
        .into_iter()
        .map(|n| format!("- {n}"))
        .collect::<Vec<_>>()
        .join("\n");

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    format!(
        "You are lodan, a coding assistant operating in a terminal.\n\
\n\
Environment:\n\
- cwd: {cwd}\n\
- os: {os} ({arch})\n\
- model: {model}\n\
\n\
Available tools:\n\
{tools}\n\
\n\
Rules:\n\
- Only call tools when the user's request actually needs to inspect or modify the file system, run commands, or search the codebase. For greetings, clarifications, and conceptual questions, reply in plain text without any tool call.\n\
- Use Read before editing existing files.\n\
- Prefer Grep/Glob over Bash for searches.\n\
- Use absolute paths.\n\
- Run commands non-interactively (no prompts, no editors).\n\
- Be concise. No preamble. Never invent file contents.\n\
- Ask before destructive multi-file changes.\n",
        cwd = cwd.display(),
    )
}
