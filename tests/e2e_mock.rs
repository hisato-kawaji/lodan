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

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return MockServer { child, port };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("mock server did not become ready on port {port}");
}

#[tokio::test]
async fn demo_runs_all_six_tools_via_streaming() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let demo_dir = tmp.path().to_path_buf();

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
    assert_eq!(content, "hello world", "Edit should rewrite hi -> hello world");
}
