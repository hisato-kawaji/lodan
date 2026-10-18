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
    start_with(port, &[])
}

fn start_with(port: u16, envs: &[(&str, &str)]) -> Server {
    let child = Command::new("python3")
        .arg(fixture())
        .arg(port.to_string())
        .envs(envs.iter().copied())
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
        tool_timeout_secs: None,
        max_output_bytes: None,
    }
}

/// 一式の往復。`MOCK_MCP_NO_GET=1` は GET ストリームを 405 で断るサーバ — 断られても
/// 普通の POST の往復は変わらない (#83)。
#[tokio::test]
async fn http_handshake_list_and_call_round_trip() {
    for envs in [&[][..], &[("MOCK_MCP_NO_GET", "1")][..]] {
        handshake_list_and_call(envs).await;
    }
}

async fn handshake_list_and_call(envs: &[(&str, &str)]) {
    let port = pick_port();
    let _server = start_with(port, envs);
    let spec = spec_for(port);

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

async fn connected(port: u16) -> McpClient {
    McpClient::connect("mock-http", &spec_for(port), None, None)
        .await
        .expect("connect HTTP MCP mock")
}

fn text_of(out: lodan::tools::ToolOutput) -> String {
    assert!(!out.is_error, "{}", out.content);
    out.content
}

/// #83: GET ストリームで届いた `notifications/tools/list_changed` を、stdio と同じ印で受ける。
#[tokio::test]
async fn http_server_stream_delivers_list_changed() {
    let port = pick_port();
    let _server = start(port);
    let client = connected(port).await;
    assert!(!client.take_tools_changed());
    text_of(
        client
            .call_tool("trigger_list_changed", serde_json::json!({}))
            .await
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !client.take_tools_changed() {
        assert!(
            Instant::now() < deadline,
            "list_changed never arrived on the GET stream"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let names: Vec<String> = client
        .list_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, ["echo", "extra"]);
}

/// #83: tools/call の SSE 応答の途中に来たサーバからのリクエストに、別の POST で答える
/// (答えないとサーバは応答を返さず、呼び出しはタイムアウトまで固まる)。
#[tokio::test]
async fn http_server_requests_in_a_post_stream_are_answered() {
    let port = pick_port();
    let _server = start(port);
    let client = connected(port).await;
    let text = text_of(
        client
            .call_tool("ask_roots", serde_json::json!({}))
            .await
            .unwrap(),
    );
    let response: serde_json::Value = serde_json::from_str(&text).expect(&text);
    assert_eq!(response["id"], 901, "{text}");
    assert!(
        response["result"]["roots"][0]["uri"]
            .as_str()
            .is_some_and(|u| u.starts_with("file://")),
        "{text}"
    );
}

/// #83: GET ストリームで来たサーバからのリクエストにも答える。
#[tokio::test]
async fn http_server_requests_on_the_get_stream_are_answered() {
    let port = pick_port();
    let _server = start(port);
    let client = connected(port).await;
    text_of(
        client
            .call_tool("ping_via_get", serde_json::json!({}))
            .await
            .unwrap(),
    );
    let text = text_of(
        client
            .call_tool("last_response", serde_json::json!({ "id": 902 }))
            .await
            .unwrap(),
    );
    let response: serde_json::Value = serde_json::from_str(&text).expect(&text);
    assert_eq!(response["id"], 902, "{text}");
    assert!(response["result"]["roots"].is_array(), "{text}");
}
