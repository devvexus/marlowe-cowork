//! The API key: no `Display`, no `Serialize`, a `Debug` that cannot print it, and a redactor for
//! upstream error bodies that echo it back.

/// The environment variable read when no stored credential is supplied.
pub const KEY_VAR: &str = "OPENROUTER_API_KEY";

/// Replaces the key wherever an upstream quoted it.
pub const REDACTED: &str = "<OPENROUTER_API_KEY redacted>";

#[derive(Clone)]
pub struct ApiKey {
    secret: String,
}

impl ApiKey {
    /// `None` for an empty or whitespace secret.
    pub fn new(secret: &str) -> Option<Self> {
        let s = secret.trim();
        (!s.is_empty()).then(|| Self { secret: s.to_string() })
    }

    /// From [`KEY_VAR`], when set and non-empty.
    pub fn from_environment() -> Option<Self> {
        std::env::var(KEY_VAR).ok().and_then(|v| Self::new(&v))
    }

    /// For tests and callers that already validated the value.
    pub fn from_secret(secret: &str) -> Self {
        Self { secret: secret.to_string() }
    }

    /// The only accessor. Used for the `Authorization` header and nothing else.
    pub fn expose_for_authorization_header(&self) -> &str {
        &self.secret
    }

    /// A non-reversible fingerprint (length + FNV-1a) for telling two keys apart in logs.
    pub fn fingerprint(&self) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.secret.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("len={} fnv={h:016x}", self.secret.len())
    }

    /// `text` with every occurrence of the key replaced by [`REDACTED`].
    pub fn redact(&self, text: &str) -> String {
        if self.secret.is_empty() {
            return text.to_string();
        }
        text.replace(&self.secret, REDACTED)
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ApiKey({REDACTED}, {})", self.fingerprint())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "sk-or-v1-0123456789abcdef0123456789abcdef";

    #[test]
    fn debug_cannot_print_the_key_and_neither_can_a_struct_that_holds_one() {
        let key = ApiKey::from_secret(SECRET);
        let rendered = format!("{key:?}");
        assert!(!rendered.contains(SECRET), "{rendered}");
        assert!(rendered.contains(REDACTED));

        #[derive(Debug)]
        #[allow(dead_code)]
        struct Holder {
            key: ApiKey,
        }
        assert!(!format!("{:?}", Holder { key }).contains(SECRET));
    }

    #[test]
    fn the_fingerprint_is_not_a_prefix_of_the_key() {
        let fp = ApiKey::from_secret(SECRET).fingerprint();
        for window in SECRET.as_bytes().windows(6) {
            let w = std::str::from_utf8(window).unwrap();
            assert!(!fp.contains(w), "{fp} contains {w:?}");
        }
        assert_ne!(fp, ApiKey::from_secret("sk-or-v1-different").fingerprint());
    }

    #[test]
    fn redaction_removes_a_key_an_upstream_echoed_back() {
        let key = ApiKey::from_secret(SECRET);
        let clean = key.redact(&format!("{{\"error\":{{\"message\":\"invalid key {SECRET}\"}}}}"));
        assert!(!clean.contains(SECRET) && clean.contains(REDACTED), "{clean}");
    }

    #[test]
    fn an_empty_secret_is_not_a_key() {
        assert!(ApiKey::new("  ").is_none());
        assert!(ApiKey::new(" sk-or-v1-x ").is_some());
    }
}
