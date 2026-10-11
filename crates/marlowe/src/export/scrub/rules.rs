//! The pattern layers: credentials with a known shape, credential assignments, personal data and
//! high-entropy tokens. Each rule names a kind (the `[REDACTED:<kind>]` marker and the manifest
//! tally), a regex, and optionally a capture group `s` (only that span is replaced, so `Bearer `,
//! `password = ` and `https://` survive) and a validator that can veto a match.

use regex::{Captures, Regex};

/// Vetoes a match. Gets the captures and the whole haystack (for the character after a match).
pub(super) type Validator = fn(&Captures<'_>, &str) -> bool;

pub(super) struct Rule {
    pub kind: &'static str,
    pub layer: Layer,
    pub re: Regex,
    pub check: Option<Validator>,
}

/// Which layer a rule belongs to; reported in the manifest and decides whether `allow_terms` apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Layer {
    /// Credentials with a recognisable shape (prefix, structure or a secret-named key).
    Credential,
    /// Personal data: email, phone, card, national id, public IP.
    Personal,
    /// Random-looking tokens no other rule named.
    Entropy,
}

impl Layer {
    pub fn as_str(self) -> &'static str {
        match self {
            Layer::Credential => "credential",
            Layer::Personal => "personal",
            Layer::Entropy => "entropy",
        }
    }
}

fn rule(kind: &'static str, layer: Layer, re: &str, check: Option<Validator>) -> Rule {
    Rule { kind, layer, re: Regex::new(re).unwrap_or_else(|e| panic!("rule {kind}: {e}")), check }
}

/// Secret-named keys, as a regex fragment matched against a whole key (lower-cased, `-` and
/// spaces folded to `_`). `max_tokens` and `tokenizer` are not secrets; `auth_token` is.
pub(super) const SECRET_KEY: &str = r"^(?:[a-z0-9_.]*_)?(?:password|passwd|pwd|passphrase|secret|token|api_?key|apikey|access_?key|private_?key|secret_?key|client_?secret|auth|authorization|credentials?|cookie|session_?token|refresh_?token|access_?token|bearer|signature)$";

