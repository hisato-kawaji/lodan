//! 宣言的な permission ルール (#74)。`[permissions] allow / deny / ask` に書く。
//!
//! 構文は Claude Code と同じ `Tool` / `Tool(pattern)`:
//!
//! - `Bash` — Bash の全呼び出し。`Bash(git status)` は完全一致、`Bash(git diff *)` は `*` が任意の
//!   文字列。`Bash(npm run test:*)` の `:*` は「そのコマンド、またはその後に引数が続く」
//! - `Read(src/**)` / `Edit(*.md)` / `Write(/etc/**)` / `Read(~/.ssh/**)` — パスの glob。相対パターンは
//!   cwd 基準、`/` を含まないパターンはどの階層のファイル名にも一致 (gitignore と同じ)
//! - `WebFetch(domain:example.com)` — ホスト名。サブドメインも含む
//! - `mcp__github` — その MCP サーバの全ツール。`mcp__github__create_issue` で 1 つだけ
//!
//! 評価順は **deny → ask → allow**。deny は read-only ツールにも、`--yes` (bypass) にも効く。
//!
//! # Bash の複合コマンド
//!
//! `Bash(git *)` を allow していても `git status && rm -rf /` を通してはいけない。コマンドを
//! `&&` `||` `;` `|` `&` と改行で分割し、**全ての部分が allow に一致したときだけ** allow とする。
//! `$(…)`・バッククォート・プロセス置換・リダイレクトを含むコマンドは中身を追い切れないので、
//! allow には決して一致させない (= 尋ねる側に倒す)。deny は逆に、コマンド全体か**いずれかの
//! 部分**が一致すれば効く。

use anyhow::{Result, bail};
use std::path::{Component, Path, PathBuf};

/// `path` 引数を glob で照合するツール。
const PATH_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "MultiEdit",
    "NotebookEdit",
    "Glob",
    "Grep",
];

#[derive(Debug, Clone)]
pub struct Rule {
    tool: String,
    matcher: Matcher,
    /// 設定に書かれたままの文字列 (拒否理由に出す)。
    raw: String,
}

#[derive(Debug, Clone)]
enum Matcher {
    /// パターンなし: そのツールの全呼び出し。
    Any,
    Bash(BashPattern),
    Path(PathPattern),
    Domain(String),
}

#[derive(Debug, Clone)]
struct BashPattern {
    /// `*` で区切った固定部分。1 要素なら完全一致。
    parts: Vec<String>,
    /// `cmd:*` 形式: `cmd` そのもの、または `cmd` + 空白 + 任意。
    word_prefix: bool,
}

#[derive(Debug, Clone)]
struct PathPattern {
    glob: globset::GlobMatcher,
    /// `/` か `~/` で始まるパターン。絶対パスと照合する。それ以外は cwd 相対のパスと照合する。
    absolute: bool,
}

impl Rule {
    pub fn parse(raw: &str) -> Result<Self> {
        let text = raw.trim();
        let (tool, pattern) = match text.split_once('(') {
            None => (text, None),
            Some((tool, rest)) => {
                let Some(pattern) = rest.strip_suffix(')') else {
                    bail!("permission rule `{raw}`: missing closing `)`");
                };
                (tool.trim(), Some(pattern))
            }
        };
        if tool.is_empty() || !tool.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!("permission rule `{raw}`: `{tool}` is not a tool name");
        }

