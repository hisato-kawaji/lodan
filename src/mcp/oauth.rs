//! HTTP の MCP サーバの OAuth 2.1 認可 (#164)。MCP authorization 仕様 (2025-06-18) に沿う。
//!
//! - 探索: 401 の `WWW-Authenticate` の `resource_metadata` (無ければ `/.well-known/oauth-protected-resource`)
//!   → 認可サーバのメタデータ (RFC 8414、だめなら OpenID の discovery)
//! - 登録: `registration_endpoint` があれば動的クライアント登録 (RFC 7591、公開クライアント)
//! - 認可: PKCE (S256) + `state` + `resource` (RFC 8707)。`127.0.0.1` の空きポートで待ち受ける
//! - 保存: `<config_dir>/mcp-tokens.json` (0600)。**発行先のサーバ URL をキーにし、そのサーバにだけ送る**
//!
//! 認可サーバのエンドポイントは https に限る (loopback だけはテスト用に許す)。トークンを平文で
//! 流させない。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};

/// 利用者がブラウザで許可するまで待つ時間。
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// 期限のこれだけ前から、期限切れとみなして先に更新する。
const EXPIRY_MARGIN_SECS: u64 = 30;

/// 保存しているトークン 1 件 (サーバ URL ごと)。`Debug` はトークンを伏せる。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// 期限 (UNIX 秒)。無ければ期限を知らない (401 が来たら更新する)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    pub token_endpoint: String,
    pub client_id: String,
}

impl std::fmt::Debug for StoredToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredToken")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at", &self.expires_at)
            .field("token_endpoint", &self.token_endpoint)
            .field("client_id", &self.client_id)
            .finish()
    }
}

impl StoredToken {
    fn expired(&self, now: u64) -> bool {
        self.expires_at
            .is_some_and(|at| now + EXPIRY_MARGIN_SECS >= at)
    }
}

