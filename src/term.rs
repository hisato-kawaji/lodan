//! 端末装飾（ANSI）ヘルパ。
//!
//! stdout が tty かつ `NO_COLOR` 未設定のときだけ着色する。パイプ／リダイレクト時は
//! エスケープを混ぜない。依存クレートを増やさず生 ANSI を使う（`repl.rs` の
//! `\x1b[2J` と同じ流儀）。

use std::io::IsTerminal;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// 人間向けの表示 (ストリーム本文・ツール出力の要約・各種通知) を stderr へ回すか。
/// ヘッドレスの `json` / `stream-json` では stdout が機械可読な契約になるため。
static DISPLAY_TO_STDERR: AtomicBool = AtomicBool::new(false);

/// 以後の人間向け表示を stderr へ回す (プロセス内で戻す経路は無い)。
pub fn route_display_to_stderr() {
    DISPLAY_TO_STDERR.store(true, Ordering::Relaxed);
}

pub fn display_to_stderr() -> bool {
    DISPLAY_TO_STDERR.load(Ordering::Relaxed)
}

/// 表示されない、または表示順を変える文字。モデルやツールが渡した文字列を端末に出すとき、
/// これらが混ざっていると「見えているもの」と実際の内容が食い違う。
/// 制御文字 (Cc) に加えて、書式文字 (Cf) のうち実害のあるもの — 双方向テキストの上書き
/// (U+202E など)、ゼロ幅文字、ソフトハイフン、タグ文字、空白として描画される文字 — を含める。
pub fn is_invisible(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{115F}'..='\u{1160}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{2029}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{2800}'
                | '\u{3164}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{E0000}'..='\u{E007F}'
        )
}

/// モデル由来・ツール由来の文字列を端末に出す前に通す (#100)。
///
/// アシスタントの本文、計画、ツール出力 (ファイルの中身・Web ページ・コマンド出力) は lodan の
/// 外から来る文字列で、ANSI エスケープ (カーソル移動・行消去) や双方向テキストの上書きを含み得る。
/// そのまま端末へ流すと、画面上の内容を書き換えて「直前に何が起きたか」「これから承認するものは
/// 何か」を偽れる。不可視文字を `\u{1b}` のような見える形にする。
///
/// 改行とタブは残す。`\r\n` は改行として扱い、単独の CR は (行の上書きに使えるので) 見える形にする。
/// 1 文字ずつ独立に変換するので、ストリーミングの途中で切れた断片に対して呼んでも結果は変わらない
/// (ESC 自体が無害化されるので、後続の `[2K` はただの文字になる)。ただし `\r\n` が断片の境目で
/// 分かれた場合だけは CR が `\r` と表示される。
///
/// **表示専用**。モデルへ返す tool 応答・履歴・runlog・`-p` の stdout には使わないこと。
pub fn sanitize(text: &str) -> std::borrow::Cow<'_, str> {
    let needs_work = |c: char| is_invisible(c) && c != '\n' && c != '\t';
    if !text.chars().any(needs_work) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' | '\t' => out.push(c),
            '\r' if chars.peek() == Some(&'\n') => {}
            c if is_invisible(c) => out.extend(c.escape_default()),
            c => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// 人間向けの 1 行。`println!` の代わりに `crate::say!` 経由で使う。
pub fn say_line(args: std::fmt::Arguments<'_>) {
    if display_to_stderr() {
        eprintln!("{args}");
    } else {
        println!("{args}");
    }
}

/// 人間向け表示用の `println!`。行き先は `route_display_to_stderr` に従う。
#[macro_export]
macro_rules! say {
    () => { $crate::term::say_line(format_args!("")) };
    ($($arg:tt)*) => { $crate::term::say_line(format_args!($($arg)*)) };
}

/// stdout が端末に繋がっているか（プロセス内で 1 度だけ判定）。
pub fn is_terminal() -> bool {
    static TTY: OnceLock<bool> = OnceLock::new();
    *TTY.get_or_init(|| std::io::stdout().is_terminal())
}

/// stderr が端末に繋がっているか（プロセス内で 1 度だけ判定）。
fn stderr_is_terminal() -> bool {
    static TTY: OnceLock<bool> = OnceLock::new();
    *TTY.get_or_init(|| std::io::stderr().is_terminal())
}

