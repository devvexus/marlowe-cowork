//! The layered scrubber every exported string goes through. Layers, in order:
//!
//! 1. **Known secrets**: the exact values of this machine's credentials (Credential Manager, secret
//!    named environment variables), wherever they appear.
//! 2. **Secret files**: a tool call that touches `.env`, `id_rsa`, `*.pem`, `.npmrc`, `~/.ssh/` ...
//!    loses its result and its non-path arguments whole (applied in `hf.rs`, which links calls to
//!    results); a grep/rg line from a secret file (`.env:3:X=y`) in any other result loses its
//!    text after the `path:line:` prefix.
//! 3. **Credential shapes**: provider key prefixes, PEM blocks, JWTs, bearer/basic headers, URL
//!    credentials, `--token x` flags, secret-named assignments and secret-named JSON keys.
//! 4. **Personal data**: email, phone, card numbers (Luhn), US SSNs, public IPv4 addresses.
//! 5. **Identity**: the home directory becomes `~`; other user directories, the user's names,
//!    account name, host name and git identity become markers; then `redact_terms`.
//! 6. **Entropy**: random-looking tokens no rule named (hex is left alone: shas and hashes).
//! 7. **Gate**: the scrubbed record is scanned again with every detector, and the serialized line
//!    for every known secret (raw and JSON-escaped); a record with any hit is dropped, never
//!    written.
//!
//! Matches become `[REDACTED:<kind>]`; markers are never re-matched, so scrubbing is idempotent.
//! `allow_terms` veto layers 3, 4 and 6 only: a known secret or an identity term always goes.

mod rules;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use regex::{Regex, RegexBuilder};
use serde_json::Value;

use rules::{is_marker, Layer, Rule};

/// Version of the rule set, recorded in the manifest. Bump on any rule change.
pub const SCRUB_VERSION: u32 = 2;

/// Who the user is, for the identity layer. Everything here is redacted on word boundaries.
#[derive(Debug, Clone, Default)]
pub struct Identity {
    /// Full names and their parts (`user.json`, git `user.name`).
    pub names: Vec<String>,
    /// OS account names (`USERNAME`, the home directory's last component).
    pub accounts: Vec<String>,
    /// Machine and domain names (`COMPUTERNAME`, `USERDOMAIN`).
    pub hosts: Vec<String>,
    /// The user's own addresses (git `user.email`); any other address is caught as personal data.
    pub emails: Vec<String>,
    /// Home directories, rewritten to `~` in any separator style.
    pub homes: Vec<String>,
}

/// Everything the scrubber is built from.
#[derive(Debug, Clone, Default)]
pub struct ScrubInputs {
    pub known_secrets: Vec<String>,
    pub identity: Identity,
    pub redact_terms: Vec<String>,
    pub allow_terms: Vec<String>,
}

/// Redactions by kind (`openai_key`, `email`, ...), for the manifest and the dry-run report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tally(pub BTreeMap<String, u64>);

impl Tally {
    fn add(&mut self, kind: &str) {
        *self.0.entry(kind.to_string()).or_default() += 1;
    }

    pub fn merge(&mut self, other: &Tally) {
        for (k, v) in &other.0 {
            *self.0.entry(k.clone()).or_default() += v;
        }
    }

    pub fn total(&self) -> u64 {
        self.0.values().sum()
    }
}

/// A literal redaction (known secret, identity term, user term).
struct Literal {
    kind: &'static str,
    re: Regex,
    replacement: String,
}

pub struct Scrubber {
    known: Vec<String>,
    known_re: Option<Literal>,
    rules: Vec<Rule>,
    identity: Vec<Literal>,
    terms: Option<Literal>,
    allow: Vec<String>,
    secret_key: Regex,
    secret_path: Regex,
    /// [`SECRET_PATH`] anchored at the end: a path that ends in a secret file.
    secret_path_end: Regex,
    /// grep/rg separators after a path: `:`, `:N:`, `:N:C:`, `-N-`.
    grep_sep: Regex,
}

