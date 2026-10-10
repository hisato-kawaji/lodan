//! 要件台帳 (#65 B1/B2)。利用者の依頼から「検査可能な要件」を抜き出して `.lodan/requirements.json`
//! に持ち、system prompt の末尾に常に描画する (pinned block: 圧縮しても消えない)。モデルは
//! `Requirements` ツールで done / add / drop を記録し、未達が残ったまま終わろうとしたらループが
//! 促す。小型モデルが長い作業の途中で要件を落とす (mini-renovater ベンチの「33 分で要件が脱落」)
//! のは、要件が文脈の中にしか無いからで、ハーネス側に持たせる。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 台帳のファイル名 (cwd の `.lodan/` 直下)。
pub const LEDGER_FILE: &str = "requirements.json";
/// 1 回の抽出で受け付ける要件数と 1 件の長さの上限 (モデルが際限なく書いても台帳が膨らまないように)。
const MAX_ITEMS_PER_EXTRACTION: usize = 30;
const MAX_ITEM_CHARS: usize = 300;
/// 抽出を掛ける依頼の最短の長さ。挨拶や一言の質問には要件が無い。
pub const MIN_PROMPT_CHARS: usize = 40;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Requirement {
    pub id: u32,
    pub text: String,
    #[serde(default)]
    pub done: bool,
    /// done にしたときの根拠 (走らせたコマンドなど)、drop したときの理由。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// 対象外として外した (未達には数えない)。
    #[serde(default)]
    pub dropped: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    items: Vec<Requirement>,
}

/// 台帳本体。`ToolCtx` に `Arc<Mutex<Ledger>>` で載り、ループとツールが共有する。
#[derive(Debug, Default)]
pub struct Ledger {
    items: Vec<Requirement>,
    /// 永続化先。None なら持ち回るだけ (テストや機能 off)。
    path: Option<PathBuf>,
    /// 変わるたびに増える。system prompt の pinned block を作り直す合図。
    version: u64,
}

impl Ledger {
    /// `<cwd>/.lodan/requirements.json` を読む (無ければ空)。読めない内容は捨てて空から始める
    /// (壊れたファイルで起動を止めない)。
    pub fn load(cwd: &Path) -> Self {
        let path = cwd.join(".lodan").join(LEDGER_FILE);
        let items = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<File>(&s).ok())
            .map(|f| f.items)
            .unwrap_or_default();
        Self {
            items,
            path: Some(path),
            version: 0,
        }
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn items(&self) -> &[Requirement] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// done でも drop でもないもの。
    pub fn unmet(&self) -> Vec<&Requirement> {
        self.items
            .iter()
            .filter(|r| !r.done && !r.dropped)
            .collect()
    }

    /// 要件を足して、付けた id を返す。空や長すぎるものは整える。
    pub fn add<I: IntoIterator<Item = String>>(&mut self, texts: I) -> Vec<u32> {
        let mut ids = Vec::new();
        for text in texts.into_iter().take(MAX_ITEMS_PER_EXTRACTION) {
            let text: String = text.trim().chars().take(MAX_ITEM_CHARS).collect();
            if text.is_empty() {
                continue;
            }
            let id = self.items.iter().map(|r| r.id).max().unwrap_or(0) + 1;
            self.items.push(Requirement {
                id,
                text,
                done: false,
                evidence: None,
                dropped: false,
            });
            ids.push(id);
        }
        if !ids.is_empty() {
            self.touch();
        }
        ids
    }

    pub fn mark_done(&mut self, id: u32, evidence: &str) -> Result<(), String> {
        let item = self.get_mut(id)?;
        item.done = true;
        item.dropped = false;
        item.evidence = Some(evidence.trim().to_string()).filter(|e| !e.is_empty());
        self.touch();
        Ok(())
    }

    pub fn drop_item(&mut self, id: u32, reason: &str) -> Result<(), String> {
        let item = self.get_mut(id)?;
        item.dropped = true;
        item.evidence = Some(reason.trim().to_string()).filter(|e| !e.is_empty());
        self.touch();
        Ok(())
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.touch();
    }