fn no_color() -> bool {
    static NC: OnceLock<bool> = OnceLock::new();
    *NC.get_or_init(|| std::env::var_os("NO_COLOR").is_some())
}

/// stdout を着色してよいか（tty かつ `NO_COLOR` 未設定）。
fn stdout_color() -> bool {
    is_terminal() && !no_color()
}

/// stderr を着色してよいか（stderr が tty かつ `NO_COLOR` 未設定）。
fn stderr_color() -> bool {
    stderr_is_terminal() && !no_color()
}

/// `enabled` のときだけ `code` の SGR で囲む純粋関数（テスト用に分離）。
fn style(code: &str, s: &str, enabled: bool) -> String {
    if enabled {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn paint(code: &str, s: &str) -> String {
    // 表示を stderr へ回しているときは、着色の可否も stderr の側で決める。
    let enabled = if display_to_stderr() {
        stderr_color()
    } else {
        stdout_color()
    };
    style(code, s, enabled)
}

// stdout 向け（`println!` / stdout ロックに使う）。
pub fn dim(s: &str) -> String {
    paint("2", s)
}
pub fn bold(s: &str) -> String {
    paint("1", s)
}
pub fn red(s: &str) -> String {
    paint("31", s)
}
pub fn yellow(s: &str) -> String {
    paint("33", s)
}
pub fn cyan(s: &str) -> String {
    paint("36", s)
}
pub fn green(s: &str) -> String {
    paint("32", s)
}

/// stderr 向けの赤（`eprintln!` に使う。stderr の tty 判定で着色可否を決める）。
pub fn red_err(s: &str) -> String {
    style("31", s, stderr_color())
}

#[cfg(test)]
mod tests {
    use super::{sanitize, style};

    #[test]
    fn sanitize_defuses_escapes_and_bidi_but_keeps_ordinary_text() {
        // カーソルを 2 行上げて行を消し、書き換える。
        let attack = "done\x1b[2A\x1b[2K\x1b[1Gall tests passed";
        let shown = sanitize(attack);
        assert!(!shown.contains('\x1b'));
        assert_eq!(shown, "done\\u{1b}[2A\\u{1b}[2K\\u{1b}[1Gall tests passed");
        assert_eq!(sanitize("fdp\u{202E}.exe"), "fdp\\u{202e}.exe");
        assert_eq!(sanitize("a\u{200B}b"), "a\\u{200b}b");
        // 行の上書きに使える単独の CR は見える形に。CRLF は改行。
        assert_eq!(
            sanitize("progress 10%\rprogress 99%"),
            "progress 10%\\rprogress 99%"
        );
        assert_eq!(sanitize("a\r\nb"), "a\nb");
    }

    #[test]
    fn sanitize_leaves_normal_output_untouched_and_unallocated() {
        let normal = "fn main() {\n\tprintln!(\"こんにちは 🎉\");\n}\n";
        assert!(matches!(sanitize(normal), std::borrow::Cow::Borrowed(_)));
        assert_eq!(sanitize(normal), normal);
    }

    #[test]
    fn sanitize_is_stable_across_streaming_fragment_boundaries() {
        // ストリーミングではエスケープ列が断片の境目で分かれて届く。
        let whole = sanitize("ok\x1b[2Kgone").into_owned();
        let pieces: String = ["ok\x1b", "[2", "Kgone"]
            .iter()
            .map(|p| sanitize(p).into_owned())
            .collect();
        assert_eq!(whole, pieces);
    }

    #[test]
    fn wraps_only_when_enabled() {
        assert_eq!(style("31", "x", true), "\x1b[31mx\x1b[0m");
        assert_eq!(style("31", "x", false), "x");
    }

    #[test]
    fn disabled_is_passthrough() {
        // 非 tty のテスト環境では実 API も素通しになる（stdout/stderr 両系）。
        assert_eq!(super::red("err"), "err");
        assert_eq!(super::cyan("[Tool]"), "[Tool]");
        assert_eq!(super::red_err("boom"), "boom");
    }
}
