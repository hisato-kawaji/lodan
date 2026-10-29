//! MCP の OAuth (#164) の end-to-end。`lodan mcp login` を実バイナリで走らせ、認可ページを
//! 「ブラウザの代わりに」辿ってサインインし、保存されたトークンで HTTP の MCP サーバに繋ぐ。
//! トークンが失効したら refresh_token で更新して送り直すこと、別の URL のサーバには送らないことも見る。
//!
//! トークンの置き場所は HOME で決まるので、このファイルはテストを 1 本だけにして、
//! 最初にこのプロセスの HOME を隔離した場所へ向ける。

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use lodan::mcp::client::McpClient;
use lodan::mcp::config::McpServerSpec;
use lodan::mcp::oauth::TokenStore;

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start() -> (Server, u16) {
    start_in("")
}

fn start_in(mode: &str) -> (Server, u16) {
    let port = {
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        l.local_addr().unwrap().port()
    };
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock_mcp_oauth_server.py");
    let child = Command::new("python3")
        .arg(script)
        .arg(port.to_string())
        .arg(mode)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn python3");
    let server = Server(child);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return (server, port);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("mock OAuth server did not start");
}

fn spec(url: String) -> McpServerSpec {
    McpServerSpec {
        command: None,
        args: vec![],
        env: BTreeMap::new(),
        url: Some(url),
        headers: BTreeMap::new(),
        allow_sampling: false,
        trust_annotations: false,
        enabled_tools: Vec::new(),
        disabled_tools: Vec::new(),
        tool_timeout_secs: Some(15),
        max_output_bytes: None,
    }
}

#[tokio::test]
async fn login_stores_a_token_that_is_used_refreshed_and_kept_to_its_server() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: このテストバイナリのテストはこの 1 本だけで、他のスレッドはまだ環境を読んでいない。
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::remove_var("XDG_CONFIG_HOME");
    }
    let store = TokenStore::default_store().expect("config dir");
    let config_dir = store.path().parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&config_dir).unwrap();
    let (_server, port) = start();
    let base = format!("http://127.0.0.1:{port}");
    let url = format!("{base}/mcp");
    std::fs::write(
        config_dir.join("mcp.json"),
        serde_json::json!({ "mcpServers": { "srv": { "url": url } } }).to_string(),
    )
    .unwrap();

    // 1) サインインしていなければ、繋がらずに login を勧める。
    let err = McpClient::connect("srv", &spec(url.clone()), None, None)
        .await
        .err()
        .expect("unauthenticated connect must fail")
        .to_string();
    assert!(err.contains("lodan mcp login srv"), "{err}");

    // 2) `lodan mcp login srv`。表示された URL をブラウザの代わりに辿る。
    let mut child = Command::new(env!("CARGO_BIN_EXE_lodan"))
        .args(["mcp", "login", "srv"])
        .current_dir(home.path())
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lodan");
    let stderr = child.stderr.take().unwrap();
    let auth_url = BufReader::new(stderr)
        .lines()
        .map_while(Result::ok)
        .find_map(|l| l.trim().starts_with("http").then(|| l.trim().to_string()))
        .expect("the authorization URL is printed");
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let redirect = http.get(&auth_url).send().await.unwrap();
    assert_eq!(redirect.status(), 302, "{:?}", redirect.text().await);
    let callback = redirect.headers()["location"].to_str().unwrap().to_string();
    assert!(callback.starts_with("http://127.0.0.1:"), "{callback}");
    let page = http
        .get(&callback)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("signed in"), "{page}");
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("signed in to srv"));

    // 3) トークンは 0600 で、そのサーバの URL で保存されている。
    let first = store.get(&url).expect("stored token");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // 4) 保存したトークンで繋がる。
    let client = McpClient::connect("srv", &spec(url.clone()), None, None)
        .await
        .expect("connect with the stored token");
    assert_eq!(client.list_tools().await.unwrap()[0].name, "echo");

    // 5) 失効させると、refresh_token で更新して送り直す。
    http.post(format!("{base}/admin/revoke"))
        .send()
        .await
        .unwrap();
    assert_eq!(client.list_tools().await.unwrap()[0].name, "echo");
    let second = store.get(&url).unwrap();
    assert_ne!(second.access_token, first.access_token, "refreshed");

    // 6) 別の URL のサーバには、トークンを送らない。
    let other = McpClient::connect("other", &spec(format!("{base}/other")), None, None).await;
    assert!(other.is_err());
    let seen: BTreeMap<String, Vec<Option<String>>> = http
        .get(format!("{base}/admin/seen"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        seen["/other"].iter().all(Option::is_none),
        "a token leaked to another URL: {seen:?}"
    );

    // 7) logout で消える。
    let out = Command::new(env!("CARGO_BIN_EXE_lodan"))
        .args(["mcp", "logout", "srv"])
        .current_dir(home.path())
        .env_clear()
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("signed out of srv"));
    assert!(store.get(&url).is_none());

    // 8) メタデータの食い違いは断る (別の #[tokio::test] にすると、上の set_var と並行して
    //    環境変数を読むことになるので、同じテストの中で行う)。
    mismatched_metadata_is_refused_before_any_sign_in().await;
}

/// 認可サーバのメタデータの issuer が取りに行った先と違う / 保護リソースのメタデータが別の
/// リソースのものなら、サインインに進まない (RFC 8414 / 9728。#165 のレビュー)。
async fn mismatched_metadata_is_refused_before_any_sign_in() {
    for (mode, expected) in [
        ("bad-issuer", "issuer"),
        ("bad-resource", "elsewhere.example"),
    ] {
        let (_server, port) = start_in(mode);
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::at(dir.path().join("t.json"));
        let opened = std::sync::atomic::AtomicBool::new(false);
        let err =
            lodan::mcp::oauth::login(&format!("http://127.0.0.1:{port}/mcp"), &store, &|_| {
                opened.store(true, std::sync::atomic::Ordering::SeqCst)
            })
            .await
            .err()
            .unwrap_or_else(|| panic!("{mode}: login must fail"))
            .to_string();
        assert!(err.contains(expected), "{mode}: {err}");
        assert!(
            !opened.load(std::sync::atomic::Ordering::SeqCst),
            "{mode}: no browser"
        );
        assert!(!store.path().exists());
    }
}
