//! バックグラウンドプロセスの共有ストア。
//!
//! `Bash` の `run_in_background` が子プロセスを spawn して登録し、`Monitor` が
//! 増分出力（前回読んだ位置以降）と終了状態を読み出す。Bash の reader タスクが
//! 出力バッファへ append し、wait タスクが終了コードを書き込む。両者は `Arc<Mutex>`
//! 越しに共有され、ストア本体は `ToolCtx::bg`（セッション単位）に置かれる。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

/// 1 プロセスあたりの出力バッファ上限（バイト）。超過後は append を止め、
/// 一度だけ truncation マーカを付す（cap を最大 1 チャンク分だけ超え得るソフト上限）。
pub const BG_OUTPUT_CAP: usize = 1 << 20; // 1 MiB

/// reader タスクが append、`Monitor` が cursor 以降を読む共有出力バッファ。
pub type SharedBuf = Arc<Mutex<String>>;
/// wait タスクが終了時に書き込む共有ステータス。
pub type SharedStatus = Arc<Mutex<BgStatus>>;

#[derive(Debug, Clone)]
pub enum BgStatus {
    Running,
    Exited(i32),
    Killed,
    Failed(String),
}

impl BgStatus {
    pub fn label(&self) -> String {
        match self {
            BgStatus::Running => "running".to_string(),
            BgStatus::Exited(code) => format!("exited({code})"),
            BgStatus::Killed => "killed".to_string(),
            BgStatus::Failed(err) => format!("failed: {err}"),
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self, BgStatus::Running)
    }
}

struct BgEntry {
    command: String,
    output: SharedBuf,
    status: SharedStatus,
    /// これまでに `Monitor` が返した出力末尾位置（バイト）。
    cursor: usize,
    /// `KillShell` が叩くと wait タスクが child を kill する合図。
    kill: Arc<Notify>,
}

/// `Monitor` 1 回分の読み出し結果。
pub struct BgRead {
    pub command: String,
    pub new_output: String,
    pub status: BgStatus,
}

#[derive(Default)]
pub struct BgStore {
    procs: BTreeMap<String, BgEntry>,
    next: u64,
}

impl BgStore {
    /// 新しいバックグラウンドプロセスを登録し、ID (`bash_N`) を返す。
    /// `kill` は `KillShell` が叩く合図で、spawn 側の wait タスクと共有する。
    pub fn register(
        &mut self,
        command: String,
        output: SharedBuf,
        status: SharedStatus,
        kill: Arc<Notify>,
    ) -> String {
        self.next += 1;
        let id = format!("bash_{}", self.next);
        self.procs.insert(
            id.clone(),
            BgEntry {
                command,
                output,
                status,
                cursor: 0,
                kill,
            },
        );
        id
    }

    /// `id` のプロセスに kill 合図を送り、合図直前の状態を返す。未知 ID は `None`。
    /// 実際の終了とステータス更新は spawn 側の wait タスクが非同期に行う
    /// （既に終了済みなら合図は無害に無視される）。
    pub fn request_kill(&self, id: &str) -> Option<BgStatus> {
        let entry = self.procs.get(id)?;
        let prior = entry
            .status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| BgStatus::Failed("status lock poisoned".into()));
        entry.kill.notify_one();
        Some(prior)
    }

    /// `id` の cursor 以降の新規出力と現在の状態を返し、cursor を進める。
    /// 未知の ID は `None`。
    pub fn read(&mut self, id: &str) -> Option<BgRead> {
        let entry = self.procs.get_mut(id)?;
        let new_output = match entry.output.lock() {
            Ok(buf) => {
                let slice = buf.get(entry.cursor..).unwrap_or("").to_string();
                entry.cursor = buf.len();
                slice
            }
            Err(_) => String::new(),
        };
        let status = entry
            .status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| BgStatus::Failed("status lock poisoned".into()));
        Some(BgRead {
            command: entry.command.clone(),
            new_output,
            status,
        })
    }

    /// 既知の ID 一覧（昇順）。
    pub fn ids(&self) -> Vec<String> {
        self.procs.keys().cloned().collect()
    }
}