pub fn marker(kind: &str) -> String {
    format!("[REDACTED:{kind}]")
}

fn alternation(items: &[String], words: bool) -> Option<String> {
    let mut items: Vec<&String> = items.iter().filter(|s| !s.trim().is_empty()).collect();
    if items.is_empty() {
        return None;
    }
    // Longest first, so `Matthew Priester` wins over `Matthew`.
    items.sort_by_key(|s| std::cmp::Reverse(s.len()));
    items.dedup();
    let alts: Vec<String> = items.iter().map(|s| regex::escape(s)).collect();
    let body = alts.join("|");
    Some(if words { format!(r"\b(?:{body})\b") } else { format!("(?:{body})") })
}

fn literal(kind: &'static str, items: &[String], words: bool, case_insensitive: bool) -> Option<Literal> {
    let pat = alternation(items, words)?;
    let re = RegexBuilder::new(&pat).case_insensitive(case_insensitive).size_limit(64 << 20).build().ok()?;
    Some(Literal { kind, re, replacement: marker(kind) })
}

/// `C:\Users\matth` as a pattern that also matches `c:/users/matth` and `C:\\Users\\matth`.
fn home_pattern(home: &str) -> Option<String> {
    let parts: Vec<&str> = home.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    let lead = if home.starts_with('/') { r"[\\/]+" } else { "" };
    let body: Vec<String> = parts.iter().map(|p| regex::escape(p)).collect();
    Some(format!(r"(?i){lead}{}(?:\b|$)", body.join(r"[\\/]+")))
}

