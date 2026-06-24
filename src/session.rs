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

        let meta = SessionMeta {
            id,
            created_at_ms,
            cwd: cwd.display().to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
        };
        let meta_json = serde_json::to_string_pretty(&meta)?;
        fs::write(dir.join("meta.json"), meta_json).context("write meta.json")?;
        // transcript を空で用意しておく。
        File::create(dir.join("transcript.jsonl")).context("create transcript.jsonl")?;

        Ok(Self { dir, persisted: 0 })
    }

    /// 既存セッションを継続する。transcript の既存行数を保存済みとして扱い、
    /// 以降の `sync` は新規メッセージのみ追記する。
    pub fn open(id: &str) -> Result<Self> {
        let dir = sessions_root()
            .context("could not resolve sessions directory")?
            .join(id);
        let path = dir.join("transcript.jsonl");
        if !path.is_file() {
            anyhow::bail!("no such session: {id} (looked in {dir:?})");
        }
        let persisted = BufReader::new(File::open(&path)?)
            .lines()
            .filter(|l| l.as_ref().map(|s| !s.trim().is_empty()).unwrap_or(false))
            .count();
        Ok(Self { dir, persisted })
    }

    /// `history` のうち未保存の末尾を transcript.jsonl へ追記する。
    pub fn sync(&mut self, history: &[Message]) -> Result<()> {
        if history.len() <= self.persisted {
            return Ok(());
        }
        let mut file = OpenOptions::new()
            .append(true)
            .open(self.dir.join("transcript.jsonl"))
            .context("open transcript.jsonl for append")?;
        for msg in &history[self.persisted..] {
            let line = serde_json::to_string(msg)?;
            writeln!(file, "{line}")?;
        }
        self.persisted = history.len();
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
    use crate::agent::messages::Message;

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
        assert_eq!(parsed.len(), 2);
        assert!(matches!(parsed[1], Message::Tool { .. }));
    }
}
