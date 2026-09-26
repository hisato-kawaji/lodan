//! ヘッドレス実行 (`lodan -p`) の end-to-end。実バイナリを起動して **stdout を検証する** —
//! stdout は呼び出し側との契約で、`Session` を直接叩くテストからは見えない。

use std::io::{BufRead, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const GREETING: &str = "Hello from mock LLM. Send 'demo' to run the full tool sequence.";
/// 1 回の lodan 実行に許す時間。ハングの退行をテストの失敗として表に出す。
const RUN_LIMIT: Duration = Duration::from_secs(60);

struct MockServer {
    child: Child,
    port: u16,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// mock を起動して、実際に bind されたポートを受け取る。
///
/// 以前は「空きポートを選んで閉じ、その番号を mock に渡す」方式だった。閉じてから mock が
/// bind するまでの間に、並列で走る別のテストが同じ番号を引くことがある。すると後発の mock は
/// bind に失敗して終了するのに、接続確認は先発の mock に繋がって成功してしまい、先発のテストが
/// 終わって mock を kill した時点で後発のテストが落ちる (稀な flake として実際に出た)。
/// ポート 0 で bind させ、mock 自身に番号を名乗らせれば競合しない。
fn start_mock(demo_dir: &Path) -> MockServer {
    start_mock_saying(demo_dir, None)
}

/// `text` を挨拶の代わりに返す mock。敵対的な文字列を本文として流すテスト用。
fn start_mock_saying(demo_dir: &Path, text: Option<&str>) -> MockServer {
    let envs: Vec<(&str, &str)> = text.map(|t| ("MOCK_LLM_TEXT", t)).into_iter().collect();
    start_mock_with(demo_dir, &envs)
}

/// mock の振る舞いを環境変数で差し替えて起動する (`mock_llm.py` の冒頭を参照)。
fn start_mock_with(demo_dir: &Path, envs: &[(&str, &str)]) -> MockServer {
    let script: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock_llm.py");
    let mut child = Command::new("python3")
        .arg(script)
        .arg("0")
        .arg(demo_dir)
        .envs(envs.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn python3 mock_llm.py (is python3 on PATH?)");
    let stdout = child.stdout.take().expect("piped stdout");
    // child を先に MockServer に包む。以降で panic しても Drop が kill + wait する。
    let mut server = MockServer { child, port: 0 };
    let mut line = String::new();
    std::io::BufReader::new(stdout)
        .read_line(&mut line)
        .expect("read the PORT line from mock_llm.py");
    server.port = line
        .trim()
        .strip_prefix("PORT ")
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("mock_llm.py did not announce its port (got {line:?})"));
    server
}

/// ハーネスへの合図 (lodan には渡さない): cwd を `<home>/REL` にする。
const CWD_PREFIX: &str = "<harness:cwd=";

/// ハーネスへの合図 (lodan には渡さない): 既定の `LODAN_TRUST=1` を設定しない。
/// 「信頼を与える環境変数が本当に無い」状態を作るためのもの。
const NO_TRUST_ENV: &str = "<harness:no-trust-env>";

/// stdin に何を繋ぐか。
enum Stdin {
    /// 開いたまま何も送らない (CI や親プロセスから継承した stdin の再現)。
    OpenAndSilent,
    Piped(&'static str),
}

/// 隔離した HOME / cwd で lodan を走らせ、終了まで待つ。`RUN_LIMIT` を超えたら kill して panic。
fn lodan(home: &Path, port: u16, args: &[&str], stdin: Stdin) -> Output {
    // 既定の cwd は `<home>/work`。`<harness:cwd=REL>` で `<home>/REL` に変えられる。
    let cwd = args
        .iter()
        .find_map(|a| a.strip_prefix(CWD_PREFIX))
        .map_or_else(|| home.join("work"), |rel| home.join(rel));
    std::fs::create_dir_all(&cwd).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_lodan"))
        .args(
            args.iter()
                .filter(|a| **a != NO_TRUST_ENV && !a.starts_with(CWD_PREFIX)),
        )
        .current_dir(&cwd)
        .env_clear()
        .env("HOME", home)
        // 接続先は env で渡す。テスト側が `--provider` などのフラグで上書きできるように。
        // テストが cwd に置く `.lodan/config.toml` を読ませる。未信頼の挙動を見るテストは
        // `--trust=false` で打ち消す (フラグは env より優先)。
        .envs((!args.contains(&NO_TRUST_ENV)).then_some(("LODAN_TRUST", "1")))
        .env("LODAN_PROVIDER", "local")
        .env("LODAN_BASE_URL", format!("http://127.0.0.1:{port}/v1"))
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lodan");

    // OpenAndSilent では書き込み端を最後まで握り続ける (drop すると EOF になってしまう)。
    let mut held_stdin = child.stdin.take();
    if let Stdin::Piped(data) = stdin {
        let mut pipe = held_stdin.take().unwrap();
        pipe.write_all(data.as_bytes()).unwrap();
    }

    // 出力は走っている間に別スレッドで吸い出す。終了後にまとめて読むと、パイプの容量
    // (64 KiB 前後) を超えて書く子が書き込みで固まり、「時間内に終了しなかった」という
    // 見当違いの失敗になる。
    let out_reader = drain(child.stdout.take().unwrap());
    let err_reader = drain(child.stderr.take().unwrap());

    let deadline = Instant::now() + RUN_LIMIT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("lodan {args:?} did not exit within {RUN_LIMIT:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    drop(held_stdin);
    Output {
        status,
        stdout: out_reader.join().unwrap(),
        stderr: err_reader.join().unwrap(),
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn text_mode_prints_only_the_final_answer_and_ignores_an_open_stdin() {
    let home = tempfile::tempdir().unwrap();
    let server = start_mock(home.path());
    let out = lodan(
        home.path(),
        server.port,
        &["-p", "hi"],
        Stdin::OpenAndSilent,
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // バナー・session 行・ストリーム表示は stderr。stdout は本文ちょうど 1 回。
    assert_eq!(stdout(&out), format!("{GREETING}\n"));
}

#[test]
fn prompt_can_come_from_stdin() {
    let home = tempfile::tempdir().unwrap();
    let server = start_mock(home.path());
    let out = lodan(
        home.path(),
        server.port,
        &["-p"],
        Stdin::Piped("hi from a pipe\n"),
    );
    assert!(out.status.success());
    assert_eq!(stdout(&out), format!("{GREETING}\n"));
}

#[test]
fn json_mode_is_a_single_result_object() {
    let home = tempfile::tempdir().unwrap();
    let server = start_mock(home.path());
    let out = lodan(
        home.path(),
        server.port,
        &["-p", "hi", "--output-format", "json"],
        Stdin::OpenAndSilent,
    );
    assert!(out.status.success());
    let text = stdout(&out);
    assert_eq!(
        text.lines().count(),
        1,
        "stdout must be exactly one JSON line: {text}"
    );
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["type"], "result");
    assert_eq!(v["is_error"], false);
    assert_eq!(v["exit_code"], 0);
    assert_eq!(v["result"], GREETING);
    assert_eq!(v["usage"]["llm_calls"], 1);
    assert!(v["session_id"].is_string());
}

#[test]
fn without_yes_destructive_tools_are_denied_without_hanging() {
    let home = tempfile::tempdir().unwrap();
    let demo = home.path().join("demo");
    std::fs::create_dir_all(&demo).unwrap();
    let server = start_mock(&demo);
    let out = lodan(
        home.path(),
        server.port,
        &["-p", "run the demo", "--output-format", "stream-json"],
        Stdin::OpenAndSilent,
    );
    assert!(out.status.success());
    let events: Vec<serde_json::Value> = stdout(&out)
        .lines()
        .map(|l| serde_json::from_str(l).expect("every stdout line is JSON"))
        .collect();
    let denied: Vec<&str> = events
        .iter()
        .filter(|e| e["event"] == "tool_result" && e["reason"] == "denied")
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(denied, ["Write", "Edit", "Bash"]);
    assert!(
        !demo.join("hello.txt").exists(),
        "a denied Write must not touch the disk"
    );
    assert_eq!(events.last().unwrap()["event"], "result");
}

#[test]
fn stream_json_with_yes_runs_the_tools_and_ends_with_a_result() {
    let home = tempfile::tempdir().unwrap();
    let demo = home.path().join("demo");
    std::fs::create_dir_all(&demo).unwrap();
    let server = start_mock(&demo);
    let out = lodan(
        home.path(),
        server.port,
        &[
            "--yes",
            "-p",
            "run the demo",
            "--output-format",
            "stream-json",
        ],
        Stdin::OpenAndSilent,
    );
    assert!(out.status.success());
    let events: Vec<serde_json::Value> = stdout(&out)
        .lines()
        .map(|l| serde_json::from_str(l).expect("every stdout line is JSON"))
        .collect();
    assert_eq!(events.first().unwrap()["event"], "run_start");
    let tools: Vec<&str> = events
        .iter()
        .filter(|e| e["event"] == "tool_result")
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["Write", "Read", "Edit", "Grep", "Glob", "Bash"]);
    let last = events.last().unwrap();
    assert_eq!(last["event"], "result");
    assert_eq!(last["is_error"], false);
    assert_eq!(
        std::fs::read_to_string(demo.join("hello.txt")).unwrap(),
        "hello world"
    );
}

#[test]
fn an_unreachable_server_is_exit_code_1_with_an_error_result() {
    let home = tempfile::tempdir().unwrap();
    // 誰も listen していないポート。
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let out = lodan(
        home.path(),
        port,
        &["-p", "hi", "--output-format", "json"],
        Stdin::OpenAndSilent,
    );
    assert_eq!(out.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["is_error"], true);
    assert!(v["result"].is_null());
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("error sending request")
    );
}

#[test]
fn running_out_of_iterations_is_exit_code_3() {
    let home = tempfile::tempdir().unwrap();
    let demo = home.path().join("demo");
    std::fs::create_dir_all(&demo).unwrap();
    let cfg_dir = home.path().join("work/.lodan");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), "[agent]\nmax_iterations = 2\n").unwrap();
    let server = start_mock(&demo);
    let out = lodan(
        home.path(),
        server.port,
        &["--yes", "-p", "run the demo", "--output-format", "json"],
        Stdin::OpenAndSilent,
    );
    assert_eq!(out.status.code(), Some(3));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["exit_code"], 3);
    assert!(v["error"].as_str().unwrap().contains("max_iterations"));
}

#[test]
fn a_startup_failure_still_answers_in_the_requested_format() {
    // API キーの無い provider は LLM クライアントの構築で失敗する = ターンに入る前。
    // eval ハーネスで最もありがちな設定ミスなので、json を頼んだ呼び出し側には json で返す。
    let home = tempfile::tempdir().unwrap();
    let out = lodan(
        home.path(),
        1,
        &["--provider", "kimi", "-p", "hi", "--output-format", "json"],
        Stdin::OpenAndSilent,
    );
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert_eq!(text.lines().count(), 1, "stdout: {text:?}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["is_error"], true);
    assert!(v["error"].as_str().unwrap().contains("KIMI_API_KEY"), "{v}");
}

#[test]
fn a_broken_config_file_is_reported_as_a_stream_json_result() {
    let home = tempfile::tempdir().unwrap();
    let cfg_dir = home.path().join("work/.lodan");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    // 閉じ忘れの api_key: 壊れた行の値がエラー文に出てはいけない (#114)。
    std::fs::write(
        cfg_dir.join("config.toml"),
        "[llm.local]\napi_key = \"sk-BROKEN-SECRET\n",
    )
    .unwrap();
    let out = lodan(
        home.path(),
        1,
        &["-p", "hi", "--output-format", "stream-json"],
        Stdin::OpenAndSilent,
    );
    assert_eq!(out.status.code(), Some(1));
    let events: Vec<serde_json::Value> = stdout(&out)
        .lines()
        .map(|l| serde_json::from_str(l).expect("every stdout line is JSON"))
        .collect();
    assert_eq!(
        events.first().unwrap()["event"],
        "run_start",
        "the stream always opens with run_start"
    );
    let last = events.last().unwrap();
    assert_eq!(last["event"], "result");
    assert_eq!(
        last["usage"]["llm_calls"], 0,
        "usage keeps its shape on failure"
    );
    assert_eq!(last["is_error"], true);
    assert!(
        last["error"].as_str().unwrap().contains("config.toml"),
        "{last}"
    );
    assert!(
        !stdout(&out).contains("BROKEN")
            && !String::from_utf8_lossy(&out.stderr).contains("BROKEN"),
        "the broken line's value must not be quoted: {last}"
    );
}

#[test]
fn log_jsonl_gets_the_result_event_in_text_mode_too() {
    let home = tempfile::tempdir().unwrap();
    let server = start_mock(home.path());
    let log = home.path().join("run.jsonl");
    let out = lodan(
        home.path(),
        server.port,
        &["-p", "hi", "--log-jsonl", log.to_str().unwrap()],
        Stdin::OpenAndSilent,
    );
    assert!(out.status.success());
    assert_eq!(
        stdout(&out),
        format!("{GREETING}\n"),
        "the log must not leak onto stdout"
    );
    let events: Vec<serde_json::Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(events.first().unwrap()["event"], "run_start");
    assert_eq!(events.last().unwrap()["event"], "result");
    assert_eq!(events.last().unwrap()["result"], GREETING);
}

#[test]
fn a_usage_error_is_exit_code_2_and_distinct_from_max_iterations() {
    let home = tempfile::tempdir().unwrap();
    let out = lodan(
        home.path(),
        1,
        &["-p", "hi", "--output-format", "yaml"],
        Stdin::OpenAndSilent,
    );
    assert_eq!(out.status.code(), Some(2), "clap's usage-error code");
    assert!(stdout(&out).is_empty());
}

#[test]
fn tool_profile_core_is_reported_and_shrinks_the_tool_specs() {
    let home = tempfile::tempdir().unwrap();
    let server = start_mock(home.path());
    let tools_event = |args: &[&str]| -> serde_json::Value {
        let out = lodan(home.path(), server.port, args, Stdin::OpenAndSilent);
        assert!(out.status.success());
        stdout(&out)
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .find(|e| e["event"] == "tools")
            .expect("a tools event")
    };
    let full = tools_event(&["-p", "hi", "--output-format", "stream-json"]);
    let core = tools_event(&[
        "-p",
        "hi",
        "--output-format",
        "stream-json",
        "--tool-profile",
        "core",
    ]);

    assert_eq!(core["profile"], "core");
    assert_eq!(
        core["visible"],
        serde_json::json!(["Bash", "Edit", "Glob", "Grep", "Read", "Write"])
    );
    assert_eq!(
        full["registered"], core["registered"],
        "hidden tools stay registered"
    );
    let (full_bytes, core_bytes) = (
        full["spec_bytes"].as_u64().unwrap(),
        core["spec_bytes"].as_u64().unwrap(),
    );
    assert!(
        core_bytes * 2 <= full_bytes,
        "core = {core_bytes}, full = {full_bytes}"
    );
}

#[test]
fn a_tool_list_that_matches_nothing_fails_before_calling_the_model() {
    let home = tempfile::tempdir().unwrap();
    let out = lodan(
        home.path(),
        1, // 誰も listen していない。LLM を呼びに行けば別のエラーになる。
        &[
            "-p",
            "hi",
            "--output-format",
            "json",
            "--tools",
            "Raed,Grpe",
        ],
        Stdin::OpenAndSilent,
    );
    assert_eq!(out.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let error = v["error"].as_str().unwrap();
    assert!(error.contains("matches no registered tool"), "{error}");
    assert!(
        error.contains("Read"),
        "the error lists what exists: {error}"
    );
    assert_eq!(v["usage"]["llm_calls"], 0);
}

#[test]
fn a_runtime_tool_profile_beats_a_tool_list_in_the_config_file() {
    let home = tempfile::tempdir().unwrap();
    let cfg_dir = home.path().join("work/.lodan");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), "[agent]\ntools = [\"Read\"]\n").unwrap();
    let server = start_mock(home.path());
    let out = lodan(
        home.path(),
        server.port,
        &[
            "-p",
            "hi",
            "--output-format",
            "stream-json",
            "--tool-profile",
            "core",
        ],
        Stdin::OpenAndSilent,
    );
    assert!(out.status.success());
    let tools = stdout(&out)
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|e| e["event"] == "tools")
        .unwrap();
    assert_eq!(tools["visible"].as_array().unwrap().len(), 6, "{tools}");
    assert_eq!(tools["explicit"], false);
}