impl Scrubber {
    pub fn new(inputs: &ScrubInputs) -> Scrubber {
        // Very short "secrets" would shred ordinary text; a key is never this short.
        let known: Vec<String> = inputs
            .known_secrets
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| s.len() >= 8)
            .collect();
        let mut known_forms = known.clone();
        for k in &known {
            if let Ok(esc) = serde_json::to_string(k) {
                known_forms.push(esc.trim_matches('"').to_string());
            }
        }
        let id = &inputs.identity;
        let mut identity = Vec::new();
        for home in &id.homes {
            if let Some(p) = home_pattern(home) {
                if let Ok(re) = Regex::new(&p) {
                    identity.push(Literal { kind: "home", re, replacement: "~".into() });
                }
            }
        }
        // Anyone's profile directory names its owner.
        identity.push(Literal {
            kind: "user",
            re: Regex::new(r"(?i)(?P<p>\b[a-z]:[\\/]+(?:users|documents and settings)[\\/]+|(?:^|[\s'\x22=(])/(?:home|Users)/)(?P<s>[^\\/\s'\x22:*?<>|]+)").expect("user dir"),
            replacement: marker("user"),
        });
        let words = |v: &Vec<String>| -> Vec<String> {
            let mut out = Vec::new();
            for s in v {
                let s = s.trim();
                if s.len() >= 3 {
                    out.push(s.to_string());
                }
                for part in s.split_whitespace() {
                    if part.len() >= 3 && part != s {
                        out.push(part.to_string());
                    }
                }
            }
            out
        };
        // A generic account or host name (`user`, `admin`) is no one's, and redacting the word
        // everywhere would shred ordinary text (and JSON keys).
        const GENERIC: &[&str] = &[
            "user", "users", "admin", "administrator", "root", "dev", "guest", "owner", "default", "public", "test",
            "localhost", "workgroup", "home",
        ];
        let specific = |v: Vec<String>| -> Vec<String> {
            v.into_iter().filter(|s| !GENERIC.contains(&s.to_ascii_lowercase().as_str())).collect()
        };
        identity.extend(literal("email", &id.emails, false, true));
        identity.extend(literal("name", &specific(words(&id.names)), true, true));
        identity.extend(literal("account", &specific(words(&id.accounts)), true, true));
        identity.extend(literal("host", &specific(words(&id.hosts)), true, true));
        Scrubber {
            known_re: literal("known_secret", &known_forms, false, false),
            known,
            rules: rules::rules(),
            identity,
            terms: literal("term", &inputs.redact_terms, false, true),
            allow: inputs.allow_terms.iter().map(|s| s.to_lowercase()).filter(|s| !s.is_empty()).collect(),
            secret_key: Regex::new(rules::SECRET_KEY).expect("secret key"),
            secret_path: Regex::new(SECRET_PATH).expect("secret path"),
            secret_path_end: Regex::new(&format!("{SECRET_PATH}$")).expect("secret path end"),
            grep_sep: Regex::new(r":(?:\d+:(?:\d+:)?)?|-\d+-").expect("grep separator"),
        }
    }

    /// The scrubbed string. Every redaction is counted in `tally`.
    pub fn scrub(&self, s: &str, tally: &mut Tally) -> String {
        let mut cur = s.to_string();
        if let Some(k) = &self.known_re {
            cur = replace_literal(k, &cur, tally);
        }
        for layer in [Layer::Credential, Layer::Personal] {
            for r in self.rules.iter().filter(|r| r.layer == layer) {
                cur = self.apply_rule(r, &cur, Some(tally));
            }
        }
        for l in &self.identity {
            cur = replace_literal(l, &cur, tally);
        }
        if let Some(t) = &self.terms {
            cur = replace_literal(t, &cur, tally);
        }
        for r in self.rules.iter().filter(|r| r.layer == Layer::Entropy) {
            cur = self.apply_rule(r, &cur, Some(tally));
        }
        cur
    }

    /// A JSON value with every string scrubbed (keys too). A string under a secret-named key
    /// (`password`, `api_key`, `client_secret`, ...) is replaced whole.
    pub fn scrub_value(&self, v: &Value, tally: &mut Tally) -> Value {
        match v {
            Value::String(s) => Value::String(self.scrub(s, tally)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.scrub_value(x, tally)).collect()),
            Value::Object(o) => {
                let mut out = serde_json::Map::new();
                for (k, x) in o {
                    let key = self.scrub(k, tally);
                    let val = match x {
                        Value::String(s) if !s.is_empty() && !is_marker(s) && self.is_secret_key(k) => {
                            tally.add("credential");
                            Value::String(marker("credential"))
                        }
                        _ => self.scrub_value(x, tally),
                    };
                    out.insert(key, val);
                }
                Value::Object(out)
            }
            other => other.clone(),
        }
    }

    /// `api_key`, `clientSecret`, `X-Auth-Token` are secret-named; `max_tokens` is not.
    pub fn is_secret_key(&self, key: &str) -> bool {
        let mut norm = String::new();
        let mut prev_lower = false;
        for ch in key.chars() {
            if ch.is_ascii_uppercase() && prev_lower {
                norm.push('_');
            }
            prev_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
            norm.push(match ch {
                '-' | ' ' => '_',
                c => c.to_ascii_lowercase(),
            });
        }
        self.secret_key.is_match(&norm)
    }

    /// True when `s` names a secret-shaped file (`.env.production.local`, `prod.env`,
    /// `id_ed25519`, `server.pem`, `~/.ssh/x`, `~/.kube/config`). Templates (`.env.example`,
    /// `example.env`) are not: the last segment decides.
    pub fn mentions_secret_file(&self, s: &str) -> bool {
        self.secret_path.captures_iter(s).any(|c| !is_template(c.name("f").map(|m| m.as_str()).unwrap_or_default()))
    }

    /// `grep`/`rg` output lines from a secret file (`.env:3:SEED=x`, `cfg/.env-4-X=y`) keep their
    /// `path:line:` prefix and lose the rest. Other lines pass through unchanged.
    pub fn redact_secret_file_lines(&self, s: &str, tally: &mut Tally) -> String {
        let mut out = String::with_capacity(s.len());
        for line in s.split_inclusive('\n') {
            match self.secret_line_prefix(line) {
                Some(keep) => {
                    out.push_str(&line[..keep]);
                    out.push_str(&marker("secret_file"));
                    tally.add("secret_file");
                    let body = line.trim_end_matches(['\r', '\n']);
                    out.push_str(&line[body.len()..]);
                }
                None => out.push_str(line),
            }
        }
        out
    }

    /// The length of a grep-style `path:` / `path:N:` / `path-N-` prefix whose path ends in a
    /// secret file, if the line has one.
    fn secret_line_prefix(&self, line: &str) -> Option<usize> {
        let body = line.trim_end_matches(['\r', '\n']);
        // A drive letter's colon is part of the path.
        let skip = if body.as_bytes().get(1) == Some(&b':') && body.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) { 2 } else { 0 };
        let mut at = skip;
        while let Some(m) = self.grep_sep.find_at(body, at) {
            at = m.end().max(m.start() + 1);
            // The path is the last whitespace-free run before the separator.
            let path = &body[..m.start()];
            let tail = path.rsplit(char::is_whitespace).next().unwrap_or(path);
            if !tail.is_empty() && self.secret_path_end.is_match(tail) && self.mentions_secret_file(tail) {
                return (m.end() < body.len()).then_some(m.end());
            }
        }
        None
    }

    /// Detector kinds still present in `s` (the gate). Empty for any output of [`Scrubber::scrub`].
    pub fn findings(&self, s: &str) -> Vec<&'static str> {
        let mut out = Vec::new();
        if let Some(k) = &self.known_re {
            if k.re.is_match(s) {
                out.push(k.kind);
            }
        }
        for r in &self.rules {
            if self.apply_rule(r, s, None) != s {
                out.push(r.kind);
            }
        }
        for l in self.identity.iter().filter(|l| l.kind != "user") {
            if l.re.is_match(s) {
                out.push(l.kind);
            }
        }
        if self.identity.iter().any(|l| l.kind == "user" && l.re.captures_iter(s).any(|c| c.name("s").is_some_and(|m| !is_marker(m.as_str()) && !is_public_user(m.as_str())))) {
            out.push("user");
        }
        if let Some(t) = &self.terms {
            if t.re.is_match(s) {
                out.push(t.kind);
            }
        }
        out
    }

    /// True when a serialized line still holds a known secret, raw or JSON-escaped.
    pub fn leaks_known(&self, line: &str) -> bool {
        self.known.iter().any(|k| {
            line.contains(k.as_str())
                || serde_json::to_string(k).is_ok_and(|e| line.contains(e.trim_matches('"')))
        })
    }

    fn allowed(&self, text: &str) -> bool {
        if self.allow.is_empty() {
            return false;
        }
        let t = text.to_lowercase();
        self.allow.iter().any(|a| t.contains(a.as_str()))
    }

    /// Applies one rule; with no tally it only detects (the returned string differs on a hit).
    fn apply_rule(&self, r: &Rule, s: &str, mut tally: Option<&mut Tally>) -> String {
        let mut out = String::with_capacity(s.len());
        let mut last = 0;
        let mut at = 0;
        while at <= s.len() {
            let Some(c) = r.re.captures_at(s, at) else { break };
            let whole = c.get(0).expect("group 0");
            let m = c.name("s").unwrap_or(whole);
            let vetoed = is_marker(m.as_str())
                || inside_marker(s, m.start())
                || r.check.is_some_and(|check| !check(&c, s))
                || self.allowed(m.as_str());
            if vetoed {
                // Retry one character on: `8.8.8.8 4111...` must still find the card after the
                // longer, vetoed digit run that started inside the address. A vetoed token is
                // judged whole (its suffixes are not new tokens).
                at = if r.layer == Layer::Entropy {
                    whole.end().max(whole.start() + 1)
                } else {
                    whole.start() + s[whole.start()..].chars().next().map_or(1, char::len_utf8)
                };
                continue;
            }
            at = whole.end().max(whole.start() + 1);
            if m.start() < last {
                continue;
            }
            out.push_str(&s[last..m.start()]);
            out.push_str(&marker(r.kind));
            last = m.end();
            if let Some(t) = tally.as_deref_mut() {
                t.add(r.kind);
            }
        }
        out.push_str(&s[last..]);
        out
    }

    /// The layers, for the manifest.
    pub fn describe(&self) -> Vec<String> {
        let mut v = vec![format!("known_secret: {} value(s)", self.known.len()), "secret_file".to_string()];
        for layer in [Layer::Credential, Layer::Personal] {
            let mut kinds: Vec<&str> = Vec::new();
            for r in self.rules.iter().filter(|r| r.layer == layer) {
                if !kinds.contains(&r.kind) {
                    kinds.push(r.kind);
                }
            }
            v.push(format!("{}: {}", layer.as_str(), kinds.join(", ")));
        }
        let ids: Vec<&str> = self.identity.iter().map(|l| l.kind).collect();
        v.push(format!("identity: {}", ids.join(", ")));
        if self.terms.is_some() {
            v.push("redact_terms".into());
        }
        v.push(format!("{}: high_entropy", Layer::Entropy.as_str()));
        v.push("gate: re-detect every string; known secrets on the serialized line".into());
        v
    }
}