        let matcher = match pattern {
            None => Matcher::Any,
            Some(p) if p.trim().is_empty() || p.trim() == "*" => Matcher::Any,
            Some(p) if tool == "Bash" => Matcher::Bash(BashPattern::parse(p.trim())),
            Some(p) if PATH_TOOLS.contains(&tool) => {
                Matcher::Path(PathPattern::parse(raw, p.trim())?)
            }
            Some(p) if tool == "WebFetch" => match p.trim().strip_prefix("domain:") {
                Some(d) if !d.trim().is_empty() => Matcher::Domain(d.trim().to_ascii_lowercase()),
                _ => bail!(
                    "permission rule `{raw}`: WebFetch patterns look like `WebFetch(domain:example.com)`"
                ),
            },
            // 黙って無視すると「書いたのに効いていない」deny が生まれる。
            Some(_) => bail!(
                "permission rule `{raw}`: `{tool}` does not take a pattern (only Bash, WebFetch and the \
                 file tools {PATH_TOOLS:?} do); write `{tool}` to match every call"
            ),
        };
        Ok(Self {
            tool: tool.to_string(),
            matcher,
            raw: text.to_string(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    fn names(&self, tool: &str) -> bool {
        self.tool == tool
            // `mcp__server` はそのサーバの全ツール。
            || (self.tool.starts_with("mcp__")
                && tool
                    .strip_prefix(self.tool.as_str())
                    .is_some_and(|rest| rest.starts_with("__")))
    }

    /// allow / ask として一致するか。追い切れない Bash コマンドには一致しない。
    fn permits(&self, tool: &str, args: &serde_json::Value, cwd: &Path) -> bool {
        if !self.names(tool) {
            return false;
        }
        match &self.matcher {
            Matcher::Any => true,
            Matcher::Bash(p) => bash_command(args).is_some_and(|cmd| match split_command(cmd) {
                Some(parts) => !parts.is_empty() && parts.iter().all(|part| p.matches(part)),
                None => false,
            }),
            // allow は「どの見え方でも一致」を要求する (symlink で cwd の外を指していたら不一致)。
            Matcher::Path(p) => path_candidates(args, cwd)
                .is_some_and(|c| c.iter().all(|path| p.matches(path, cwd))),
            Matcher::Domain(d) => url_host(args).is_some_and(|h| host_matches(&h, d)),
        }
    }

    /// deny として一致するか。疑わしいものは一致させる側に倒す。
    fn forbids(&self, tool: &str, args: &serde_json::Value, cwd: &Path) -> bool {
        if !self.names(tool) {
            return false;
        }
        match &self.matcher {
            Matcher::Any => true,
            Matcher::Bash(p) => bash_command(args).is_some_and(|cmd| {
                p.matches(cmd.trim()) || split_command_lossy(cmd).iter().any(|part| p.matches(part))
            }),
            Matcher::Path(p) => path_candidates(args, cwd)
                .is_some_and(|c| c.iter().any(|path| p.matches(path, cwd))),
            Matcher::Domain(d) => url_host(args).is_some_and(|h| host_matches(&h, d)),
        }
    }
}

impl BashPattern {
    fn parse(pattern: &str) -> Self {
        if let Some(prefix) = pattern.strip_suffix(":*") {
            return Self {
                parts: vec![prefix.trim().to_string()],
                word_prefix: true,
            };
        }
        Self {
            parts: pattern.split('*').map(str::to_string).collect(),
            word_prefix: false,
        }
    }

    fn matches(&self, command: &str) -> bool {
        let command = command.trim();
        if self.word_prefix {
            let prefix = &self.parts[0];
            return command == prefix
                || command
                    .strip_prefix(prefix.as_str())
                    .is_some_and(|rest| rest.starts_with(char::is_whitespace));
        }
        wildcard_match(&self.parts, command)
    }
}

/// `*` で区切った固定部分の列が `text` に順に現れるか (先頭と末尾は固定)。
fn wildcard_match(parts: &[String], text: &str) -> bool {
    let [first, middle @ .., last] = parts else {
        return parts.first().is_some_and(|only| only == text);
    };
    let Some(mut rest) = text.strip_prefix(first.as_str()) else {
        return false;
    };
    for part in middle {
        match rest.find(part.as_str()) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last.as_str())
}

impl PathPattern {
    fn parse(raw: &str, pattern: &str) -> Result<Self> {
        let (absolute, body) = if let Some(rest) = pattern.strip_prefix("~/") {
            let home = directories::BaseDirs::new()
                .map(|d| d.home_dir().to_path_buf())
                .ok_or_else(|| anyhow::anyhow!("permission rule `{raw}`: cannot resolve `~`"))?;
            (true, format!("{}/{rest}", home.display()))
        } else if pattern.starts_with('/') {
            (true, pattern.to_string())
        } else if pattern.contains('/') {
            (false, pattern.trim_start_matches("./").to_string())
        } else {
            // gitignore と同じく、`/` の無いパターンはどの階層のファイル名にも一致する。
            (false, format!("**/{pattern}"))
        };
        let glob = globset::GlobBuilder::new(&body)
            .literal_separator(true)
            .build()
            .map_err(|e| anyhow::anyhow!("permission rule `{raw}`: {e}"))?
            .compile_matcher();
        Ok(Self { glob, absolute })
    }

    fn matches(&self, path: &Path, cwd: &Path) -> bool {
        if self.absolute {
            return self.glob.is_match(path);
        }
        path.strip_prefix(cwd)
            .is_ok_and(|rel| self.glob.is_match(rel))
    }
}

fn bash_command(args: &serde_json::Value) -> Option<&str> {
    args.get("command").and_then(|v| v.as_str())
}

/// 照合するパスの「見え方」を全て返す: `..` を字句的に畳んだ絶対パスと、実在すれば symlink を
/// 解決したパス。`src/../.env` や cwd の外を指す symlink でルールをすり抜けさせない。
fn path_candidates(args: &serde_json::Value, cwd: &Path) -> Option<Vec<PathBuf>> {
    let raw = args.get("path").and_then(|v| v.as_str())?;
    let lexical = normalize(&cwd.join(raw));
    let mut out = vec![lexical.clone()];
    if let Ok(real) = std::fs::canonicalize(&lexical)
        && real != lexical
    {
        out.push(real);
    }
    Some(out)
}

/// `.` と `..` を字句的に畳む (ファイルシステムには触らない)。
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn url_host(args: &serde_json::Value) -> Option<String> {
    let url = args.get("url").and_then(|v| v.as_str())?;
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next()?;
    // userinfo を落としてから port を落とす (`user@evil.com:80` を `user` と読まない)。
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

fn host_matches(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|rest| rest.ends_with('.'))
}

/// コマンドを単純コマンドに分割する。中身を追い切れない構文 (コマンド置換・プロセス置換・
/// リダイレクト・閉じていない引用符) を含むなら `None`。
fn split_command(command: &str) -> Option<Vec<String>> {
    let (parts, opaque) = scan_command(command);
    (!opaque).then_some(parts)
}

/// deny 用: 追い切れない構文があっても、分割できた範囲を返す。
fn split_command_lossy(command: &str) -> Vec<String> {
    scan_command(command).0
}

fn scan_command(command: &str) -> (Vec<String>, bool) {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut opaque = false;
    let mut chars = command.chars().peekable();
    // None = 引用符の外、Some(q) = q で始まった引用符の中。
    let mut quote: Option<char> = None;

    while let Some(c) = chars.next() {
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                }
                current.push(c);
            }
            Some(_) => {
                // 二重引用符の中でも `$(…)` とバッククォートは展開される。
                match c {
                    '"' => quote = None,
                    '`' => opaque = true,
                    '$' if chars.peek() == Some(&'(') => opaque = true,
                    '\\' => {
                        current.push(c);
                        if let Some(next) = chars.next() {
                            current.push(next);
                        }
                        continue;
                    }
                    _ => {}
                }
                current.push(c);
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    current.push(c);
                }
                '\\' => {
                    current.push(c);
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                '`' => {
                    opaque = true;
                    current.push(c);
                }
                '$' if chars.peek() == Some(&'(') => {
                    opaque = true;
                    current.push(c);
                }
                '<' | '>' => {
                    // リダイレクトもプロセス置換も、allow の対象を超えてファイルに触れ得る。
                    opaque = true;
                    current.push(c);
                }
                ';' | '\n' | '|' | '&' => {
                    // `&&` `||` も 1 文字ずつここに来る。空の部分は捨てるので区別は要らない。
                    let part = current.trim();
                    if !part.is_empty() {
                        parts.push(part.to_string());
                    }
                    current.clear();
                }
                _ => current.push(c),
            },
        }
    }
    if quote.is_some() {
        opaque = true;
    }
    let part = current.trim();
    if !part.is_empty() {
        parts.push(part.to_string());
    }
    (parts, opaque)
}