/// このプロセスの中で、読み → 足し → 書きを 1 つずつにする (POST と GET のストリームが同時に
/// 更新すると、片方の書き込みが消える)。プロセスをまたぐ同時書き込みは扱わない。
static STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// 更新 (refresh) を 1 つずつにする。refresh_token を使い捨てにする認可サーバでは、同時に 2 回
/// 更新すると後の方が失敗する。
static REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `mcp-tokens.json`。中身は `{ "<server url>": StoredToken }`。
#[derive(Debug, Clone)]
pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    /// 既定の場所 (`mcp.json` と同じディレクトリ)。
    pub fn default_store() -> Option<Self> {
        directories::ProjectDirs::from("", "", "lodan").map(|d| Self {
            path: d.config_dir().join("mcp-tokens.json"),
        })
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 読めない (壊れた) ファイルは Err。空として扱うと、次の保存で他のサーバのトークンを消してしまう。
    fn load(&self) -> Result<BTreeMap<String, StoredToken>> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("{} is not valid JSON", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    pub fn get(&self, server_url: &str) -> Option<StoredToken> {
        self.load().ok()?.remove(server_url)
    }

    pub fn put(&self, server_url: &str, token: StoredToken) -> Result<()> {
        let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.load()?;
        all.insert(server_url.to_string(), token);
        self.save(&all)
    }

    /// 消したら true。
    pub fn remove(&self, server_url: &str) -> Result<bool> {
        let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.load()?;
        let had = all.remove(server_url).is_some();
        if had {
            self.save(&all)?;
        }
        Ok(had)
    }

    fn save(&self, all: &BTreeMap<String, StoredToken>) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        // 別名で書いてから差し替える (書きかけを残さない)。別名のファイルは作る時点で 0600 にする
        // (書いてから chmod すると、umask 次第で他人に読める瞬間ができる)。
        let tmp = self.path.with_extension("json.tmp");
        let _ = std::fs::remove_file(&tmp);
        write_private(&tmp, (serde_json::to_string_pretty(all)? + "\n").as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }
}

/// 新しいファイルを本人だけが読める形で作って書く (既にあれば失敗する)。
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_loopback(u: &reqwest::Url) -> bool {
    let host = u.host_str().unwrap_or("");
    host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// 使ってよい URL か: https、または `allow_loopback` のときだけ loopback の http。
/// loopback の例外は MCP サーバ自身が loopback のとき (手元のテスト・開発) に限る — 遠くのサーバの
/// メタデータが、こちらの機械の中の URL を叩かせられないように (#165 のレビュー)。
fn check_endpoint(url: &str, what: &str, allow_loopback: bool) -> Result<reqwest::Url> {
    let u = reqwest::Url::parse(url).with_context(|| format!("{what}: not a URL: {url}"))?;
    if u.scheme() != "https" && !(u.scheme() == "http" && allow_loopback && is_loopback(&u)) {
        bail!("{what} must use https (got {url})");
    }
    Ok(u)
}

/// このサーバ URL にトークンを送ってよいか (https、または loopback の http)。平文で遠くへ
/// Bearer を流さない (#165 のレビュー)。
pub fn may_send_token(server_url: &str) -> bool {
    check_endpoint(server_url, "MCP server", true).is_ok()
}

/// 認可に要るエンドポイント。
#[derive(Debug, Clone)]
struct AuthServer {
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    scopes: Vec<String>,
}

#[derive(Deserialize)]
struct ProtectedResource {
    /// このメタデータが述べている保護リソース (RFC 9728)。あれば、頼んだサーバの URL と一致させる。
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

#[derive(Deserialize)]
struct AuthServerMetadata {
    /// RFC 8414: メタデータを取りに行った issuer と一致しなければ使わない。
    #[serde(default)]
    issuer: Option<String>,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
}

/// `WWW-Authenticate: Bearer resource_metadata="…"` から URL を取り出す。
fn resource_metadata_from(header: &str) -> Option<String> {
    let at = header.find("resource_metadata=")? + "resource_metadata=".len();
    let rest = &header[at..];
    let value = match rest.strip_prefix('"') {
        Some(quoted) => &quoted[..quoted.find('"')?],
        None => rest.split([',', ' ']).next()?,
    };
    (!value.is_empty()).then(|| value.to_string())
}

/// `<origin>/.well-known/<suffix><path>` の形 (RFC 8414 / 9728 のパス挿入)。パスが空なら付けない。
fn well_known(base: &reqwest::Url, suffix: &str) -> Vec<String> {
    let origin = base.origin().ascii_serialization();
    let path = base.path().trim_end_matches('/');
    let mut out = Vec::new();
    if !path.is_empty() {
        out.push(format!("{origin}/.well-known/{suffix}{path}"));
    }
    out.push(format!("{origin}/.well-known/{suffix}"));
    out
}

async fn get_json<T: for<'de> Deserialize<'de>>(http: &reqwest::Client, url: &str) -> Option<T> {
    let resp = http
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

async fn discover(http: &reqwest::Client, server_url: &str) -> Result<AuthServer> {
    let server = check_endpoint(server_url, "the MCP server url", true)?;
    let local = is_loopback(&server);
    // 未認証の initialize が 401 なら、そのヘッダが保護リソースのメタデータの場所を教える。
    let probe = http
        .post(server_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"lodan","version":"0"}}}"#)
        .send()
        .await
        .with_context(|| format!("contacting {server_url}"))?;
    let hinted = probe
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .and_then(resource_metadata_from);
    // ヘッダはサーバが言うことなので、取りに行く先も https (か loopback) に限る。
    let mut candidates: Vec<String> = hinted
        .filter(|u| check_endpoint(u, "resource_metadata", local).is_ok())
        .into_iter()
        .collect();
    candidates.extend(well_known(&server, "oauth-protected-resource"));
    let mut prm = None;
    for url in &candidates {
        if let Some(found) = get_json::<ProtectedResource>(http, url).await {
            prm = Some(found);
            break;
        }
    }
    let prm = prm.ok_or_else(|| {
        anyhow!(
            "{server_url} does not publish OAuth protected-resource metadata (tried {candidates:?})"
        )
    })?;
    // 別のリソースのメタデータ (別サーバのものを指された) は使わない。
    if let Some(resource) = &prm.resource
        && resource.trim_end_matches('/') != server_url.trim_end_matches('/')
    {
        bail!("protected-resource metadata is for {resource}, not {server_url}");
    }
    let issuer = prm
        .authorization_servers
        .first()
        .ok_or_else(|| anyhow!("protected-resource metadata names no authorization server"))?;
    let issuer_url = check_endpoint(issuer, "authorization server", local)?;
    let mut tried = well_known(&issuer_url, "oauth-authorization-server");
    tried.extend(well_known(&issuer_url, "openid-configuration"));
    tried.push(format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    ));
    let mut meta = None;
    for url in &tried {
        if let Some(found) = get_json::<AuthServerMetadata>(http, url).await {
            meta = Some(found);
            break;
        }
    }
    let meta = meta.ok_or_else(|| {
        anyhow!("no authorization server metadata for {issuer} (tried {tried:?})")
    })?;
    if meta.issuer.as_deref().map(|i| i.trim_end_matches('/')) != Some(issuer.trim_end_matches('/'))
    {
        bail!(
            "authorization server metadata names issuer {:?}, expected {issuer}",
            meta.issuer
        );
    }
    // PKCE (S256) を広告していない認可サーバには進まない (仕様: 確かめられなければ断る)。
    if !meta
        .code_challenge_methods_supported
        .iter()
        .any(|m| m == "S256")
    {
        bail!("the authorization server {issuer} does not advertise PKCE S256");
    }
    check_endpoint(
        &meta.authorization_endpoint,
        "authorization_endpoint",
        local,
    )?;
    check_endpoint(&meta.token_endpoint, "token_endpoint", local)?;
    if let Some(r) = &meta.registration_endpoint {
        check_endpoint(r, "registration_endpoint", local)?;
    }
    Ok(AuthServer {
        authorization_endpoint: meta.authorization_endpoint,
        token_endpoint: meta.token_endpoint,
        registration_endpoint: meta.registration_endpoint,
        scopes: prm.scopes_supported,
    })
}

fn random_b64(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).map_err(|e| anyhow!("no randomness: {e}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf))
}

/// PKCE の `code_challenge` (S256)。
fn challenge_for(verifier: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_ref())
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

async fn register(http: &reqwest::Client, endpoint: &str, redirect_uri: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Registered {
        client_id: String,
    }
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
        .context("dynamic client registration")?;
    if !resp.status().is_success() {
        bail!("dynamic client registration failed: HTTP {}", resp.status());
    }
    Ok(resp.json::<Registered>().await?.client_id)
}

/// 認可コードの届け先 (`/callback`) で待つ。`state` が違うものは受け取らずに待ち続ける
/// (同じ機械の誰かが偽の戻りを送っても、サインインを潰せない)。`error` も `state` が合うときだけ受ける。
async fn wait_for_code(listener: tokio::net::TcpListener, state: &str) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let (mut sock, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        // 何も送らずに繋ぎっぱなしの接続で、待ち受けを塞がせない。
        let n = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf))
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]);
        let target = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("");
        // favicon など関係ないリクエストは捨てて待ち続ける。
        let Some(query) = target.strip_prefix("/callback") else {
            let _ = sock
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            continue;
        };
        let url = reqwest::Url::parse(&format!("http://127.0.0.1/callback{query}"))?;
        let params: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
        if params.get("state").map(String::as_str) != Some(state) {
            let body = "lodan: this sign-in link does not match the one lodan opened.";
            let _ = sock
                .write_all(
                    format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
            eprintln!("mcp: ignored an OAuth callback with the wrong state");
            continue;
        }
        let outcome = if let Some(err) = params.get("error") {
            Err(anyhow!("the authorization server refused: {err}"))
        } else {
            params
                .get("code")
                .cloned()
                .ok_or_else(|| anyhow!("the OAuth callback has no code"))
        };
        let body = match &outcome {
            Ok(_) => "lodan: signed in. You can close this tab.",
            Err(_) => "lodan: sign-in failed. See the terminal.",
        };
        let _ = sock
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await;
        return outcome;
    }
}

