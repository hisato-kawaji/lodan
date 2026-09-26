//! カスタムエージェント定義 (`.lodan/agents/<name>.md`, #77)。
//!
//! `Task` の `subagent_type` で選べる子エージェントの種類。frontmatter で使えるツール・モデル・
//! 反復上限を、本文で追加の指示を書く。いまは読み取り専用のツール (Read / Grep / Glob) の範囲で
//! 絞り込むだけ — 書き込み可の子は、親の承認ゲートとプランモードを通す形が要るので別 PR。
//!
//! ```markdown
//! ---
//! name: reviewer
//! description: Reviews a diff for correctness and style
//! tools: Read, Grep
//! model: kimi:kimi-k3
//! max_turns: 6
//! ---
//! You are a strict reviewer. Report only concrete problems with file:line.
//! ```
//!
//! ユーザー全体 (`~/.lodan/agents/`) とプロジェクト (`<cwd>/.lodan/agents/`、信頼済みのときだけ)
//! から読み、同名はプロジェクトが勝つ。

use std::path::{Path, PathBuf};

use crate::config::Provider;

/// 既定の子 (読み取り専用の調査エージェント) の名前。定義ファイルでは使えない。
pub const DEFAULT_AGENT: &str = "general-purpose";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// 使えるツール名。空なら既定 (Read / Grep / Glob)。
    pub tools: Vec<String>,
    pub provider: Option<Provider>,
    pub model: Option<String>,
    pub max_turns: Option<usize>,
    /// 本文。子の system prompt の末尾に足す。
    pub prompt: String,
    pub path: PathBuf,
}

/// ユーザー全体とプロジェクトの定義を読む。戻り値は名前順。
pub fn load_agent_defs(
    cwd: &Path,
    home: Option<&Path>,
    include_project: bool,
) -> (Vec<AgentDef>, Vec<String>) {
    let mut by_name = std::collections::BTreeMap::new();
    let mut warnings = Vec::new();
    let mut dirs = Vec::new();
    if let Some(home) = home {
        dirs.push(home.join(".lodan/agents"));
    }
    if include_project {
        dirs.push(cwd.join(".lodan/agents"));
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "md") && p.is_file())
            .collect();
        paths.sort();
        for path in paths {
            match std::fs::read_to_string(&path) {
                Ok(content) => match parse_agent_def(&path, &content) {
                    // 後のディレクトリ (プロジェクト) が同名を上書きする。
                    Ok(def) => {
                        by_name.insert(def.name.clone(), def);
                    }
                    Err(e) => warnings.push(format!("{}: {e}", path.display())),
                },
                Err(e) => warnings.push(format!("{}: {e}", path.display())),
            }
        }
    }
    (by_name.into_values().collect(), warnings)
}

pub fn parse_agent_def(path: &Path, content: &str) -> Result<AgentDef, String> {
    let (front, body) = crate::frontmatter::split(content);
    let field = |key: &str| front.and_then(|f| crate::frontmatter::field(f, key));
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = field("name").filter(|n| !n.is_empty()).unwrap_or(stem);
    if name == DEFAULT_AGENT {
        return Err(format!(
            "`{DEFAULT_AGENT}` is the built-in agent; pick another name"
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("name `{name}` must be [A-Za-z0-9_-]"));
    }
    let tools: Vec<String> = field("tools")
        .map(|v| {
            v.trim_matches(|c| c == '[' || c == ']')
                .split(',')
                .map(|t| t.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();
    // `model: kimi:kimi-k3` は provider とモデル、`model: qwen3.5:9b` はモデルだけ (ollama の
    // name:tag はコロンを含むので、`:` の前が provider として読めるときだけ分ける)。
    let mut provider = field("provider")
        .map(|p| parse_provider(&p).ok_or_else(|| format!("unknown provider `{p}`")))
        .transpose()?;
    let mut model = field("model").filter(|m| !m.is_empty());
    if let Some(m) = &model
        && let Some((head, rest)) = m.split_once(':')
        && let Some(p) = parse_provider(head)
    {
        provider = Some(p);
        model = (!rest.trim().is_empty()).then(|| rest.trim().to_string());
    }
    let max_turns = field("max_turns")
        .map(|v| {
            v.parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("max_turns `{v}` must be a positive integer"))
        })
        .transpose()?;
    Ok(AgentDef {
        name,
        description: field("description").unwrap_or_default(),
        tools,
        provider,
        model,
        max_turns,
        prompt: body.trim().to_string(),
        path: path.to_path_buf(),
    })
}

fn parse_provider(s: &str) -> Option<Provider> {
    <Provider as clap::ValueEnum>::from_str(s.trim(), true).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_field_and_splits_provider_from_model() {
        let def = parse_agent_def(
            Path::new("/p/.lodan/agents/reviewer.md"),
            "---\nname: reviewer\ndescription: strict review\ntools: [Read, \"Grep\"]\nmodel: kimi:kimi-k3\nmax_turns: 6\n---\nReport only problems.\n",
        )
        .unwrap();
        assert_eq!(def.name, "reviewer");
        assert_eq!(def.tools, ["Read", "Grep"]);
        assert_eq!(def.provider, Some(Provider::Kimi));
        assert_eq!(def.model.as_deref(), Some("kimi-k3"));
        assert_eq!(def.max_turns, Some(6));
        assert_eq!(def.prompt, "Report only problems.");
    }

    #[test]
    fn a_model_with_a_tag_keeps_its_colon_and_the_name_defaults_to_the_file_stem() {
        let def = parse_agent_def(
            Path::new("/p/.lodan/agents/local-fast.md"),
            "---\nmodel: qwen3.5:9b\n---\nbe quick",
        )
        .unwrap();
        assert_eq!(def.name, "local-fast");
        assert_eq!(def.provider, None);
        assert_eq!(def.model.as_deref(), Some("qwen3.5:9b"));
        assert!(def.tools.is_empty());
    }

    #[test]
    fn bad_names_providers_and_turns_are_errors() {
        let p = Path::new("/p/.lodan/agents/x.md");
        assert!(parse_agent_def(p, "---\nname: general-purpose\n---\nx").is_err());
        assert!(parse_agent_def(p, "---\nname: has space\n---\nx").is_err());
        assert!(parse_agent_def(p, "---\nprovider: openai\n---\nx").is_err());
        assert!(parse_agent_def(p, "---\nmax_turns: 0\n---\nx").is_err());
        assert!(parse_agent_def(p, "no frontmatter at all").is_ok());
    }

    #[test]
    fn project_definitions_override_user_ones_by_name() {
        let home = tempfile::tempdir().unwrap();
        let cwd = home.path().join("work");
        for (dir, body) in [
            (
                home.path().join(".lodan/agents"),
                "---\ndescription: user\n---\nU",
            ),
            (
                cwd.join(".lodan/agents"),
                "---\ndescription: project\n---\nP",
            ),
        ] {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("reviewer.md"), body).unwrap();
        }
        std::fs::write(home.path().join(".lodan/agents/only-user.md"), "only").unwrap();
        std::fs::write(
            cwd.join(".lodan/agents/broken.md"),
            "---\nmax_turns: x\n---\nb",
        )
        .unwrap();
        let (defs, warnings) = load_agent_defs(&cwd, Some(home.path()), true);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["only-user", "reviewer"]);
        assert_eq!(defs[1].description, "project");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        // 未信頼ならプロジェクトの定義は読まない。
        let (defs, _) = load_agent_defs(&cwd, Some(home.path()), false);
        assert_eq!(defs[1].description, "user");
    }
}
