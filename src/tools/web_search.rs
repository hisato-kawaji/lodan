// Brave Search API でウェブ検索し、結果を番号付きリストで返す。
// API キーは env `BRAVE_API_KEY` から読む (.env は main で dotenvy 読込)。
// エンドポイントは env `BRAVE_SEARCH_API_URL` で差し替え可 (既定は Brave 本番)。
// read-only なので非破壊。
//
// ⚠️ 信頼前提: クエリは外部 (Brave) へ送られる。WebFetch と同じく実行環境を
// 信頼する前提で使う。

use async_trait::async_trait;
use serde::Deserialize;
use std::time::Duration;

use super::{Tool, ToolCtx, ToolError, ToolOutput};

const DEFAULT_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_COUNT: u32 = 5;
const MAX_COUNT: u32 = 20;

pub struct WebSearch;

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "WebSearch"
    }

    fn description(&self) -> &str {
        "Search the web via the Brave Search API and return ranked results \
         (title, url, snippet). Requires the BRAVE_API_KEY environment variable."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "count": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_COUNT,
                    "default": DEFAULT_COUNT
                }
            },
            "required": ["query"]
        })
    }

    fn is_destructive(&self) -> bool {
        // read-only: 検索クエリのみ (外部への書き込みなし)
        false
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ToolError::InvalidArgs("WebSearch: non-empty `query` (string) required".into())
            })?
            .to_string();

        let count = args
            .get("count")
            .and_then(|v| v.as_u64())
            .map(|c| (c as u32).clamp(1, MAX_COUNT))
            .unwrap_or(DEFAULT_COUNT);

        let key = std::env::var("BRAVE_API_KEY").unwrap_or_default();
        if key.is_empty() {
            return Ok(ToolOutput::error(
                "WebSearch: BRAVE_API_KEY is not set (add it to .env or the environment)",
            ));
        }
        let endpoint =
            std::env::var("BRAVE_SEARCH_API_URL").unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string());
        // env 由来 (信頼) だが、WebFetch と同様に http/https へ揃えておく。
        if !is_http_url(&endpoint) {
            return Ok(ToolOutput::error(format!(
                "WebSearch: BRAVE_SEARCH_API_URL must be http/https (got {endpoint})"
            )));
        }

        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| ToolError::Other(format!("building HTTP client: {e}")))?;

        match run_search(&client, &endpoint, &key, &query, count).await {
            Ok(results) => Ok(ToolOutput::ok(format_results(&query, &results))),
            Err(e) => Ok(ToolOutput::error(format!("WebSearch: {e}"))),
        }
    }
}

#[derive(Debug, Clone)]
struct SearchResult {
    title: String,
    url: String,
    description: String,
}

#[derive(Deserialize)]
struct BraveResponse {
    #[serde(default)]
    web: Option<BraveWeb>,
}

#[derive(Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(Deserialize)]
struct BraveResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    description: String,
}

async fn run_search(
    client: &reqwest::Client,
    endpoint: &str,
    key: &str,
    query: &str,
    count: u32,
) -> Result<Vec<SearchResult>, String> {
    let resp = client
        .get(endpoint)
        .query(&[("q", query), ("count", &count.to_string())])
        .header("Accept", "application/json")
        .header("X-Subscription-Token", key)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("reading response: {e}"))?;
    if !status.is_success() {
        return Err(format!("HTTP {}: {}", status.as_u16(), body.trim()));
    }

    parse_results(&body)
}

fn parse_results(body: &str) -> Result<Vec<SearchResult>, String> {
    let parsed: BraveResponse =
        serde_json::from_str(body).map_err(|e| format!("parsing JSON: {e}"))?;
    let results = parsed
        .web
        .map(|w| w.results)
        .unwrap_or_default()
        .into_iter()
        .map(|r| SearchResult {
            title: strip_tags(&r.title),
            url: r.url,
            description: strip_tags(&r.description),
        })
        .collect();
    Ok(results)
}

fn format_results(query: &str, results: &[SearchResult]) -> String {
    if results.is_empty() {
        return format!("No results for \"{query}\".");
    }
    let mut out = format!("Results for \"{query}\":\n");
    for (i, r) in results.iter().enumerate() {
        out.push_str(&format!("\n{}. {}\n   {}\n", i + 1, r.title, r.url));
        if !r.description.is_empty() {
            out.push_str(&format!("   {}\n", r.description));
        }
    }
    out
}

fn is_http_url(u: &str) -> bool {
    let l = u.to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

/// Brave の title/description に含まれる簡易 HTML タグ (<strong> 等) を落とす。
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    const BRAVE_JSON: &str = r#"{
        "web": { "results": [
            {"title": "Rust <strong>lang</strong>", "url": "https://rust-lang.org",
             "description": "A <strong>systems</strong> language"},
            {"title": "Tokio", "url": "https://tokio.rs", "description": "async runtime"}
        ]}
    }"#;

    #[test]
    fn parses_and_strips_tags() {
        let r = parse_results(BRAVE_JSON).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "Rust lang");
        assert_eq!(r[0].url, "https://rust-lang.org");
        assert_eq!(r[0].description, "A systems language");
    }

    #[test]
    fn missing_web_key_yields_empty() {
        let r = parse_results(r#"{"query":{}}"#).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn formats_numbered_list() {
        let r = parse_results(BRAVE_JSON).unwrap();
        let out = format_results("rust", &r);
        assert!(out.contains("1. Rust lang"));
        assert!(out.contains("https://tokio.rs"));
        assert!(out.contains("Results for \"rust\""));
    }

    #[test]
    fn formats_empty_results() {
        assert_eq!(format_results("x", &[]), "No results for \"x\".");
    }

    #[test]
    fn endpoint_scheme_check() {
        assert!(is_http_url(DEFAULT_ENDPOINT));
        assert!(is_http_url("http://127.0.0.1:8080/"));
        assert!(!is_http_url("file:///x"));
        assert!(!is_http_url("ftp://x"));
    }

    #[tokio::test]
    async fn empty_query_is_invalid_args() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let res = WebSearch
            .execute(serde_json::json!({ "query": "   " }), &ctx)
            .await;
        assert!(matches!(res, Err(ToolError::InvalidArgs(_))));
    }

    // ローカル one-shot サーバを Brave 互換レスポンスで応答させ、HTTP→parse を検証する。
    // base_url / key を直接渡すので env を触らない (並列テストでの競合なし)。
    #[tokio::test]
    async fn run_search_round_trips_against_local_server() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    BRAVE_JSON.len(),
                    BRAVE_JSON
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });

        let client = reqwest::Client::builder().timeout(TIMEOUT).build().unwrap();
        let url = format!("http://127.0.0.1:{port}/");
        let results = run_search(&client, &url, "test-key", "rust", 2)
            .await
            .unwrap();

        server.join().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust lang");
    }
}