/// サインインする。`open` は認可 URL を受け取って利用者に開かせる (ブラウザを起動する / 表示する)。
pub async fn login(server_url: &str, store: &TokenStore, open: &dyn Fn(&str)) -> Result<()> {
    // 保存先が壊れていたら、ブラウザでの手順の前に止める (最後に保存できずに終わらない)。
    store.load()?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let auth = discover(&http, server_url).await?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let redirect_uri = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr()?.port()
    );
    let client_id = match &auth.registration_endpoint {
        Some(endpoint) => register(&http, endpoint, &redirect_uri).await?,
        None => bail!(
            "the authorization server has no registration endpoint; lodan needs dynamic client registration"
        ),
    };
    let verifier = random_b64(32)?;
    let state = random_b64(16)?;
    let mut url = reqwest::Url::parse(&auth.authorization_endpoint)?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("code_challenge", &challenge_for(&verifier))
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("resource", server_url);
        if !auth.scopes.is_empty() {
            q.append_pair("scope", &auth.scopes.join(" "));
        }
    }
    open(url.as_str());
    let code = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_code(listener, &state))
        .await
        .map_err(|_| anyhow!("timed out waiting for the browser sign-in"))??;
    let resp = http
        .post(&auth.token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", client_id.as_str()),
            ("code_verifier", verifier.as_str()),
            ("resource", server_url),
        ])
        .send()
        .await
        .context("token request")?;
    if !resp.status().is_success() {
        bail!("token request failed: HTTP {}", resp.status());
    }
    let token: TokenResponse = resp.json().await.context("decoding the token response")?;
    store.put(
        server_url,
        StoredToken {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_at: token.expires_in.map(|s| now_secs() + s),
            token_endpoint: auth.token_endpoint,
            client_id,
        },
    )
}

