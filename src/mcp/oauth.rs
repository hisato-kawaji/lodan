//! リモート MCP サーバの OAuth (#83)。MCP の Authorization 仕様 (2025-06-18) の範囲:
//!
//! 1. 認証なしで叩いて 401 を受け、`WWW-Authenticate` の `resource_metadata` から
//!    Protected Resource Metadata (RFC 9728) を読み、認可サーバを知る
//! 2. 認可サーバの metadata (RFC 8414 / OpenID Discovery) から各エンドポイントを知る
//! 3. 動的クライアント登録 (RFC 7591) で client_id を得る
//! 4. 認可コード + PKCE (S256)。リダイレクト先は 127.0.0.1 の一時ポート。`resource` (RFC 8707) で
//!    トークンをこのサーバに縛る
//! 5. トークンを `<config_dir>/mcp-auth/` に 0600 で保存し、期限が近い / 401 なら refresh する
//!
//! ブラウザを開くのは `lodan mcp login` だけ。セッション中 (REPL / `-p`) は保存済みのトークンを
//! 使い、更新できなければ「login し直せ」と知らせる (勝手にブラウザを開かない)。
//!
//! トークンは**サーバの URL ごと**に保存する (名前ではなく)。`.mcp.json` の名前を別のホストに
//! 付け替えても、前のサーバのトークンをそちらへ送らない。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};

/// ブラウザでの承認を待つ上限。
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// discovery / 登録 / トークンの各リクエストの上限。
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// 期限のこれだけ前から更新する (送った直後に切れるのを避ける)。
const REFRESH_MARGIN_SECS: u64 = 60;
const CALLBACK_PATH: &str = "/callback";

/// 保存するトークン一式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    /// このトークンを使ってよいサーバ (`.mcp.json` の `url` そのまま)。
    pub server_url: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    pub token_endpoint: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// 期限 (UNIX 秒)。サーバが `expires_in` を返さなければ None (401 まで使う)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