/// Rules in the order they run. Shapes before assignments (so `OPENAI_API_KEY=sk-...` is named
/// by its shape), credentials before personal data, entropy last.
pub(super) fn rules() -> Vec<Rule> {
    use Layer::*;
    vec![
        // A PEM private key block; an unterminated block is cut to the end of the string.
        rule(
            "private_key",
            Credential,
            r"-----BEGIN[A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----(?s:.*?)(?:-----END[A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----|\z)",
            None,
        ),
        // Header values first, so a token of any shape goes whole.
        // Spelled out rather than `(?i)`: Unicode case folding would let `K` (Kelvin) into the class.
        rule("bearer", Credential, r"\b[Bb][Ee][Aa][Rr][Ee][Rr][ \t]+(?P<s>[A-Za-z0-9._~+/=_-]{16,})", None),
        rule("basic_auth", Credential, r"(?i)\bauthorization\s*[:=]\s*basic[ \t]+(?P<s>[A-Za-z0-9+/=]{8,})", None),
        rule("anthropic_key", Credential, r"sk-ant-[A-Za-z0-9_-]{20,}", None),
        rule("openrouter_key", Credential, r"sk-or-(?:v1-)?[A-Za-z0-9_-]{20,}", None),
        rule("stripe_key", Credential, r"\b(?:sk|rk|pk)_(?:live|test)_[A-Za-z0-9]{16,}", None),
        rule("openai_key", Credential, r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_-]{20,}", None),
        rule("github_token", Credential, r"\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,})", None),
        rule("gitlab_token", Credential, r"\bglpat-[A-Za-z0-9_-]{20,}", None),
        rule("aws_access_key", Credential, r"\b(?:AKIA|ASIA|ABIA|ACCA)[A-Z0-9]{16}\b", None),
        rule("google_key", Credential, r"\bAIza[0-9A-Za-z_-]{35}", None),
        rule("slack_token", Credential, r"\bxox[abposr]-[A-Za-z0-9-]{10,}", None),
        rule("slack_webhook", Credential, r"https://hooks\.slack\.com/services/[A-Za-z0-9/_-]+", None),
        rule("huggingface_token", Credential, r"\bhf_[A-Za-z0-9]{30,}", None),
        rule("npm_token", Credential, r"\bnpm_[A-Za-z0-9]{36}\b", None),
        rule("pypi_token", Credential, r"\bpypi-[A-Za-z0-9_-]{50,}", None),
        rule("sendgrid_key", Credential, r"\bSG\.[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}", None),
        rule("api_key", Credential, r"\b(?:xai|nvapi|pplx|gsk|fw|tgp_v1)[-_][A-Za-z0-9_-]{20,}", None),
        rule("jwt", Credential, r"\beyJ[A-Za-z0-9_-]{5,}\.eyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}", None),
        rule("azure_key", Credential, r"(?i)(?:AccountKey|SharedAccessKey|sig)=(?P<s>[A-Za-z0-9+/%=]{20,})", None),
        rule("wandb_key", Credential, r"(?i)\bwandb\s+login\s+(?:--relogin\s+)?(?P<s>[0-9a-f]{40})\b", None),
        // `scheme://user:password@host`: the user part goes too (it is often an email or a name).
        rule("url_credentials", Credential, r"(?i)\b[a-z][a-z0-9+.-]*://(?P<s>[^\s/:@'\x22]+:[^\s/'\x22]+)@", None),
        // `--password x`, `--token=x`, `-p:x` style flags.
        rule(
            "credential_flag",
            Credential,
            r#"(?i)(?:^|\s)--?(?:password|passwd|token|api-?key|secret|auth-?token|access-?token|client-?secret)(?:[ \t]+|=)(?:\\?['"])?(?P<s>[^\s'"`\\]{4,})"#,
            Some(not_code_ref),
        ),
        // `password = "x"`, `API_KEY: x`, `"client_secret": "x"` in prose, code, logs and configs.
        rule(
            "credential",
            Credential,
            // `\\?` before a quote: the same assignment inside JSON text (`\"password\": \"x\"`).
            r#"(?i)\b[A-Za-z0-9_.-]*(?:password|passwd|pwd|passphrase|secret|token|api[_-]?key|apikey|access[_-]?key|private[_-]?key|(?:secret|signing|encryption|master)[_-]?key|credential|cookie)s?(?:\\?['"])?[ \t]*(?:=|:|=>|:=)[ \t]*(?:\\?['"])?(?P<s>[^\s'"`,;)}\]>\\]{6,})"#,
            Some(not_code_ref),
        ),
        // Personal data.
        rule(
            "email",
            Personal,
            r"\b[A-Za-z0-9][A-Za-z0-9._%+-]*@[A-Za-z0-9][A-Za-z0-9-]*(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}\b",
            Some(real_email),
        ),
        rule(
            "phone",
            Personal,
            r"(?:\+\d{1,3}[ .-]?)?(?:\(\d{3}\)[ .-]?|\b\d{3}[ .-])\d{3}[ .-]\d{4}\b|\+\d{1,3}(?:[ .-]\d{2,5}){2,4}\b",
            None,
        ),
        rule("card_number", Personal, r"\b(?:\d[ -]?){12,18}\d\b", Some(card)),
        rule("national_id", Personal, r"\b\d{3}-\d{2}-\d{4}\b", Some(ssn)),
        rule("ip_address", Personal, r"\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}\b", Some(public_ip)),
        // Random-looking tokens.
        rule("high_entropy", Entropy, r"[A-Za-z0-9+/_=-]{20,}", Some(random_token)),
    ]
}

/// True when `s` is already a redaction marker (keeps the scrubber idempotent).
pub(super) fn is_marker(s: &str) -> bool {
    s.starts_with("[REDACTED")
}

/// Vetoes values that are code, not secrets: `os.environ["X"]`, `config.api_key`, `$TOKEN`,
/// `getenv(`, `None`, an env-var name, or a call/index right after the value.
fn not_code_ref(c: &Captures<'_>, hay: &str) -> bool {
    let Some(m) = c.name("s") else { return true };
    let v = m.as_str();
    if is_marker(v) {
        return false;
    }
    let next = hay[m.end()..].chars().next();
    if matches!(next, Some('(' | '[' | '{')) {
        return false;
    }
    // `os.environ[`, `getenv(`, `config.get(`: a call or index whose argument follows.
    if let Some(head) = v.strip_suffix(['(', '[', '{']) {
        let ident_path = head
            .split(['.', ':'])
            .filter(|p| !p.is_empty())
            .all(|p| p.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_'));
        if !head.is_empty() && ident_path {
            return false;
        }
    }
    let lower = v.to_ascii_lowercase();
    const LITERALS: &[&str] = &["none", "null", "nil", "true", "false", "undefined", "required", "optional", "string", "str"];
    if LITERALS.contains(&lower.as_str()) {
        return false;
    }
    // A count (`input_tokens: 123456`), not a credential.
    if v.chars().all(|ch| ch.is_ascii_digit() || ch == '.') {
        return false;
    }
    if v.starts_with('$') || v.starts_with('%') || v.starts_with('<') || v.starts_with("process.env") {
        return false;
    }
    // A bare env-var name (`OPENAI_API_KEY`) or a dotted identifier path (`self.config.token`).
    let ident = |s: &str| !s.is_empty() && s.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') && !s.starts_with(|ch: char| ch.is_ascii_digit());
    if v.chars().all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_') && v.contains('_') {
        return false;
    }
    if v.contains('.') && v.split('.').all(ident) && v.chars().filter(|ch| ch.is_ascii_digit()).count() == 0 {
        return false;
    }
    if v.contains("::") && v.split("::").all(ident) {
        return false;
    }
    true
}