/// ルールだけで決まる判定。どのルールにも当たらなければ `None` (既定の扱いに任せる)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 一致した deny ルール。
    Deny(String),
    Ask,
    Allow,
}

#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    allow: Vec<Rule>,
    deny: Vec<Rule>,
    ask: Vec<Rule>,
}

impl RuleSet {
    /// 1 つでも解釈できないルールがあればエラー。権限の設定を黙って読み飛ばさない。
    pub fn parse(allow: &[String], deny: &[String], ask: &[String]) -> Result<Self> {
        let parse_all = |rules: &[String]| {
            rules
                .iter()
                .map(|r| Rule::parse(r))
                .collect::<Result<Vec<_>>>()
        };
        Ok(Self {
            allow: parse_all(allow)?,
            deny: parse_all(deny)?,
            ask: parse_all(ask)?,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty() && self.ask.is_empty()
    }

    pub fn evaluate(&self, tool: &str, args: &serde_json::Value, cwd: &Path) -> Option<Verdict> {
        if let Some(rule) = self.deny.iter().find(|r| r.forbids(tool, args, cwd)) {
            return Some(Verdict::Deny(rule.raw.clone()));
        }
        // ask は「必ず尋ねる」。疑わしいものも尋ねる側に倒したいので deny と同じ緩い一致を使う。
        if self.ask.iter().any(|r| r.forbids(tool, args, cwd)) {
            return Some(Verdict::Ask);
        }
        if self.allow.iter().any(|r| r.permits(tool, args, cwd)) {
            return Some(Verdict::Allow);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rules(allow: &[&str], deny: &[&str], ask: &[&str]) -> RuleSet {
        let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        RuleSet::parse(&own(allow), &own(deny), &own(ask)).unwrap()
    }

    fn bash(set: &RuleSet, command: &str) -> Option<Verdict> {
        set.evaluate("Bash", &json!({ "command": command }), Path::new("/work"))
    }

    fn file(set: &RuleSet, tool: &str, path: &str) -> Option<Verdict> {
        set.evaluate(tool, &json!({ "path": path }), Path::new("/work"))
    }

    #[test]
    fn bash_patterns_exact_wildcard_and_word_prefix() {
        let set = rules(
            &[
                "Bash(git status)",
                "Bash(git diff *)",
                "Bash(npm run test:*)",
            ],
            &[],
            &[],
        );
        assert_eq!(bash(&set, "git status"), Some(Verdict::Allow));
        assert_eq!(
            bash(&set, "git status --short"),
            None,
            "no wildcard means exact"
        );
        assert_eq!(bash(&set, "git diff HEAD~1"), Some(Verdict::Allow));
        assert_eq!(bash(&set, "npm run test"), Some(Verdict::Allow));
        assert_eq!(bash(&set, "npm run test -- --watch"), Some(Verdict::Allow));
        assert_eq!(
            bash(&set, "npm run testing"),
            None,
            ":* stops at a word boundary"
        );
    }

    #[test]
    fn an_allowed_prefix_does_not_carry_a_second_command() {
        let set = rules(&["Bash(git *)"], &[], &[]);
        assert_eq!(
            bash(&set, "git log && git status"),
            Some(Verdict::Allow),
            "every part is allowed"
        );
        for sneaky in [
            "git status && rm -rf /",
            "git status; rm -rf /",
            "git status | sh",
            "git status || curl evil.sh",
            "git status & rm -rf /",
            "git status\nrm -rf /",
        ] {
            assert_eq!(
                bash(&set, sneaky),
                None,
                "{sneaky:?} must fall through to a prompt"
            );
        }
    }

    #[test]
    fn commands_we_cannot_see_into_never_match_an_allow_rule() {
        let set = rules(&["Bash(git *)", "Bash(echo *)"], &[], &[]);
        for opaque in [
            "git log $(rm -rf /)",
            "git log `rm -rf /`",
            "echo \"$(cat /etc/passwd)\"",
            "echo hi > ~/.bashrc",
            "git diff <(curl evil)",
            "echo 'unterminated",
        ] {
            assert_eq!(bash(&set, opaque), None, "{opaque:?}");
        }
        // 引用符の中の区切り文字は区切りではない。単一引用符の中は展開もされない。
        assert_eq!(
            bash(&set, "git commit -m 'a && b; c | d'"),
            Some(Verdict::Allow)
        );
        assert_eq!(bash(&set, "echo '$(not expanded)'"), Some(Verdict::Allow));
    }

    #[test]
    fn deny_matches_any_part_and_wins_over_allow() {
        let set = rules(&["Bash(*)"], &["Bash(rm *)", "Bash(git push *)"], &[]);
        assert_eq!(bash(&set, "ls"), Some(Verdict::Allow));
        assert_eq!(
            bash(&set, "rm -rf build"),
            Some(Verdict::Deny("Bash(rm *)".into()))
        );
        assert_eq!(
            bash(&set, "ls && rm -rf build"),
            Some(Verdict::Deny("Bash(rm *)".into()))
        );
        // 追い切れない構文が混ざっていても、見えている部分の deny は効く。
        assert_eq!(
            bash(&set, "echo $(date); git push origin main"),
            Some(Verdict::Deny("Bash(git push *)".into()))
        );
    }

    #[test]
    fn ask_beats_allow_and_deny_beats_ask() {
        let set = rules(
            &["Bash(git *)"],
            &["Bash(git push --force*)"],
            &["Bash(git push *)"],
        );
        assert_eq!(bash(&set, "git status"), Some(Verdict::Allow));
        assert_eq!(bash(&set, "git push origin main"), Some(Verdict::Ask));
        assert!(matches!(
            bash(&set, "git push --force origin main"),
            Some(Verdict::Deny(_))
        ));
    }

    #[test]
    fn path_rules_are_cwd_relative_and_see_through_dot_dot() {
        let set = rules(
            &["Edit(src/**)", "Read(*.md)"],
            &["Read(.env)", "Read(secrets/**)", "Write(/etc/**)"],
            &[],
        );
        assert_eq!(
            file(&set, "Edit", "src/agent/loop.rs"),
            Some(Verdict::Allow)
        );
        assert_eq!(
            file(&set, "Edit", "/work/src/lib.rs"),
            Some(Verdict::Allow),
            "absolute path under cwd"
        );
        assert_eq!(
            file(&set, "Edit", "/elsewhere/src/lib.rs"),
            None,
            "relative rules stop at the cwd"
        );
        assert_eq!(
            file(&set, "Edit", "src/../Cargo.toml"),
            None,
            "`..` must not smuggle a path in"
        );
        // `/` の無いパターンはどの階層にも一致する。
        assert_eq!(
            file(&set, "Read", "docs/eval/README.md"),
            Some(Verdict::Allow)
        );
        assert!(matches!(
            file(&set, "Read", "crates/app/.env"),
            Some(Verdict::Deny(_))
        ));
        assert!(matches!(
            file(&set, "Read", "src/../secrets/key.pem"),
            Some(Verdict::Deny(_))
        ));
        assert!(matches!(
            file(&set, "Write", "/etc/hosts"),
            Some(Verdict::Deny(_))
        ));
        // ツール名が違えば別のルール。
        assert_eq!(file(&set, "Write", "src/lib.rs"), None);
    }

    #[test]
    fn a_symlink_out_of_the_cwd_does_not_satisfy_an_allow_but_does_trip_a_deny() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("key.pem"), "secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("key.pem"), cwd.join("src/innocent.rs")).unwrap();
        #[cfg(not(unix))]
        return;
        let cwd = std::fs::canonicalize(&cwd).unwrap();

        let args = json!({ "path": "src/innocent.rs" });
        let allow = rules(&["Read(src/**)"], &[], &[]);
        assert_eq!(
            allow.evaluate("Read", &args, &cwd),
            None,
            "the real file is outside src/"
        );

        let deny_pattern = format!(
            "Read({}/**)",
            std::fs::canonicalize(&outside).unwrap().display()
        );
        let deny = rules(&[], &[&deny_pattern], &[]);
        assert!(matches!(
            deny.evaluate("Read", &args, &cwd),
            Some(Verdict::Deny(_))
        ));
    }

    #[test]
    fn webfetch_domains_include_subdomains_and_ignore_userinfo_tricks() {
        let set = rules(
            &["WebFetch(domain:docs.rs)"],
            &["WebFetch(domain:internal.example)"],
            &[],
        );
        let fetch =
            |url: &str| set.evaluate("WebFetch", &json!({ "url": url }), Path::new("/work"));
        assert_eq!(fetch("https://docs.rs/tokio"), Some(Verdict::Allow));
        assert_eq!(fetch("https://static.docs.rs/x"), Some(Verdict::Allow));
        assert_eq!(
            fetch("https://notdocs.rs/"),
            None,
            "suffix match needs a dot boundary"
        );
        assert_eq!(
            fetch("https://docs.rs@evil.com/"),
            None,
            "userinfo is not the host"
        );
        assert!(matches!(
            fetch("http://api.internal.example:8080/"),
            Some(Verdict::Deny(_))
        ));
    }

    #[test]
    fn a_bare_tool_name_matches_every_call_and_mcp_prefixes_match_a_server() {
        let set = rules(
            &["WebSearch", "mcp__github"],
            &["mcp__github__delete_repo"],
            &[],
        );
        let any = json!({});
        assert_eq!(
            set.evaluate("WebSearch", &any, Path::new("/")),
            Some(Verdict::Allow)
        );
        assert_eq!(
            set.evaluate("mcp__github__list_issues", &any, Path::new("/")),
            Some(Verdict::Allow)
        );
        assert_eq!(
            set.evaluate("mcp__githubx__anything", &any, Path::new("/")),
            None
        );
        assert!(matches!(
            set.evaluate("mcp__github__delete_repo", &any, Path::new("/")),
            Some(Verdict::Deny(_))
        ));
        assert_eq!(
            set.evaluate("Bash", &json!({ "command": "ls" }), Path::new("/")),
            None
        );
    }

    #[test]
    fn rules_that_cannot_work_are_rejected_at_load_time() {
        for bad in [
            "Bash(git *",
            "(git *)",
            "Ba sh(ls)",
            "TodoWrite(anything)",
            "WebFetch(example.com)",
            "Read([)",
        ] {
            assert!(Rule::parse(bad).is_err(), "{bad:?} should not parse");
        }
        assert!(Rule::parse("  Bash( git status )  ").is_ok());
        assert!(Rule::parse("Read(*)").is_ok());
    }
}
