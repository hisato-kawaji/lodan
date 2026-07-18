// 指定 URL を GET し、本文をテキストとして返す。
// HTML は軽量にタグ除去してテキスト化する (依存追加なし)。
// read-only な GET なので非破壊。サイズ上限とタイムアウトを課す。
//
// ⚠️ 信頼前提: フェッチ先 URL はモデルが決める。内部ネットワーク等への到達
// (SSRF) や、クエリ経由の情報送出があり得る。`.mcp.json` / hooks と同じく
// 「実行環境を信頼する」前提で使う。

use async_trait::async_trait;
use std::time::Duration;

use super::{Tool, ToolCtx, ToolError, ToolOutput};

/// 返す本文の最大文字数。超過分は切り詰めて注記する。
const MAX_CHARS: usize = 50_000;
const TIMEOUT: Duration = Duration::from_secs(15);
/// 追従するリダイレクトの最大ホップ数。
const MAX_REDIRECTS: usize = 5;

pub struct WebFetch;

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "WebFetch"
    }

    fn description(&self) -> &str {
        "Fetch an http(s) URL with GET and return its text body. HTML is reduced to \
         plain text. Output is capped; large bodies are truncated."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "http:// or https:// URL" }
            },
            "required": ["url"]
        })
    }

    fn is_destructive(&self) -> bool {
        // read-only: HTTP GET のみ (外部への書き込みなし)
        false
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("WebFetch: `url` (string) required".into()))?
            .to_string();

        if !is_http_url(&url) {
            return Err(ToolError::InvalidArgs(format!(
                "WebFetch: only http/https URLs are allowed (got {url})"
            )));
        }

        // リダイレクトは追うが、各ホップを http/https に限定しホップ数を制限する。
        // (初回 URL だけ検証してもリダイレクト先が非 http へ逃げると検証を迂回できるため。
        //  なお内部ホストへのリダイレクトは依然あり得る — README の信頼前提を参照。)
        let redirect = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if matches!(attempt.url().scheme(), "http" | "https") {
                attempt.follow()
            } else {
                attempt.error("redirect to non-http(s) scheme")
            }
        });

        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .redirect(redirect)
            .build()
            .map_err(|e| ToolError::Other(format!("building HTTP client: {e}")))?;

        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => return Ok(ToolOutput::error(format!("WebFetch: request failed: {e}"))),
        };

        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => return Ok(ToolOutput::error(format!("WebFetch: reading body: {e}"))),
        };

        let text = if content_type.contains("html") || looks_like_html(&body) {
            html_to_text(&body)
        } else {
            body
        };

        let (text, truncated) = truncate(&text, MAX_CHARS);
        let note = if truncated {
            format!("\n\n[truncated to {MAX_CHARS} chars]")
        } else {
            String::new()
        };

        let output = format!(
            "GET {url} -> {} ({})\n\n{text}{note}",
            status.as_u16(),
            if content_type.is_empty() {
                "unknown content-type"
            } else {
                &content_type
            }
        );

        if status.is_success() {
            Ok(ToolOutput::ok(output))
        } else {
            // 非 2xx でも本文は有用なことがあるため返すが、is_error を立てる。
            Ok(ToolOutput::error(output))
        }
    }
}

fn is_http_url(u: &str) -> bool {
    let l = u.to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

fn looks_like_html(s: &str) -> bool {
    let head = s.trim_start();
    head.starts_with("<!DOCTYPE html")
        || head.starts_with("<!doctype html")
        || head.starts_with("<html")
        || head.starts_with("<HTML")
}

/// 文字数で切り詰める。バイトではなく char 境界で切るので UTF-8 を壊さない。
fn truncate(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        return (s.to_string(), false);
    }
    (s.chars().take(max).collect(), true)
}

/// 軽量な HTML → テキスト変換。script/style ブロックを除去し、残るタグを
/// 落とし、基本的な実体参照を戻し、空行を畳む。完全な HTML パーサではない。
fn html_to_text(html: &str) -> String {
    let without_blocks = strip_blocks(html, "script");
    let without_blocks = strip_blocks(&without_blocks, "style");

    let mut out = String::with_capacity(without_blocks.len());
    let mut in_tag = false;
    for ch in without_blocks.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }

    let decoded = decode_entities(&out);
    collapse_blank_lines(&decoded)
}

