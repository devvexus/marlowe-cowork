//! OAuth 2.1 for MCP servers (MCP authorization, 2025-06-18): protected-resource and
//! authorization-server discovery, dynamic client registration (RFC 7591), authorization code with
//! PKCE S256 and the `resource` parameter (RFC 8707), refresh with rotation.
//!
//! The daemon never opens a browser. A server that needs authorization reports `needs auth`;
//! `marlowe mcp auth <name>` runs [`OAuth::authorize`] (loopback redirect listener, browser
//! opened by the caller) and stores the tokens, after which the server reconnects. Tokens live in
//! Windows Credential Manager through [`KeyringTokens`], never on disk.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::http::{HttpBackend, HttpRequest};
use crate::http_wire::Authorizer;
use crate::util::{base64url, form_decode, form_encode, percent_encode, random_token, sha256_bytes, unix_now, Url};
use crate::McpError;

/// A token is renewed this long before it expires.
const EXPIRY_MARGIN_SECS: i64 = 60;
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const LIMIT: usize = 1 << 20;

/// What the config says about OAuth for one server (`"oauth": {...}`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthConfig {
    #[serde(default, rename = "clientId", alias = "client_id", skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, rename = "clientSecret", alias = "client_secret", skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// A fixed loopback port for the redirect, for servers that pre-register one.
    #[serde(default, rename = "callbackPort", alias = "callback_port", skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u16>,
}

/// Everything kept for one authorized server.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenSet {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds; 0 when the server did not say.
    #[serde(default)]
    pub expires_at: i64,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet").field("client_id", &self.client_id).field("expires_at", &self.expires_at).finish_non_exhaustive()
    }
}

impl TokenSet {
    fn expiring(&self, now: i64) -> bool {
        self.expires_at != 0 && self.expires_at - EXPIRY_MARGIN_SECS <= now
    }
}

/// Where tokens are kept, by server key.
pub trait TokenStore: Send + Sync {
    fn load(&self, key: &str) -> Option<TokenSet>;
    fn save(&self, key: &str, tokens: &TokenSet) -> Result<(), String>;
    fn delete(&self, key: &str);
}

/// In-process store: tests, and servers whose tokens should not outlive the run.
#[derive(Default)]
pub struct MemoryTokens(Mutex<HashMap<String, TokenSet>>);

impl TokenStore for MemoryTokens {
    fn load(&self, key: &str) -> Option<TokenSet> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).get(key).cloned()
    }
    fn save(&self, key: &str, tokens: &TokenSet) -> Result<(), String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).insert(key.to_string(), tokens.clone());
        Ok(())
    }
    fn delete(&self, key: &str) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
    }
}

/// Windows Credential Manager. A credential blob holds about 1,280 UTF-16 characters, less than
/// one JWT access token plus a refresh token, so the JSON is split across numbered entries.
pub struct KeyringTokens {
    service: String,
}

/// Characters per Credential Manager entry.
const CHUNK: usize = 1000;

impl KeyringTokens {
    pub const SERVICE: &'static str = "MarloweCowork-mcp";

    pub fn new() -> Self {
        Self { service: Self::SERVICE.into() }
    }

    pub fn with_service(service: impl Into<String>) -> Self {
        Self { service: service.into() }
    }

    fn entry(&self, key: &str, part: &str) -> Option<keyring::Entry> {
        keyring::Entry::new(&self.service, &format!("{key}#{part}")).ok()
    }

    fn read(&self, key: &str, part: &str) -> Option<String> {
        self.entry(key, part)?.get_password().ok()
    }
}

impl Default for KeyringTokens {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenStore for KeyringTokens {
    fn load(&self, key: &str) -> Option<TokenSet> {
        let n: usize = self.read(key, "n")?.parse().ok()?;
        let mut json = String::new();
        for i in 0..n {
            json.push_str(&self.read(key, &i.to_string())?);
        }
        serde_json::from_str(&json).ok()
    }

