//! Skills: モデルが起動できる名前付き手順書。
//!
//! `$CWD/.lodan/skills/<name>/SKILL.md` を読み込み、`Skill` ツールとしてモデルに
//! 公開する。ツールの説明に利用可能な skill 一覧 (name + description) を載せ、
//! モデルが `Skill { name }` を呼ぶと当該 skill の本文 (instructions) を返す
//! (progressive disclosure: 本文は呼ばれて初めて文脈に載る)。
//!
//! ユーザーが `/name` で明示起動する slash コマンド (`crate::slash`) と対になる。

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;

use crate::tools::{Tool, ToolCtx, ToolError, ToolOutput};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// frontmatter `name`、無ければディレクトリ名。
    pub name: String,
    /// frontmatter `description` (一覧表示・ツール説明用)。
    pub description: String,
    /// frontmatter を除いた本文。呼ばれたときにモデルへ返す。
    pub instructions: String,
}

/// `dir` (例: `.lodan/skills`) 直下の各サブディレクトリから `SKILL.md` を読み込む。
/// ディレクトリが無ければ空 Vec。読めない個別 skill は警告して飛ばす。name 昇順。
pub fn load_from(dir: &Path) -> Result<Vec<Skill>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut skills = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let sub = entry?.path();
        if !sub.is_dir() {
            continue;
        }
        let manifest = sub.join("SKILL.md");
        if !manifest.is_file() {
            continue;
        }
        let dir_name = sub
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        match std::fs::read_to_string(&manifest) {
            Ok(content) => skills.push(parse_skill(&dir_name, &content)),
            Err(e) => eprintln!("skill[{dir_name}]: read failed: {e}"),
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

/// frontmatter (`name` / `description`) を抜き、本文を instructions とする。
/// frontmatter が無ければ name はディレクトリ名、description は空。
fn parse_skill(dir_name: &str, content: &str) -> Skill {
    if let Some(rest) = content.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---")
    {
        let front = &rest[..end];
        let after = &rest[end + 4..];
        let mut name = dir_name.to_string();
        let mut description = String::new();
        for line in front.lines() {
            if let Some(v) = line.strip_prefix("name:") {
                let v = v.trim().trim_matches('"');
                if !v.is_empty() {
                    name = v.to_string();
                }
            } else if let Some(v) = line.strip_prefix("description:") {
                description = v.trim().trim_matches('"').to_string();
            }
        }
        return Skill {
            name,
            description,
            instructions: after
                .strip_prefix('\n')
                .unwrap_or(after)
                .trim_start()
                .to_string(),
        };
    }

    Skill {
        name: dir_name.to_string(),
        description: String::new(),
        instructions: content.trim_start().to_string(),
    }
}

/// 読み込んだ skills をモデルへ公開する `Skill` ツール。
pub struct SkillTool {
    skills: BTreeMap<String, Skill>,
    description: String,
}

impl SkillTool {
    pub fn new(skills: Vec<Skill>) -> Self {
        let mut listing = String::from(
            "Invoke a named skill to load its full instructions into context, then follow them. \
             Available skills:\n",
        );
        let mut map = BTreeMap::new();
        for skill in skills {
            listing.push_str(&format!("- {}: {}\n", skill.name, skill.description));
            map.insert(skill.name.clone(), skill);
        }
        Self {
            skills: map,
            description: listing,
        }
    }

    /// 公開する skill 名 (schema の enum 用)。
    fn names(&self) -> Vec<String> {
        self.skills.keys().cloned().collect()
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "Skill"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The skill to invoke.",
                    "enum": self.names(),
                }
            },
            "required": ["name"]
        })
    }

    fn is_destructive(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("Skill: missing `name`".into()))?;
        match self.skills.get(name) {
            Some(skill) => Ok(ToolOutput::ok(skill.instructions.clone())),
            None => Ok(ToolOutput::error(format!(
                "unknown skill: {name} (available: {})",
                self.names().join(", ")
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn parse_skill_reads_frontmatter() {
        let s = parse_skill(
            "dirname",
            "---\nname: deploy\ndescription: ship it\n---\nRun the deploy steps.\n",
        );
        assert_eq!(s.name, "deploy");
        assert_eq!(s.description, "ship it");
        assert_eq!(s.instructions, "Run the deploy steps.\n");
    }

    #[test]
    fn parse_skill_defaults_name_to_dir() {
        let s = parse_skill("my-skill", "no frontmatter here\n");
        assert_eq!(s.name, "my-skill");
        assert_eq!(s.description, "");
        assert_eq!(s.instructions, "no frontmatter here\n");
    }

    #[test]
    fn load_from_reads_skill_dirs_sorted() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("zebra")).unwrap();
        fs::write(
            root.join("zebra/SKILL.md"),
            "---\ndescription: z\n---\nz body",
        )
        .unwrap();
        fs::create_dir(root.join("alpha")).unwrap();
        fs::write(
            root.join("alpha/SKILL.md"),
            "---\nname: alpha\ndescription: a\n---\na body",
        )
        .unwrap();
        // SKILL.md の無いディレクトリは無視。
        fs::create_dir(root.join("empty")).unwrap();

        let skills = load_from(root).unwrap();
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "alpha");
        assert_eq!(skills[1].name, "zebra"); // dir 名 fallback
    }

    #[test]
    fn load_from_missing_dir_is_empty() {
        let dir = tempdir().unwrap();
        assert!(load_from(&dir.path().join("nope")).unwrap().is_empty());
    }

    #[tokio::test]
    async fn skill_tool_returns_instructions_or_error() {
        let skills = vec![Skill {
            name: "greet".into(),
            description: "say hi".into(),
            instructions: "Say hello warmly.".into(),
        }];
        let tool = SkillTool::new(skills);
        assert!(tool.description().contains("greet: say hi"));

        let ok = tool
            .execute(
                serde_json::json!({ "name": "greet" }),
                &ToolCtx::new(".".into()),
            )
            .await
            .unwrap();
        assert_eq!(ok.content, "Say hello warmly.");
        assert!(!ok.is_error);

        let miss = tool
            .execute(
                serde_json::json!({ "name": "nope" }),
                &ToolCtx::new(".".into()),
            )
            .await
            .unwrap();
        assert!(miss.is_error);
    }
}