/// refresh_token で更新して保存し、新しいトークンを返す。refresh_token が無ければ Err。
///
/// `used` は呼び出し側が使って断られたアクセストークン。待っている間に別の呼び出しが更新して
/// いたら (保存されているものが `used` と違えば)、更新せずにそれを返す。
pub async fn refresh(
    server_url: &str,
    store: &TokenStore,
    used: Option<&str>,
) -> Result<StoredToken> {
    let _guard = REFRESH_LOCK.lock().await;
    let old = store
        .get(server_url)
        .ok_or_else(|| anyhow!("not signed in to {server_url}"))?;
    if used.is_some_and(|u| u != old.access_token) {
        return Ok(old);
    }
    let refresh_token = old
        .refresh_token
        .clone()
        .ok_or_else(|| anyhow!("the token for {server_url} cannot be refreshed"))?;
    let local = reqwest::Url::parse(server_url).is_ok_and(|u| is_loopback(&u));
    check_endpoint(&old.token_endpoint, "token_endpoint", local)?;
    // リダイレクトはたどらない: 307 は本文 (refresh_token) ごと転送先へ送り直すので、
    // https の確認を素通りして別の場所へ渡してしまう (#165 のレビュー)。
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let resp = http
        .post(&old.token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", old.client_id.as_str()),
            ("resource", server_url),
        ])
        .send()
        .await
        .context("token refresh")?;
    if !resp.status().is_success() {
        bail!("token refresh failed: HTTP {}", resp.status());
    }
    let token: TokenResponse = resp.json().await.context("decoding the token response")?;
    let fresh = StoredToken {
        access_token: token.access_token,
        // 新しい refresh_token が来なければ、前のものを使い続ける。
        refresh_token: token.refresh_token.or(Some(refresh_token)),
        expires_at: token.expires_in.map(|s| now_secs() + s),
        ..old
    };
    store.put(server_url, fresh.clone())?;
    Ok(fresh)
}

