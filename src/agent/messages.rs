use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    /// Per OpenAI spec, the arguments is a JSON-encoded **string**.
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec<'a> {
    #[serde(rename = "type")]
    pub kind: &'a str,
    pub function: ToolSpecFunction<'a>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolSpecFunction<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub parameters: serde_json::Value,
}
