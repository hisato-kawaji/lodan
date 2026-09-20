//! Bash ツールのサンドボックス (#75)。
//!
//! 承認ゲートも permission ルールも、見ているのはコマンドの**文字列**。承認した `cargo test` が
//! 内部で何を実行するか、`make` が何を書き換えるかは制御できない。ここでは OS の仕組みで
//! 「どこに書けるか」「外へ出られるか」を制限する。
//!
//! - macOS: `sandbox-exec` に、起動のたびに組み立てたプロファイル (SBPL) を渡す
//! - Linux: `bwrap` (bubblewrap)。ルートを読み取り専用で bind し、書ける場所だけ上書きで bind する
//!
//! 読み取りは制限しない (制限すると普通のビルドが動かなくなる)。秘密を読ませたくないなら
//! permission ルールの deny と併用すること。
//!
//! 道具が無い環境で `mode` が off 以外なら、素通しにせず**実行を拒否する**。サンドボックスを
//! 頼んだのに黙って外で走るのが一番まずい。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// サンドボックスなし (既定)。
    #[default]
    Off,
    /// cwd 以下と一時ディレクトリにだけ書ける。
    WorkspaceWrite,
    /// どこにも書けない (`/dev/null` などを除く)。
    ReadOnly,
}

impl SandboxMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxMode::Off => "off",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::ReadOnly => "read-only",
        }
    }
}

/// `[sandbox]`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    pub mode: SandboxMode,
    /// false ならサンドボックス内からのネットワークを遮断する。`mode = "off"` では無視される。
    pub network: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: SandboxMode::Off,
            network: true,
        }
    }
}

/// 実行時の方針。`ToolCtx` に載せて Bash ツールへ渡す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    pub network: bool,
    /// 書き込みを許す作業ディレクトリ (symlink 解決済み)。
    pub cwd: PathBuf,
}

impl SandboxPolicy {
    pub fn off() -> Self {
        Self {
            mode: SandboxMode::Off,
            network: true,
            cwd: PathBuf::new(),
        }
    }

    pub fn new(cfg: &SandboxConfig, cwd: &Path) -> Self {
        Self {
            mode: cfg.mode,
            network: cfg.network,
            // プロファイルは実パスで照合される (macOS の /tmp → /private/tmp など)。
            cwd: std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()),
        }
    }

    pub fn is_on(&self) -> bool {
        self.mode != SandboxMode::Off
    }
}

/// `sh -c <command>` を方針に従って包んだ (プログラム, 引数)。off ならそのまま `sh -c`。
/// サンドボックスが要るのに道具が無ければ `Err` (呼び出し側は実行せずにエラーを返す)。
pub fn wrap(policy: &SandboxPolicy, command: &str) -> Result<(String, Vec<String>), String> {
    wrap_with(policy, command, &temp_dirs())
}

fn wrap_with(
    policy: &SandboxPolicy,
    command: &str,
    temp_dirs: &[PathBuf],
) -> Result<(String, Vec<String>), String> {
    if !policy.is_on() {
        return Ok(("sh".into(), vec!["-c".into(), command.into()]));
    }
    if cfg!(target_os = "macos") {
        let exe = "/usr/bin/sandbox-exec";
        if !Path::new(exe).exists() {
            return Err(unavailable(policy, "sandbox-exec"));
        }
        return Ok((
            exe.into(),
            vec![
                "-p".into(),
                seatbelt_profile(policy, temp_dirs),
                "sh".into(),
                "-c".into(),
                command.into(),
            ],
        ));
    }
    if cfg!(target_os = "linux") {
        let Some(exe) = find_in_path("bwrap") else {
            return Err(unavailable(policy, "bwrap (bubblewrap)"));
        };
        return Ok((exe, bwrap_args(policy, command, temp_dirs)));
    }
    Err(unavailable(
        policy,
        "a supported sandbox (macOS sandbox-exec or Linux bwrap)",
    ))
}

fn unavailable(policy: &SandboxPolicy, what: &str) -> String {
    format!(
        "sandbox.mode = \"{}\" but {what} is not available on this system. Refusing to run the \
         command unsandboxed. Install it, or set sandbox.mode = \"off\".",
        policy.mode.as_str()
    )
}

fn find_in_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
        .map(|p| p.display().to_string())
}

/// 書き込みを許す一時ディレクトリ (実パス)。コンパイラやテストランナーは TMPDIR に書く。
fn temp_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/private/tmp"), PathBuf::from("/tmp")];
    if let Some(t) = std::env::var_os("TMPDIR") {
        dirs.push(PathBuf::from(t));
    }
    let mut real: Vec<PathBuf> = dirs
        .into_iter()
        .map(|d| std::fs::canonicalize(&d).unwrap_or(d))
        .collect();
    real.sort();
    real.dedup();
    real
}