/// 送る前に使うトークン。期限が近ければ先に更新する (更新に失敗したら古いものを返し、401 に任せる)。
pub async fn current(server_url: &str, store: &TokenStore) -> Option<StoredToken> {
    if !may_send_token(server_url) {
        return None;
    }
    let token = store.get(server_url)?;
    if token.expired(now_secs()) && token.refresh_token.is_some() {
        return Some(
            refresh(server_url, store, Some(&token.access_token))
                .await
                .unwrap_or(token),
        );
    }
    Some(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn the_pkce_challenge_is_the_rfc_7636_example() {
        // RFC 7636 Appendix B。
        assert_eq!(
            challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let a = random_b64(32).unwrap();
        assert_eq!(a.len(), 43);
        assert_ne!(a, random_b64(32).unwrap());
    }

    #[test]
    fn resource_metadata_is_read_from_the_challenge_header() {
        assert_eq!(
            resource_metadata_from(
                r#"Bearer realm="mcp", resource_metadata="https://ex.com/.well-known/oauth-protected-resource""#
            )
            .as_deref(),
            Some("https://ex.com/.well-known/oauth-protected-resource")
        );
        assert_eq!(
            resource_metadata_from("Bearer resource_metadata=https://ex.com/m, error=x").as_deref(),
            Some("https://ex.com/m")
        );
        assert!(resource_metadata_from("Bearer realm=\"x\"").is_none());
    }

    #[test]
    fn well_known_urls_insert_the_path_after_the_host() {
        let u = reqwest::Url::parse("https://as.example.com/tenant1").unwrap();
        assert_eq!(
            well_known(&u, "oauth-authorization-server"),
            vec![
                "https://as.example.com/.well-known/oauth-authorization-server/tenant1".to_string(),
                "https://as.example.com/.well-known/oauth-authorization-server".to_string(),
            ]
        );
        let root = reqwest::Url::parse("https://as.example.com/").unwrap();
        assert_eq!(well_known(&root, "x").len(), 1);
    }

    #[test]
    fn only_https_or_loopback_endpoints_are_used() {
        assert!(check_endpoint("https://as.example.com/token", "t", false).is_ok());
        assert!(check_endpoint("http://127.0.0.1:9/token", "t", true).is_ok());
        assert!(check_endpoint("http://localhost:9/token", "t", true).is_ok());
        assert!(check_endpoint("http://[::1]:9/token", "t", true).is_ok());
        // loopback の例外は、MCP サーバ自身が loopback のときだけ。
        assert!(check_endpoint("http://127.0.0.1:9/token", "t", false).is_err());
        assert!(check_endpoint("http://127.0.0.1.evil.com/token", "t", true).is_err());
        assert!(check_endpoint("http://as.example.com/token", "t", true).is_err());
        assert!(check_endpoint("ftp://as.example.com/token", "t", true).is_err());
        // 平文の遠くのサーバには Bearer を送らない。
        assert!(may_send_token("https://mcp.example.com/mcp"));
        assert!(may_send_token("http://127.0.0.1:8080/mcp"));
        assert!(!may_send_token("http://mcp.example.com/mcp"));
    }

    /// トークンは 0600 で保存し、URL ごとに分ける (別の URL のサーバには出さない)。
    #[test]
    fn tokens_are_private_and_keyed_by_the_exact_server_url() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::at(dir.path().join("sub/mcp-tokens.json"));
        let token = StoredToken {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at: Some(10),
            token_endpoint: "https://as/token".into(),
            client_id: "cid".into(),
        };
        store
            .put("https://mcp.example.com/mcp", token.clone())
            .unwrap();
        assert_eq!(store.get("https://mcp.example.com/mcp"), Some(token));
        assert!(store.get("https://evil.example.com/mcp").is_none());
        assert!(store.get("https://mcp.example.com/other").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(store.remove("https://mcp.example.com/mcp").unwrap());
        assert!(!store.remove("https://mcp.example.com/mcp").unwrap());
    }

    /// 戻ってきた `state` が違うコードは受け取らず、待ち続ける (偽の戻りでサインインを潰せない)。
    #[tokio::test]
    async fn a_callback_with_the_wrong_state_is_ignored_not_accepted() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let waiter = tokio::spawn(async move { wait_for_code(listener, "expected").await });
        let hit = |req: &'static [u8]| async move {
            let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            s.write_all(req).await.unwrap();
            let mut out = String::new();
            let _ = s.read_to_string(&mut out).await;
            out
        };
        // 何も送らない接続があっても塞がらない。
        let idle = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let forged = hit(b"GET /callback?code=evil&state=forged HTTP/1.1\r\n\r\n").await;
        assert!(forged.starts_with("HTTP/1.1 400"), "{forged}");
        let aborted = hit(b"GET /callback?error=access_denied&state=forged HTTP/1.1\r\n\r\n").await;
        assert!(aborted.starts_with("HTTP/1.1 400"), "{aborted}");
        assert!(
            !waiter.is_finished(),
            "a forged callback must not end the login"
        );
        hit(b"GET /callback?code=the-code&state=expected HTTP/1.1\r\n\r\n").await;
        drop(idle);
        assert_eq!(waiter.await.unwrap().unwrap(), "the-code");
    }

    /// 1 回だけ HTTP の応答を返すサーバ。受けた接続の数を数える。
    async fn one_shot(response: String) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = vec![0u8; 8192];
                let _ = s.read(&mut buf).await;
                let _ = s.write_all(response.as_bytes()).await;
            }
        });
        (port, hits)
    }

    fn token_at(endpoint: String, access: &str) -> StoredToken {
        StoredToken {
            access_token: access.into(),
            refresh_token: Some("SECRET_RT".into()),
            expires_at: None,
            token_endpoint: endpoint,
            client_id: "cid".into(),
        }
    }

    /// 更新はリダイレクトをたどらない: 307 は refresh_token ごと転送先へ送り直す (#165 のレビュー)。
    #[tokio::test]
    async fn a_refresh_does_not_follow_redirects_with_the_refresh_token() {
        let (thief, stolen) = one_shot(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 23\r\n\r\n{\"access_token\":\"evil\"}".into(),
        )
        .await;
        let (as_port, _) = one_shot(format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:{thief}/stolen\r\nContent-Length: 0\r\n\r\n"
        ))
        .await;
        let endpoint = format!("http://127.0.0.1:{as_port}/token");
        // 陽性対照: 既定の client なら転送先に届く (= 仕掛けが本物)。
        let _ = reqwest::Client::new()
            .post(&endpoint)
            .form(&[("refresh_token", "x")])
            .send()
            .await;
        assert_eq!(stolen.load(std::sync::atomic::Ordering::SeqCst), 1);

        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::at(dir.path().join("t.json"));
        store
            .put("https://mcp/x", token_at(endpoint, "old"))
            .unwrap();
        assert!(refresh("https://mcp/x", &store, Some("old")).await.is_err());
        assert_eq!(
            stolen.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the refresh token must not reach the redirect target"
        );
        assert_eq!(store.get("https://mcp/x").unwrap().access_token, "old");
    }

    /// 待っている間に別の呼び出しが更新していたら、もう一度は更新しない (使い捨ての refresh_token)。
    #[tokio::test]
    async fn a_token_someone_else_already_refreshed_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::at(dir.path().join("t.json"));
        // 送られれば必ず失敗する (接続できない) エンドポイント。
        store
            .put(
                "https://mcp/x",
                token_at("http://127.0.0.1:1/token".into(), "new"),
            )
            .unwrap();
        let got = refresh("https://mcp/x", &store, Some("old")).await.unwrap();
        assert_eq!(got.access_token, "new");
        assert!(refresh("https://mcp/x", &store, Some("new")).await.is_err());
    }

    /// 平文 (http) の遠くのサーバには、保存してあってもトークンを出さない。`Debug` も伏せる。
    #[tokio::test]
    async fn a_token_is_not_handed_out_for_a_plain_http_remote_server() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::at(dir.path().join("t.json"));
        let token = token_at("https://as/t".into(), "SECRET_AT");
        store
            .put("http://mcp.example.com/mcp", token.clone())
            .unwrap();
        assert!(
            current("http://mcp.example.com/mcp", &store)
                .await
                .is_none()
        );
        let shown = format!("{token:?}");
        assert!(
            !shown.contains("SECRET_AT") && !shown.contains("SECRET_RT"),
            "{shown}"
        );
    }

    /// 読めない保存ファイルは上書きしない (他のサーバのトークンを消さない)。
    #[test]
    fn a_broken_store_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.json");
        std::fs::write(&path, "{ not json").unwrap();
        let store = TokenStore::at(&path);
        assert!(
            store
                .put("https://mcp/x", token_at("https://as/t".into(), "a"))
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn a_token_near_its_expiry_counts_as_expired() {
        let t = StoredToken {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: Some(1000),
            token_endpoint: "https://as/t".into(),
            client_id: "c".into(),
        };
        assert!(!t.expired(900));
        assert!(t.expired(1000 - EXPIRY_MARGIN_SECS));
        assert!(
            !StoredToken {
                expires_at: None,
                ..t
            }
            .expired(u64::MAX - 100)
        );
    }
}
