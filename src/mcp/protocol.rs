// JSON-RPC 2.0 + MCP メッセージ型。
// MCP protocolVersion: 2025-06-18

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const CLIENT_NAME: &str = "lodan";
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Serialize)]
pub struct JsonRpcRequest<'a, P: Serialize> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<P>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcNotification<'a, P: Serialize> {
    pub jsonrpc: &'static str,
    pub method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<P>,
}

/// Server → client frame. Either a response (has `id`) or a notification (no `id`).
#[derive(Debug, Deserialize)]
pub struct JsonRpcIncoming {
    #[allow(dead_code)]
    #[serde(default)]
    pub jsonrpc: Option<String>,
    #[serde(default)]
    pub id: Option<u64>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<JsonRpcError>,
    #[serde(default)]
    #[allow(dead_code)]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// ---------------- initialize ----------------

#[derive(Debug, Serialize)]
pub struct InitializeParams<'a> {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: &'a str,
    pub capabilities: ClientCapabilities,
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo<'a>,
}

#[derive(Debug, Default, Serialize)]
pub struct ClientCapabilities {
    /// roots を提供できることをサーバへ知らせる。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roots: Option<RootsCapability>,
    /// sampling (server→client の LLM 補完要求) を受け付けることを知らせる。
    /// opt-in したサーバにのみ広告する。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling: Option<SamplingCapability>,
}

#[derive(Debug, Serialize)]
pub struct RootsCapability {
    /// roots が動的に変わると通知するか。cwd 固定なので false。
    #[serde(rename = "listChanged")]
    pub list_changed: bool,
}

/// sampling capability は中身を持たない (空オブジェクトとして広告する)。
#[derive(Debug, Serialize)]
pub struct SamplingCapability {}

impl ClientCapabilities {
    /// roots は常に提供する。sampling は opt-in 時のみ広告する。
    pub fn new(sampling_enabled: bool) -> Self {
        Self {
            roots: Some(RootsCapability {
                list_changed: false,
            }),
            sampling: sampling_enabled.then_some(SamplingCapability {}),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ClientInfo<'a> {
    pub name: &'a str,
    pub version: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct InitializeResult {
    #[allow(dead_code)]
    #[serde(default, rename = "protocolVersion")]
    pub protocol_version: Option<String>,
    #[allow(dead_code)]
    #[serde(default, rename = "serverInfo")]
    pub server_info: Option<ServerInfo>,
    #[allow(dead_code)]
    #[serde(default)]
    pub capabilities: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct ServerInfo {
    #[allow(dead_code)]
    pub name: String,
    #[allow(dead_code)]
    pub version: String,
}

// ---------------- tools/list ----------------

#[derive(Debug, Serialize)]
pub struct ToolsListParams<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
pub struct ToolsListResult {
    pub tools: Vec<McpToolMeta>,
    #[serde(default, rename = "nextCursor")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpToolMeta {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Option<Value>,
    #[allow(dead_code)]
    #[serde(default)]
    pub annotations: Option<Value>,
}

// ---------------- tools/call ----------------

#[derive(Debug, Serialize)]
pub struct ToolsCallParams<'a> {
    pub name: &'a str,
    pub arguments: Value,
}

#[derive(Debug, Deserialize)]
pub struct ToolsCallResult {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default, rename = "isError")]
    pub is_error: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    // 他 (image / audio / resource / resource_link) は今回テキスト化して扱わない →
    // 受け取った場合は捨てるか stub 出力にする
    #[serde(other)]
    Other,
}

impl ToolsCallResult {
    /// Flatten content blocks into a single string. Non-text blocks are noted but skipped.
    pub fn flatten_text(&self) -> String {
        let mut buf = String::new();
        let mut non_text = 0usize;
        for block in &self.content {
            match block {
                ContentBlock::Text { text } => {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(text);
                }
                ContentBlock::Other => non_text += 1,
            }
        }
        if non_text > 0 {
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(&format!("[{non_text} non-text block(s) omitted]"));
        }
        buf
    }
}

// ---------------- prompts/list ----------------

#[derive(Debug, Serialize)]
pub struct PromptsListParams<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
pub struct PromptsListResult {
    #[serde(default)]
    pub prompts: Vec<PromptMeta>,
    #[serde(default, rename = "nextCursor")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PromptMeta {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub arguments: Vec<PromptArgument>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PromptArgument {
    pub name: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub description: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub required: bool,
}

// ---------------- prompts/get ----------------

#[derive(Debug, Serialize)]
pub struct PromptsGetParams<'a> {
    pub name: &'a str,
    pub arguments: Value,
}

#[derive(Debug, Deserialize)]
pub struct PromptsGetResult {
    #[allow(dead_code)]
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub messages: Vec<PromptMessage>,
}

