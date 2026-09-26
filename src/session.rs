//! 会話履歴の永続化。
//!
//! セッションごとに `<data_dir>/lodan/sessions/<id>/` を作り、
//! `meta.json` にメタ情報 (id / 作成時刻 / cwd / provider / model) を、
//! `transcript.jsonl` に [`Message`] を 1 行 1 件で追記する。
//! `--resume <id>` で transcript を読み戻してエージェントへ再投入する。

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::agent::messages::Message;

#[derive(Debug, Default, Clone)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// セッションのメタ情報 (`meta.json`)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    /// 作成時刻 (Unix epoch ミリ秒)。
    pub created_at_ms: u128,
    pub cwd: String,
    pub provider: String,
    pub model: String,
    /// `/rename` で付けた名前 (#80)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `--fork` / `/fork` で複製した元のセッション (#80)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
}

impl SessionMeta {
    /// この cwd で作られたセッションか。保存した文字列と、実体 (symlink 解決) の両方で比べる。
    pub fn is_in(&self, cwd: &Path) -> bool {
        let saved = Path::new(&self.cwd);
        if saved == cwd {
            return true;
        }
        match (std::fs::canonicalize(saved), std::fs::canonicalize(cwd)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    }
}

/// 進行中セッションを transcript.jsonl へ追記するレコーダ。
pub struct Recorder {
    dir: PathBuf,
    /// transcript.jsonl に書き込み済みのメッセージ数。
    persisted: usize,
}

impl Recorder {
    /// 新規セッションのディレクトリと `meta.json` を作成する。
    pub fn create(cwd: &Path, provider: &str, model: &str) -> Result<Self> {
        let created_at_ms = now_ms();
        let id = format!("{created_at_ms}-{}", std::process::id());
        let dir = sessions_root()
            .context("could not resolve sessions directory")?
            .join(&id);
        fs::create_dir_all(&dir).with_context(|| format!("create session dir {dir:?}"))?;
        // transcript には Read したファイル内容や貼り付けた秘密が平文で残るため、
        // 本人のみアクセス可能に制限する (unix のみ; それ以外は no-op)。
        restrict(&dir, 0o700);

        let meta = SessionMeta {
            id,
            created_at_ms,
            cwd: cwd.display().to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            name: None,
            forked_from: None,
        };
        let meta_path = dir.join("meta.json");
        let transcript_path = dir.join("transcript.jsonl");
        let meta_json = serde_json::to_string_pretty(&meta)?;
        fs::write(&meta_path, meta_json).context("write meta.json")?;
        File::create(&transcript_path).context("create transcript.jsonl")?;
        restrict(&meta_path, 0o600);
        restrict(&transcript_path, 0o600);

        Ok(Self { dir, persisted: 0 })
    }

    /// `/rename`: このセッションに名前を付ける。
    pub fn rename(&self, name: &str) -> Result<()> {
        rename_session(self.id(), name)
    }

    /// 既存セッションを継続する。`history`（復元済みの会話）のうち、API 上有効な
    /// 接頭辞ぶんを保存済みとして扱い、以降の `sync` は新規ぶんだけ追記する。
    /// （transcript の行数ではなく history を基準にするため、復元時の system 差し替え
    ///   や将来の履歴整形に依存しない。）
    pub fn open_resumed(id: &str, history: &[Message]) -> Result<Self> {
        let dir = sessions_root()
            .context("could not resolve sessions directory")?
            .join(id);
        if !dir.join("transcript.jsonl").is_file() {
            anyhow::bail!("no such session: {id} (looked in {dir:?})");
        }
        Ok(Self {
            dir,
            persisted: valid_prefix_len(history),
        })
    }

    /// `history` のうち未保存かつ API 上有効な末尾を transcript.jsonl へ追記する。
    /// 宙ぶらりんの `Assistant(tool_calls)`（直後に Tool 結果が無い）は、解決される
    /// まで書き込まない。これにより transcript は常に再投入可能な整合状態を保つ。
    pub fn sync(&mut self, history: &[Message]) -> Result<()> {
        let valid = valid_prefix_len(history);
        if valid <= self.persisted {
            return Ok(());
        }
        let mut file = OpenOptions::new()
            .append(true)
            .open(self.dir.join("transcript.jsonl"))
            .context("open transcript.jsonl for append")?;
        for msg in &history[self.persisted..valid] {
            let line = serde_json::to_string(msg)?;
            writeln!(file, "{line}")?;
        }
        self.persisted = valid;
        Ok(())
    }