/// Addresses that are no one's: `git@github.com`, noreply addresses, documentation domains, and
/// retina asset names like `icon@2x.png`.
fn real_email(c: &Captures<'_>, _: &str) -> bool {
    let m = c.get(0).map(|m| m.as_str()).unwrap_or_default().to_ascii_lowercase();
    let (local, domain) = m.split_once('@').unwrap_or((&m, ""));
    if matches!(local, "git" | "noreply" | "no-reply") || domain.ends_with("users.noreply.github.com") {
        return false;
    }
    const DOC: &[&str] = &["example.com", "example.org", "example.net", "localhost"];
    if DOC.iter().any(|d| domain == *d || domain.ends_with(&format!(".{d}"))) {
        return false;
    }
    let tld = domain.rsplit('.').next().unwrap_or("");
    const FILE_EXT: &[&str] = &[
        "png", "jpg", "jpeg", "gif", "svg", "webp", "ico", "js", "mjs", "ts", "tsx", "jsx", "rs", "py", "md", "json",
        "toml", "lock", "yaml", "yml", "css", "html", "txt",
    ];
    !FILE_EXT.contains(&tld)
}

/// A 15- or 16-digit card number with an issuer prefix that passes Luhn.
fn card(c: &Captures<'_>, _: &str) -> bool {
    let digits: Vec<u32> = c[0].chars().filter_map(|ch| ch.to_digit(10)).collect();
    if !(digits.len() == 15 || digits.len() == 16) || !matches!(digits[0], 3..=6) {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| if i % 2 == 1 { let x = d * 2; if x > 9 { x - 9 } else { x } } else { d })
        .sum();
    sum.is_multiple_of(10)
}

/// US SSN shape with an issuable area and non-zero group and serial.
fn ssn(c: &Captures<'_>, _: &str) -> bool {
    let s = &c[0];
    let area: u32 = s[0..3].parse().unwrap_or(0);
    area != 0 && area != 666 && area < 900 && &s[4..6] != "00" && &s[7..11] != "0000"
}

/// A public IPv4 address: not private, loopback, link-local, CGNAT, multicast, reserved or a
/// documentation range.
fn public_ip(c: &Captures<'_>, hay: &str) -> bool {
    let m = c.get(0).expect("match");
    // `1.2.3.4.5` is a version or an OID, not an address.
    let before = hay[..m.start()].chars().next_back();
    let after = hay[m.end()..].chars().next();
    let dotted = |ch: Option<char>| ch == Some('.') || ch.is_some_and(|ch| ch.is_ascii_digit());
    if dotted(before) || (after == Some('.') && hay[m.end() + 1..].starts_with(|ch: char| ch.is_ascii_digit())) {
        return false;
    }
    // `v1.2.3.4`, `version 1.2.3.4`.
    let mut cut = m.start().saturating_sub(8);
    while !hay.is_char_boundary(cut) {
        cut -= 1;
    }
    let lead = hay[cut..m.start()].to_ascii_lowercase();
    if lead.ends_with('v') || lead.ends_with("version ") || lead.ends_with("ver ") {
        return false;
    }
    let o: Vec<u32> = m.as_str().split('.').filter_map(|p| p.parse().ok()).collect();
    if o.len() != 4 || o.iter().any(|&x| x > 255) {
        return false;
    }
    let private = o[0] == 0
        || o[0] == 10
        || o[0] == 127
        || o[0] >= 224
        || (o[0] == 100 && (64..128).contains(&o[1]))
        || (o[0] == 169 && o[1] == 254)
        || (o[0] == 172 && (16..32).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168)
        || (o[0] == 192 && o[1] == 0 && o[2] == 2)
        || (o[0] == 198 && o[1] == 51 && o[2] == 100)
        || (o[0] == 203 && o[1] == 0 && o[2] == 113)
        || (o[0] == 198 && (18..20).contains(&o[1]));
    !private
}