impl Credentials {
    /// 期限切れか、切れかけているか。
    pub fn expiring(&self, now: u64) -> bool {
        self.expires_at
            .is_some_and(|t| t <= now.saturating_add(REFRESH_MARGIN_SECS))
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// トークンの置き場 (`<config_dir>/mcp-auth/<URL のハッシュ>.json`)。
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn default_location() -> Option<Self> {
        directories::ProjectDirs::from("", "", "lodan")
            .map(|d| Self::at(d.config_dir().join("mcp-auth")))
    }

    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path_for(&self, server_url: &str) -> PathBuf {
        let digest = ring::digest::digest(&ring::digest::SHA256, server_url.as_bytes());
        let hex: String = digest.as_ref()[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        self.dir.join(format!("{hex}.json"))
    }

    /// 保存済みのトークン。中身の `server_url` が違えば (ハッシュの衝突・手で置いたファイル) 使わない。
    pub fn load(&self, server_url: &str) -> Option<Credentials> {
        let text = std::fs::read_to_string(self.path_for(server_url)).ok()?;
        let creds: Credentials = serde_json::from_str(&text).ok()?;
        (creds.server_url == server_url).then_some(creds)
    }

    /// 本人だけが読める形で、途中で落ちても前の内容が残るように書く。
    pub fn save(&self, creds: &Credentials) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        restrict(&self.dir, 0o700);
        let path = self.path_for(&creds.server_url);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(creds)? + "\n")
            .with_context(|| format!("writing {}", tmp.display()))?;
        restrict(&tmp, 0o600);
        std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// 消したら true (元から無ければ false)。
    pub fn remove(&self, server_url: &str) -> Result<bool> {
        let path = self.path_for(server_url);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) {}

/// 認可サーバのエンドポイント。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthServer {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
}

/// `WWW-Authenticate: Bearer resource_metadata="…"` の値。
pub fn resource_metadata_url(www_authenticate: &str) -> Option<String> {
    let rest = &www_authenticate[www_authenticate.find("resource_metadata=")? + 18..];
    let value = match rest.strip_prefix('"') {
        Some(quoted) => &quoted[..quoted.find('"')?],
        None => rest.split([',', ' ']).next()?,
    };
    (!value.is_empty()).then(|| value.to_string())
}

/// `.well-known/<name>` の候補。パスつきなら RFC 8414 の「間に挟む」形を先に、次にルート。
fn well_known(base: &url::Url, name: &str) -> Vec<url::Url> {
    let path = base.path().trim_end_matches('/');
    let mut out = Vec::new();
    if !path.is_empty() {
        let mut u = base.clone();
        u.set_path(&format!("/.well-known/{name}{path}"));
        u.set_query(None);
        out.push(u);
    }
    let mut u = base.clone();
    u.set_path(&format!("/.well-known/{name}"));
    u.set_query(None);
    out.push(u);
    out
}

/// https か、loopback の http だけを許す (OAuth 2.1。トークンや認可コードを平文で外へ流さない)。
fn require_secure(what: &str, raw: &str) -> Result<url::Url> {
    let u = url::Url::parse(raw).with_context(|| format!("{what} is not a URL: {raw}"))?;
    let loopback = matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    match u.scheme() {
        "https" => Ok(u),
        "http" if loopback => Ok(u),
        _ => bail!("{what} must use https: {raw}"),
    }
}

async fn get_json(http: &reqwest::Client, url: &url::Url) -> Option<serde_json::Value> {
    let resp = http
        .get(url.clone())
        .header("Accept", "application/json")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

/// サーバ (`server_url`) の認可サーバを探す。`www_authenticate` は 401 の応答ヘッダ。
pub async fn discover(
    http: &reqwest::Client,
    server_url: &str,
    www_authenticate: Option<&str>,
) -> Result<AuthServer> {
    let server = url::Url::parse(server_url).context("the MCP server url")?;
    // 1. Protected Resource Metadata → 認可サーバ。無ければサーバのオリジンを認可サーバとみなす
    //    (2025-03-26 版の仕様の後方互換)。
    let mut candidates = Vec::new();
    if let Some(u) = www_authenticate.and_then(resource_metadata_url) {
        candidates.push(require_secure("resource_metadata", &u)?);
    }
    candidates.extend(well_known(&server, "oauth-protected-resource"));
    let mut issuer = None;
    for c in &candidates {
        if let Some(meta) = get_json(http, c).await
            && let Some(first) = meta["authorization_servers"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
        {
            issuer = Some(require_secure("authorization server", first)?);
            break;
        }
    }
    let issuer = match issuer {
        Some(i) => i,
        None => {
            let mut origin = server.clone();
            origin.set_path("/");
            origin.set_query(None);
            origin
        }
    };
    // 2. 認可サーバの metadata。
    let mut meta_urls = well_known(&issuer, "oauth-authorization-server");
    meta_urls.extend(well_known(&issuer, "openid-configuration"));
    if let Ok(appended) = issuer.join(&format!(
        "{}/.well-known/openid-configuration",
        issuer.path().trim_end_matches('/')
    )) {
        meta_urls.push(appended);
    }
    for u in &meta_urls {
        let Some(meta) = get_json(http, u).await else {
            continue;
        };
        let field = |k: &str| meta[k].as_str().map(str::to_string);
        let (Some(authorization_endpoint), Some(token_endpoint)) =
            (field("authorization_endpoint"), field("token_endpoint"))
        else {
            continue;
        };
        if let Some(methods) = meta["code_challenge_methods_supported"].as_array()
            && !methods.iter().any(|m| m == "S256")
        {
            bail!("the authorization server does not support PKCE with S256");
        }
        require_secure("authorization_endpoint", &authorization_endpoint)?;
        require_secure("token_endpoint", &token_endpoint)?;
        let registration_endpoint = field("registration_endpoint");
        if let Some(r) = &registration_endpoint {
            require_secure("registration_endpoint", r)?;
        }
        return Ok(AuthServer {
            authorization_endpoint,
            token_endpoint,
            registration_endpoint,
        });
    }
    bail!(
        "could not find the authorization server metadata for {} (tried {})",
        server_url,
        meta_urls
            .iter()
            .map(url::Url::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// URL に載せても安全な乱数 (base64url、パディング無し)。
fn random_token(bytes: usize) -> Result<String> {
    use ring::rand::SecureRandom;
    let mut buf = vec![0u8; bytes];
    ring::rand::SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| anyhow!("the system random number generator failed"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf))
}

/// PKCE の verifier と、その S256 challenge。
pub fn pkce_challenge(verifier: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_ref())
}

/// 動的クライアント登録。返すのは (client_id, client_secret)。
async fn register(
    http: &reqwest::Client,
    endpoint: &str,
    redirect_uri: &str,
) -> Result<(String, Option<String>)> {
    let resp = http
        .post(endpoint)
        .json(&serde_json::json!({
            "client_name": "lodan",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        }))
        .send()
        .await
        .context("client registration")?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    let Some(client_id) = body["client_id"].as_str().filter(|_| status.is_success()) else {
        bail!(
            "client registration failed (HTTP {status}): {}",
            oauth_error(&body)
        );
    };
    Ok((
        client_id.to_string(),
        body["client_secret"].as_str().map(str::to_string),
    ))
}

/// 認可サーバのエラー応答の要約 (`error` / `error_description` だけ。他の値は出さない)。
fn oauth_error(body: &serde_json::Value) -> String {
    let error = body["error"].as_str().unwrap_or("unknown error");
    let text = match body["error_description"].as_str() {
        Some(d) => format!("{error}: {d}"),
        None => error.to_string(),
    };
    crate::term::sanitize(&text).into_owned()
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

async fn token_request(
    http: &reqwest::Client,
    endpoint: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse> {
    let resp = http
        .post(endpoint)
        .header("Accept", "application/json")
        .form(form)
        .send()
        .await
        .context("token request")?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "the token endpoint refused (HTTP {status}): {}",
            oauth_error(&body)
        );
    }
    serde_json::from_value(body).context("decoding the token response")
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        // リダイレクトの先で認可コードやトークンを別のホストへ送らない。
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building HTTP client")
}

/// refresh token で更新する。refresh token が無ければエラー。
pub async fn refresh(creds: &Credentials) -> Result<Credentials> {
    let Some(refresh_token) = creds.refresh_token.as_deref() else {
        bail!("no refresh token");
    };
    let http = http_client()?;
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", creds.client_id.as_str()),
        ("resource", creds.server_url.as_str()),
    ];
    if let Some(secret) = creds.client_secret.as_deref() {
        form.push(("client_secret", secret));
    }
    let t = token_request(&http, &creds.token_endpoint, &form).await?;
    Ok(Credentials {
        access_token: t.access_token,
        // 回転しないサーバは refresh token を返さない。そのときは前のものを使い続ける。
        refresh_token: t.refresh_token.or_else(|| creds.refresh_token.clone()),
        expires_at: t.expires_in.map(|s| now_secs() + s),
        ..creds.clone()
    })
}

/// ブラウザで承認してもらい、トークンを得て保存する。`open` は承認ページの URL を受け取る
/// (ブラウザを開く・URL を表示する)。
pub async fn login(
    server_url: &str,
    store: &Store,
    open: impl FnOnce(&str),
) -> Result<Credentials> {
    // トークンを平文で外へ流さない (セッション中もこの URL へ Bearer を付けて送る)。
    require_secure("the MCP server url", server_url)?;
    let http = http_client()?;
    // 認証なしで initialize を送り、401 と WWW-Authenticate を受ける。
    let probe = http
        .post(server_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(
            serde_json::json!({
                "jsonrpc": "2.0", "id": 0, "method": "initialize",
                "params": {
                    "protocolVersion": crate::mcp::protocol::PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "lodan", "version": env!("CARGO_PKG_VERSION") },
                },
            })
            .to_string(),
        )
        .send()
        .await
        .with_context(|| format!("connecting to {server_url}"))?;
    if probe.status() != reqwest::StatusCode::UNAUTHORIZED {
        bail!(
            "the server did not ask for authorization (HTTP {}); nothing to log in to",
            probe.status()
        );
    }
    let www_authenticate = probe
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let auth = discover(&http, server_url, www_authenticate.as_deref()).await?;
    let Some(registration) = auth.registration_endpoint.as_deref() else {
        bail!(
            "the authorization server does not support dynamic client registration, which lodan \
             needs to log in"
        );
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("opening the local callback port")?;
    let redirect_uri = format!(
        "http://127.0.0.1:{}{CALLBACK_PATH}",
        listener.local_addr()?.port()
    );
    let (client_id, client_secret) = register(&http, registration, &redirect_uri).await?;

    let verifier = random_token(32)?;
    let state = random_token(16)?;
    let mut authorize = url::Url::parse(&auth.authorization_endpoint)?;
    authorize
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("code_challenge", &pkce_challenge(&verifier))
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("resource", server_url);
    open(authorize.as_str());

    let code = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_code(&listener, &state))
        .await
        .map_err(|_| {
            anyhow!(
                "timed out after {}s waiting for the browser",
                LOGIN_TIMEOUT.as_secs()
            )
        })??;

    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("client_id", client_id.as_str()),
        ("code_verifier", verifier.as_str()),
        ("resource", server_url),
    ];
    if let Some(secret) = client_secret.as_deref() {
        form.push(("client_secret", secret));
    }
    let t = token_request(&http, &auth.token_endpoint, &form).await?;
    let creds = Credentials {
        server_url: server_url.to_string(),
        client_id,
        client_secret,
        token_endpoint: auth.token_endpoint,
        access_token: t.access_token,
        refresh_token: t.refresh_token,
        expires_at: t.expires_in.map(|s| now_secs() + s),
    };
    store.save(&creds)?;
    Ok(creds)
}

/// ローカルのコールバックで認可コードを受ける。`state` が合わない要求には答えて待ち続ける
/// (別のページが叩いた可能性がある。止めるとログインを妨害できてしまう)。
async fn wait_for_code(listener: &tokio::net::TcpListener, state: &str) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let (mut sock, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        let mut len = 0;
        // 要求行とヘッダだけ読めればよい (本文は来ない)。
        while len < buf.len() {
            let n = sock.read(&mut buf[len..]).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            len += n;
            if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buf[..len]);
        let target = head
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("GET "))
            .and_then(|l| l.split(' ').next())
            .unwrap_or("");
        let Ok(u) = url::Url::parse(&format!("http://127.0.0.1{target}")) else {
            respond(&mut sock, "400 Bad Request", "Bad request.").await;
            continue;
        };
        if u.path() != CALLBACK_PATH {
            respond(&mut sock, "404 Not Found", "Not found.").await;
            continue;
        }
        let param = |k: &str| {
            u.query_pairs()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.into_owned())
        };
        if param("state").as_deref() != Some(state) {
            respond(
                &mut sock,
                "400 Bad Request",
                "State mismatch. Try logging in again.",
            )
            .await;
            continue;
        }
        if let Some(error) = param("error") {
            respond(
                &mut sock,
                "400 Bad Request",
                "Authorization was denied. You can close this tab.",
            )
            .await;
            let detail = param("error_description").unwrap_or_default();
            bail!(
                "authorization was denied: {}",
                crate::term::sanitize(format!("{error} {detail}").trim())
            );
        }
        let Some(code) = param("code") else {
            respond(&mut sock, "400 Bad Request", "No authorization code.").await;
            continue;
        };
        respond(
            &mut sock,
            "200 OK",
            "lodan is authorized. You can close this tab.",
        )
        .await;
        let _ = sock.shutdown().await;
        return Ok(code);
    }
}

