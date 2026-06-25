// MCP (Model Context Protocol) クライアント。
// 現状は stdio transport + tools / prompts / resources capability。
// sampling / roots / Streamable HTTP は未対応 (将来)。

pub mod client;
pub mod config;
pub mod prompt;
pub mod protocol;
pub mod registry;
pub mod resource;
pub mod tool;
pub mod transport;
