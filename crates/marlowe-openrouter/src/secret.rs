//! The API key, and the machinery that keeps it out of everything a person or a model can read.
//!
//! # This is an interim, and M5 is what replaces it
//!
//! STATE.md's ruling for this session was *"build the provider adapter; do NOT build the
//! credential broker"*. The broker is M5, and M5's acceptance row is **"credential exposure to
//! model context: zero, structurally enforced and tested"**. So the key arrives from one
//! environment variable, is read once at startup, and is held in [`ApiKey`] — a type that
//! deliberately cannot be printed.
//!
//! **What M5 replaces:** the *sourcing* and the *lifetime*. A broker owns storage, rotation,
//! per-tool scoping and an audit trail of every use; none of that exists here and none of it is
//! faked. What M5 does **not** need to replace is this file's one property — that the secret has
//! no `Display`, that its `Debug` cannot print it, and that every string leaving this crate goes
//! through [`Redact`]. Those should survive the broker, because a broker that hands out a
//! `String` has moved the leak rather than closed it.
//!
//! # Why an environment variable and not a flag
//!
//! A `--openrouter-key` flag lands in shell history, in `ps` output, and in any CI log that
//! echoes its own command line. The variable is not *safe*; it is **less bad**, and the
//! difference is stated rather than glossed. It is read exactly once, by
//! [`ApiKey::from_environment`], and the process never puts it anywhere else.

/// The environment variable, named in one place so the refusal message and the reader cannot
/// disagree about what to set.
pub const KEY_VAR: &str = "OPENROUTER_API_KEY";

/// An API key.
///
/// # The properties, and where each one is enforced
///
/// | Property | Enforced by |
/// |---|---|
/// | `{}` cannot print it | there is **no** `Display` impl, so `format!("{key}")` does not compile |
/// | `{:?}` cannot print it | the hand-written [`std::fmt::Debug`] below, which is what a panic, an `unwrap`, a `dbg!` and every derived `Debug` on a struct containing one will use |
/// | it cannot be serialised | no `Serialize`, so it cannot reach the journal, a run record or a wire message by being a field of something that can |
/// | it cannot be compared into existence | no `PartialEq`; a timing oracle is not the risk here, but an accidental `assert_eq!` printing both sides is |
///
/// The single way to obtain the bytes is [`ApiKey::expose_for_authorization_header`], whose name
/// is the review prompt: a call site that is not writing an `Authorization` header is wrong, and
/// a grep for the method name finds every one of them. There are **two** in this crate.
#[derive(Clone)]
pub struct ApiKey {
    secret: String,
}

impl ApiKey {
    /// Read the key from the environment. **The only reader.**
    ///
    /// `Err` names the variable, because "no credential" is not something a user can act on.
    /// An empty or whitespace-only value is treated as absent: a variable exported as `""` is the
    /// permissive-default shape — it looks configured, refuses on the first call, and the refusal
    /// arrives a network round trip away from the mistake.
    pub fn from_environment() -> Result<Self, KeyMissing> {
        match std::env::var(KEY_VAR) {
            Ok(v) if !v.trim().is_empty() => Ok(Self { secret: v.trim().to_string() }),
            Ok(_) => Err(KeyMissing { var: KEY_VAR, empty: true }),
            Err(_) => Err(KeyMissing { var: KEY_VAR, empty: false }),
        }
    }

    /// Construct from a string. **Tests and the live probe only** — nothing in the product calls
    /// this, because the product has exactly one source and it is the environment.
    pub fn from_secret(secret: &str) -> Self {
        Self { secret: secret.to_string() }
    }

    /// The bytes, for the one header that needs them.
    ///
    /// Named so that every call site is a sentence about what it is doing, and so that a grep for
    /// `expose_for_authorization_header` enumerates the whole exposure surface.
    pub fn expose_for_authorization_header(&self) -> &str {
        &self.secret
    }

    /// A stable, non-reversing handle for logs and run records.
    ///
    /// **Not a hash and not a prefix of the secret.** OpenRouter keys begin `sk-or-v1-`, so a
    /// "first eight characters" fingerprint is the same eight characters for every key on earth
    /// — an identifier that identifies nothing, which is worse than none because it looks like
    /// evidence. This is the length and a checksum over the whole value: two keys differ in it,
    /// and it does not narrow a search for the key itself.
    pub fn fingerprint(&self) -> String {
        // FNV-1a, 64-bit. Not a cryptographic digest and not offered as one; it exists so two
        // runs can be told apart, not so a key can be verified.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.secret.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("len={} fnv={h:016x}", self.secret.len())
    }