    pub fn transcript_path(&self) -> PathBuf {
        self.dir.join("transcript.jsonl")
    }

    fn goal_path(&self) -> PathBuf {
        self.dir.join("goal.json")
    }

    /// `/goal` の状態をセッションと一緒に残す (`--resume` で paused として戻る)。
    /// `None` は「goal は無い」: 達成・解除のあとに古い状態が残らないよう、ファイルを消す。
    pub fn save_goal(&self, goal: Option<&crate::goal::GoalRecord>) -> Result<()> {
        let path = self.goal_path();
        match goal {
            Some(record) => {
                // 毎ターン書き直すので、途中で落ちても前の内容が残るよう、別名で書いてから差し替える。
                let tmp = path.with_extension("json.tmp");
                fs::write(&tmp, serde_json::to_string_pretty(record)?)
                    .context("write goal.json")?;
                // 条件文には作業の中身が書かれる。transcript と同じ扱いにする。
                restrict(&tmp, 0o600);
                fs::rename(&tmp, &path).context("replace goal.json")?;
            }
            None => match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).context("remove goal.json"),
            },
        }
        Ok(())
    }

    /// 保存された goal。無ければ None。壊れたファイルは無いものとして扱う (警告は呼び出し側)。
    pub fn load_goal(&self) -> Result<Option<crate::goal::GoalRecord>> {
        match fs::read_to_string(self.goal_path()) {
            Ok(text) => Ok(Some(
                serde_json::from_str(&text).context("parse goal.json")?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("read goal.json"),
        }
    }

    pub fn id(&self) -> &str {
        // dir 名 = id。
        self.dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
    }
}

/// `<data_dir>/lodan/sessions`。`LODAN_SESSIONS_DIR` で差し替えられる (テストや持ち運び用)。
fn sessions_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("LODAN_SESSIONS_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    directories::ProjectDirs::from("", "", "lodan").map(|d| d.data_dir().join("sessions"))
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 本人のみアクセス可に制限する (unix)。失敗は致命でないため無視する。
#[cfg(unix)]
fn restrict(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}
#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) {}

/// LLM に再投入できる接頭辞の長さを返す。
/// 各 `Assistant(tool_calls)` は直後に全 `call.id` ぶんの `Tool` が同順で続く必要があり、
/// 破れた時点（宙ぶらりんの tool_call / 孤立した Tool）で打ち切る。
fn valid_prefix_len(messages: &[Message]) -> usize {
    let mut i = 0;
    let mut valid = 0;
    while i < messages.len() {
        match &messages[i] {
            Message::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                let answered = tool_calls.iter().enumerate().all(|(k, call)| {
                    matches!(
                        messages.get(i + 1 + k),
                        Some(Message::Tool { tool_call_id, .. }) if *tool_call_id == call.id
                    )
                });
                if !answered {
                    break;
                }
                i += 1 + tool_calls.len();
                valid = i;
            }
            // 直前に対応する Assistant を伴わない Tool は不整合。
            Message::Tool { .. } => break,
            _ => {
                i += 1;
                valid = i;
            }
        }
    }
    valid
}

/// 保存済みセッションの transcript を読み戻す。
pub fn load_transcript(id: &str) -> Result<Vec<Message>> {
    let dir = sessions_root()
        .context("could not resolve sessions directory")?
        .join(id);
    let path = dir.join("transcript.jsonl");
    let file =
        File::open(&path).with_context(|| format!("no such session: {id} (looked in {dir:?})"))?;
    let mut messages = Vec::new();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Message = serde_json::from_str(&line)
            .with_context(|| format!("parse transcript {id} line {}", i + 1))?;
        messages.push(msg);
    }
    // 末尾が宙ぶらりんの tool_call で終わっていても再投入できるよう整える。
    messages.truncate(valid_prefix_len(&messages));
    Ok(messages)
}

