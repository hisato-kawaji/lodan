// MCP (Model Context Protocol) クライアント。
// 現状は stdio transport + tools capability のみ。
// resources / prompts / sampling / roots / Streamable HTTP は未対応 (将来)。

pub mod client;
pub mod config;
pub mod protocol;
pub mod registry;
pub mod tool;
