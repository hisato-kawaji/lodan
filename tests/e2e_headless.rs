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
    let script: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock_llm.py");
    let mut child = Command::new("python3")
        .arg(script)
        .arg("0")
        .arg(demo_dir)
        .envs(text.map(|t| ("MOCK_LLM_TEXT", t)))
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

/// stdin に何を繋ぐか。
enum Stdin {
    /// 開いたまま何も送らない (CI や親プロセスから継承した stdin の再現)。
    OpenAndSilent,
    Piped(&'static str),
}

/// 隔離した HOME / cwd で lodan を走らせ、終了まで待つ。`RUN_LIMIT` を超えたら kill して panic。
fn lodan(home: &Path, port: u16, args: &[&str], stdin: Stdin) -> Output {
    let cwd = home.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_lodan"))
        .args(args)
        .current_dir(&cwd)
        .env_clear()
        .env("HOME", home)
        // 接続先は env で渡す。テスト側が `--provider` などのフラグで上書きできるように。
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
    std::fs::write(
        cfg_dir.join("config.toml"),
        "[agent]\nmax_iterations = \"many\"\n",
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