fn session_dir(id: &str) -> Result<PathBuf> {
    // id はディレクトリ名。`..` や区切りを含む文字列でセッション置き場の外を指させない。
    if id.is_empty() || id.contains(['/', '\\']) || id == "." || id == ".." {
        anyhow::bail!("invalid session id: {id:?}");
    }
    Ok(sessions_root()
        .context("could not resolve sessions directory")?
        .join(id))
}

pub fn read_meta(id: &str) -> Result<SessionMeta> {
    let path = session_dir(id)?.join("meta.json");
    let text = fs::read_to_string(&path).with_context(|| format!("no such session: {id}"))?;
    serde_json::from_str(&text).with_context(|| format!("parse {path:?}"))
}

fn write_meta(meta: &SessionMeta) -> Result<()> {
    let path = session_dir(&meta.id)?.join("meta.json");
    fs::write(&path, serde_json::to_string_pretty(meta)?)
        .with_context(|| format!("write {path:?}"))?;
    restrict(&path, 0o600);
    Ok(())
}

/// セッションに名前を付ける (`meta.json` の `name`)。
pub fn rename_session(id: &str, name: &str) -> Result<()> {
    let mut meta = read_meta(id)?;
    let name = name.trim();
    meta.name = (!name.is_empty()).then(|| name.to_string());
    write_meta(&meta)
}

/// transcript を新しい id に複製する (#80)。元は変わらない。`goal.json` は写さない
/// (未達の goal を 2 か所で走らせない)。
pub fn fork_session(id: &str) -> Result<SessionMeta> {
    let src = read_meta(id)?;
    let src_dir = session_dir(id)?;
    let created_at_ms = now_ms();
    let new_id = format!("{created_at_ms}-{}", std::process::id());
    let dir = session_dir(&new_id)?;
    fs::create_dir_all(&dir).with_context(|| format!("create session dir {dir:?}"))?;
    restrict(&dir, 0o700);
    let transcript = dir.join("transcript.jsonl");
    fs::copy(src_dir.join("transcript.jsonl"), &transcript)
        .with_context(|| format!("copy transcript of {id}"))?;
    restrict(&transcript, 0o600);
    let meta = SessionMeta {
        id: new_id,
        created_at_ms,
        forked_from: Some(src.id.clone()),
        name: None,
        ..src
    };
    write_meta(&meta)?;
    Ok(meta)
}

/// 一覧のプレビュー: 最初のユーザ発話の 1 行目 (長ければ切る)。
pub fn first_user_line(id: &str) -> Option<String> {
    let path = session_dir(id).ok()?.join("transcript.jsonl");
    let file = File::open(path).ok()?;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if let Ok(Message::User { content }) = serde_json::from_str::<Message>(&line) {
            return Some(preview(&content));
        }
    }
    None
}

/// 1 行目を 60 文字までに詰める。
pub fn preview(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    let mut out: String = first.chars().take(60).collect();
    if first.chars().count() > 60 {
        out.push('…');
    }
    out
}

/// `/export`: 会話を Markdown にする。system prompt は含めない (長く、ツール一覧やメモリが入る)。
pub fn transcript_markdown(id: &str, history: &[Message]) -> String {
    let mut out = format!("# lodan session {id}\n");
    for m in history {
        match m {
            Message::System { .. } => {}
            Message::User { content } => {
                out.push_str("\n## user\n\n");
                out.push_str(content.trim_end());
                out.push('\n');
            }
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                out.push_str("\n## assistant\n\n");
                if let Some(text) = content.as_deref().filter(|t| !t.trim().is_empty()) {
                    out.push_str(text.trim_end());
                    out.push('\n');
                }
                for call in tool_calls {
                    out.push_str(&format!(
                        "\n```json\n// tool call {} ({})\n{}\n```\n",
                        call.function.name, call.id, call.function.arguments
                    ));
                }
            }
            Message::Tool {
                tool_call_id,
                content,
            } => {
                out.push_str(&format!("\n## tool result ({tool_call_id})\n\n```\n"));
                out.push_str(content.trim_end());
                out.push_str("\n```\n");
            }
        }
    }
    out
}

/// この cwd のセッションだけ (`None` なら全部)。作成時刻の昇順。
pub fn list_sessions_in(cwd: Option<&Path>) -> Result<Vec<SessionMeta>> {
    let mut all = list_sessions()?;
    if let Some(cwd) = cwd {
        all.retain(|m| m.is_in(cwd));
    }
    Ok(all)
}