/// 作業ディレクトリの中でも書かせない場所。ここに書けると、サンドボックスの中のコマンドが
/// **次に外で動くもの**を仕込める: lodan の設定 (`sandbox.mode = "off"` や hooks)、起動時に読む
/// `.env`、MCP サーバの定義、git が実行する hooks と `core.fsmonitor` など。
/// (パス, ディレクトリごとか)。
fn protected_paths(cwd: &Path) -> Vec<(PathBuf, bool)> {
    vec![
        (cwd.join(".lodan"), true),
        (cwd.join(".git").join("hooks"), true),
        (cwd.join(".git").join("config"), false),
        (cwd.join(".mcp.json"), false),
        (cwd.join(".env"), false),
    ]
}

/// SBPL の文字列リテラル。パスに `"` や `\` が入っていてもプロファイルを壊させない。
fn sbpl_string(path: &Path) -> String {
    let mut out = String::from("\"");
    for c in path.to_string_lossy().chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// macOS の seatbelt プロファイル。既定は許可で、書き込み (と、指定があればネットワーク) だけを絞る。
/// 後に書いた規則が優先されるので、「書き込みを全部拒否 → 必要な場所だけ許可」の順に並べる。
pub fn seatbelt_profile(policy: &SandboxPolicy, temp_dirs: &[PathBuf]) -> String {
    let mut p = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    // シェルやツールが普通に使うデバイス。
    p.push_str(
        "(allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/tty\") \
         (literal \"/dev/dtracehelper\") (regex #\"^/dev/fd/\") (regex #\"^/dev/ttys[0-9]+$\"))\n",
    );
    if policy.mode == SandboxMode::WorkspaceWrite {
        p.push_str(&format!(
            "(allow file-write* (subpath {}))\n",
            sbpl_string(&policy.cwd)
        ));
        for dir in temp_dirs {
            p.push_str(&format!(
                "(allow file-write* (subpath {}))\n",
                sbpl_string(dir)
            ));
        }
        // 許可より後ろに置く (後勝ち)。パスで照合されるので、まだ無いファイルの作成も、
        // ディレクトリごと rename して作り直す回り道も止まる。
        for (path, is_dir) in protected_paths(&policy.cwd) {
            let filter = if is_dir { "subpath" } else { "literal" };
            p.push_str(&format!(
                "(deny file-write* ({filter} {}))\n",
                sbpl_string(&path)
            ));
        }
    }
    if !policy.network {
        p.push_str("(deny network*)\n");
    }
    p
}

/// bwrap の引数。ルートを読み取り専用で見せ、書ける場所だけを上から bind し直す。
pub fn bwrap_args(policy: &SandboxPolicy, command: &str, temp_dirs: &[PathBuf]) -> Vec<String> {
    let mut a: Vec<String> = ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    match policy.mode {
        SandboxMode::WorkspaceWrite => {
            let cwd = policy.cwd.display().to_string();
            a.extend(["--bind".into(), cwd.clone(), cwd]);
            for dir in temp_dirs {
                let dir = dir.display().to_string();
                a.extend(["--bind-try".into(), dir.clone(), dir]);
            }
            // bind し直せるのは既にあるパスだけ (`-try` は無ければ黙って飛ばす)。まだ無い
            // `.lodan/` を新しく作られるのは bwrap では止められない — README に明記している。
            for (path, _) in protected_paths(&policy.cwd) {
                let path = path.display().to_string();
                a.extend(["--ro-bind-try".into(), path.clone(), path]);
            }
        }
        // 読み取り専用でも /tmp が無いと動かないプログラムが多い。中身の残らない tmpfs を見せる。
        SandboxMode::ReadOnly => a.extend(["--tmpfs".into(), "/tmp".into()]),
        SandboxMode::Off => {}
    }
    if !policy.network {
        a.push("--unshare-net".into());
    }
    a.extend(
        ["--die-with-parent", "--chdir"]
            .iter()
            .map(|s| s.to_string()),
    );
    a.push(policy.cwd.display().to_string());
    a.extend(["sh".into(), "-c".into(), command.into()]);
    a
}

/// サンドボックスに拒否されたらしい失敗に添える注記。モデルが同じコマンドを繰り返したり、
/// 回り道を探したりしないように、何が起きたかを伝える。
pub fn denial_hint(policy: &SandboxPolicy, stderr: &str, succeeded: bool) -> Option<String> {
    if !policy.is_on() || succeeded {
        return None;
    }
    let blocked_write = stderr.contains("Operation not permitted")
        || stderr.contains("Read-only file system")
        || stderr.contains("Permission denied");
    let blocked_net = !policy.network
        && [
            "Could not resolve host",
            "Network is unreachable",
            "Operation not permitted",
        ]
        .iter()
        .any(|m| stderr.contains(m));
    (blocked_write || blocked_net).then(|| {
        format!(
            "[sandbox] This command ran under sandbox.mode = \"{}\" (network {}). Writes outside {} \
             and the temp directories are blocked{}. If the failure above is such a block, do not \
             retry or work around it — tell the user what needs to run outside the sandbox.",
            policy.mode.as_str(),
            if policy.network { "allowed" } else { "blocked" },
            policy.cwd.display(),
            if policy.mode == SandboxMode::ReadOnly { " (read-only mode: all writes are)" } else { "" },
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: SandboxMode, network: bool) -> SandboxPolicy {
        SandboxPolicy {
            mode,
            network,
            cwd: PathBuf::from("/work/my \"proj\""),
        }
    }

    #[test]
    fn off_is_plain_sh() {
        let (exe, args) = wrap(&SandboxPolicy::off(), "echo hi").unwrap();
        assert_eq!(
            (exe.as_str(), args),
            ("sh", vec!["-c".to_string(), "echo hi".to_string()])
        );
    }

    #[test]
    fn the_seatbelt_profile_denies_writes_first_then_reopens_only_what_is_needed() {
        let p = seatbelt_profile(
            &policy(SandboxMode::WorkspaceWrite, false),
            &[PathBuf::from("/private/tmp")],
        );
        let deny = p.find("(deny file-write*)").unwrap();
        let cwd = p
            .find(r#"(allow file-write* (subpath "/work/my \"proj\""))"#)
            .expect("escaped cwd");
        assert!(
            deny < cwd,
            "later rules win in SBPL, so the deny must come first:\n{p}"
        );
        assert!(p.contains(r#"(subpath "/private/tmp")"#));
        let protect = p
            .find(r#"(deny file-write* (subpath "/work/my \"proj\"/.lodan"))"#)
            .expect("the config dir stays read-only");
        assert!(
            cwd < protect,
            "and that deny must come after the allow:\n{p}"
        );
        assert!(p.contains(r#"(literal "/work/my \"proj\"/.git/config")"#));
        assert!(p.contains("(deny network*)"));

        let ro = seatbelt_profile(
            &policy(SandboxMode::ReadOnly, true),
            &[PathBuf::from("/private/tmp")],
        );
        assert!(
            !ro.contains("subpath"),
            "read-only reopens no directory:\n{ro}"
        );
        assert!(!ro.contains("(deny network*)"));
    }

    #[test]
    fn bwrap_binds_the_root_read_only_and_only_the_workspace_writable() {
        let a = bwrap_args(
            &policy(SandboxMode::WorkspaceWrite, false),
            "make",
            &[PathBuf::from("/tmp")],
        );
        let joined = a.join(" ");
        assert!(
            joined.starts_with("--ro-bind / / --dev /dev --proc /proc"),
            "{joined}"
        );
        assert!(joined.contains("--bind /work/my \"proj\" /work/my \"proj\""));
        assert!(joined.contains("--ro-bind-try /work/my \"proj\"/.lodan /work/my \"proj\"/.lodan"));
        assert!(joined.contains("--bind-try /tmp /tmp"));
        assert!(joined.contains("--unshare-net"));
        assert_eq!(&a[a.len() - 3..], ["sh", "-c", "make"]);

        let ro = bwrap_args(
            &policy(SandboxMode::ReadOnly, true),
            "ls",
            &[PathBuf::from("/tmp")],
        )
        .join(" ");
        assert!(
            !ro.contains("--bind /work") && ro.contains("--tmpfs /tmp"),
            "{ro}"
        );
        assert!(!ro.contains("--unshare-net"));
    }

    #[test]
    fn a_hint_is_added_only_for_failures_that_look_like_a_sandbox_block() {
        let on = policy(SandboxMode::WorkspaceWrite, false);
        assert!(denial_hint(&on, "touch: /etc/x: Operation not permitted", false).is_some());
        assert!(denial_hint(&on, "curl: (6) Could not resolve host: example.com", false).is_some());
        assert!(denial_hint(&on, "error[E0308]: mismatched types", false).is_none());
        assert!(
            denial_hint(&on, "Operation not permitted", true).is_none(),
            "the command succeeded"
        );
        assert!(denial_hint(&SandboxPolicy::off(), "Operation not permitted", false).is_none());
    }

    // ---- ここから下は OS のサンドボックスを実際に起動する ----
    //
    // テスト用の作業場も「外側」も一時ディレクトリの下に作るので、一時ディレクトリへの許可は
    // 外して (`wrap_with(.., &[])`) 作業場だけが書ける状態で確かめる。

    struct Arena {
        _root: tempfile::TempDir,
        ws: PathBuf,
        outside: PathBuf,
    }

    /// サンドボックスを起動できる環境なら作業場を返す。macOS では必ず起動できるはずなので、
    /// できなければ黙って飛ばさずに落とす。
    fn usable_arena() -> Option<Arena> {
        let a = arena();
        let usable = sandbox_usable(&a);
        assert!(
            usable || !cfg!(target_os = "macos"),
            "sandbox-exec should work on macOS"
        );
        usable.then_some(a)
    }

    fn arena() -> Arena {
        let root = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(root.path()).unwrap();
        let (ws, outside) = (real.join("ws"), real.join("outside"));
        std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
        std::fs::create_dir_all(ws.join(".lodan")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("victim"), "orig").unwrap();
        Arena {
            _root: root,
            ws,
            outside,
        }
    }

    /// サンドボックスの中で走らせ、成功したかを返す。
    fn run(policy: &SandboxPolicy, command: &str) -> bool {
        let (exe, args) = wrap_with(policy, command, &[]).expect("sandbox tool");
        std::process::Command::new(exe)
            .args(args)
            .current_dir(&policy.cwd)
            .output()
            .expect("spawn")
            .status
            .success()
    }

    /// この環境でサンドボックスを起動できるか。Linux は bwrap が入っていて、かつ
    /// user namespace が使えるときだけ (CI のコンテナでは塞がれていることがある)。
    fn sandbox_usable(a: &Arena) -> bool {
        let probe = SandboxPolicy {
            mode: SandboxMode::ReadOnly,
            network: true,
            cwd: a.ws.clone(),
        };
        match wrap_with(&probe, "true", &[]) {
            Ok((exe, args)) => std::process::Command::new(exe)
                .args(args)
                .current_dir(&a.ws)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    fn workspace_write(a: &Arena, network: bool) -> SandboxPolicy {
        SandboxPolicy {
            mode: SandboxMode::WorkspaceWrite,
            network,
            cwd: a.ws.clone(),
        }
    }

    #[test]
    fn workspace_write_really_confines_writes_to_the_workspace() {
        let Some(a) = usable_arena() else {
            return;
        };
        let p = workspace_write(&a, true);
        assert!(run(
            &p,
            "echo a > inside.txt && mkdir sub && echo b > sub/x"
        ));
        assert_eq!(std::fs::read_to_string(a.ws.join("sub/x")).unwrap(), "b\n");

        assert!(!run(&p, "echo a > ../outside/new"));
        assert!(!a.outside.join("new").exists());
        // 作業場の中の名前を経由しても、外のファイルは書き換えられない。
        let _ = run(&p, "ln -s ../outside/victim sl; echo pwned > sl");
        let _ = run(&p, "ln ../outside/victim hl && echo pwned > hl");
        assert_eq!(
            std::fs::read_to_string(a.outside.join("victim")).unwrap(),
            "orig"
        );
    }

    #[test]
    fn the_workspace_cannot_rewrite_what_runs_outside_the_sandbox_next() {
        let Some(a) = usable_arena() else {
            return;
        };
        let p = workspace_write(&a, true);
        assert!(!run(&p, "echo 'mode = \"off\"' > .lodan/config.toml"));
        assert!(!run(&p, "echo x > .git/hooks/pre-commit"));
        assert!(!a.ws.join(".lodan/config.toml").exists());
        assert!(!a.ws.join(".git/hooks/pre-commit").exists());
        // まだ無いファイルの作成と、rename しての作り直しは seatbelt でだけ止められる。
        if cfg!(target_os = "macos") {
            assert!(!run(&p, "echo x > .env"));
            assert!(!run(&p, "echo x > .mcp.json"));
            let _ = run(
                &p,
                "mv .lodan .l2; mkdir -p .lodan; echo x > .lodan/config.toml",
            );
            assert!(!a.ws.join(".lodan/config.toml").exists());
        }
    }

    #[test]
    fn read_only_blocks_every_write_but_still_runs_commands() {
        let Some(a) = usable_arena() else {
            return;
        };
        let p = SandboxPolicy {
            mode: SandboxMode::ReadOnly,
            network: true,
            cwd: a.ws.clone(),
        };
        assert!(run(&p, "ls / > /dev/null && cat ../outside/victim"));
        assert!(!run(&p, "echo a > inside.txt"));
        assert!(!a.ws.join("inside.txt").exists());
    }

    #[test]
    fn network_off_blocks_even_loopback() {
        let Some(a) = usable_arena() else {
            return;
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // bash の /dev/tcp は外部コマンド無しで TCP 接続を試せる。
        let connect = format!("bash -c 'exec 3<>/dev/tcp/127.0.0.1/{port}'");
        assert!(
            run(&workspace_write(&a, true), &connect),
            "positive control: reachable when the network is allowed"
        );
        assert!(!run(&workspace_write(&a, false), &connect));
    }
}
