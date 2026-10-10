//! Streamable HTTP and OAuth 2.1 against an in-process fake server on loopback: sessions, SSE
//! replies, the server push stream, discovery, dynamic registration, PKCE, refresh, revocation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use marlowe_mcp::client::ConnectOpts;
use marlowe_mcp::config::{self, Paths};
use marlowe_mcp::fake::{FakeHttp, FakeOpts, HttpOpts};
use marlowe_mcp::http::{DefaultHttp, HttpBackend, HttpRequest};
use marlowe_mcp::hub::{oauth_for, Hub, HubOpts, SlotStatus};
use marlowe_mcp::oauth::{MemoryTokens, TokenStore};
use marlowe_mcp::util::Url;
use serde_json::json;

fn tmp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("marlowe-mcp-http-{tag}-{}-{}", std::process::id(), marlowe_mcp::util::random_token(6)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn no_cancel() -> bool {
    false
}

fn hub_for(server: &FakeHttp, tokens: Arc<MemoryTokens>, extra: serde_json::Value) -> (Arc<Hub>, HubOpts) {
    let mut entry = json!({"type": "http", "url": server.url(), "consequence": "inert"});
    if let (Some(e), Some(x)) = (entry.as_object_mut(), extra.as_object()) {
        e.extend(x.clone());
    }
    let text = json!({"mcpServers": {"web": entry}}).to_string();
    let mut opts = HubOpts::new(Paths::new(&tmp("home"), None), ConnectOpts::new(tokens));
    opts.session = config::parse_session(&[text]).unwrap();
    let hub = Hub::start(opts.clone());
    assert!(hub.wait_settled(Duration::from_secs(20)));
    (hub, opts)
}

#[test]
fn a_plain_json_server_with_sessions_serves_tools_resources_and_prompts() {
    let server = FakeHttp::start(HttpOpts { sessions: true, ..Default::default() });
    let (hub, _) = hub_for(&server, Arc::new(MemoryTokens::default()), json!({}));
    assert_eq!(hub.statuses()[0].status, SlotStatus::Connected, "{:?}", hub.statuses());
    let out = hub.call("mcp__web__echo", json!({"text": "over http"}), &no_cancel).unwrap();
    assert_eq!(out.text, "over http");
    assert_eq!(hub.read_resource("web", "file:///notes.txt", &no_cancel).unwrap().text, "remember the milk");
    assert!(hub.prompts().iter().any(|(_, p)| p.name == "greet"));
    let log = server.log();
    assert!(log.iter().filter(|l| *l == "POST /mcp").count() >= 4, "{log:?}");
}

#[test]
fn an_sse_reply_server_works_the_same() {
    let server = FakeHttp::start(HttpOpts { sessions: true, sse: true, ..Default::default() });
    let (hub, _) = hub_for(&server, Arc::new(MemoryTokens::default()), json!({}));
    assert_eq!(hub.statuses()[0].status, SlotStatus::Connected, "{:?}", hub.statuses());
    assert_eq!(hub.call("mcp__web__add", json!({"a": 1, "b": 2}), &no_cancel).unwrap().text, "3");
}

#[test]
fn list_changed_arrives_on_the_push_stream() {
    let server = FakeHttp::start(HttpOpts { push_stream: true, mcp: FakeOpts { list_changed: true, ..Default::default() }, ..Default::default() });
    let (hub, _) = hub_for(&server, Arc::new(MemoryTokens::default()), json!({}));
    assert_eq!(hub.statuses()[0].status, SlotStatus::Connected);
    assert!(hub.find("mcp__web__fresh").is_none());
    // `add_tool` answers over POST; its list_changed notification goes down the GET stream.
    hub.call("mcp__web__add_tool", json!({"name": "fresh"}), &no_cancel).unwrap();
    let mut seen = false;
    for _ in 0..150 {
        if hub.find("mcp__web__fresh").is_some() {
            seen = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(seen, "the push stream did not refresh the tool list; log: {:?}", server.log());
    assert!(server.log().iter().any(|l| l == "GET /mcp"));
}

#[test]
fn plain_http_to_a_remote_host_is_a_config_error() {
    let text = json!({"mcpServers": {"r": {"type": "http", "url": "http://example.com/mcp"}}}).to_string();
    let mut opts = HubOpts::new(Paths::new(&tmp("remote"), None), ConnectOpts::new(Arc::new(MemoryTokens::default())));
    opts.session = config::parse_session(&[text]).unwrap();
    let hub = Hub::start(opts);
    assert!(hub.wait_settled(Duration::from_secs(5)));
    assert!(matches!(&hub.statuses()[0].status, SlotStatus::Failed(w) if w.contains("plain http")), "{:?}", hub.statuses());
}

/// The browser: follows the authorization URL to the fake server, then the redirect to the
/// listener Marlowe opened.
fn browser(url: &str) -> Result<(), String> {
    let http = DefaultHttp;
    let u = Url::parse(url)?;
    let resp = http.send(&HttpRequest::get(&u), Duration::from_secs(5)).map_err(|e| e.to_string())?;
    let loc = resp.header("location").ok_or("no redirect from the authorization endpoint")?.to_string();
    let target = Url::parse(&loc)?;
    // Fire the callback from another thread so the listener can accept it.
    std::thread::spawn(move || {
        let _ = DefaultHttp.send(&HttpRequest::get(&target), Duration::from_secs(5));
    });
    Ok(())
}

#[test]
fn oauth_discovers_registers_authorizes_with_pkce_and_connects() {
    let server = FakeHttp::start(HttpOpts { oauth: true, token_ttl: 3600, ..Default::default() });
    let tokens = Arc::new(MemoryTokens::default());

    // Before authorization the server needs auth, and says how.
    let (hub, opts) = hub_for(&server, tokens.clone(), json!({}));
    match &hub.statuses()[0].status {
        SlotStatus::NeedsAuth(note) => assert!(note.contains("marlowe mcp auth web"), "{note}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(hub.statuses()[0].status.label(), "needs auth");

    // `marlowe mcp auth web`.
    let cfg = config::parse_entry("web", config::Scope::Session, &json!({"type": "http", "url": server.url()})).unwrap();
    let oauth = oauth_for(&cfg, &opts.connect, &|_| None).unwrap();
    let set = oauth.authorize(None, &browser, Duration::from_secs(15)).expect("the flow completes");
    assert!(set.access_token.starts_with("at-") && set.refresh_token.as_deref().is_some_and(|r| r.starts_with("rt-")));
    assert!(set.client_id.starts_with("client-"), "dynamic registration ran");
    let log = server.log();
    for step in ["GET /.well-known/oauth-protected-resource/mcp", "GET /.well-known/oauth-authorization-server", "POST /register", "GET /authorize", "POST /token"] {
        assert!(log.iter().any(|l| l == step), "{step} missing from {log:?}");
    }

    // A reload connects with the stored token.
    hub.reload();
    assert!(hub.wait_settled(Duration::from_secs(20)));
    assert_eq!(hub.statuses()[0].status, SlotStatus::Connected, "{:?}", hub.statuses());
    assert_eq!(hub.call("mcp__web__echo", json!({"text": "authed"}), &no_cancel).unwrap().text, "authed");
}

#[test]
fn an_expired_access_token_is_refreshed_and_the_rotation_is_stored() {
    let server = FakeHttp::start(HttpOpts { oauth: true, ..Default::default() });
    let tokens = Arc::new(MemoryTokens::default());
    let (hub, opts) = hub_for(&server, tokens.clone(), json!({}));
    let cfg = config::parse_entry("web", config::Scope::Session, &json!({"type": "http", "url": server.url()})).unwrap();
    let oauth = oauth_for(&cfg, &opts.connect, &|_| None).unwrap();
    let first = oauth.authorize(None, &browser, Duration::from_secs(15)).unwrap();
    hub.reload();
    assert!(hub.wait_settled(Duration::from_secs(20)));
    assert_eq!(hub.call("mcp__web__echo", json!({"text": "a"}), &no_cancel).unwrap().text, "a");

    // The server forgets the access token; the next call gets a 401 and refreshes.
    server.expire_access_tokens();
    assert_eq!(hub.call("mcp__web__echo", json!({"text": "b"}), &no_cancel).unwrap().text, "b");
    let key = oauth.key().to_string();
    let now = tokens.load(&key).unwrap();
    assert_ne!(now.access_token, first.access_token);
    assert_ne!(now.refresh_token, first.refresh_token, "the rotated refresh token was kept");
}

#[test]
fn a_revoked_authorization_asks_for_auth_again_instead_of_failing_obscurely() {
    let server = FakeHttp::start(HttpOpts { oauth: true, ..Default::default() });
    let tokens = Arc::new(MemoryTokens::default());
    let (hub, opts) = hub_for(&server, tokens.clone(), json!({}));
    let cfg = config::parse_entry("web", config::Scope::Session, &json!({"type": "http", "url": server.url()})).unwrap();
    oauth_for(&cfg, &opts.connect, &|_| None).unwrap().authorize(None, &browser, Duration::from_secs(15)).unwrap();
    hub.reload();
    assert!(hub.wait_settled(Duration::from_secs(20)));
    server.revoke_all();
    let err = hub.call("mcp__web__echo", json!({"text": "x"}), &no_cancel).unwrap_err();
    assert!(err.to_string().contains("marlowe mcp auth web"), "{err}");
}

#[test]
fn a_denied_or_forged_redirect_does_not_authorize() {
    let server = FakeHttp::start(HttpOpts { oauth: true, ..Default::default() });
    let tokens = Arc::new(MemoryTokens::default());
    let (_hub, opts) = hub_for(&server, tokens.clone(), json!({}));
    let cfg = config::parse_entry("web", config::Scope::Session, &json!({"type": "http", "url": server.url()})).unwrap();
    let oauth = oauth_for(&cfg, &opts.connect, &|_| None).unwrap();

    // A "browser" that returns the wrong state.
    let forged = |url: &str| -> Result<(), String> {
        let u = Url::parse(url)?;
        let resp = DefaultHttp.send(&HttpRequest::get(&u), Duration::from_secs(5)).map_err(|e| e.to_string())?;
        let loc = resp.header("location").ok_or("no redirect")?.replace("state=", "state=forged");
        let target = Url::parse(&loc)?;
        std::thread::spawn(move || {
            let _ = DefaultHttp.send(&HttpRequest::get(&target), Duration::from_secs(5));
        });
        Ok(())
    };
    let err = oauth.authorize(None, &forged, Duration::from_secs(10)).unwrap_err();
    assert!(err.to_string().contains("state mismatch"), "{err}");
    assert!(tokens.load(oauth.key()).is_none());

    // A user who never comes back: bounded.
    let ignore = |_: &str| -> Result<(), String> { Ok(()) };
    let started = std::time::Instant::now();
    let err = oauth.authorize(None, &ignore, Duration::from_millis(400)).unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_static_authorization_header_bypasses_oauth() {
    let server = FakeHttp::start(HttpOpts::default());
    let flag = Arc::new(AtomicBool::new(false));
    let _ = flag.load(Ordering::SeqCst);
    let (hub, _) = hub_for(&server, Arc::new(MemoryTokens::default()), json!({"headers": {"Authorization": "Bearer static-token"}}));
    assert_eq!(hub.statuses()[0].status, SlotStatus::Connected);
}
