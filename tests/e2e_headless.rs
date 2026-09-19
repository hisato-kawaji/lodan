//! ヘッドレス実行 (`lodan -p`) の end-to-end。実バイナリを起動して **stdout を検証する** —
//! stdout は呼び出し側との契約で、`Session` を直接叩くテストからは見えない。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
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

fn start_mock(demo_dir: &Path) -> MockServer {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let script: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock_llm.py");
    let child = Command::new("python3")
        .arg(script)
        .arg(port.to_string())
        .arg(demo_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn python3 mock_llm.py (is python3 on PATH?)");
    let server = MockServer { child, port };
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return server;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("mock server did not become ready on port {port}");
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