    fn save(&self, key: &str, tokens: &TokenSet) -> Result<(), String> {
        let json = serde_json::to_string(tokens).map_err(|e| e.to_string())?;
        let chars: Vec<char> = json.chars().collect();
        let parts: Vec<String> = chars.chunks(CHUNK).map(|c| c.iter().collect()).collect();
        let put = |part: &str, value: &str| -> Result<(), String> {
            self.entry(key, part).ok_or("no credential store")?.set_password(value).map_err(|e| e.to_string())
        };
        for (i, p) in parts.iter().enumerate() {
            put(&i.to_string(), p)?;
        }
        put("n", &parts.len().to_string())
    }

    fn delete(&self, key: &str) {
        let n: usize = self.read(key, "n").and_then(|v| v.parse().ok()).unwrap_or(0);
        for part in (0..n).map(|i| i.to_string()).chain(std::iter::once("n".to_string())) {
            if let Some(e) = self.entry(key, &part) {
                let _ = e.delete_credential();
            }
        }
    }
}

/// Server metadata gathered by discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthServer {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub scopes_supported: Vec<String>,
}

pub struct OAuth {
    name: String,
    key: String,
    server_url: Url,
    cfg: OAuthConfig,
    http: Arc<dyn HttpBackend>,
    store: Arc<dyn TokenStore>,
    now: fn() -> i64,
    cache: Mutex<Option<TokenSet>>,
}

impl OAuth {
    pub fn new(name: &str, server_url: &Url, cfg: OAuthConfig, http: Arc<dyn HttpBackend>, store: Arc<dyn TokenStore>) -> Self {
        Self {
            name: name.to_string(),
            key: token_key(name, server_url),
            server_url: server_url.clone(),
            cfg,
            http,
            store,
            now: unix_now,
            cache: Mutex::new(None),
        }
    }

