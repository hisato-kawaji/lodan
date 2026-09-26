//! 実行トレース (JSONL)。`--log-jsonl <PATH>` / `LODAN_LOG_JSONL` で有効化する。
//!
//! 評価ハーネスが指標を機械的に取れるようにするための出力で、stdout の表示形式に
//! 依存せずターン数・ツール呼び出しの成否と所要時間・整形破綻や緩和策の発火回数を
//! 数えられる。無効時は何も出力せず、書き込みに失敗しても実行は止めない
//! (同じ失敗を毎行叫ばないよう警告は 1 度だけ)。
//!
//! 1 行 1 イベントで、全イベントが `ts_ms` と `event` を持つ:
//!
//! | event | 主なフィールド |
//! |---|---|
//! | `run_start` | `version`, `provider`, `model`, `cwd`。設定が読めずに起動に失敗したときも出るが、その場合 `provider` / `model` は `null` |
//! | `tools` | 起動時に 1 回。`profile`, `explicit`, `visible` (モデルに見せるツール名), `deferred` (`ToolSearch` で読み込めるツール名), `registered`, `spec_bytes` (毎リクエスト送るツール定義の JSON バイト数。`ToolSearch` を見せるならその分を含む。読み込み後に増える分は含まない), `tool_search_bytes` (そのうち `ToolSearch` の定義の分) |
//! | `tool_search` | `turn`, `query`, `loaded` (読み込んだツール名) |
//! | `turn_start` | `turn`, `mode`, `input_chars` |
//! | `llm_response` | `turn`, `iter`, `text_chars`, `tool_calls`, `prompt_tokens`, `completion_tokens`, `estimated` |
//! | `api_retry` | `attempt`, `max_retries`, `why` (`HTTP 503` / `connect error` / `stream interrupted`), `delay_ms` |
//! | `provider_fallback` | primary が一時的に使えず fallback provider を試した。`to`, `model`, `switched` (fallback が成功して以後そちらに固定したか), `why` |
//! | `malformed_retry` | `turn`, `iter`, `n` |
//! | `finish_nudge` | `turn`, `iter`, `kind` (`act` / `verify`) |
//! | `stop_hook_block` | `turn`, `iter` |
//! | `tool_result` | `turn`, `iter`, `name`, `outcome` (`ok` / `error`), `reason`, `ms`, `parallel` (同じ応答の他の呼び出しと同時に実行したか。このとき `ms` の合計は経過時間より大きくなる), `args_bytes`, `output_bytes` |
//! | `compact` | `turn`, `outcome` |
//! | `schema_retry` | `--output-schema` に合わない最終応答を出し直させた。`attempt`, `max` |
//! | `result` | ヘッドレス実行 (`-p`) の最後に 1 回。`is_error`, `exit_code`, `result`, `error`, `session_id`, `usage`, `structured_output` |
//! | `turn_end` | `turn`, `iterations`, `tool_calls`, `reason` (`final` / `max_iterations` / `error` / `aborted`), `ms` |
//!
//! `turn_end` は `turn_start` と必ず対になる (エラー終了は `error`、Ctrl-C 中断は `aborted`)。
//! `input_chars` / `text_chars` はどちらも Unicode スカラ値の個数で、バイト数ではない。

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
    /// 追記先のファイル。`stream-json` だけを使う実行では無い。
    out: Mutex<Option<std::fs::File>>,
    /// 同じ行を stdout にも流す (ヘッドレスの `--output-format stream-json`)。
    echo_stdout: bool,
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
            out: Mutex::new(Some(file)),
            echo_stdout: false,
            warned: AtomicBool::new(false),
        })
    }

    /// stdout にだけ流す sink (ファイル無し)。
    pub fn stdout_only() -> Self {
        Self {
            out: Mutex::new(None),
            echo_stdout: true,
            warned: AtomicBool::new(false),
        }
    }

    /// 同じ行を stdout にも流すようにする。
    pub fn with_stdout_echo(mut self) -> Self {
        self.echo_stdout = true;
        self
    }

    /// 1 イベントを追記する。失敗しても呼び出し元へは伝えない (計測が実行を壊さない)。
    pub fn record(&self, event: &str, fields: Value) {
        let line = format_line(event, fields, now_ms());
        let mut guard = match self.out.lock() {
            Ok(g) => g,
            // 書き込み中に panic したスレッドがいた場合でも記録は続ける。
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(file) = guard.as_mut()
            && let Err(e) = file.write_all(line.as_bytes())
            && !self.warned.swap(true, Ordering::Relaxed)
        {
            eprintln!("runlog: write failed, further errors suppressed ({e})");
        }
        if self.echo_stdout {
            // ファイルと同じロックの内側で書くので、行が混ざらない。
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(line.as_bytes());
            let _ = stdout.flush();
        }
    }
}

/// グローバル sink を初期化する。2 回目以降の呼び出しは無視される。
pub fn init(path: &Path) -> Result<()> {
    let log = RunLog::create(path)?;
    let _ = SINK.set(log);
    Ok(())
}

/// ファイル (任意) と stdout へのエコー (任意) を指定してグローバル sink を初期化する。
/// どちらも無ければ何もしない。2 回目以降の呼び出しは無視される。
pub fn init_with(path: Option<&Path>, echo_stdout: bool) -> Result<()> {
    let log = match (path, echo_stdout) {
        (Some(p), true) => RunLog::create(p)?.with_stdout_echo(),
        (Some(p), false) => RunLog::create(p)?,
        (None, true) => RunLog::stdout_only(),
        (None, false) => return Ok(()),
    };
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
