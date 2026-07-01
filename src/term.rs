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

/// 着色してよいか（tty かつ `NO_COLOR` 未設定）。
fn color_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| is_terminal() && std::env::var_os("NO_COLOR").is_none())
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
    style(code, s, color_enabled())
}

pub fn dim(s: &str) -> String {
    paint("2", s)
}
pub fn bold(s: &str) -> String {
    paint("1", s)
}
pub fn red(s: &str) -> String {
    paint("31", s)
}
pub fn green(s: &str) -> String {
    paint("32", s)
}
pub fn yellow(s: &str) -> String {
    paint("33", s)
}
pub fn cyan(s: &str) -> String {
    paint("36", s)
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
        // 非 tty のテスト環境では実 API も素通しになる。
        assert_eq!(super::red("err"), "err");
        assert_eq!(super::cyan("[Tool]"), "[Tool]");
    }
}