#[derive(Debug, Deserialize)]
pub struct PromptMessage {
    pub role: String,
    pub content: PromptContent,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PromptContent {
    Text {
        text: String,
    },
    // image / resource 等の非テキストは今回テキスト化せず捨てる。
    #[serde(other)]
    Other,
}

impl PromptsGetResult {
    /// メッセージ群を 1 つのテキストへ畳む。user 以外の role は接頭辞を付ける
    /// (run_turn は単一の user 入力を取るため)。非テキストブロックは捨てる。
    pub fn render(&self) -> String {
        let mut buf = String::new();
        for msg in &self.messages {
            let PromptContent::Text { text } = &msg.content else {
                continue;
            };
            if !buf.is_empty() {
                buf.push_str("\n\n");
            }
            if msg.role == "user" {
                buf.push_str(text);
            } else {
                buf.push_str(&format!("[{}] {text}", msg.role));
            }
        }
        buf
    }
}

// ---------------- resources/list ----------------

#[derive(Debug, Serialize)]
pub struct ResourcesListParams<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
pub struct ResourcesListResult {
    #[serde(default)]
    pub resources: Vec<ResourceMeta>,
    #[serde(default, rename = "nextCursor")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResourceMeta {
    pub uri: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[allow(dead_code)]
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
}

// ---------------- resources/read ----------------

#[derive(Debug, Serialize)]
pub struct ResourcesReadParams<'a> {
    pub uri: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct ResourcesReadResult {
    #[serde(default)]
    pub contents: Vec<ResourceContents>,
}

#[derive(Debug, Deserialize)]
pub struct ResourceContents {
    #[allow(dead_code)]
    #[serde(default)]
    pub uri: Option<String>,
    #[allow(dead_code)]
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
    /// テキストリソースの本文。バイナリ (blob) は今回扱わない。
    #[serde(default)]
    pub text: Option<String>,
}

impl ResourcesReadResult {
    /// テキスト content を 1 つの文字列へ畳む。非テキスト (blob) は件数のみ注記。
    pub fn flatten_text(&self) -> String {
        let mut buf = String::new();
        let mut non_text = 0usize;
        for c in &self.contents {
            match &c.text {
                Some(t) => {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(t);
                }
                None => non_text += 1,
            }
        }
        if non_text > 0 {
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(&format!("[{non_text} non-text resource(s) omitted]"));
        }
        buf
    }
}

// ---------------- sampling/createMessage (server → client) ----------------

/// サーバから来る `sampling/createMessage` の params。MVP では messages /
/// systemPrompt / maxTokens のみ解釈し、modelPreferences 等は無視する。
#[derive(Debug, Deserialize)]
pub struct CreateMessageParams {
    #[serde(default)]
    pub messages: Vec<SamplingMessage>,
    #[serde(default, rename = "systemPrompt")]
    pub system_prompt: Option<String>,
    #[allow(dead_code)]
    #[serde(default, rename = "maxTokens")]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct SamplingMessage {
    pub role: String,
    pub content: SamplingContent,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SamplingContent {
    Text {
        text: String,
    },
    /// image / audio 等の非テキストは MVP では扱わず捨てる。
    #[serde(other)]
    Other,
}

impl SamplingMessage {
    /// テキスト content を取り出す。非テキストは None。
    pub fn text(&self) -> Option<&str> {
        match &self.content {
            SamplingContent::Text { text } => Some(text),
            SamplingContent::Other => None,
        }
    }
}

/// `sampling/createMessage` の result。client→server へ返す。
#[derive(Debug, Serialize)]
pub struct CreateMessageResult {
    pub role: &'static str,
    pub content: CreateMessageContent,
    pub model: String,
    #[serde(rename = "stopReason")]
    pub stop_reason: &'static str,
}

#[derive(Debug, Serialize)]
pub struct CreateMessageContent {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub text: String,
}

impl CreateMessageResult {
    /// assistant のテキスト応答を MCP result 形へ包む。
    pub fn assistant_text(text: String, model: String) -> Self {
        Self {
            role: "assistant",
            content: CreateMessageContent { kind: "text", text },
            model,
            stop_reason: "endTurn",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_initialize_params() {
        let p = InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            capabilities: ClientCapabilities::default(),
            client_info: ClientInfo {
                name: CLIENT_NAME,
                version: CLIENT_VERSION,
            },
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("protocolVersion"));
        assert!(s.contains("clientInfo"));
    }

    #[test]
    fn parses_tools_list_response() {
        let s = r#"{
            "tools": [
                {"name":"echo", "description":"x", "inputSchema":{"type":"object"}}
            ]
        }"#;
        let r: ToolsListResult = serde_json::from_str(s).unwrap();
        assert_eq!(r.tools.len(), 1);
        assert_eq!(r.tools[0].name, "echo");
        assert!(r.next_cursor.is_none());
    }