    /// Replaces the clock (tests).
    pub fn with_clock(mut self, now: fn() -> i64) -> Self {
        self.now = now;
        self
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    /// The canonical resource identifier sent as `resource` (RFC 8707): the server URL without
    /// query or fragment.
    pub fn resource(&self) -> String {
        format!("{}{}", self.server_url.origin(), self.server_url.path_only().trim_end_matches('/'))
    }

    /// Whether tokens are stored for this server.
    pub fn has_tokens(&self) -> bool {
        self.current().is_some()
    }

    pub fn sign_out(&self) {
        self.store.delete(&self.key);
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn current(&self) -> Option<TokenSet> {
        let mut c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if c.is_none() {
            *c = self.store.load(&self.key);
        }
        c.clone()
    }

    fn remember(&self, t: &TokenSet) {
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Some(t.clone());
        let _ = self.store.save(&self.key, t);
    }

    /// Exchanges the refresh token for a new access token and stores the result.
    pub fn refresh(&self) -> Result<TokenSet, McpError> {
        let old = self.current().ok_or_else(|| self.needs_auth("no stored authorization"))?;
        let refresh = old.refresh_token.clone().ok_or_else(|| self.needs_auth("the access token expired and there is no refresh token"))?;
        let resource = old.resource.clone().unwrap_or_else(|| self.resource());
        let mut form = vec![("grant_type", "refresh_token"), ("refresh_token", refresh.as_str()), ("client_id", old.client_id.as_str()), ("resource", resource.as_str())];
        if let Some(s) = &old.client_secret {
            form.push(("client_secret", s));
        }
        let url = Url::parse(&old.token_endpoint).map_err(|e| self.needs_auth(&e))?;
        let reply = self.post_form(&url, &form_encode(&form))?;
        let mut fresh = self.parse_token(&reply, &old.client_id, old.client_secret.clone(), &old.token_endpoint, Some(resource))?;
        if fresh.refresh_token.is_none() {
            fresh.refresh_token = Some(refresh);
        }
        self.remember(&fresh);
        Ok(fresh)
    }

    fn needs_auth(&self, why: &str) -> McpError {
        McpError::NeedsAuth { server: self.name.clone(), detail: format!("{why}; run `marlowe mcp auth {}`", self.name) }
    }

    fn http_err(&self, detail: String) -> McpError {
        McpError::Http { server: self.name.clone(), status: 0, detail }
    }

    /// The authorization URL is shown by the desktop's URL opener (the shell's own, on Windows),
    /// which will launch whatever scheme it is given, so it comes
    /// from untrusted server metadata only as `https`, or `http` to this machine; nothing that
    /// could carry a second argument or a control character.
    fn browsable(&self, endpoint: &str) -> Result<(), McpError> {
        let bad = |why: &str| self.http_err(format!("refusing to open the authorization page {endpoint:?}: {why}"));
        if endpoint.chars().any(|c| c.is_control() || c.is_whitespace() || c == '"' || c == '<' || c == '>') {
            return Err(bad("it has a space, quote or control character"));
        }
        let url = Url::parse(endpoint).map_err(|e| bad(&e))?;
        if !url.https && !url.is_loopback() {
            return Err(bad("plain http is only allowed to this machine; the server must use https"));
        }
        Ok(())
    }

    fn get_json(&self, url: &Url) -> Option<Value> {
        let req = HttpRequest::get(url).header("Accept", "application/json");
        let mut resp = self.http.send(&req, HTTP_TIMEOUT).ok()?;
        (resp.status == 200).then(|| resp.json(LIMIT)).flatten()
    }

    fn post_form(&self, url: &Url, form: &str) -> Result<Value, McpError> {
        let req = HttpRequest::post_form(url, form).header("Accept", "application/json");
        let mut resp = self.http.send(&req, HTTP_TIMEOUT)?;
        let status = resp.status;
        let body = resp.json(LIMIT);
        match (status, body) {
            (200..=299, Some(v)) => Ok(v),
            (_, Some(v)) => {
                let err = v.get("error").and_then(Value::as_str).unwrap_or("error");
                let desc = v.get("error_description").and_then(Value::as_str).unwrap_or("");
                if err == "invalid_grant" {
                    Err(self.needs_auth("the authorization was revoked or expired"))
                } else {
                    Err(self.http_err(format!("token endpoint {status}: {err} {desc}")))
                }
            }
            (s, None) => Err(self.http_err(format!("token endpoint answered {s} with no JSON body"))),
        }
    }

    fn parse_token(&self, v: &Value, client_id: &str, client_secret: Option<String>, endpoint: &str, resource: Option<String>) -> Result<TokenSet, McpError> {
        let access = v.get("access_token").and_then(Value::as_str).ok_or_else(|| self.http_err("the token response has no access_token".into()))?;
        let expires_in = v.get("expires_in").and_then(Value::as_i64);
        Ok(TokenSet {
            client_id: client_id.to_string(),
            client_secret,
            access_token: access.to_string(),
            refresh_token: v.get("refresh_token").and_then(Value::as_str).map(str::to_string),
            expires_at: expires_in.map_or(0, |s| (self.now)() + s),
            token_endpoint: endpoint.to_string(),
            scope: v.get("scope").and_then(Value::as_str).map(str::to_string),
            resource,
        })
    }

    // ------------------------------------------------------------ discovery

    /// Finds the authorization server for this MCP server. `challenge` is a `WWW-Authenticate`
    /// header from a 401, when there is one.
    pub fn discover(&self, challenge: Option<&str>) -> Result<AuthServer, McpError> {
        let origin = self.server_url.with_path("/");
        let path = self.server_url.path_only().trim_end_matches('/').to_string();
        let mut candidates: Vec<Url> = Vec::new();
        if let Some(u) = challenge.and_then(|c| header_param(c, "resource_metadata")).and_then(|u| Url::parse(&u).ok()) {
            candidates.push(u);
        }
        if !path.is_empty() {
            candidates.push(origin.with_path(&format!("/.well-known/oauth-protected-resource{path}")));
        }
        candidates.push(origin.with_path("/.well-known/oauth-protected-resource"));
        let issuer = candidates
            .iter()
            .find_map(|u| self.get_json(u))
            .and_then(|m| m.get("authorization_servers")?.as_array()?.first()?.as_str().map(str::to_string))
            .unwrap_or_else(|| origin.origin());
        let issuer_url = Url::parse(&issuer).map_err(|e| self.http_err(format!("bad authorization server {issuer:?}: {e}")))?;
        let ipath = issuer_url.path_only().trim_end_matches('/').to_string();
        let ibase = issuer_url.with_path("/");
        let meta_urls = [
            ibase.with_path(&format!("/.well-known/oauth-authorization-server{ipath}")),
            ibase.with_path(&format!("/.well-known/openid-configuration{ipath}")),
            ibase.with_path(&format!("{ipath}/.well-known/openid-configuration")),
        ];
        let field = |m: &Value, k: &str| m.get(k).and_then(Value::as_str).map(str::to_string);
        if let Some(m) = meta_urls.iter().find_map(|u| self.get_json(u)) {
            if let (Some(a), Some(t)) = (field(&m, "authorization_endpoint"), field(&m, "token_endpoint")) {
                self.browsable(&a)?;
                return Ok(AuthServer {
                    issuer,
                    authorization_endpoint: a,
                    token_endpoint: t,
                    registration_endpoint: field(&m, "registration_endpoint"),
                    scopes_supported: m
                        .get("scopes_supported")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
                        .unwrap_or_default(),
                });
            }
        }
        // Servers from before metadata discovery: the conventional paths on the issuer.
        Ok(AuthServer {
            authorization_endpoint: ibase.with_path("/authorize").to_string(),
            token_endpoint: ibase.with_path("/token").to_string(),
            registration_endpoint: Some(ibase.with_path("/register").to_string()),
            scopes_supported: Vec::new(),
            issuer,
        })
    }

    fn register(&self, server: &AuthServer, redirect_uri: &str) -> Result<(String, Option<String>), McpError> {
        let endpoint = server
            .registration_endpoint
            .as_deref()
            .ok_or_else(|| self.http_err("the server offers no dynamic client registration; set oauth.clientId in the config".into()))?;
        let url = Url::parse(endpoint).map_err(|e| self.http_err(e))?;
        let body = json!({
            "client_name": "Marlowe",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        });
        let mut resp = self.http.send(&HttpRequest::post_json(&url, &body).header("Accept", "application/json"), HTTP_TIMEOUT)?;
        let status = resp.status;
        let v = resp.json(LIMIT).ok_or_else(|| self.http_err(format!("registration answered {status} with no JSON")))?;
        if !(200..300).contains(&status) {
            return Err(self.http_err(format!("registration refused ({status}): {}", v.get("error_description").or(v.get("error")).and_then(Value::as_str).unwrap_or(""))));
        }
        let id = v.get("client_id").and_then(Value::as_str).ok_or_else(|| self.http_err("registration returned no client_id".into()))?;
        Ok((id.to_string(), v.get("client_secret").and_then(Value::as_str).map(str::to_string)))
    }

    // ------------------------------------------------------------ the interactive flow

    /// Runs the authorization-code flow. `open` receives the URL to show the user (a browser, or
    /// a printed link); the redirect lands on a loopback listener. Waits up to `wait`.
    pub fn authorize(&self, challenge: Option<&str>, open: &dyn Fn(&str) -> Result<(), String>, wait: Duration) -> Result<TokenSet, McpError> {
        let server = self.discover(challenge)?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, self.cfg.callback_port.unwrap_or(0)))
            .map_err(|e| self.http_err(format!("cannot listen for the redirect: {e}")))?;
        let port = listener.local_addr().map_err(|e| self.http_err(e.to_string()))?.port();
        let redirect = format!("http://127.0.0.1:{port}/callback");
        let (client_id, client_secret) = match &self.cfg.client_id {
            Some(id) => (id.clone(), self.cfg.client_secret.clone()),
            None => match self.current().filter(|t| !t.client_id.is_empty()) {
                // Registered before; the redirect port may differ, so register again when it does.
                Some(t) if self.cfg.callback_port.is_some() => (t.client_id, t.client_secret),
                _ => self.register(&server, &redirect)?,
            },
        };

        let verifier = random_token(48);
        let challenge_s256 = base64url(&sha256_bytes(verifier.as_bytes()));
        let state = random_token(24);
        let resource = self.resource();
        let scope = if self.cfg.scopes.is_empty() { server.scopes_supported.join(" ") } else { self.cfg.scopes.join(" ") };
        let mut query = vec![
            ("response_type", "code"),
            ("client_id", client_id.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("state", state.as_str()),
            ("code_challenge", challenge_s256.as_str()),
            ("code_challenge_method", "S256"),
            ("resource", resource.as_str()),
        ];
        if !scope.is_empty() {
            query.push(("scope", scope.as_str()));
        }
        let sep = if server.authorization_endpoint.contains('?') { '&' } else { '?' };
        let auth_url = format!("{}{sep}{}", server.authorization_endpoint, form_encode(&query));
        // The conventional-path fallback is built from the issuer, so it is checked here as well.
        self.browsable(&auth_url)?;
        open(&auth_url).map_err(|e| self.http_err(format!("could not show the authorization page: {e}")))?;

        let code = self.await_redirect(&listener, &state, wait)?;
        let token_url = Url::parse(&server.token_endpoint).map_err(|e| self.http_err(e))?;
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("client_id", client_id.as_str()),
            ("code_verifier", verifier.as_str()),
            ("resource", resource.as_str()),
        ];
        if let Some(s) = &client_secret {
            form.push(("client_secret", s));
        }
        let reply = self.post_form(&token_url, &form_encode(&form))?;
        let tokens = self.parse_token(&reply, &client_id, client_secret, &server.token_endpoint, Some(resource))?;
        self.store.save(&self.key, &tokens).map_err(|e| self.http_err(format!("could not store the tokens: {e}")))?;
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Some(tokens.clone());
        Ok(tokens)
    }

