// MCP (Model Context Protocol) クライアント。
// transport: stdio / Streamable HTTP。
// capability: tools / prompts / resources / roots、および opt-in の sampling。

pub mod client;
pub mod config;
pub mod prompt;
pub mod protocol;
pub mod registry;
pub mod resource;
pub mod roots;
pub mod sampling;
pub mod tool;
pub mod transport;