/// 上限付きで共有バッファへ追記する。`BG_OUTPUT_CAP` 到達後は追記を止め、
/// 初回到達時のみ `...[truncated]...` を付す。UTF-8 境界を割らないよう
/// チャンク単位でしか切らない（最大 1 チャンク分のオーバーシュートを許容）。
pub fn append_capped(buf: &SharedBuf, chunk: &str) {
    if let Ok(mut s) = buf.lock() {
        if s.len() >= BG_OUTPUT_CAP {
            return;
        }
        s.push_str(chunk);
        if s.len() >= BG_OUTPUT_CAP {
            s.push_str("\n...[truncated]...");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(s: &str) -> SharedBuf {
        Arc::new(Mutex::new(s.to_string()))
    }

    #[test]
    fn read_returns_incremental_output_and_advances_cursor() {
        let mut store = BgStore::default();
        let out = shared("");
        let status = Arc::new(Mutex::new(BgStatus::Running));
        let id = store.register(
            "echo hi".into(),
            out.clone(),
            status.clone(),
            Arc::new(Notify::new()),
        );
        assert_eq!(id, "bash_1");

        // 最初は空。
        let first = store.read(&id).unwrap();
        assert_eq!(first.new_output, "");
        assert!(first.status.is_running());

        // append 後は新規分だけ返る。
        append_capped(&out, "line1\n");
        let second = store.read(&id).unwrap();
        assert_eq!(second.new_output, "line1\n");

        // さらに append → cursor 以降のみ。
        append_capped(&out, "line2\n");
        *status.lock().unwrap() = BgStatus::Exited(0);
        let third = store.read(&id).unwrap();
        assert_eq!(third.new_output, "line2\n");
        assert_eq!(third.status.label(), "exited(0)");
        assert!(!third.status.is_running());
    }

    #[test]
    fn unknown_id_is_none() {
        let mut store = BgStore::default();
        assert!(store.read("bash_99").is_none());
        assert!(store.request_kill("bash_99").is_none());
        assert!(store.ids().is_empty());
    }

    #[test]
    fn ids_are_sorted_and_sequential() {
        let mut store = BgStore::default();
        let s = || Arc::new(Mutex::new(BgStatus::Running));
        store.register("a".into(), shared(""), s(), Arc::new(Notify::new()));
        store.register("b".into(), shared(""), s(), Arc::new(Notify::new()));
        assert_eq!(store.ids(), vec!["bash_1", "bash_2"]);
    }

    #[tokio::test]
    async fn request_kill_signals_and_reports_prior_status() {
        let mut store = BgStore::default();
        let status = Arc::new(Mutex::new(BgStatus::Running));
        let kill = Arc::new(Notify::new());
        let id = store.register("sleep 9".into(), shared(""), status, kill.clone());

        // 合図直前の状態 (running) が返る。
        let prior = store.request_kill(&id).unwrap();
        assert!(prior.is_running());
        // notify が届いていることを確認（待機側が即座に起きる）。
        kill.notified().await;
    }

    #[test]
    fn killed_status_label() {
        assert_eq!(BgStatus::Killed.label(), "killed");
        assert!(!BgStatus::Killed.is_running());
    }

    #[test]
    fn append_caps_at_limit_with_marker() {
        let buf = shared("");
        let big = "x".repeat(BG_OUTPUT_CAP + 10);
        append_capped(&buf, &big);
        let len = buf.lock().unwrap().len();
        assert!(len >= BG_OUTPUT_CAP);
        assert!(buf.lock().unwrap().ends_with("...[truncated]..."));
        // 到達後の追記は無視される。
        append_capped(&buf, "more");
        assert_eq!(buf.lock().unwrap().len(), len);
    }
}