    fn get_mut(&mut self, id: u32) -> Result<&mut Requirement, String> {
        self.items
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| format!("no requirement with id {id}"))
    }

    /// 変更のたびに版を上げて保存する。保存の失敗は警告だけ (台帳はメモリ上で生きている)。
    fn touch(&mut self) {
        self.version += 1;
        if let Some(path) = &self.path
            && let Err(e) = self.save_to(path)
        {
            crate::say!(
                "[lodan] requirements: could not save {}: {e}",
                path.display()
            );
        }
    }

    fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(&File {
            items: self.items.clone(),
        })
        .map_err(std::io::Error::other)?;
        // 途中で落ちても前の内容が残るよう、別名で書いてから差し替える。
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text + "\n")?;
        std::fs::rename(&tmp, path)
    }

    /// system prompt に差し込む pinned block。空なら空文字。
    pub fn render(&self) -> String {
        if self.items.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "\nRequirements ledger (kept by lodan outside the conversation, so it survives context \
             compaction; update it with the Requirements tool — mark each item done with evidence \
             once verified, drop items that are out of scope with a reason):\n",
        );
        for r in &self.items {
            let mark = if r.dropped {
                "-"
            } else if r.done {
                "x"
            } else {
                " "
            };
            out.push_str(&format!("  {}. [{mark}] {}", r.id, r.text));
            if let Some(e) = &r.evidence {
                out.push_str(&format!(" — {e}"));
            }
            out.push('\n');
        }
        out
    }

    /// 未達の一覧 (ループの促しと `/requirements` 用)。
    pub fn describe_unmet(&self) -> String {
        self.unmet()
            .iter()
            .map(|r| format!("  {}. {}", r.id, r.text))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `/requirements` の表示。
    pub fn describe(&self) -> String {
        if self.items.is_empty() {
            return "requirements: (none recorded)".to_string();
        }
        let unmet = self.unmet().len();
        let mut out = format!(
            "requirements: {} recorded, {} open{}\n",
            self.items.len(),
            unmet,
            self.path
                .as_ref()
                .map(|p| format!(" ({})", p.display()))
                .unwrap_or_default()
        );
        out.push_str(self.render().trim_start_matches('\n'));
        out
    }
}

/// 抽出の system prompt。依頼文をそのまま user として渡す。
pub const EXTRACT_SYSTEM: &str = "Extract the concrete, checkable requirements stated in the \
    user's request as a JSON array of short strings (one requirement per string, in the user's \
    language, keeping names, paths and exact values). Include only what the request states or \
    clearly requires as a deliverable; do not add advice or steps of your own. If the request is \
    a question, a greeting, or has no deliverable, return []. Output only the JSON array.";

/// 抽出の応答から要件の配列を読む。コードフェンスや前置きが混ざっていても、最初の `[` から
/// 最後の `]` までを JSON として読む。読めなければ空。
pub fn parse_extraction(text: &str) -> Vec<String> {
    let start = text.find('[');
    let end = text.rfind(']');
    let Some((s, e)) = start.zip(end).filter(|(s, e)| s < e) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<serde_json::Value>>(&text[s..=e])
        .map(|items| {
            items
                .into_iter()
                .filter_map(|v| match v {
                    serde_json::Value::String(s) => Some(s),
                    serde_json::Value::Object(o) => o
                        .get("text")
                        .or_else(|| o.get("requirement"))
                        .and_then(|t| t.as_str())
                        .map(str::to_string),
                    _ => None,
                })
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_done_drop_and_render() {
        let mut ledger = Ledger::default();
        let ids = ledger.add([
            "write a.txt".to_string(),
            "  ".to_string(),
            "add a test".to_string(),
        ]);
        assert_eq!(ids, [1, 2]);
        assert_eq!(ledger.unmet().len(), 2);
        ledger.mark_done(1, "ran: cat a.txt").unwrap();
        ledger.drop_item(2, "out of scope").unwrap();
        assert!(ledger.unmet().is_empty());
        assert!(ledger.mark_done(9, "").is_err());
        let block = ledger.render();
        assert!(
            block.contains("1. [x] write a.txt — ran: cat a.txt"),
            "{block}"
        );
        assert!(
            block.contains("2. [-] add a test — out of scope"),
            "{block}"
        );
        // id は消えた後も再利用しない (最大 + 1)。
        assert_eq!(ledger.add(["more".to_string()]), [3]);
        assert_eq!(ledger.version(), 4);
    }

    #[test]
    fn persists_under_dot_lodan_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::load(dir.path());
        assert!(ledger.is_empty());
        ledger.add(["x".to_string()]);
        ledger.mark_done(1, "ok").unwrap();
        let again = Ledger::load(dir.path());
        assert_eq!(again.items(), ledger.items());
        assert!(dir.path().join(".lodan/requirements.json").is_file());
        // 壊れたファイルは空として読む。
        std::fs::write(dir.path().join(".lodan/requirements.json"), "{not json").unwrap();
        assert!(Ledger::load(dir.path()).is_empty());
    }

    #[test]
    fn extraction_tolerates_fences_prose_and_objects() {
        assert_eq!(parse_extraction(r#"["a", "b"]"#), ["a", "b"]);
        assert_eq!(
            parse_extraction("Sure:\n```json\n[\"a\",\n \"b\"]\n```"),
            ["a", "b"]
        );
        assert_eq!(
            parse_extraction(r#"[{"text": "a"}, {"requirement": "b"}, 3]"#),
            ["a", "b"]
        );
        assert!(parse_extraction("[]").is_empty());
        assert!(parse_extraction("no list here").is_empty());
        assert!(parse_extraction("]oops[").is_empty());
    }
}
