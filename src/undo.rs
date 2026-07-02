//! ターン単位のファイル変更ロールバック (#37)。
//!
//! ファイル系ツール (Write/Edit/MultiEdit/NotebookEdit) の実行直前に
//! **変更前スナップショット** (first-touch のみ) を取り、`/undo` で
//! 直近ターンの変更をまとめて巻き戻す。`Bash` などの副作用は一般に
//! 巻き戻せないため対象外 (README 参照)。undo 自体の変更は記録しない
//! (redo なし)。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// 保持するターン数の上限 (それより古い undo 情報は捨てる)。
pub const MAX_TURNS: usize = 10;

/// 退避するファイルサイズの上限。超えるものはスナップショットせず、
/// undo 時に「スキップ」として報告する。
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// 変更前のファイル状態。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Before {
    /// 存在しなかった → undo で削除する。
    Absent,
    /// 変更前の内容 → undo で書き戻す。
    Content(Vec<u8>),
    /// 大きすぎる・読めない等で退避できなかった → undo 不可 (報告のみ)。
    Unsnapshotted(String),
}

#[derive(Debug)]
struct FileBefore {
    path: PathBuf,
    before: Before,
}

#[derive(Debug)]
struct TurnRecord {
    turn: u64,
    files: Vec<FileBefore>,
}

/// ターン単位の変更前スナップショット台帳。
#[derive(Debug, Default)]
pub struct UndoLog {
    turns: VecDeque<TurnRecord>,
}

impl UndoLog {
    /// `turn` で最初に触るファイルの変更前状態を退避する。同一ターン内の
    /// 2 回目以降は無視する (ターン開始時点へ戻すため first-touch が正)。
    pub fn record_before(&mut self, turn: u64, path: &Path) {
        if self.turns.back().is_none_or(|t| t.turn != turn) {
            self.turns.push_back(TurnRecord {
                turn,
                files: Vec::new(),
            });
            while self.turns.len() > MAX_TURNS {
                self.turns.pop_front();
            }
        }
        let rec = self.turns.back_mut().expect("just ensured");
        if rec.files.iter().any(|f| f.path == path) {
            return;
        }
        let before = snapshot(path);
        rec.files.push(FileBefore {
            path: path.to_path_buf(),
            before,
        });
    }

    /// 直近ターンの変更を巻き戻す。記録が無ければ None。
    pub fn undo_last(&mut self) -> Option<UndoReport> {
        let rec = self.turns.pop_back()?;
        let mut report = UndoReport {
            turn: rec.turn,
            restored: Vec::new(),
            removed: Vec::new(),
            skipped: Vec::new(),
            failed: Vec::new(),
        };
        // 触った順の逆で戻す (同一ターン内の依存があっても安全側)。
        for f in rec.files.into_iter().rev() {
            match f.before {
                Before::Absent => match std::fs::remove_file(&f.path) {
                    Ok(()) => report.removed.push(f.path),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        report.removed.push(f.path)
                    }
                    Err(e) => report.failed.push((f.path, e.to_string())),
                },
                Before::Content(bytes) => match std::fs::write(&f.path, &bytes) {
                    Ok(()) => report.restored.push(f.path),
                    Err(e) => report.failed.push((f.path, e.to_string())),
                },
                Before::Unsnapshotted(_) => report.skipped.push(f.path),
            }
        }
        Some(report)
    }
}

/// 変更前状態を読む。NotFound は Absent、サイズ超過や読み取り失敗は
/// Unsnapshotted として undo 不可扱いにする。
fn snapshot(path: &Path) -> Before {
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Before::Absent,
        Err(e) => return Before::Unsnapshotted(e.to_string()),
        Ok(m) if m.len() > MAX_FILE_BYTES => {
            return Before::Unsnapshotted(format!("file too large ({} bytes)", m.len()));
        }
        Ok(_) => {}
    }
    match std::fs::read(path) {
        Ok(bytes) => Before::Content(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Before::Absent,
        Err(e) => Before::Unsnapshotted(e.to_string()),
    }
}

