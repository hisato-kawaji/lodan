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
    wrap_with(
        policy,
        command,
        &temp_dirs(&policy.cwd),
        sandbox_tool().as_deref(),
    )
}

/// この OS でサンドボックスを起動するプログラム。無ければ None。
fn sandbox_tool() -> Option<String> {
    if cfg!(target_os = "macos") {
        let exe = "/usr/bin/sandbox-exec";
        Path::new(exe).exists().then(|| exe.to_string())
    } else if cfg!(target_os = "linux") {
        find_in_path("bwrap")
    } else {
        None
    }
}

fn wrap_with(
    policy: &SandboxPolicy,
    command: &str,
    temp_dirs: &[PathBuf],
    tool: Option<&str>,
) -> Result<(String, Vec<String>), String> {
    if !policy.is_on() {
        return Ok(("sh".into(), vec!["-c".into(), command.into()]));
    }
    let Some(exe) = tool else {
        return Err(unavailable(policy));
    };
    if cfg!(target_os = "macos") {
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
    prepare_workspace(policy);
    Ok((exe.into(), bwrap_args(policy, command, temp_dirs)))
}

fn unavailable(policy: &SandboxPolicy) -> String {
    let what = if cfg!(target_os = "macos") {
        "sandbox-exec"
    } else if cfg!(target_os = "linux") {
        "bwrap (bubblewrap)"
    } else {
        "a supported sandbox (macOS sandbox-exec or Linux bwrap)"
    };
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
fn temp_dirs(cwd: &Path) -> Vec<PathBuf> {
    let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf());
    temp_dirs_from(
        std::env::var_os("TMPDIR").map(PathBuf::from),
        cwd,
        home.as_deref(),
    )
}

/// `TMPDIR` は環境変数なので鵜呑みにしない。`/` やホーム、作業ディレクトリを含む場所を指していたら、
/// 「一時ディレクトリは書ける」がそのまま「どこでも書ける」になり、保護したパスの意味も消える。
fn temp_dirs_from(tmpdir: Option<PathBuf>, cwd: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let real_path = |d: PathBuf| std::fs::canonicalize(&d).unwrap_or(d);
    let mut real: Vec<PathBuf> = vec![
        real_path(PathBuf::from("/private/tmp")),
        real_path(PathBuf::from("/tmp")),
    ];
    if let Some(dir) = tmpdir.map(real_path) {
        let swallows = |inner: &Path| real_path(inner.to_path_buf()).starts_with(&dir);
        if dir.is_absolute() && !swallows(cwd) && !home.is_some_and(swallows) {
            real.push(dir);
        }
    }
    real.sort();
    real.dedup();
    real
}

/// 作業ディレクトリの中でも書かせない場所。ここに書けると、サンドボックスの中のコマンドが
/// **次に外で動くもの**を仕込める: lodan の設定 (`sandbox.mode = "off"` や hooks)、起動時に読む
/// `.env`、MCP サーバの定義、git が実行する hooks と `core.fsmonitor` など。
/// サブモジュールの git ディレクトリ (`.git/modules/`) も、それぞれが hooks と config を持つ。
/// (パス, ディレクトリごとか)。
fn protected_paths(cwd: &Path) -> Vec<(PathBuf, bool)> {
    let mut paths = vec![
        (cwd.join(".lodan"), true),
        (cwd.join(".mcp.json"), false),
        (cwd.join(".env"), false),
    ];
    // 規則はパスの文字列で照合される。`.git` が symlink のリポジトリでは、リンク先の実パスを
    // 直接書かれると `.git/hooks` という名前の保護をすり抜けるので、実体の側も並べる。
    let dot_git = cwd.join(".git");
    let mut git_dirs = vec![dot_git.clone()];
    if dot_git.is_symlink()
        && let Ok(real) = std::fs::canonicalize(&dot_git)
        && real.is_dir()
    {
        git_dirs.push(real);
    }
    for git_dir in git_dirs {
        paths.push((git_dir.join("hooks"), true));
        paths.push((git_dir.join("modules"), true));
        paths.push((git_dir.join("config"), false));
        paths.push((git_dir.join("config.worktree"), false));
    }
    paths
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
        // `.git` という名前そのものも固定する。中身を守っても、`.git` ごと rename して、hooks を
        // 仕込んだ別のディレクトリ (や symlink) を同じ名前で置かれたら意味が無い。中への書き込み
        // (index / objects / refs) はこれまでどおり通る。`git init` はサンドボックスの外で。
        let dot_git_path = policy.cwd.join(".git");
        let dot_git = sbpl_string(&dot_git_path);
        p.push_str(&format!(
            "(deny file-write-create (literal {dot_git}))\n(deny file-write-unlink (literal {dot_git}))\n"
        ));
        // linked worktree やサブモジュールでは `.git` は `gitdir: <path>` と書かれた**ファイル**。
        // 中身を書き換えて、hooks を仕込んだ偽の git ディレクトリを指させることができてしまうので、
        // ディレクトリでないときは書き込みを丸ごと拒否する。
        if !dot_git_path.is_dir() {
            p.push_str(&format!("(deny file-write* (literal {dot_git}))\n"));
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
            // 順序が効く: 後の bind は、その下にある前の mount を覆い隠す。作業ディレクトリが
            // /tmp 以下にあるとき、一時ディレクトリを後から bind すると下の保護がまるごと消える。
            for dir in temp_dirs {
                let dir = dir.display().to_string();
                a.extend(["--bind-try".into(), dir.clone(), dir]);
            }
            let cwd = policy.cwd.display().to_string();
            a.extend(["--bind".into(), cwd.clone(), cwd]);
            // `.git` をそれ自身に bind して mount point にする。mount point は rename も削除も
            // できない (EBUSY) ので、`.git` ごと差し替えて hooks の保護を外す回り道が塞がる。
            let dot_git_path = policy.cwd.join(".git");
            let dot_git = dot_git_path.display().to_string();
            if dot_git_path.is_dir() {
                a.extend(["--bind-try".into(), dot_git.clone(), dot_git]);
            } else {
                // linked worktree やサブモジュールの `.git` は `gitdir: <path>` と書かれたファイル。
                // 読み取り専用にして、偽の git ディレクトリを指すように書き換えさせない。
                a.extend(["--ro-bind-try".into(), dot_git.clone(), dot_git]);
            }
            // bind し直せるのは既にあるパスだけ (`-try` は無ければ黙って飛ばす)。だからディレクトリは
            // `prepare_workspace` が先に作っておく。まだ無い**ファイル**は止められないので、
            // 実行後に `planted` で検出して知らせる。
            for (path, _) in protected_paths(&policy.cwd) {
                // `.git` がファイルのとき、その「下」のパスは ENOTDIR になる。`-try` が見逃すのは
                // ENOENT だけで、ENOTDIR では bwrap ごと起動に失敗する。
                if path.starts_with(&dot_git_path) && !dot_git_path.is_dir() {
                    continue;
                }
                let path = path.display().to_string();
                a.extend(["--ro-bind-try".into(), path.clone(), path]);
            }
        }
        // 読み取り専用では何も bind し直さない。`--tmpfs /tmp` で書ける一時領域を見せる手もあるが、
        // /tmp の中身 (作業ディレクトリがそこにあればそれごと) を隠してしまい、macOS とも揃わない。
        SandboxMode::ReadOnly | SandboxMode::Off => {}
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

/// bwrap は無いパスを守れないので、守りたいディレクトリを先に作っておく (Linux の workspace-write
/// だけ)。空の `.lodan/`・`.git/hooks/`・`.git/modules/` は無害で、どれも lodan / git が普通に作るもの。
/// 作れなければ諦める (その場合は `planted` の検出だけが残る)。
fn prepare_workspace(policy: &SandboxPolicy) {
    if policy.mode != SandboxMode::WorkspaceWrite {
        return;
    }
    let _ = std::fs::create_dir_all(policy.cwd.join(".lodan"));
    let dot_git = policy.cwd.join(".git");
    if dot_git.is_dir() {
        let _ = std::fs::create_dir_all(dot_git.join("hooks"));
        let _ = std::fs::create_dir_all(dot_git.join("modules"));
    }
}

/// 実行前に呼ぶ: 守りたいのに OS では守れない (= まだ存在しない) パス。macOS では seatbelt が
/// 作成そのものを止めるので常に空。
pub fn unguarded(policy: &SandboxPolicy) -> Vec<PathBuf> {
    if !cfg!(target_os = "linux") || policy.mode != SandboxMode::WorkspaceWrite {
        return Vec::new();
    }
    // 自分で作る分を先に済ませる (後から現れると、仕込まれたものと区別がつかない)。
    prepare_workspace(policy);
    let mut paths: Vec<PathBuf> = protected_paths(&policy.cwd)
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    let dot_git = policy.cwd.join(".git");
    // `.git` がファイル (worktree) なら、その下のパスは作りようがない。
    let gitfile = dot_git.is_file();
    paths.retain(|p| !(gitfile && p.starts_with(&dot_git)));
    paths.push(dot_git);
    paths.retain(|p| std::fs::symlink_metadata(p).is_err());
    paths
}

/// 実行後に呼ぶ: `unguarded` のうち、コマンドの実行中に現れたもの。
pub fn planted(unguarded: &[PathBuf]) -> Vec<PathBuf> {
    unguarded
        .iter()
        .filter(|p| std::fs::symlink_metadata(p).is_ok())
        .cloned()
        .collect()
}

/// `planted` が空でないときにツール出力と利用者の両方へ出す警告。
pub fn planted_warning(paths: &[PathBuf]) -> String {
    let list: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    format!(
        "[sandbox] WARNING: this command created {} — a file lodan or git acts on OUTSIDE the \
         sandbox next time. The Linux sandbox cannot block the creation of paths that did not \
         exist yet. Review it before running lodan or git here again.",
        list.join(", ")
    )
}

/// サンドボックスに拒否されたらしい失敗に添える注記。モデルが同じコマンドを繰り返したり、
/// 回り道を探したりしないように、何が起きたかを伝える。
pub fn denial_hint(policy: &SandboxPolicy, stderr: &str, succeeded: bool) -> Option<String> {
    if !policy.is_on() || succeeded {
        return None;
    }
    // 注記は助言にすぎないので、取りこぼすより広めに拾う (ただの `chmod` 忘れにも付くことがある)。
    let stderr = stderr.to_lowercase();
    let mentions = |needles: &[&str]| needles.iter().any(|m| stderr.contains(m));
    let blocked_write = mentions(&[
        "operation not permitted",
        "read-only file system",
        "permission denied",
        "eacces",
        "eperm",
        "erofs",
        "device or resource busy",
    ]);
    let blocked_net = !policy.network
        && mentions(&[
            "could not resolve host",
            "network is unreachable",
            "enotfound",
            "no such host",
            "name resolution",
            "name or service not known",
            "connection refused",
        ]);
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
        assert!(!ro.contains("--bind") && !ro.contains("tmpfs"), "{ro}");
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
        if !usable {
            // 飛ばしたことが `--nocapture` で分かるようにする (黙って green にしない)。
            eprintln!(
                "skipped: no usable OS sandbox here (bwrap missing or user namespaces blocked)"
            );
        }
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
        run_with_temp(policy, command, &[])
    }

    fn run_with_temp(policy: &SandboxPolicy, command: &str, temp_dirs: &[PathBuf]) -> bool {
        let (exe, args) =
            wrap_with(policy, command, temp_dirs, sandbox_tool().as_deref()).expect("sandbox tool");
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
        match wrap_with(&probe, "true", &[], sandbox_tool().as_deref()) {
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

    #[test]
    fn without_the_os_tool_nothing_runs_unsandboxed() {
        let on = policy(SandboxMode::WorkspaceWrite, true);
        let err = wrap_with(&on, "echo hi", &[], None).unwrap_err();
        assert!(err.contains("Refusing to run"), "{err}");
        // off は道具が無くても影響を受けない。
        assert!(wrap_with(&SandboxPolicy::off(), "echo hi", &[], None).is_ok());
    }

    #[test]
    fn a_tmpdir_that_swallows_the_workspace_or_home_is_ignored() {
        let cwd = Path::new("/work/proj");
        let home = Some(Path::new("/home/me"));
        let has = |tmpdir: &str| {
            temp_dirs_from(Some(PathBuf::from(tmpdir)), cwd, home).contains(&PathBuf::from(tmpdir))
        };
        assert!(has("/var/scratch"));
        for hostile in [
            "/",
            "/work",
            "/work/proj",
            "/home",
            "/home/me",
            "relative/tmp",
        ] {
            assert!(!has(hostile), "{hostile}");
        }
    }

    /// `.git` の中身を守っても、`.git` ごと別物に差し替えられたら意味が無い (レビューで実際に通った)。
    #[test]
    fn the_git_directory_cannot_be_swapped_for_one_with_hooks() {
        let Some(a) = usable_arena() else {
            return;
        };
        let p = workspace_write(&a, true);
        let _ = run(
            &p,
            "mkdir -p st/hooks && echo pwn > st/hooks/pre-commit && mv .git .gitold && mv st .git",
        );
        let _ = run(
            &p,
            "mv .git .gold; ln -s .gold .git; echo pwn > .git/hooks/pre-commit",
        );
        let _ = run(
            &p,
            "mkdir -p .git/modules/sub/hooks && echo pwn > .git/modules/sub/hooks/pre-commit",
        );
        assert!(a.ws.join(".git").is_dir() && !a.ws.join(".git").is_symlink());
        assert!(!a.ws.join(".git/hooks/pre-commit").exists());
        assert!(!a.ws.join(".git/modules/sub/hooks/pre-commit").exists());
        // git の普段の書き込み先は生きている。
        assert!(run(&p, "echo x > .git/index && mkdir -p .git/objects/ab"));
    }

    /// 作業ディレクトリが一時ディレクトリの下にあっても (CI やスクラッチではよくある)、
    /// 一時ディレクトリへの許可が保護を上書きしない。
    #[test]
    fn a_workspace_under_the_temp_dir_keeps_its_protection() {
        let Some(a) = usable_arena() else {
            return;
        };
        let p = workspace_write(&a, true);
        let temp = [a.ws.parent().unwrap().to_path_buf()];
        assert!(run_with_temp(&p, "echo a > ok.txt", &temp));
        assert!(!run_with_temp(&p, "echo x > .lodan/config.toml", &temp));
        assert!(!run_with_temp(&p, "echo x > .git/hooks/pre-commit", &temp));
        assert!(!a.ws.join(".lodan/config.toml").exists());
    }

    /// まだ無い `.lodan/` を作って設定を置く、という一番ありそうな形。
    #[test]
    fn a_config_directory_that_does_not_exist_yet_cannot_be_planted() {
        let Some(a) = usable_arena() else {
            return;
        };
        std::fs::remove_dir_all(a.ws.join(".lodan")).unwrap();
        let p = workspace_write(&a, true);
        let _ = run(
            &p,
            "mkdir -p .lodan && echo 'mode = \"off\"' > .lodan/config.toml",
        );
        assert!(!a.ws.join(".lodan/config.toml").exists());
    }

    /// Linux では、まだ無い**ファイル**の作成は止められない。止められない代わりに必ず気づく。
    #[cfg(target_os = "linux")]
    #[test]
    fn on_linux_a_planted_file_is_at_least_reported() {
        let Some(a) = usable_arena() else {
            return;
        };
        let p = workspace_write(&a, true);
        let before = unguarded(&p);
        assert!(before.contains(&a.ws.join(".env")), "{before:?}");
        assert!(
            !before.contains(&a.ws.join(".lodan")),
            "exists, so it is guarded"
        );
        assert!(planted(&before).is_empty());
        assert!(run(&p, "echo LODAN_SANDBOX=off > .env"));
        assert_eq!(planted(&before), vec![a.ws.join(".env")]);
        assert!(planted_warning(&planted(&before)).contains(".env"));
    }

    /// linked worktree / サブモジュールでは `.git` はファイル。そこでもサンドボックスが起動でき、
    /// かつ `.git` を書き換えて偽の git ディレクトリ (hooks 入り) を指させることはできない。
    #[test]
    fn a_gitfile_cannot_be_repointed_at_a_fake_git_directory() {
        let Some(a) = usable_arena() else {
            return;
        };
        std::fs::remove_dir_all(a.ws.join(".git")).unwrap();
        let gitfile = format!("gitdir: {}/real-gitdir\n", a.outside.display());
        std::fs::write(a.ws.join(".git"), &gitfile).unwrap();
        let p = workspace_write(&a, true);

        assert!(
            run(&p, "echo a > ok.txt"),
            "the sandbox must still start in a worktree"
        );
        let _ = run(
            &p,
            "mkdir -p fake/hooks && printf 'gitdir: %s/fake\\n' \"$PWD\" > .git",
        );
        let _ = run(&p, "mv .git .gitfile.bak; printf 'gitdir: fake\\n' > .git");
        let _ = run(
            &p,
            "rm -f .git; mkdir -p .git/hooks; echo pwn > .git/hooks/pre-commit",
        );
        assert_eq!(std::fs::read_to_string(a.ws.join(".git")).unwrap(), gitfile);
    }

    /// `.git` が最初から symlink のリポジトリ。リンク先を実パスで直接書いても hooks は置けない。
    #[cfg(unix)]
    #[test]
    fn a_symlinked_git_directory_is_protected_at_its_real_path_too() {
        let Some(a) = usable_arena() else {
            return;
        };
        std::fs::rename(a.ws.join(".git"), a.ws.join("realgit")).unwrap();
        std::os::unix::fs::symlink("realgit", a.ws.join(".git")).unwrap();
        let p = workspace_write(&a, true);
        let _ = run(&p, "echo pwn > realgit/hooks/pre-commit");
        let _ = run(&p, "echo pwn > .git/hooks/pre-commit");
        assert!(!a.ws.join("realgit/hooks/pre-commit").exists());
        assert!(
            run(&p, "echo x > realgit/index"),
            "git's own files stay writable"
        );
    }
}