    /// Remove this key from `text`, wherever it appears.
    ///
    /// # This is the belt to the type's braces, and it exists because of upstream echoes
    ///
    /// The type system stops *us* printing the key. It cannot stop a server from quoting the
    /// credential it was sent back at us inside an error body — which several APIs do, and which
    /// then travels into a `ProviderError`, onto the user's screen, and into the journal. So
    /// every string this crate lets out of an error path passes through here.
    pub fn redact(&self, text: &str) -> String {
        if self.secret.is_empty() {
            return text.to_string();
        }
        text.replace(&self.secret, REDACTED)
    }
}

/// What replaces a key in a redacted string. A literal, so a test can look for it and a reader
/// can tell redaction from absence.
pub const REDACTED: &str = "<OPENROUTER_API_KEY redacted>";

impl std::fmt::Debug for ApiKey {
    /// **The property, at the site where it is enforced.**
    ///
    /// A derived `Debug` here would print the secret from every `{:?}` in the process, including
    /// ones nobody wrote: a `#[derive(Debug)]` on any struct holding an `ApiKey`, an
    /// `unwrap()` on a `Result<_, SomethingHoldingAKey>`, a `dbg!`, and `assert_eq!`'s failure
    /// message. This is the one impl that decides all of them.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ApiKey({REDACTED}, {})", self.fingerprint())
    }
}

/// No key. **A load-time refusal, never a fallback.**
///
/// CLAUDE.md's standing rule: prefer a load-time error to a sensible default. The tempting
/// default here — quietly using Ollama when the key is missing — would mean a benchmark run
/// launched against a frontier model silently measuring a local 9B, with every label in the
/// output reading the name that was asked for.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "`{var}` {} — OpenRouter needs an API key and this build has no credential broker to find one \
     with (that is M5). Get a key at https://openrouter.ai/keys and export it:\n    \
     export {var}=sk-or-v1-...\nMarlowe does NOT fall back to the local model when the key is \
     missing: a benchmark that silently measured a different model would be worse than one that \
     refused to start.",
    if *.empty { "is set but empty" } else { "is not set" }
)]
pub struct KeyMissing {
    pub var: &'static str,
    pub empty: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "sk-or-v1-0123456789abcdef0123456789abcdef";

    #[test]
    fn debug_cannot_print_the_key_and_neither_can_a_struct_that_holds_one() {
        // `{:?}` is not a formatting choice — it is what a panic, an `unwrap`, a `dbg!` and every
        // DERIVED Debug reach for. Asserting it here asserts all of those at once.
        let key = ApiKey::from_secret(SECRET);
        let rendered = format!("{key:?}");
        assert!(!rendered.contains(SECRET), "the key printed: {rendered}");
        assert!(rendered.contains(REDACTED), "{rendered}");

        // The case the type exists for: a derived `Debug` on something containing a key.
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Holder {
            key: ApiKey,
            model: &'static str,
        }
        let rendered = format!("{:?}", Holder { key, model: "anthropic/claude-sonnet-4.5" });
        assert!(
            !rendered.contains(SECRET),
            "a derived Debug leaked the key through its field: {rendered}"
        );
    }

    #[test]
    fn the_fingerprint_is_not_a_prefix_of_the_key() {
        // Every OpenRouter key starts `sk-or-v1-`, so a prefix fingerprint identifies nothing and
        // still narrows a brute force. The fingerprint must contain no run of the secret.
        let key = ApiKey::from_secret(SECRET);
        let fp = key.fingerprint();
        assert!(!fp.contains("sk-or"), "{fp}");
        for window in SECRET.as_bytes().windows(6) {
            let w = std::str::from_utf8(window).unwrap();
            assert!(!fp.contains(w), "the fingerprint contains {w:?} from the key: {fp}");
        }
        // ...and it still distinguishes two keys, or it is not doing its job.
        assert_ne!(fp, ApiKey::from_secret("sk-or-v1-different").fingerprint());
    }

    #[test]
    fn redaction_removes_a_key_an_upstream_echoed_back() {
        // The case the type system cannot reach: the server quotes our own credential in its
        // error body.
        let key = ApiKey::from_secret(SECRET);
        let echoed = format!("{{\"error\":{{\"message\":\"invalid key {SECRET}\"}}}}");
        let clean = key.redact(&echoed);
        assert!(!clean.contains(SECRET), "{clean}");
        assert!(clean.contains(REDACTED), "{clean}");
    }

    #[test]
    fn an_empty_variable_is_absent_rather_than_a_key() {
        // `export OPENROUTER_API_KEY=` looks configured and fails a round trip later. The
        // distinction is carried into the message so the remedy differs.
        let set_but_empty = KeyMissing { var: KEY_VAR, empty: true };
        let unset = KeyMissing { var: KEY_VAR, empty: false };
        assert!(set_but_empty.to_string().contains("is set but empty"));
        assert!(unset.to_string().contains("is not set"));
        for e in [&set_but_empty, &unset] {
            assert!(e.to_string().contains(KEY_VAR));
            assert!(
                e.to_string().contains("does NOT fall back"),
                "the refusal must say that it is a refusal: {e}"
            );
        }
    }
}