    #[test]
    fn flattens_text_content() {
        let r = ToolsCallResult {
            content: vec![
                ContentBlock::Text { text: "a".into() },
                ContentBlock::Text { text: "b".into() },
            ],
            is_error: false,
        };
        assert_eq!(r.flatten_text(), "a\nb");
    }

    #[test]
    fn flatten_notes_non_text_blocks() {
        let r: ToolsCallResult = serde_json::from_str(
            r#"{"content":[{"type":"text","text":"x"},{"type":"image","data":"...","mimeType":"image/png"}]}"#,
        )
        .unwrap();
        let s = r.flatten_text();
        assert!(s.contains("x"));
        assert!(s.contains("non-text"));
    }

    #[test]
    fn parses_prompts_list_response() {
        let s = r#"{
            "prompts": [
                {"name":"review","description":"review code",
                 "arguments":[{"name":"path","required":true},{"name":"focus"}]}
            ]
        }"#;
        let r: PromptsListResult = serde_json::from_str(s).unwrap();
        assert_eq!(r.prompts.len(), 1);
        assert_eq!(r.prompts[0].name, "review");
        assert_eq!(r.prompts[0].arguments.len(), 2);
        assert_eq!(r.prompts[0].arguments[0].name, "path");
        assert!(r.prompts[0].arguments[0].required);
    }

    #[test]
    fn renders_prompt_messages_into_text() {
        let r: PromptsGetResult = serde_json::from_str(
            r#"{"messages":[
                {"role":"user","content":{"type":"text","text":"do X"}},
                {"role":"assistant","content":{"type":"text","text":"context"}},
                {"role":"user","content":{"type":"image","data":"..","mimeType":"image/png"}}
            ]}"#,
        )
        .unwrap();
        let out = r.render();
        assert!(out.contains("do X"));
        assert!(out.contains("[assistant] context"));
        // 非テキスト (image) は捨てられる。
        assert!(!out.contains("image"));
    }

    #[test]
    fn parses_resources_list_response() {
        let s = r#"{
            "resources": [
                {"uri":"file:///a.txt","name":"a","description":"file a","mimeType":"text/plain"}
            ]
        }"#;
        let r: ResourcesListResult = serde_json::from_str(s).unwrap();
        assert_eq!(r.resources.len(), 1);
        assert_eq!(r.resources[0].uri, "file:///a.txt");
        assert_eq!(r.resources[0].name.as_deref(), Some("a"));
    }

    #[test]
    fn flattens_resource_contents_and_notes_blob() {
        let r: ResourcesReadResult = serde_json::from_str(
            r#"{"contents":[
                {"uri":"file:///a","text":"hello"},
                {"uri":"file:///b","mimeType":"image/png","blob":"=="}
            ]}"#,
        )
        .unwrap();
        let out = r.flatten_text();
        assert!(out.contains("hello"));
        assert!(out.contains("non-text resource"));
    }

    #[test]
    fn parses_create_message_params_and_skips_non_text() {
        let p: CreateMessageParams = serde_json::from_str(
            r#"{
                "systemPrompt": "be brief",
                "maxTokens": 100,
                "messages": [
                    {"role":"user","content":{"type":"text","text":"hi"}},
                    {"role":"user","content":{"type":"image","data":"..","mimeType":"image/png"}}
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(p.system_prompt.as_deref(), Some("be brief"));
        assert_eq!(p.max_tokens, Some(100));
        assert_eq!(p.messages.len(), 2);
        assert_eq!(p.messages[0].text(), Some("hi"));
        assert_eq!(p.messages[1].text(), None);
    }

    #[test]
    fn serializes_create_message_result() {
        let r = CreateMessageResult::assistant_text("hello".into(), "gpt".into());
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"]["type"], "text");
        assert_eq!(v["content"]["text"], "hello");
        assert_eq!(v["model"], "gpt");
        assert_eq!(v["stopReason"], "endTurn");
    }

    #[test]
    fn sampling_capability_advertised_only_when_enabled() {
        let off = serde_json::to_value(ClientCapabilities::new(false)).unwrap();
        assert!(off.get("sampling").is_none());
        assert!(off.get("roots").is_some());
        let on = serde_json::to_value(ClientCapabilities::new(true)).unwrap();
        assert!(on.get("sampling").is_some());
    }

    #[test]
    fn incoming_decodes_response() {
        let s = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let inc: JsonRpcIncoming = serde_json::from_str(s).unwrap();
        assert_eq!(inc.id, Some(1));
        assert!(inc.result.is_some());
        assert!(inc.error.is_none());
    }

    #[test]
    fn incoming_decodes_error() {
        let s = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"x"}}"#;
        let inc: JsonRpcIncoming = serde_json::from_str(s).unwrap();
        assert_eq!(inc.error.unwrap().code, -32601);
    }
}
