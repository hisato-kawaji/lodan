//! 端末装飾（ANSI）ヘルパ。
//!
//! stdout が tty かつ `NO_COLOR` 未設定のときだけ着色する。パイプ／リダイレクト時は
//! エスケープを混ぜない。依存クレートを増やさず生 ANSI を使う（`repl.rs` の
//! `\x1b[2J` と同じ流儀）。

use std::io::IsTerminal;
use std::sync::OnceLock;

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
    style(code, s, stdout_color())
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
    use super::style;

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
