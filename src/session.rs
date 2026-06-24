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

    pub fn id(&self) -> &str {
        // dir 名 = id。
        self.dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
    }
}

/// `<data_dir>/lodan/sessions`。
fn sessions_root() -> Option<PathBuf> {
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
}