/// `Public`, `Default` and `All Users` are no one's name.
fn is_public_user(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "public" | "default" | "all users" | "default user" | "~")
}

/// True when byte `at` sits inside a `[REDACTED:...]` marker.
fn inside_marker(s: &str, at: usize) -> bool {
    match s[..at].rfind("[REDACTED") {
        Some(open) => !s[open..at].contains(']'),
        None => false,
    }
}

fn replace_literal(l: &Literal, s: &str, tally: &mut Tally) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for c in l.re.captures_iter(s) {
        let Some(m) = c.name("s").or_else(|| c.get(0)) else { continue };
        if m.start() < last || is_marker(m.as_str()) || inside_marker(s, m.start()) {
            continue;
        }
        if l.kind == "user" && is_public_user(m.as_str()) {
            continue;
        }
        out.push_str(&s[last..m.start()]);
        out.push_str(&l.replacement);
        last = m.end();
        tally.add(l.kind);
    }
    out.push_str(&s[last..]);
    out
}

/// True for a dotenv template: `.env.example`, `.env.local.sample`, `example.env`.
fn is_template(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    const TEMPLATES: &[&str] = &["example", "sample", "template", "dist", "defaults"];
    if name.starts_with(".env.") {
        return name.rsplit('.').next().is_some_and(|last| TEMPLATES.contains(&last));
    }
    if let Some(stem) = name.strip_suffix(".env").filter(|s| !s.is_empty()) {
        return stem.rsplit(['.', '_', '-']).next().is_some_and(|last| TEMPLATES.contains(&last));
    }
    false
}