/// Shannon entropy in bits per character.
pub(super) fn entropy(s: &str) -> f64 {
    let mut counts = [0u32; 256];
    let mut n = 0u32;
    for b in s.bytes() {
        counts[b as usize] += 1;
        n += 1;
    }
    if n == 0 {
        return 0.0;
    }
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n as f64;
            -p * p.log2()
        })
        .sum()
}

/// A token that looks random rather than written: mixed classes that switch often, high entropy.
/// Pure hex is left alone (git shas, content hashes, UUIDs are everywhere in coding transcripts and
/// are not credentials); hex secrets are caught by their key, flag or a known value instead.
fn random_token(c: &Captures<'_>, _: &str) -> bool {
    let t = &c[0];
    if is_marker(t) {
        return false;
    }
    // Score each run of letters and digits separately: `crate/module_name/v2` is three words.
    t.split(|ch: char| !ch.is_ascii_alphanumeric()).any(random_run) || (t.len() >= 32 && random_run_joined(t))
}

fn class(ch: char) -> u8 {
    if ch.is_ascii_uppercase() {
        0
    } else if ch.is_ascii_lowercase() {
        1
    } else {
        2
    }
}

fn switches(s: &str) -> usize {
    let cl: Vec<u8> = s.chars().map(class).collect();
    cl.windows(2).filter(|w| w[0] != w[1]).count()
}

fn random_run(s: &str) -> bool {
    let n = s.len();
    if n < 20 || s.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return false;
    }
    let digits = s.chars().filter(|c| c.is_ascii_digit()).count();
    let upper = s.chars().filter(|c| c.is_ascii_uppercase()).count();
    let lower = s.chars().filter(|c| c.is_ascii_lowercase()).count();
    let sw = switches(s);
    let e = entropy(s);
    // Identifiers are made of words (`Utf8ToUtf16ConverterImpl`); keys are not, though random
    // base62 still forms pseudo-words over a third or more of its length. Long tokens are
    // identifiers only when nearly all words; short ones must also scatter their digits.
    let cover = word_cover(s);
    if n >= 32 {
        if cover * 10 >= n * 7 {
            return false;
        }
    } else if cover * 2 >= n || digit_runs(s) < 2 {
        return false;
    }
    // Mixed case with digits (base62/base64 keys).
    let mixed = digits > 0 && upper > 0 && lower > 0 && sw * 10 >= n * 3 && e >= 3.8;
    // One case plus digits (base36 keys), held to a longer, denser bar.
    let single = n >= 24 && digits * 5 >= n && (upper + lower) * 5 >= n && sw * 10 >= n * 3 && e >= 3.8;
    mixed || single
}

fn digit_runs(s: &str) -> usize {
    let b = s.as_bytes();
    (0..b.len()).filter(|&i| b[i].is_ascii_digit() && (i == 0 || !b[i - 1].is_ascii_digit())).count()
}

/// Characters inside word-like runs: a capital or nothing, then three or more lower-case letters
/// (or four or more capitals, for `HTTPS`-style acronyms).
fn word_cover(s: &str) -> usize {
    let b = s.as_bytes();
    let mut i = 0;
    let mut cover = 0;
    while i < b.len() {
        let start = i;
        if b[i].is_ascii_uppercase() {
            let mut j = i;
            while j < b.len() && b[j].is_ascii_uppercase() {
                j += 1;
            }
            let mut k = j;
            while k < b.len() && b[k].is_ascii_lowercase() {
                k += 1;
            }
            if k - j >= 3 {
                cover += k - (j - 1);
                i = k;
                continue;
            }
            if j - start >= 4 {
                cover += j - start;
            }
            i = j.max(start + 1);
        } else if b[i].is_ascii_lowercase() {
            let mut k = i;
            while k < b.len() && b[k].is_ascii_lowercase() {
                k += 1;
            }
            if k - i >= 3 {
                cover += k - i;
            }
            i = k;
        } else {
            i += 1;
        }
    }
    cover
}

/// Base64 with separators (`+`, `/`, `-`, `_`) inside: judge the token whole.
fn random_run_joined(t: &str) -> bool {
    let alnum: String = t.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let seps = t.len() - alnum.len();
    // Paths and kebab identifiers have long words between separators; base64 does not care.
    let longest_word = t.split(|ch: char| !ch.is_ascii_alphanumeric()).map(str::len).max().unwrap_or(0);
    seps * 8 <= t.len() && longest_word >= 8 && random_run(&alnum)
}
