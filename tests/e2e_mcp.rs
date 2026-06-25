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

    // prompts/list + prompts/get round-trip.
    let prompts = client.list_prompts().await.expect("list_prompts");
    assert_eq!(prompts.len(), 1, "expected one prompt, got {prompts:?}");
    assert_eq!(prompts[0].name, "greet");
    assert_eq!(prompts[0].arguments.len(), 1);
    assert_eq!(prompts[0].arguments[0].name, "who");

    let got = client
        .get_prompt("greet", serde_json::json!({ "who": "Ada" }))
        .await
        .expect("get_prompt");
    assert_eq!(got.render(), "Say hello to Ada.");

    // resources/list + resources/read round-trip.
    let resources = client.list_resources().await.expect("list_resources");
    assert_eq!(
        resources.len(),
        1,
        "expected one resource, got {resources:?}"
    );
    assert_eq!(resources[0].uri, "mem://notes");

    let read = client
        .read_resource("mem://notes")
        .await
        .expect("read_resource");
    assert_eq!(read.flatten_text(), "remember the milk");
}