/// demo フロー (Write → Read → Edit → Grep → Glob → Bash) の各ツールがどう扱われたか。
fn demo_reasons(home: &Path, port: u16, extra: &[&str]) -> Vec<(String, String)> {
    let mut args = vec!["-p", "run the demo", "--output-format", "stream-json"];
    args.extend_from_slice(extra);
    let out = lodan(home, port, &args, Stdin::OpenAndSilent);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout(&out)
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|e| e["event"] == "tool_result")
        .map(|e| {
            (
                e["name"].as_str().unwrap().to_string(),
                e["reason"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn a_deny_rule_holds_even_with_yes() {
    let home = tempfile::tempdir().unwrap();
    let demo = home.path().join("demo");
    std::fs::create_dir_all(&demo).unwrap();
    let server = start_mock(&demo);
    let reasons = demo_reasons(
        home.path(),
        server.port,
        &["--yes", "--disallowed-tools", "Bash"],
    );
    let of = |tool: &str| {
        reasons
            .iter()
            .find(|(n, _)| n == tool)
            .map(|(_, r)| r.as_str())
    };
    assert_eq!(
        of("Write"),
        Some("ok"),
        "--yes still approves what no rule forbids"
    );
    assert_eq!(of("Bash"), Some("denied"), "deny wins over --yes");
    assert!(demo.join("hello.txt").exists());
}

#[test]
fn dont_ask_with_allow_rules_runs_exactly_what_was_allowed() {
    let home = tempfile::tempdir().unwrap();
    let demo = home.path().join("demo");
    std::fs::create_dir_all(&demo).unwrap();
    let server = start_mock(&demo);
    // Write と Edit だけを許可。Bash は尋ねる相手がいないので拒否される。
    let reasons = demo_reasons(
        home.path(),
        server.port,
        &[
            "--permission-mode",
            "dont-ask",
            "--allowed-tools",
            "Write",
            "--allowed-tools",
            "Edit",
        ],
    );
    let of = |tool: &str| {
        reasons
            .iter()
            .find(|(n, _)| n == tool)
            .map(|(_, r)| r.as_str())
    };
    assert_eq!(of("Write"), Some("ok"));
    assert_eq!(of("Edit"), Some("ok"));
    assert_eq!(of("Bash"), Some("denied"));
    assert_eq!(
        std::fs::read_to_string(demo.join("hello.txt")).unwrap(),
        "hello world"
    );
}

#[test]
fn a_malformed_permission_rule_fails_at_startup_in_the_requested_format() {
    let home = tempfile::tempdir().unwrap();
    let out = lodan(
        home.path(),
        1,
        &[
            "-p",
            "hi",
            "--output-format",
            "json",
            "--disallowed-tools",
            "Bash(rm *",
        ],
        Stdin::OpenAndSilent,
    );
    assert_eq!(out.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert!(
        v["error"].as_str().unwrap().contains("missing closing"),
        "{v}"
    );
}

/// カーソルを 2 行上げて行を消し、書き換える。双方向テキストの上書きも混ぜる。
const HOSTILE_TEXT: &str = "done\x1b[2A\x1b[2Kall tests passed \u{202E}txt.exe";

#[test]
fn hostile_model_text_is_defused_on_the_repl_screen() {
    let home = tempfile::tempdir().unwrap();
    let server = start_mock_saying(home.path(), Some(HOSTILE_TEXT));
    // REPL にプロンプトをパイプで渡す (承認は要らない)。
    let out = lodan(home.path(), server.port, &[], Stdin::Piped("hi\n/exit\n"));
    assert!(out.status.success());
    for (name, bytes) in [("stdout", &out.stdout), ("stderr", &out.stderr)] {
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains('\x1b'), "raw ESC on {name}: {text:?}");
        assert!(
            !text.contains('\u{202E}'),
            "raw bidi override on {name}: {text:?}"
        );
    }
    assert!(stdout(&out).contains("done\\u{1b}[2A\\u{1b}[2Kall tests passed \\u{202e}txt.exe"));
}

#[test]
fn headless_keeps_the_piped_result_verbatim_but_defuses_what_a_human_reads() {
    let home = tempfile::tempdir().unwrap();
    let server = start_mock_saying(home.path(), Some(HOSTILE_TEXT));
    let text = lodan(
        home.path(),
        server.port,
        &["-p", "hi"],
        Stdin::OpenAndSilent,
    );
    assert!(text.status.success());
    // stdout はパイプ = 呼び出し側との契約。無加工。
    assert_eq!(stdout(&text), format!("{HOSTILE_TEXT}\n"));
    // stderr は人が読む進行表示。
    assert!(!String::from_utf8_lossy(&text.stderr).contains('\x1b'));

    let json = lodan(
        home.path(),
        server.port,
        &["-p", "hi", "--output-format", "json"],
        Stdin::OpenAndSilent,
    );
    let v: serde_json::Value = serde_json::from_str(&stdout(&json)).unwrap();
    assert_eq!(
        v["result"], HOSTILE_TEXT,
        "the JSON result carries the model's exact text"
    );
}

#[test]
fn an_untrusted_directory_contributes_no_project_settings() {
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(work.join(".lodan")).unwrap();
    // 信頼されていれば 2 回で打ち切られ (exit 3)、hook がファイルを作り、MCP サーバが起動する設定。
    let marker = home.path().join("hook-ran");
    std::fs::write(
        work.join(".lodan/config.toml"),
        format!(
            "[agent]\nmax_iterations = 2\n\n[permissions]\nmode = \"bypass\"\n\n[[hooks]]\nevent = \"SessionStart\"\ncommand = \"touch {}\"\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::write(
        work.join(".mcp.json"),
        r#"{"mcpServers":{"x":{"command":"false"}}}"#,
    )
    .unwrap();
    std::fs::write(work.join("CLAUDE.md"), "PROJECT-MEMORY-MARKER").unwrap();
    let demo = home.path().join("demo");
    std::fs::create_dir_all(&demo).unwrap();
    let server = start_mock(&demo);

    let out = lodan(
        home.path(),
        server.port,
        &[
            "--trust=false",
            "-p",
            "run the demo",
            "--output-format",
            "stream-json",
        ],
        Stdin::OpenAndSilent,
    );
    // max_iterations = 2 が効いていれば exit 3。効いていないので demo は最後まで進む。
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!marker.exists(), "a hook from an untrusted directory ran");
    // `mode = "bypass"` も効いていない: 破壊的ツールは拒否され、ファイルは作られない。
    assert!(!demo.join("hello.txt").exists());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not a trusted directory"), "{stderr}");
    for ignored in [".lodan/config.toml", ".mcp.json", "CLAUDE.md"] {
        assert!(
            stderr.contains(ignored),
            "the notice should name {ignored}: {stderr}"
        );
    }
    // stdout は契約のまま (通知は stderr だけ)。
    for line in stdout(&out).lines() {
        serde_json::from_str::<serde_json::Value>(line).expect("every stdout line is JSON");
    }
}

#[test]
fn trusting_the_directory_makes_the_same_settings_apply() {
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(work.join(".lodan")).unwrap();
    let marker = home.path().join("hook-ran");
    std::fs::write(
        work.join(".lodan/config.toml"),
        format!(
            "[[hooks]]\nevent = \"SessionStart\"\ncommand = \"touch {}\"\n",
            marker.display()
        ),
    )
    .unwrap();
    let server = start_mock(home.path());
    // 記録による信頼: `lodan trust` を実行してから、フラグなしで走らせる。
    let trust = lodan(
        home.path(),
        server.port,
        &["--trust=false", "trust"],
        Stdin::OpenAndSilent,
    );
    assert!(
        trust.status.success(),
        "{}",
        String::from_utf8_lossy(&trust.stderr)
    );
    let out = lodan(
        home.path(),
        server.port,
        &["--trust=false", "-p", "hi"],
        Stdin::OpenAndSilent,
    );
    assert!(out.status.success());
    assert!(
        marker.exists(),
        "a recorded trust decision should let the hook run"
    );

    let listed = lodan(
        home.path(),
        server.port,
        &["trust", "--list"],
        Stdin::OpenAndSilent,
    );
    assert!(stdout(&listed).contains("work"), "{}", stdout(&listed));
}

/// メモリは祖先のディレクトリからも読まれる。`lodan trust` は、信頼した結果として読むように
/// なるものを (cwd の外にあるものも) 黙って有効にせず、一覧で見せる。
#[test]
fn trusting_a_subdirectory_says_which_ancestor_memory_comes_with_it() {
    let home = tempfile::tempdir().unwrap();
    let app = home.path().join("repo/app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(home.path().join("repo/CLAUDE.md"), "parent memory").unwrap();
    std::fs::write(app.join("LODAN.md"), "own memory").unwrap();
    let server = start_mock(home.path());
    let out = lodan(
        home.path(),
        server.port,
        &["--trust=false", "trust", "<harness:cwd=repo/app"],
        Stdin::OpenAndSilent,
    );
    assert!(out.status.success());
    let said = stdout(&out);
    assert!(said.contains("  LODAN.md"), "{said}");
    let parent = said
        .lines()
        .find(|l| l.ends_with("repo/CLAUDE.md"))
        .unwrap_or_else(|| panic!("the parent's memory file is not announced:\n{said}"));
    assert!(parent.starts_with("  /"), "shown as a full path: {parent}");
}

#[test]
fn a_repository_cannot_trust_itself_through_its_own_dotenv() {
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(work.join(".lodan")).unwrap();
    let marker = home.path().join("hook-ran");
    // リポジトリが持ち込む `.env`: 自分を信頼させ、承認を外し、接続先を変えようとする。
    std::fs::write(
        work.join(".env"),
        "LODAN_TRUST=1\nLODAN_AUTO_APPROVE=true\nLODAN_BASE_URL=http://127.0.0.1:1/v1\n",
    )
    .unwrap();
    std::fs::write(
        work.join(".lodan/config.toml"),
        format!(
            "[[hooks]]\nevent = \"SessionStart\"\ncommand = \"touch {}\"\n",
            marker.display()
        ),
    )
    .unwrap();
    let demo = home.path().join("demo");
    std::fs::create_dir_all(&demo).unwrap();
    let server = start_mock(&demo);

    let out = lodan(
        home.path(),
        server.port,
        &[
            NO_TRUST_ENV,
            "-p",
            "run the demo",
            "--output-format",
            "json",
        ],
        Stdin::OpenAndSilent,
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    // `.env` の LODAN_BASE_URL が効いていれば、誰も listen していないポートに繋ぎに行って失敗する。
    assert!(
        out.status.success(),
        "the repo's .env redirected the request: {stderr}"
    );
    assert!(
        !marker.exists(),
        "the repo trusted itself via .env and its hook ran"
    );
    assert!(
        !demo.join("hello.txt").exists(),
        "LODAN_AUTO_APPROVE from the repo's .env took effect"
    );
    assert!(
        stderr.contains("not a trusted directory") && stderr.contains(".env"),
        "{stderr}"
    );
}

#[test]
fn a_trusted_directorys_dotenv_is_still_loaded() {
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let demo = home.path().join("demo");
    std::fs::create_dir_all(&demo).unwrap();
    let server = start_mock(&demo);
    // 自分のプロジェクトの `.env` に置いた設定 (ここでは自動承認) は、信頼していれば従来どおり効く。
    std::fs::write(work.join(".env"), "LODAN_AUTO_APPROVE=true\n").unwrap();
    let out = lodan(
        home.path(),
        server.port,
        &["-p", "run the demo"],
        Stdin::OpenAndSilent,
    );
    assert!(out.status.success());
    assert!(
        demo.join("hello.txt").exists(),
        ".env from a trusted directory should apply"
    );
}

#[test]
fn a_parent_directorys_dotenv_is_not_picked_up_from_a_subdirectory() {
    // pr-review #102 F1: dotenvy::dotenv() は親ディレクトリを遡って `.env` を探す。判定は cwd しか
    // 見ていなかったので、リポジトリのサブディレクトリから起動すると親の `.env` が無警告で読まれ、
    // 接続先と承認を書き換えられた。
    let home = tempfile::tempdir().unwrap();
    let repo = home.path().join("work/repo");
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    std::fs::write(
        repo.join(".env"),
        "LODAN_BASE_URL=http://127.0.0.1:1/v1\nLODAN_AUTO_APPROVE=true\nLODAN_TRUST=1\n",
    )
    .unwrap();
    let demo = home.path().join("demo");
    std::fs::create_dir_all(&demo).unwrap();
    let server = start_mock(&demo);
    let out = lodan(
        home.path(),
        server.port,
        &[
            NO_TRUST_ENV,
            "<harness:cwd=work/repo/sub",
            "-p",
            "run the demo",
        ],
        Stdin::OpenAndSilent,
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the parent's .env redirected the request: {stderr}"
    );
    assert!(
        !demo.join("hello.txt").exists(),
        "LODAN_AUTO_APPROVE from the parent's .env took effect"
    );
}

#[test]
fn the_trust_notice_is_printed_once() {
    // pr-review #102 F2: 判断が 2 回走っていた (main と dispatch)。
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(work.join(".lodan")).unwrap();
    std::fs::write(
        work.join(".lodan/config.toml"),
        "[agent]\nmax_iterations = 40\n",
    )
    .unwrap();
    let server = start_mock(home.path());
    let out = lodan(
        home.path(),
        server.port,
        &["--trust=false", "-p", "hi"],
        Stdin::OpenAndSilent,
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.matches("not a trusted directory").count(),
        1,
        "{stderr}"
    );
}

/// `--sandbox` が設定 → セッション → Bash ツールまで届いている。read-only なら、承認済み
/// (`--yes`) のコマンドでも作業ディレクトリに書けない。
#[cfg(target_os = "macos")]
#[test]
fn the_sandbox_flag_reaches_the_bash_tool() {
    let run = |extra: &[&str]| {
        let home = tempfile::tempdir().unwrap();
        let server = start_mock_with(home.path(), &[("MOCK_LLM_BASH", "echo a > written.txt")]);
        let mut args = vec!["--yes", "-p", "run the demo", "--output-format", "json"];
        args.extend_from_slice(extra);
        let out = lodan(home.path(), server.port, &args, Stdin::OpenAndSilent);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        home.path().join("work/written.txt").exists()
    };
    assert!(run(&[]), "positive control: the command writes the file");
    assert!(!run(&["--sandbox", "read-only"]));
}

// ---- #76: hooks v2 ----

/// Claude Code のドキュメントにある `block-rm.sh` を**無改変で**置く。
const BLOCK_RM_SH: &str = r#"#!/bin/bash
# .claude/hooks/block-rm.sh
COMMAND=$(jq -r '.tool_input.command')

if echo "$COMMAND" | grep -q 'rm -rf'; then
  jq -n '{
    hookSpecificOutput: {
      hookEventName: "PreToolUse",
      permissionDecision: "deny",
      permissionDecisionReason: "Destructive command blocked by hook"
    }
  }'
else
  exit 0  # no decision; normal permission flow applies
fi
"#;

fn jq_is_installed() -> bool {
    Command::new("jq")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `--yes` で Bash を 1 回呼ばせ、(victim が残ったか, ツール結果の本文) を返す。
fn run_bash_under_block_rm(command_template: &str) -> (bool, String) {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(work.join(".lodan")).unwrap();
    let victim = work.join("victim");
    std::fs::create_dir_all(&victim).unwrap();
    let script = work.join("block-rm.sh");
    std::fs::write(&script, BLOCK_RM_SH).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        work.join(".lodan/config.toml"),
        format!(
            "[[hooks]]\nevent = \"PreToolUse\"\nmatcher = \"Bash\"\ncommand = \"{}\"\n",
            script.display()
        ),
    )
    .unwrap();
    let command = command_template.replace("{victim}", &victim.display().to_string());
    let server = start_mock_with(home.path(), &[("MOCK_LLM_BASH", &command)]);
    let log = home.path().join("run.jsonl");
    let out = lodan(
        home.path(),
        server.port,
        &[
            "--yes",
            "-p",
            "run the demo",
            "--log-jsonl",
            log.to_str().unwrap(),
        ],
        Stdin::OpenAndSilent,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let reasons: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e["event"] == "tool_result")
        .map(|e| e["reason"].as_str().unwrap_or_default().to_string())
        .collect();
    (victim.exists(), reasons.join(","))
}

#[test]
fn the_claude_code_block_rm_hook_works_unmodified() {
    if !jq_is_installed() {
        eprintln!("skipped: the documented hook script needs jq");
        return;
    }
    let (survived, reason) = run_bash_under_block_rm("rm -rf {victim}");
    assert!(survived, "the hook's JSON deny must stop the command");
    assert_eq!(reason, "hook_blocked");

    // 陽性対照: hook に当たらない消し方なら、同じ設定で実際に消える。
    let (survived, reason) = run_bash_under_block_rm("rmdir {victim}");
    assert!(!survived);
    assert_eq!(reason, "ok");
}

#[test]
fn a_broken_hook_matcher_stops_startup_instead_of_never_firing() {
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(work.join(".lodan")).unwrap();
    std::fs::write(
        work.join(".lodan/config.toml"),
        "[[hooks]]\nevent = \"PreToolUse\"\nmatcher = \"Bash(\"\ncommand = \"./guard.sh\"\n",
    )
    .unwrap();
    let out = lodan(
        home.path(),
        1,
        &["-p", "hi", "--output-format", "json"],
        Stdin::OpenAndSilent,
    );
    assert_eq!(out.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let error = v["error"].as_str().unwrap();
    assert!(error.contains("Bash(") && error.contains("guard.sh"), "{v}");
}

// ---- #84: 使用量の予算 ----

/// 予算を使い切ったら exit 4。止まるのは「次の LLM リクエストの直前」なので、保存された履歴は
/// tool_call と結果の対が揃ったまま (= そのまま API に投げ直せる形) で終わる。
#[test]
fn running_out_of_budget_is_exit_code_4_and_leaves_a_resumable_history() {
    let home = tempfile::tempdir().unwrap();
    let demo = home.path().join("demo");
    std::fs::create_dir_all(&demo).unwrap();
    let server = start_mock(&demo);
    let out = lodan(
        home.path(),
        server.port,
        &[
            "--yes",
            "-p",
            "run the demo",
            "--output-format",
            "json",
            "--max-requests",
            "2",
        ],
        Stdin::OpenAndSilent,
    );
    assert_eq!(out.status.code(), Some(4), "{}", stdout(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["exit_code"], 4);
    assert!(
        v["error"].as_str().unwrap().contains("2 of 2 requests"),
        "{v}"
    );
    assert_eq!(v["usage"]["requests"], 2);
    assert_eq!(v["usage"]["by_kind"]["main"]["llm_calls"], 2);

    // 2 回のリクエストで Write と Read まで進んでいる。3 回目は送られていない。
    assert!(demo.join("hello.txt").exists());
    let session_id = v["session_id"].as_str().expect("the session is saved");
    let transcript = find_transcript(home.path(), session_id);
    let roles: Vec<String> = std::fs::read_to_string(&transcript)
        .unwrap()
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["role"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        roles.last().map(String::as_str),
        Some("tool"),
        "every tool call has its result: {roles:?}"
    );
    assert_eq!(
        roles.iter().filter(|r| *r == "assistant").count(),
        roles.iter().filter(|r| *r == "tool").count(),
        "{roles:?}"
    );

    // 予算を付け直して再開すれば、続きから走り切る。
    let resumed = lodan(
        home.path(),
        server.port,
        &[
            "--yes",
            "--resume",
            session_id,
            "-p",
            "continue the demo",
            "--output-format",
            "json",
        ],
        Stdin::OpenAndSilent,
    );
    assert_eq!(resumed.status.code(), Some(0), "{}", stdout(&resumed));
}

fn find_transcript(home: &Path, session_id: &str) -> PathBuf {
    fn walk(dir: &Path, session_id: &str) -> Option<PathBuf> {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().and_then(|n| n.to_str()) == Some(session_id) {
                    return Some(path.join("transcript.jsonl"));
                }
                if let Some(found) = walk(&path, session_id) {
                    return Some(found);
                }
            }
        }
        None
    }
    walk(home, session_id).unwrap_or_else(|| panic!("no session dir for {session_id}"))
}

// ---- #84: /goal の永続化と再開 ----

/// 未達のまま止まった goal はセッションに残り、`--resume` で paused として戻り、`/goal resume` で
/// 続きから走って達成できる。達成したら記録は消える。
#[test]
fn a_paused_goal_survives_the_session_and_can_be_resumed() {
    let home = tempfile::tempdir().unwrap();
    // 1 回目: 評価器の返事が判定として読めない → 安全側で停止し、goal は paused で残る。
    let vague = start_mock_saying(home.path(), Some("I think it is going well."));
    let first = lodan(
        home.path(),
        vague.port,
        &[],
        Stdin::Piped("/goal make the tests pass\n/exit\n"),
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let said = stdout(&first);
    let session_id = said
        .lines()
        .find_map(|l| l.strip_prefix("session: "))
        .unwrap_or_else(|| panic!("no session id in:\n{said}"))
        .trim()
        .to_string();
    let goal_file = find_transcript(home.path(), &session_id).with_file_name("goal.json");
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&goal_file).expect("goal.json")).unwrap();
    assert_eq!(saved["condition"], "make the tests pass");
    assert_eq!(saved["total_turns"], 1);
    drop(vague);

    // 2 回目: 再開したセッションに goal が戻っている。今度の評価器は達成と判定する。
    let decisive = start_mock_saying(home.path(), Some(r#"{"met": true, "reason": "all green"}"#));
    let second = lodan(
        home.path(),
        decisive.port,
        &["--resume", &session_id],
        Stdin::Piped("/goal\n/goal resume\n/exit\n"),
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let said = stdout(&second);
    assert!(
        said.contains("[goal] restored (paused after 1 turn(s)): make the tests pass"),
        "{said}"
    );
    assert!(said.contains("goal (paused):"), "{said}");
    assert!(said.contains("[goal] resumed after 1 turn(s)"), "{said}");
    assert!(
        said.contains("[goal] achieved after 2 turn(s): all green"),
        "{said}"
    );
    assert!(!goal_file.exists(), "an achieved goal is not kept");
}

/// 同じプロセスの中で、上限で止まった goal を `/goal resume` で走らせ直せる。再開で新しい枠が
/// 始まらなければ、再開した瞬間にまた上限で止まる (レビューの変異試験で、テストが 1 つも
/// 落ちなかった箇所)。
#[test]
fn a_goal_stopped_by_the_turn_limit_runs_again_when_resumed_in_the_same_session() {
    let home = tempfile::tempdir().unwrap();
    let never = start_mock_saying(
        home.path(),
        Some(r#"{"met": false, "reason": "keep going"}"#),
    );
    let out = lodan(
        home.path(),
        never.port,
        &[],
        Stdin::Piped("/goal keep going\n/goal resume\n/exit\n"),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let said = stdout(&out);
    assert_eq!(said.matches("turn limit (20) reached").count(), 2, "{said}");
    let after_resume = said
        .split("[goal] resumed after 20 turn(s)")
        .nth(1)
        .unwrap_or_else(|| panic!("never resumed:\n{said}"));
    assert!(
        after_resume.contains("[goal] turn 1/20: not met"),
        "the resumed run did no work:\n{after_resume}"
    );

    let session_id = said
        .lines()
        .find_map(|l| l.strip_prefix("session: "))
        .unwrap()
        .trim()
        .to_string();
    let goal_file = find_transcript(home.path(), &session_id).with_file_name("goal.json");
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(goal_file).unwrap()).unwrap();
    assert_eq!(saved["total_turns"], 40);
}

/// `[goal] evaluator_model` を書いたら、達成の判定は本当にそのモデルに尋ねる。作業側の返事は
/// 判定として読めないので、評価器が作業側のままなら goal は達成されない。
#[test]
fn the_configured_evaluator_model_is_the_one_that_is_asked() {
    let run = |config: &str| {
        let home = tempfile::tempdir().unwrap();
        let work = home.path().join("work/.lodan");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("config.toml"), config).unwrap();
        let server = start_mock_with(
            home.path(),
            &[
                ("MOCK_LLM_TEXT", "I am working on it."),
                ("MOCK_LLM_JUDGE_MODEL", "the-judge"),
                (
                    "MOCK_LLM_JUDGE_TEXT",
                    r#"{"met": true, "reason": "the judge agrees"}"#,
                ),
            ],
        );
        let out = lodan(
            home.path(),
            server.port,
            &[],
            Stdin::Piped("/goal finish the work\n/exit\n"),
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        format!("{}{}", stdout(&out), String::from_utf8_lossy(&out.stderr))
    };
    let judged = run("[goal]\nevaluator_model = \"the-judge\"\n");
    assert!(
        judged.contains("[goal] achieved after 1 turn(s): the judge agrees"),
        "{judged}"
    );
    // 陽性対照: 設定が無ければ作業側が自分で判定し、その返事は判定として読めない。
    let unjudged = run("");
    assert!(unjudged.contains("evaluator failed"), "{unjudged}");
}

// ---- #88: `lodan config` は秘密を伏せる ----

#[test]
fn lodan_config_hides_secrets_unless_asked_and_the_full_output_still_round_trips() {
    let home = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let out = lodan(home.path(), 1, args, Stdin::OpenAndSilent);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        stdout(&out)
    };
    // 鍵は CLI フラグで渡す (設定ファイル由来でも同じ経路を通る)。
    let hidden = run(&["--api-key", "sk-CLI-SECRET", "config", "--show-origin"]);
    assert!(!hidden.contains("sk-CLI-SECRET"), "{hidden}");
    assert!(hidden.contains("api_key = \"***\""), "{hidden}");

    let full = run(&["--api-key", "sk-CLI-SECRET", "config", "--show-secrets"]);
    assert!(full.contains("api_key = \"sk-CLI-SECRET\""), "{full}");
    // `--show-secrets` の出力は、そのまま設定ファイルとして読み直せる。
    let pasted = home.path().join("pasted.toml");
    std::fs::write(&pasted, &full).unwrap();
    let again = run(&[
        "--config",
        pasted.to_str().unwrap(),
        "config",
        "--show-secrets",
    ]);
    assert_eq!(again, full);
}

// ---- #71: `--output-schema` ----

const REVIEW_SCHEMA: &str = r#"{
  "type": "object",
  "required": ["verdict", "score"],
  "additionalProperties": false,
  "properties": {
    "verdict": { "enum": ["approve", "request_changes"] },
    "score": { "type": "integer", "minimum": 0, "maximum": 10 }
  }
}"#;

/// スキーマを置き、mock に `replies` を順に返させて `-p --output-schema` を走らせる。
fn run_with_schema(schema: &str, replies: &[&str], extra: &[&str]) -> (Output, RunLog) {
    let home = tempfile::tempdir().unwrap();
    let schema_path = home.path().join("schema.json");
    std::fs::write(&schema_path, schema).unwrap();
    let scripted = serde_json::to_string(replies).unwrap();
    let server = start_mock_with(home.path(), &[("MOCK_LLM_TEXTS", &scripted)]);
    let log = home.path().join("run.jsonl");
    let mut args = vec![
        "-p",
        "review the change",
        "--output-schema",
        schema_path.to_str().unwrap(),
        "--log-jsonl",
        log.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    let out = lodan(home.path(), server.port, &args, Stdin::OpenAndSilent);
    (out, RunLog { home })
}

/// 実行の `--log-jsonl`。tempdir を握っているので、読み終わるまで消えない。
struct RunLog {
    home: tempfile::TempDir,
}

fn events(log: &RunLog, name: &str) -> usize {
    std::fs::read_to_string(log.home.path().join("run.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e["event"] == name)
        .count()
}

/// 受け入れ条件: スキーマ不一致 → 再要求 → 成功。
#[test]
fn an_answer_that_misses_the_schema_is_sent_back_and_the_corrected_one_is_returned() {
    let (out, log) = run_with_schema(
        REVIEW_SCHEMA,
        &[
            "Looks good to me! I'd give it an 11.",
            "```json\n{\"verdict\": \"lgtm\", \"score\": 11}\n```",
            "Sure:\n```json\n{\"verdict\": \"approve\", \"score\": 9}\n```",
        ],
        &["--output-format", "json"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(
        v["structured_output"],
        serde_json::json!({ "verdict": "approve", "score": 9 })
    );
    // `result` は前置きもコードフェンスも除いた JSON そのもの。
    assert_eq!(v["result"], r#"{"score":9,"verdict":"approve"}"#);
    assert_eq!(events(&log, "schema_retry"), 2);
    assert_eq!(
        v["usage"]["requests"], 3,
        "two corrections = two more requests"
    );
}

#[test]
fn a_matching_first_answer_needs_no_correction_and_text_mode_prints_the_json() {
    let (out, log) = run_with_schema(REVIEW_SCHEMA, &[r#"{"verdict":"approve","score":7}"#], &[]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out).trim(), r#"{"score":7,"verdict":"approve"}"#);
    assert_eq!(events(&log, "schema_retry"), 0);
}

#[test]
fn an_answer_that_never_matches_is_exit_code_5_and_says_what_was_wrong() {
    let (out, log) = run_with_schema(
        REVIEW_SCHEMA,
        &[r#"{"verdict":"approve","score":"high"}"#],
        &["--output-format", "json"],
    );
    assert_eq!(out.status.code(), Some(5), "{}", stdout(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["result"], serde_json::Value::Null);
    assert_eq!(v["structured_output"], serde_json::Value::Null);
    let error = v["error"].as_str().unwrap();
    assert!(
        error.contains("$.score: expected integer, got string"),
        "{error}"
    );
    assert_eq!(events(&log, "schema_retry"), 2, "asked twice, then gave up");
}

/// 断りの文に混ざった例示の JSON を、答えとして拾わない。拾うと、呼び出し側には成功 (exit 0) に見える。
#[test]
fn an_example_inside_a_refusal_is_not_taken_as_the_answer() {
    let (out, _log) = run_with_schema(
        r#"{"type":"object","properties":{"verdict":{"enum":["approve","request_changes"]}}}"#,
        &[
            "For example a verdict looks like {\"verdict\":\"approve\"}. But I cannot decide, so I decline.",
        ],
        &["--output-format", "json"],
    );
    assert_eq!(out.status.code(), Some(5), "{}", stdout(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["structured_output"], serde_json::Value::Null);
    assert!(v["error"].as_str().unwrap().contains("was not JSON"), "{v}");
}

/// 読めないスキーマでは LLM を呼ばない (検証できない結果が返るだけなので)。
#[test]
fn a_schema_lodan_cannot_enforce_fails_before_any_request() {
    let (out, log) = run_with_schema(
        r#"{"type":"object","properties":{"email":{"type":"string","format":"email"}}}"#,
        &["{}"],
        &["--output-format", "json"],
    );
    assert_eq!(out.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let error = v["error"].as_str().unwrap();
    assert!(
        error.contains("format") && error.contains("not supported"),
        "{error}"
    );
    assert_eq!(v["usage"]["llm_calls"], 0);
    assert_eq!(events(&log, "llm_response"), 0);
}

#[test]
fn output_schema_needs_print_mode() {
    let home = tempfile::tempdir().unwrap();
    let out = lodan(
        home.path(),
        1,
        &["--output-schema", "schema.json"],
        Stdin::OpenAndSilent,
    );
    assert_eq!(out.status.code(), Some(2), "a usage error, caught by clap");
}