/// この cwd の最新セッションの id (`--continue` / cwd スコープの `--resume last`)。
pub fn latest_session_id_in(cwd: Option<&Path>) -> Result<Option<String>> {
    Ok(list_sessions_in(cwd)?.into_iter().next_back().map(|m| m.id))
}

/// 全セッションのメタを作成時刻の昇順で返す。
pub fn list_sessions() -> Result<Vec<SessionMeta>> {
    let Some(root) = sessions_root() else {
        return Ok(Vec::new());
    };
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut metas = Vec::new();
    for entry in fs::read_dir(&root)? {
        let path = entry?.path();
        let meta_path = path.join("meta.json");
        if !meta_path.is_file() {
            continue;
        }
        match fs::read_to_string(&meta_path).and_then(|s| {
            serde_json::from_str::<SessionMeta>(&s)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        }) {
            Ok(meta) => metas.push(meta),
            Err(e) => eprintln!("session: skip {meta_path:?}: {e}"),
        }
    }
    metas.sort_by_key(|m| m.created_at_ms);
    Ok(metas)
}

/// 最新セッションの id を返す (`--resume last` 用)。
pub fn latest_session_id() -> Result<Option<String>> {
    Ok(list_sessions()?.into_iter().next_back().map(|m| m.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::messages::{Message, ToolCall, ToolCallFunction};

    fn tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: name.into(),
                arguments: "{}".into(),
            },
        }
    }

    #[test]
    fn preview_takes_the_first_line_and_caps_it() {
        assert_eq!(preview("fix the bug\nmore"), "fix the bug");
        let long = "x".repeat(80);
        assert_eq!(preview(&long).chars().count(), 61);
    }

    #[test]
    fn markdown_export_skips_the_system_prompt_and_fences_tool_traffic() {
        let history = vec![
            Message::System {
                content: "SYSTEM".into(),
            },
            Message::User {
                content: "hi".into(),
            },
            Message::Assistant {
                content: Some("calling".into()),
                tool_calls: vec![tool_call("c1", "Read")],
                reasoning_content: None,
            },
            Message::Tool {
                tool_call_id: "c1".into(),
                content: "file body".into(),
            },
            Message::Assistant {
                content: Some("done".into()),
                tool_calls: vec![],
                reasoning_content: None,
            },
        ];
        let md = transcript_markdown("s1", &history);
        assert!(md.starts_with("# lodan session s1\n"));
        assert!(!md.contains("SYSTEM"));
        assert!(md.contains("## user\n\nhi") && md.contains("// tool call Read (c1)"));
        assert!(md.contains("## tool result (c1)\n\n```\nfile body\n```"));
        assert!(md.ends_with("done\n"));
    }

    #[test]
    fn session_ids_cannot_point_outside_the_sessions_dir() {
        assert!(session_dir("../etc").is_err());
        assert!(session_dir("a/b").is_err());
        assert!(session_dir("").is_err());
    }

    #[test]
    fn sync_appends_only_new_messages() {
        // sessions_root は HOME 依存なので、ここでは Recorder を直接組み立てて
        // 一時ディレクトリに対して追記ロジックだけ検証する。
        let tmp = tempfile::tempdir().unwrap();
        File::create(tmp.path().join("transcript.jsonl")).unwrap();
        let mut rec = Recorder {
            dir: tmp.path().to_path_buf(),
            persisted: 0,
        };

        let mut history = vec![
            Message::System {
                content: "sys".into(),
            },
            Message::User {
                content: "hi".into(),
            },
        ];
        rec.sync(&history).unwrap();
        assert_eq!(rec.persisted, 2);

        history.push(Message::Assistant {
            content: Some("yo".into()),
            tool_calls: vec![],
            reasoning_content: None,
        });
        rec.sync(&history).unwrap();
        assert_eq!(rec.persisted, 3);

        let body = fs::read_to_string(tmp.path().join("transcript.jsonl")).unwrap();
        assert_eq!(body.lines().count(), 3);
        // 再 sync は no-op。
        rec.sync(&history).unwrap();
        let body = fs::read_to_string(tmp.path().join("transcript.jsonl")).unwrap();
        assert_eq!(body.lines().count(), 3);
    }

    #[test]
    fn transcript_lines_roundtrip_as_messages() {
        let tmp = tempfile::tempdir().unwrap();
        File::create(tmp.path().join("transcript.jsonl")).unwrap();
        let mut rec = Recorder {
            dir: tmp.path().to_path_buf(),
            persisted: 0,
        };
        let history = vec![
            Message::User {
                content: "write a file".into(),
            },
            Message::Assistant {
                content: None,
                tool_calls: vec![tool_call("call_1", "Write")],
                reasoning_content: None,
            },
            Message::Tool {
                tool_call_id: "call_1".into(),
                content: "ok".into(),
            },
        ];
        rec.sync(&history).unwrap();

        let body = fs::read_to_string(tmp.path().join("transcript.jsonl")).unwrap();
        let parsed: Vec<Message> = body
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(parsed.len(), 3);
        assert!(matches!(parsed[2], Message::Tool { .. }));
    }

    #[test]
    fn sync_withholds_dangling_tool_call_until_resolved() {
        let tmp = tempfile::tempdir().unwrap();
        File::create(tmp.path().join("transcript.jsonl")).unwrap();
        let mut rec = Recorder {
            dir: tmp.path().to_path_buf(),
            persisted: 0,
        };

        // Tool 結果がまだ無い宙ぶらりんの tool_call。
        let mut history = vec![
            Message::User {
                content: "do it".into(),
            },
            Message::Assistant {
                content: None,
                tool_calls: vec![tool_call("call_1", "Bash")],
                reasoning_content: None,
            },
        ];
        rec.sync(&history).unwrap();
        // User までしか書かない (Assistant(tool_calls) は保留)。
        assert_eq!(rec.persisted, 1);
        let lines = fs::read_to_string(tmp.path().join("transcript.jsonl"))
            .unwrap()
            .lines()
            .count();
        assert_eq!(lines, 1);

        // 結果が揃えば Assistant + Tool をまとめて追記する。
        history.push(Message::Tool {
            tool_call_id: "call_1".into(),
            content: "done".into(),
        });
        rec.sync(&history).unwrap();
        assert_eq!(rec.persisted, 3);
    }

    #[test]
    fn valid_prefix_truncates_trailing_dangling_call() {
        let messages = vec![
            Message::User {
                content: "q".into(),
            },
            Message::Assistant {
                content: None,
                tool_calls: vec![tool_call("c1", "Read")],
                reasoning_content: None,
            },
            // Tool 結果欠落のまま終端 → Assistant 手前で切る。
        ];
        assert_eq!(valid_prefix_len(&messages), 1);
    }

    #[test]
    fn valid_prefix_keeps_answered_calls() {
        let messages = vec![
            Message::User {
                content: "q".into(),
            },
            Message::Assistant {
                content: None,
                tool_calls: vec![tool_call("c1", "Read"), tool_call("c2", "Grep")],
                reasoning_content: None,
            },
            Message::Tool {
                tool_call_id: "c1".into(),
                content: "a".into(),
            },
            Message::Tool {
                tool_call_id: "c2".into(),
                content: "b".into(),
            },
        ];
        assert_eq!(valid_prefix_len(&messages), 4);
    }

    #[test]
    fn a_goal_is_saved_with_the_session_and_removed_when_it_is_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = Recorder {
            dir: tmp.path().to_path_buf(),
            persisted: 0,
        };
        assert_eq!(rec.load_goal().unwrap(), None);

        let goal = crate::goal::GoalRecord {
            condition: "make the tests pass".into(),
            total_turns: 3,
            elapsed_secs: 90,
        };
        rec.save_goal(Some(&goal)).unwrap();
        assert_eq!(rec.load_goal().unwrap(), Some(goal));

        // 達成・解除のあとは残さない。2 回消してもエラーにしない。
        rec.save_goal(None).unwrap();
        rec.save_goal(None).unwrap();
        assert_eq!(rec.load_goal().unwrap(), None);

        std::fs::write(tmp.path().join("goal.json"), "{not json").unwrap();
        assert!(
            rec.load_goal().is_err(),
            "a broken file is reported, not silently dropped"
        );
    }
}
