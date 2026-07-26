//! 実行トレース (JSONL)。`--log-jsonl <PATH>` / `LODAN_LOG_JSONL` で有効化する。
//!
//! 評価ハーネスが指標を機械的に取れるようにするための出力で、stdout の表示形式に
//! 依存せずターン数・ツール呼び出しの成否と所要時間・整形破綻や緩和策の発火回数を
//! 数えられる。無効時は完全な no-op、書き込みに失敗しても実行は止めない
//! (同じ失敗を毎行叫ばないよう警告は 1 度だけ)。
//!
//! 1 行 1 イベントで、全イベントが `ts_ms` と `event` を持つ:
//!
//! | event | 主なフィールド |
//! |---|---|
//! | `run_start` | `version`, `provider`, `model`, `cwd` |
//! | `turn_start` | `turn`, `mode`, `input_chars` |
//! | `llm_response` | `turn`, `iter`, `text_chars`, `tool_calls`, `prompt_tokens`, `completion_tokens`, `estimated` |
//! | `malformed_retry` | `turn`, `iter`, `n` |
//! | `finish_nudge` | `turn`, `iter`, `kind` (`act` / `verify`) |
//! | `stop_hook_block` | `turn`, `iter` |
//! | `tool_result` | `turn`, `iter`, `name`, `outcome` (`ok` / `error`), `reason`, `ms`, `args_bytes`, `output_bytes` |
//! | `compact` | `turn`, `outcome` |
//! | `turn_end` | `turn`, `iterations`, `tool_calls`, `reason`, `ms` |

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::Value;

/// プロセス全体で 1 つの sink。未初期化なら記録は no-op。
static SINK: OnceLock<RunLog> = OnceLock::new();

/// JSONL の追記先。
pub struct RunLog {
    out: Mutex<std::fs::File>,
    /// 書き込み失敗の警告済みフラグ (毎行の出力を避ける)。
    warned: AtomicBool,
}

impl RunLog {
    /// 追記モードで開く (親ディレクトリは必要なら作る)。
    pub fn create(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating run log dir {}", dir.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening run log {}", path.display()))?;
        Ok(Self {
            out: Mutex::new(file),
            warned: AtomicBool::new(false),
        })
    }

    /// 1 イベントを追記する。失敗しても呼び出し元へは伝えない (計測が実行を壊さない)。
    pub fn record(&self, event: &str, fields: Value) {
        let line = format_line(event, fields, now_ms());
        let mut guard = match self.out.lock() {
            Ok(g) => g,
            // 書き込み中に panic したスレッドがいた場合でも記録は続ける。
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Err(e) = guard.write_all(line.as_bytes())
            && !self.warned.swap(true, Ordering::Relaxed)
        {
            eprintln!("runlog: write failed, further errors suppressed ({e})");
        }
    }
}

/// グローバル sink を初期化する。2 回目以降の呼び出しは無視される。
pub fn init(path: &Path) -> Result<()> {
    let log = RunLog::create(path)?;
    let _ = SINK.set(log);
    Ok(())
}

/// 記録が有効か (呼び出し側で高価なフィールド構築を避けたいとき用)。
pub fn is_enabled() -> bool {
    SINK.get().is_some()
}

/// グローバル sink へ 1 イベント記録する。未初期化なら何もしない。
pub fn record(event: &str, fields: Value) {
    if let Some(sink) = SINK.get() {
        sink.record(event, fields);
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// `{"ts_ms":…,"event":…,<fields>}` を 1 行に組み立てる (末尾改行つき)。
/// `fields` がオブジェクトでなければ `data` キーに入れる。
fn format_line(event: &str, fields: Value, ts_ms: u128) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("ts_ms".into(), Value::from(ts_ms as u64));
    obj.insert("event".into(), Value::from(event));
    match fields {
        Value::Object(map) => obj.extend(map),
        Value::Null => {}
        other => {
            obj.insert("data".into(), other);
        }
    }
    let mut line = Value::Object(obj).to_string();
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn line_carries_event_and_fields() {
        let line = format_line(
            "tool_result",
            json!({"name": "Read", "ms": 12}),
            1_700_000_000_000,
        );
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["event"], "tool_result");
        assert_eq!(v["ts_ms"], 1_700_000_000_000u64);
        assert_eq!(v["name"], "Read");
        assert_eq!(v["ms"], 12);
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn line_is_single_line_even_with_embedded_newlines() {
        let line = format_line("turn_start", json!({"input": "a\nb"}), 1);
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn non_object_fields_go_under_data() {
        let line = format_line("x", json!("plain"), 1);
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["data"], "plain");
    }

    #[test]
    fn record_appends_one_line_per_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("run.jsonl");
        let log = RunLog::create(&path).unwrap();
        log.record("turn_start", json!({"turn": 1}));
        log.record("turn_end", json!({"turn": 1, "reason": "final"}));

        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["event"], "turn_start");
        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["reason"], "final");
    }

    #[test]
    fn create_appends_to_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        RunLog::create(&path).unwrap().record("a", json!({}));
        RunLog::create(&path).unwrap().record("b", json!({}));
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 2);
    }
}