/// `undo_last` の結果 (`/undo` の表示用)。
#[derive(Debug)]
pub struct UndoReport {
    pub turn: u64,
    /// 変更前の内容へ書き戻したファイル。
    pub restored: Vec<PathBuf>,
    /// ターン中に新規作成されたため削除したファイル。
    pub removed: Vec<PathBuf>,
    /// スナップショットが無く戻せなかったファイル。
    pub skipped: Vec<PathBuf>,
    pub failed: Vec<(PathBuf, String)>,
}

impl UndoReport {
    pub fn describe(&self) -> String {
        let mut out = format!("undo (turn {}):", self.turn);
        for p in &self.restored {
            out.push_str(&format!("\n  restored {}", p.display()));
        }
        for p in &self.removed {
            out.push_str(&format!("\n  removed  {}", p.display()));
        }
        for p in &self.skipped {
            out.push_str(&format!(
                "\n  skipped  {} (no snapshot; cannot restore)",
                p.display()
            ));
        }
        for (p, e) in &self.failed {
            out.push_str(&format!("\n  FAILED   {} ({e})", p.display()));
        }
        if self.restored.is_empty()
            && self.removed.is_empty()
            && self.skipped.is_empty()
            && self.failed.is_empty()
        {
            out.push_str(" no file changes recorded");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_removes_file_created_in_turn() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("new.txt");
        let mut log = UndoLog::default();

        log.record_before(1, &f); // 実行前: 存在しない
        std::fs::write(&f, "created").unwrap();

        let report = log.undo_last().unwrap();
        assert_eq!(report.turn, 1);
        assert_eq!(report.removed, vec![f.clone()]);
        assert!(!f.exists(), "created file must be removed by undo");
        assert!(log.undo_last().is_none(), "log is consumed");
    }

    #[test]
    fn undo_restores_previous_content() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "before").unwrap();
        let mut log = UndoLog::default();

        log.record_before(1, &f);
        std::fs::write(&f, "after").unwrap();

        let report = log.undo_last().unwrap();
        assert_eq!(report.restored, vec![f.clone()]);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "before");
    }

    #[test]
    fn first_touch_wins_within_a_turn() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "start").unwrap();
        let mut log = UndoLog::default();

        log.record_before(1, &f);
        std::fs::write(&f, "mid").unwrap();
        log.record_before(1, &f); // 2 回目は無視される
        std::fs::write(&f, "end").unwrap();

        log.undo_last().unwrap();
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "start",
            "undo must restore turn-start content, not an intermediate one"
        );
    }

    #[test]
    fn turns_undo_independently_lifo() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "v1").unwrap();
        let mut log = UndoLog::default();

        log.record_before(1, &f);
        std::fs::write(&f, "v2").unwrap();
        log.record_before(2, &f);
        std::fs::write(&f, "v3").unwrap();

        log.undo_last().unwrap(); // turn 2 を戻す
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v2");
        log.undo_last().unwrap(); // turn 1 を戻す
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
    }

    #[test]
    fn old_turns_are_dropped_beyond_cap() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = UndoLog::default();
        for turn in 0..(MAX_TURNS as u64 + 3) {
            let f = dir.path().join(format!("f{turn}.txt"));
            log.record_before(turn, &f);
            std::fs::write(&f, "x").unwrap();
        }
        let mut undone = 0;
        while log.undo_last().is_some() {
            undone += 1;
        }
        assert_eq!(undone, MAX_TURNS);
    }

    #[test]
    fn oversized_files_are_reported_as_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("big.bin");
        std::fs::write(&f, vec![0u8; (MAX_FILE_BYTES + 1) as usize]).unwrap();
        let mut log = UndoLog::default();

        log.record_before(1, &f);
        std::fs::write(&f, "overwritten").unwrap();

        let report = log.undo_last().unwrap();
        assert_eq!(report.skipped, vec![f.clone()]);
        assert!(report.restored.is_empty());
        // 戻せないので変更後の内容のまま。
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "overwritten");
    }

    #[test]
    fn snapshot_detects_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(snapshot(&dir.path().join("nope")), Before::Absent);
    }
}