    fn await_redirect(&self, listener: &TcpListener, state: &str, wait: Duration) -> Result<String, McpError> {
        listener.set_nonblocking(true).map_err(|e| self.http_err(e.to_string()))?;
        let end = Instant::now() + wait;
        loop {
            match listener.accept() {
                Ok((mut sock, _)) => {
                    sock.set_nonblocking(false).ok();
                    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    let mut line = String::new();
                    let _ = BufReader::new(&sock).read_line(&mut line);
                    let target = line.split_whitespace().nth(1).unwrap_or("");
                    let query = target.split_once('?').map_or("", |(_, q)| q);
                    let params = form_decode(query);
                    let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
                    let (page, outcome) = if !target.starts_with("/callback") {
                        ("Not found.", None)
                    } else if let Some(e) = get("error") {
                        ("Authorization was not granted. You can close this tab.", Some(Err(e)))
                    } else if get("state").as_deref() != Some(state) {
                        ("This response does not belong to the request Marlowe made.", Some(Err("state mismatch".to_string())))
                    } else if let Some(code) = get("code") {
                        ("Authorized. You can close this tab and return to Marlowe.", Some(Ok(code)))
                    } else {
                        ("No code in the response.", Some(Err("no code".to_string())))
                    };
                    let status = if outcome.as_ref().is_some_and(Result::is_ok) { "200 OK" } else { "400 Bad Request" };
                    let _ = write!(sock, "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len());
                    match outcome {
                        Some(Ok(code)) => return Ok(code),
                        Some(Err(e)) => return Err(self.http_err(format!("authorization failed: {e}"))),
                        None => {}
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= end {
                        return Err(self.http_err("timed out waiting for the browser to return".into()));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(self.http_err(e.to_string())),
            }
        }
    }
}

impl Authorizer for OAuth {
    fn authorization(&self) -> Option<String> {
        let t = self.current()?;
        if t.expiring((self.now)()) && t.refresh_token.is_some() {
            return self.refresh().ok().map(|f| format!("Bearer {}", f.access_token)).or_else(|| (t.expires_at > (self.now)()).then(|| format!("Bearer {}", t.access_token)));
        }
        Some(format!("Bearer {}", t.access_token))
    }

    fn unauthorized(&self, _www_authenticate: Option<&str>) -> bool {
        self.refresh().is_ok()
    }

    fn needs_auth_note(&self) -> String {
        format!("needs auth: run `marlowe mcp auth {}`", self.name)
    }
}

/// The credential key for one server: its name and endpoint, so a renamed or repointed server
/// does not reuse another's tokens.
pub fn token_key(name: &str, url: &Url) -> String {
    format!("{name}|{}{}", url.origin(), url.path_only())
}

/// `param="value"` or `param=value` from a `Bearer ...` challenge.
pub fn header_param(header: &str, param: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    let at = lower.find(&format!("{param}="))? + param.len() + 1;
    let rest = &header[at..];
    if let Some(q) = rest.strip_prefix('"') {
        Some(q[..q.find('"')?].to_string())
    } else {
        Some(rest.split([',', ' ']).next()?.to_string())
    }
}

#[allow(dead_code)]
fn _encode_check() -> String {
    percent_encode("")
}