/// `<tag ...> ... </tag>` のブロックを中身ごと取り除く (大小無視, 非貪欲)。
fn strip_blocks(s: &str, tag: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(s.len());
    let mut cursor = 0usize;
    while let Some(rel) = lower[cursor..].find(&open) {
        let start = cursor + rel;
        out.push_str(&s[cursor..start]);
        // 対応する閉じタグまで (なければ末尾まで) 飛ばす。
        match lower[start..].find(&close) {
            Some(end_rel) => cursor = start + end_rel + close.len(),
            None => {
                cursor = s.len();
                break;
            }
        }
    }
    out.push_str(&s[cursor..]);
    out
}

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// 連続する空白行を 1 行に畳み、各行末の空白を落とす。
fn collapse_blank_lines(s: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    let mut prev_blank = false;
    for raw in s.lines() {
        let line = raw.trim_end();
        let blank = line.trim().is_empty();
        if blank && prev_blank {
            continue;
        }
        prev_blank = blank;
        lines.push(line);
    }
    lines.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn only_http_urls_allowed() {
        assert!(is_http_url("http://x"));
        assert!(is_http_url("https://x"));
        assert!(is_http_url("HTTPS://X"));
        assert!(!is_http_url("file:///etc/passwd"));
        assert!(!is_http_url("ftp://x"));
        assert!(!is_http_url("x"));
    }

    #[test]
    fn html_is_reduced_to_text() {
        let html = "<!DOCTYPE html><html><head><style>p{color:red}</style>\
            <script>alert(1)</script></head><body><h1>Hi</h1>\
            <p>a &amp; b &lt;c&gt;</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hi"));
        assert!(text.contains("a & b <c>"));
        // script/style の中身は出ない。
        assert!(!text.contains("alert"));
        assert!(!text.contains("color:red"));
    }

    #[test]
    fn truncate_respects_char_boundary() {
        let (s, t) = truncate("héllo", 3);
        assert!(t);
        assert_eq!(s, "hél");
        let (s2, t2) = truncate("ab", 5);
        assert!(!t2);
        assert_eq!(s2, "ab");
    }

    #[tokio::test]
    async fn rejects_non_http_scheme() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let res = WebFetch
            .execute(serde_json::json!({ "url": "file:///etc/passwd" }), &ctx)
            .await;
        assert!(matches!(res, Err(ToolError::InvalidArgs(_))));
    }

    #[tokio::test]
    async fn missing_url_is_invalid_args() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let res = WebFetch.execute(serde_json::json!({}), &ctx).await;
        assert!(matches!(res, Err(ToolError::InvalidArgs(_))));
    }

    // 決定的なローカル HTTP サーバ (1 リクエストだけ応答) で実フェッチを検証する。
    #[tokio::test]
    async fn fetches_local_server_and_reduces_html() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf); // リクエスト行は読み捨て
                let body = "<html><body><p>hello &amp; bye</p></body></html>";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });

        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = WebFetch
            .execute(
                serde_json::json!({ "url": format!("http://127.0.0.1:{port}/") }),
                &ctx,
            )
            .await
            .unwrap();

        server.join().unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("-> 200"));
        assert!(out.content.contains("hello & bye"));
        assert!(!out.content.contains("<p>"));
    }

    // リダイレクト先が非 http(s) なら追従せずエラーにする (scheme 検証の迂回を防ぐ)。
    #[tokio::test]
    async fn does_not_follow_redirect_to_non_http_scheme() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let resp = "HTTP/1.1 302 Found\r\nLocation: file:///etc/passwd\r\n\
                            Content-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });

        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = WebFetch
            .execute(
                serde_json::json!({ "url": format!("http://127.0.0.1:{port}/") }),
                &ctx,
            )
            .await
            .unwrap();

        server.join().unwrap();
        // 非 http(s) への 302 は追わず、request failed として返る。
        assert!(out.is_error, "{}", out.content);
        assert!(!out.content.contains("/etc/passwd"));
    }
}
