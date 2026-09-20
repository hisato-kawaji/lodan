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
        /// モデルの思考過程 (#78)。ツール往復の間はそのまま送り返す (Moonshot / DeepSeek 系は
        /// これが無いと、続きの推論の質が落ちる)。次の利用者の入力が来たら落とす — 過去のターンの
        /// 思考はコンテキストを食うだけで、どのプロバイダも要求しない。
        /// 無ければ直列化されないので、古い transcript とも、思考を返さないサーバとも互換。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 思考過程が無ければ直列化されない (= これまでのリクエスト body と transcript のまま)。
    /// 思考過程の無い古い transcript も読める。
    #[test]
    fn reasoning_content_is_optional_on_the_wire_and_on_disk() {
        let plain = Message::Assistant {
            content: Some("hi".into()),
            tool_calls: vec![],
            reasoning_content: None,
        };
        assert_eq!(
            serde_json::to_string(&plain).unwrap(),
            r#"{"role":"assistant","content":"hi"}"#
        );
        let thinking = Message::Assistant {
            content: None,
            tool_calls: vec![],
            reasoning_content: Some("hmm".into()),
        };
        assert_eq!(
            serde_json::to_string(&thinking).unwrap(),
            r#"{"role":"assistant","reasoning_content":"hmm"}"#
        );
        let old: Message = serde_json::from_str(r#"{"role":"assistant","content":"hi"}"#).unwrap();
        assert!(matches!(
            old,
            Message::Assistant {
                reasoning_content: None,
                ..
            }
        ));
    }
}
