// MCP サーバを `.mcp.json` から読み、各サーバへ stdio で接続し、
// 公開された tools を ToolRegistry に登録する。
//
// 起動失敗 / list_tools 失敗は warning に留め、REPL は続行する。

use std::sync::Arc;

use anyhow::Result;

use crate::llm::LlmClient;
use crate::mcp::client::McpClient;
use crate::mcp::config::McpServersConfig;
use crate::mcp::prompt::{McpPrompt, namespaced as prompt_namespaced};
use crate::mcp::resource::McpResourceTool;
use crate::mcp::sampling::SamplingProvider;
use crate::mcp::tool::{McpTool, namespaced};
use crate::tools::registry::ToolRegistry;

/// `allowSampling` を opt-in したサーバに渡す LLM 補完の出口。
#[derive(Clone)]
pub struct SamplingContext {
    pub llm: Arc<dyn LlmClient>,
    pub model: String,
}

/// Load `.mcp.json` from CWD, connect each server, and register their tools.
/// Returns the live clients so the caller can keep them alive (Drop on session end).
/// `sampling` を渡すと、allowSampling=true のサーバに sampling/createMessage を許可する。
pub async fn load_and_register(
    reg: &mut ToolRegistry,
    sampling: Option<SamplingContext>,
) -> Result<LoadOutcome> {
    let cfg = match McpServersConfig::load_from_cwd()? {
        Some(c) => c,
        None => return Ok(LoadOutcome::default()),
    };

    let mut outcome = LoadOutcome::default();
    for (server_name, spec) in cfg.mcp_servers {
        // sampling は opt-in サーバかつ LLM コンテキストがある場合のみ有効化する。
        let sampling_provider = match (&sampling, spec.allow_sampling) {
            (Some(ctx), true) => Some(Arc::new(SamplingProvider::new(
                Arc::clone(&ctx.llm),
                ctx.model.clone(),
            ))),
            _ => None,
        };
        match McpClient::connect(&server_name, &spec, sampling_provider).await {
            Ok(client) => {
                let client = Arc::new(client);
                match client.list_tools().await {
                    Ok(tools) => {
                        for meta in tools {
                            let full = namespaced(&server_name, &meta.name);
                            let desc = meta.description.unwrap_or_default();
                            let schema = meta.input_schema.unwrap_or(serde_json::json!({}));
                            reg.register(Arc::new(McpTool::new(
                                full,
                                meta.name,
                                desc,
                                schema,
                                Arc::clone(&client),
                            )));
                            outcome.tools += 1;
                        }
                        outcome.servers += 1;

                        // prompts は任意 capability。未対応サーバは prompts/list が
                        // method-not-found を返すだけなので warning に留めて続行する。
                        match client.list_prompts().await {
                            Ok(prompts) => {
                                for meta in prompts {
                                    let full = prompt_namespaced(&server_name, &meta.name);
                                    let desc = meta.description.unwrap_or_default();
                                    let arg_names =
                                        meta.arguments.into_iter().map(|a| a.name).collect();
                                    outcome.prompts.push(McpPrompt::new(
                                        full,
                                        meta.name,
                                        desc,
                                        arg_names,
                                        Arc::clone(&client),
                                    ));
                                }
                            }
                            Err(e) => {
                                eprintln!("mcp[{server_name}]: prompts/list skipped: {e}");
                            }
                        }

                        // resources も任意 capability。公開していれば 1 つの
                        // read_resource ツールとして登録する。
                        match client.list_resources().await {
                            Ok(resources) if !resources.is_empty() => {
                                reg.register(Arc::new(McpResourceTool::new(
                                    &server_name,
                                    &resources,
                                    Arc::clone(&client),
                                )));
                                outcome.resources += resources.len();
                            }
                            Ok(_) => {}
                            Err(e) => {
                                eprintln!("mcp[{server_name}]: resources/list skipped: {e}");
                            }
                        }

                        outcome.clients.push(client);
                    }
                    Err(e) => {
                        eprintln!("mcp[{server_name}]: list_tools failed: {e}");
                    }
                }
            }
            Err(e) => {
                eprintln!("mcp[{server_name}]: connect failed: {e}");
            }
        }
    }
    Ok(outcome)
}

#[derive(Default)]
pub struct LoadOutcome {
    pub servers: usize,
    pub tools: usize,
    /// 公開された resource の総数 (read_resource ツールは ToolRegistry に登録済み)。
    pub resources: usize,
    /// MCP サーバが公開する prompt (slash として呼び出される)。
    pub prompts: Vec<McpPrompt>,
    /// Kept alive by the caller; on Drop the subprocess is killed.
    pub clients: Vec<Arc<McpClient>>,
}