async fn respond(sock: &mut tokio::net::TcpStream, status: &str, text: &str) {
    use tokio::io::AsyncWriteExt;
    let body = format!("<!doctype html><title>lodan</title><p>{text}</p>\n");
    let _ = sock
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await;
}

/// 承認ページを開く。`$BROWSER` があればそれ、無ければ OS の既定 (`open` / `xdg-open`)。
/// 開けなくても URL は必ず表示する (SSH 越しなどで手で開けるように)。
pub fn open_in_browser(url: &str) {
    eprintln!("Open this URL in your browser to authorize lodan:\n\n  {url}\n");
    let mut cmd = match std::env::var("BROWSER")
        .ok()
        .filter(|b| !b.trim().is_empty())
    {
        Some(browser) => {
            let mut c = std::process::Command::new("sh");
            c.arg("-c")
                .arg(format!("{browser} \"$1\""))
                .arg("sh")
                .arg(url);
            c
        }
        None if cfg!(target_os = "macos") => {
            let mut c = std::process::Command::new("open");
            c.arg(url);
            c
        }
        None => {
            let mut c = std::process::Command::new("xdg-open");
            c.arg(url);
            c
        }
    };
    let _ = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_resource_metadata_url_is_read_quoted_or_bare() {
        assert_eq!(
            resource_metadata_url(
                r#"Bearer error="invalid_token", resource_metadata="https://a.example/.well-known/oauth-protected-resource""#
            )
            .as_deref(),
            Some("https://a.example/.well-known/oauth-protected-resource")
        );
        assert_eq!(
            resource_metadata_url("Bearer resource_metadata=https://a.example/m, realm=x")
                .as_deref(),
            Some("https://a.example/m")
        );
        assert_eq!(resource_metadata_url("Bearer realm=\"x\""), None);
    }

    #[test]
    fn well_known_inserts_the_path_first_then_tries_the_root() {
        let u = url::Url::parse("https://a.example/tenant/mcp?x=1").unwrap();
        let got: Vec<String> = well_known(&u, "oauth-protected-resource")
            .iter()
            .map(|u| u.to_string())
            .collect();
        assert_eq!(
            got,
            [
                "https://a.example/.well-known/oauth-protected-resource/tenant/mcp",
                "https://a.example/.well-known/oauth-protected-resource",
            ]
        );
        let root = url::Url::parse("https://a.example").unwrap();
        assert_eq!(well_known(&root, "x").len(), 1);
    }

    #[test]
    fn only_https_or_loopback_http_endpoints_are_accepted() {
        assert!(require_secure("t", "https://auth.example/token").is_ok());
        assert!(require_secure("t", "http://127.0.0.1:8080/token").is_ok());
        assert!(require_secure("t", "http://localhost/token").is_ok());
        assert!(require_secure("t", "http://auth.example/token").is_err());
        assert!(require_secure("t", "file:///etc/passwd").is_err());
    }

    /// RFC 7636 の付録 B の例。
    #[test]
    fn the_pkce_challenge_matches_the_rfc_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let a = random_token(32).unwrap();
        assert_eq!(a.len(), 43);
        assert_ne!(a, random_token(32).unwrap());
    }

    #[test]
    fn credentials_are_stored_per_url_privately_and_only_for_that_url() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("mcp-auth"));
        let creds = Credentials {
            server_url: "https://a.example/mcp".into(),
            client_id: "c".into(),
            client_secret: None,
            token_endpoint: "https://a.example/token".into(),
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at: Some(100),
        };
        store.save(&creds).unwrap();
        assert_eq!(store.load("https://a.example/mcp"), Some(creds.clone()));
        assert_eq!(store.load("https://b.example/mcp"), None);
        // 中身の URL が違うファイルは使わない (別のサーバのトークンを送らない)。
        let other = store.path_for("https://b.example/mcp");
        std::fs::copy(store.path_for("https://a.example/mcp"), &other).unwrap();
        assert_eq!(store.load("https://b.example/mcp"), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path_for("https://a.example/mcp"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(creds.expiring(100 - REFRESH_MARGIN_SECS));
        assert!(!creds.expiring(0));
        assert!(store.remove("https://a.example/mcp").unwrap());
        assert!(!store.remove("https://a.example/mcp").unwrap());
        assert_eq!(store.load("https://a.example/mcp"), None);
    }
}
