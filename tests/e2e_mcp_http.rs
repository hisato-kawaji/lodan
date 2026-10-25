//! End-to-end test for the Streamable HTTP MCP transport.
//!
//! Spawns `tests/fixtures/mock_mcp_http_server.py`, connects via the HTTP
//! transport, and asserts handshake + tools/list + tools/call(echo) round-trip.

use std::collections::BTreeMap;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use lodan::mcp::client::McpClient;
use lodan::mcp::config::McpServerSpec;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mock_mcp_http_server.py")
}

fn pick_port() -> u16 {
    let l = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral");
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

struct Server {
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(port: u16) -> Server {
    let child = Command::new("python3")
        .arg(fixture())
        .arg(port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn python3 mock_mcp_http_server.py");
    let server = Server { child };
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return server;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("mock HTTP MCP server did not become ready on port {port}");
}

#[tokio::test]
async fn http_handshake_list_and_call_round_trip() {
    let port = pick_port();
    let _server = start(port);

    let spec = McpServerSpec {
        command: None,
        args: vec![],
        env: BTreeMap::new(),
        url: Some(format!("http://127.0.0.1:{port}/")),
        headers: BTreeMap::new(),
        allow_sampling: false,
        trust_annotations: false,
        enabled_tools: Vec::new(),
        disabled_tools: Vec::new(),
        tool_timeout_secs: None,
        max_output_bytes: None,
    };

    let client = McpClient::connect("mock-http", &spec, None, None)
        .await
        .expect("connect HTTP MCP mock");

    let tools = client.list_tools().await.expect("list_tools");
    assert_eq!(tools.len(), 1, "expected one tool, got {tools:?}");
    assert_eq!(tools[0].name, "echo");

    let result = client
        .call_tool("echo", serde_json::json!({ "msg": "hi" }))
        .await
        .expect("call_tool");
    assert!(!result.is_error);
    assert!(
        result.content.contains("echo:") && result.content.contains("\"msg\":\"hi\""),
        "unexpected content: {}",
        result.content
    );
}

fn spec_for(port: u16) -> McpServerSpec {
    McpServerSpec {
        command: None,
        args: vec![],
        env: BTreeMap::new(),
        url: Some(format!("http://127.0.0.1:{port}/")),
        headers: BTreeMap::new(),
        allow_sampling: false,
        trust_annotations: false,
        enabled_tools: Vec::new(),
        disabled_tools: Vec::new(),
        tool_timeout_secs: Some(15),
        max_output_bytes: None,
    }
}

/// #83: 初期化のあと GET の SSE ストリームを開き、そこに来た server→client リクエスト
/// (`roots/list`) には POST で答え、通知 (`tools/list_changed`) は印として持つ。
#[tokio::test]
async fn the_get_stream_carries_server_requests_and_notifications() {
    let port = pick_port();
    let _server = start(port);
    let client = McpClient::connect("mock-http", &spec_for(port), None, None)
        .await
        .expect("connect HTTP MCP mock");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut changed = false;
    let mut answer = String::new();
    while Instant::now() < deadline {
        changed |= client.take_tools_changed();
        answer = client
            .call_tool("last_roots", serde_json::json!({}))
            .await
            .expect("call_tool")
            .content;
        if changed && answer != "none" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        changed,
        "tools/list_changed from the GET stream was not seen"
    );
    assert!(
        answer.contains("\"id\":\"get-1\"") && answer.contains("file://"),
        "roots/list over the GET stream was not answered: {answer}"
    );
}

/// #83: POST の応答が SSE のとき、応答より前に来た server→client リクエストに答えてから
/// 応答を受け取る (本文を全部読んでから探すと、サーバが答えを待ったまま固まる)。
#[tokio::test]
async fn a_server_request_inside_a_post_stream_is_answered_before_the_response() {
    let port = pick_port();
    let _server = start(port);
    let client = McpClient::connect("mock-http", &spec_for(port), None, None)
        .await
        .expect("connect HTTP MCP mock");
    let result = client
        .call_tool("ask", serde_json::json!({}))
        .await
        .expect("call_tool ask");
    assert!(
        result.content.contains("\"id\":\"post-1\"") && result.content.contains("file://"),
        "{}",
        result.content
    );
}