/// Secret-shaped file names, with the name in group `f`: dotenv files with any number of
/// suffixes (`.env.production.local`) or a stem (`prod.env`), `.envrc`, package-manager and
/// cloud credentials (`.npmrc`, `~/.aws/credentials`, `~/.kube/config`,
/// `~/.docker/config.json`), SSH keys and key stores.
const SECRET_PATH: &str = r#"(?i)(?:^|[\s\\/'"=:`(<>|&;])(?P<f>\.env(?:\.[A-Za-z0-9_-]+)*|[^\s\\/'"`=:(<>|&;]+\.env|\.envrc|\.npmrc|\.netrc|_netrc|\.pypirc|\.git-credentials|\.pgpass|\.dockercfg|\.kube[\\/]config|\.docker[\\/]config\.json|\.aws[\\/](?:credentials|config)|id_(?:rsa|dsa|ecdsa|ed25519)(?:_sk)?|credentials(?:\.json|\.ya?ml|\.toml|\.csv)?|secrets?\.(?:json|ya?ml|toml|env)|\.ssh[\\/][^\s'"`]*|[^\s\\/'"`]+\.(?:pem|key|p12|pfx|jks|keystore|kdbx|ovpn|ppk|asc|gpg))(?:$|[\s\\/'"`;,)|&>])"#;
