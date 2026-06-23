//! End-to-end test for the stdio MCP client.
//!
//! Spawns `tests/fixtures/mock_mcp_server.py`, runs handshake + tools/list +
//! tools/call(echo), and asserts the flattened text content round-trips.

use std::collections::BTreeMap;
use std::path::PathBuf;

use lodan::mcp::client::McpClient;
use lodan::mcp::config::McpServerSpec;

fn fixture_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mock_mcp_server.py")
}

#[tokio::test]
async fn handshake_list_and_call_round_trip() {
    let spec = McpServerSpec {
        command: "python3".to_string(),
        args: vec![fixture_script().to_string_lossy().into_owned()],
        env: BTreeMap::new(),
    };

    let client = McpClient::connect_stdio("mock", &spec)
        .await
        .expect("connect MCP mock");

    let tools = client.list_tools().await.expect("list_tools");
    assert_eq!(tools.len(), 1, "expected one tool, got {tools:?}");
    assert_eq!(tools[0].name, "echo");
    assert!(tools[0].description.is_some());

    let result = client
        .call_tool("echo", serde_json::json!({ "msg": "hi" }))
        .await
        .expect("call_tool");
    assert!(!result.is_error);
    // Mock returns "echo:" + json-serialized args.
    assert!(
        result.content.contains("echo:") && result.content.contains("\"msg\":\"hi\""),
        "unexpected content: {}",
        result.content
    );
}
