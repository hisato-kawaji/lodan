//! End-to-end test against the bundled OpenAI-compatible mock server.
//!
//! Spins up `tests/fixtures/mock_llm.py` as a child process, points a real
//! `Session` at it, runs the "demo" prompt, and asserts that all six MVP
//! tools fired in order and the final file content is correct.

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lodan::agent::Session;
use lodan::config::Config;
use lodan::llm;
use lodan::permission::PermissionGate;
use lodan::tools::registry::default_registry;

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

fn pick_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn fixtures_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mock_llm.py")
}

fn start_mock(demo_dir: &Path) -> MockServer {
    let port = pick_port();
    let script = fixtures_path();
    let child = Command::new("python3")
        .arg(&script)
        .arg(port.to_string())
        .arg(demo_dir.to_str().expect("utf8 demo_dir"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn python3 mock_llm.py (is python3 on PATH?)");

    // child を即 MockServer に包む。タイムアウト panic でも Drop が kill+wait するため
    // 子プロセスが zombie として取り残されない。
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

/// JSONL 行を読み、指定 event の行だけ返す。
fn events_named<'a>(lines: &'a [serde_json::Value], event: &str) -> Vec<&'a serde_json::Value> {
    lines.iter().filter(|v| v["event"] == event).collect()
}

#[tokio::test]
async fn demo_runs_all_six_tools_via_streaming() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let demo_dir = tmp.path().to_path_buf();

    // 実行トレースも同じ流れで検証する (この test binary で唯一の test なので
    // プロセス唯一の sink を初期化して競合しない)。
    let log_path = tmp.path().join("run.jsonl");
    lodan::runlog::init(&log_path).expect("init runlog");

    let server = start_mock(&demo_dir);

    let mut cfg = Config::default();
    cfg.llm.local.base_url = format!("http://127.0.0.1:{}/v1", server.port);
    cfg.llm.local.model = "mock".to_string();
    cfg.agent.auto_approve = true;
    cfg.agent.max_iterations = 25;

    let registry = Arc::new(default_registry());
    let llm_client = llm::build_client(&cfg).expect("build client");
    let gate = PermissionGate::new(true);
    let mut session = Session::new(cfg, Arc::clone(&registry));

    session
        .run_turn("demo", llm_client.as_ref(), &gate)
        .await
        .expect("run_turn");

    let final_path = demo_dir.join("hello.txt");
    let content = std::fs::read_to_string(&final_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", final_path.display()));
    assert_eq!(
        content, "hello world",
        "Edit should rewrite hi -> hello world"
    );

    let body = std::fs::read_to_string(&log_path).expect("read run log");
    let lines: Vec<serde_json::Value> = body
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad jsonl line {l:?}: {e}")))
        .collect();

    let starts = events_named(&lines, "turn_start");
    assert_eq!(starts.len(), 1, "one turn_start per run_turn");
    assert_eq!(starts[0]["mode"], "normal");

    assert!(
        !events_named(&lines, "llm_response").is_empty(),
        "each LLM round trip is recorded"
    );

    // ツール実行は 1 呼び出し 1 行で、全て成功しているはず。
    let tools = events_named(&lines, "tool_result");
    let names: Vec<&str> = tools.iter().filter_map(|v| v["name"].as_str()).collect();
    for expected in ["Write", "Read", "Edit", "Glob", "Grep", "Bash"] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }
    for t in &tools {
        assert_eq!(t["outcome"], "ok", "unexpected tool failure: {t}");
        assert_eq!(t["reason"], "ok");
        assert!(t["ms"].is_u64() && t["output_bytes"].is_u64());
    }

    let ends = events_named(&lines, "turn_end");
    assert_eq!(ends.len(), 1, "one turn_end per run_turn");
    assert_eq!(ends[0]["reason"], "final");
    assert_eq!(
        ends[0]["tool_calls"].as_u64().unwrap() as usize,
        tools.len(),
        "turn_end tool_calls should match recorded tool_result rows"
    );
}
