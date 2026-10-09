//! Write-time integrity binding — CONTRACTS.md section 1.
//!
//! *"`signature`: HMAC over (seq, ts, trace, actor, kind, payload, prev_signature)."*
//!
//! The log is a **hash chain**, and section 1 (amended 2026-08-03) specifies it as such. Per-
//! event signing alone stops forgery; chaining additionally means an event cannot be removed
//! or reordered without every signature after it failing. Section 1 carries the reasoning —
//! in short, if a row can be deleted undetectably then `DELETE` is an implementation of
//! forgetting that destroys availability and leaves no trace of having run.
//!
//! This module implements the pinned scheme. It is not a local strengthening of it: the
//! contract was amended so that a session reading CONTRACTS.md sees the chain, rather than
//! finding the code and the contract in contradiction.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::event::{EventKind, Seq, TraceId};

type HmacSha256 = Hmac<Sha256>;

/// The signature of the empty chain. Every profile's first event chains from this.
pub const GENESIS: &str = "genesis";

/// A per-profile HMAC key. 32 bytes.
///
/// Not `Debug` and not `Serialize`: a key that can be printed is a key that ends up in a log
/// line. Same discipline as `AuthzToken` in section 11 — the type system, not care, is what
/// keeps it out.
#[derive(Clone)]
pub struct SigningKey([u8; 32]);

impl SigningKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes)?;
        Ok(Self(bytes))
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (i, chunk) in text.as_bytes().chunks(2).enumerate() {
            let pair = std::str::from_utf8(chunk).ok()?;
            bytes[i] = u8::from_str_radix(pair, 16).ok()?;
        }
        Some(Self(bytes))
    }
}

/// Everything the signature covers, in a fixed order.
pub struct SignedFields<'a> {
    pub seq: Seq,
    pub ts: i64,
    pub trace_id: &'a TraceId,
    pub session_id: Option<&'a str>,
    pub run_id: Option<&'a str>,
    /// **The canonical actor string, not an `Actor`.**
    ///
    /// Signing and verification must feed the MAC identical bytes. If this held a
    /// structured `Actor`, verification — which reads a string back out of the database —
    /// would need to parse it into a variant first, and that parser could disagree with
    /// [`Actor::canonical`] for any actor whose fields contain the separator. The
    /// disagreement would surface as a phantom chain break: an event that verifies at write
    /// time and fails at read time, with the log looking tampered when it is intact.
    ///
    /// Carrying the string means there is exactly one encoder and both paths use it.
    pub actor_canonical: &'a str,
    pub kind: EventKind,
    /// The **exact serialized payload bytes** as they are stored, not a re-serialization.
    ///
    /// Signing the stored string rather than a re-encoded structure removes a whole class of
    /// canonicalization bug: there is no second encoder whose output could differ from the
    /// first while both look correct.
    pub payload_json: &'a str,
    pub prev_signature: &'a str,
}

pub fn sign(key: &SigningKey, fields: &SignedFields<'_>) -> String {
    let mut mac = HmacSha256::new_from_slice(&key.0).expect("HMAC accepts any key length");

    // Length-prefixed field framing. Concatenating with a separator would let two different
    // field sets produce identical input whenever a value contains the separator -- the
    // classic length-extension-adjacent mistake in ad-hoc MAC construction.
    let mut feed = |bytes: &[u8]| {
        mac.update(&(bytes.len() as u64).to_be_bytes());
        mac.update(bytes);
    };

    feed(&fields.seq.to_be_bytes());
    feed(&fields.ts.to_be_bytes());
    feed(fields.trace_id.as_bytes());
    feed(fields.session_id.unwrap_or("").as_bytes());
    feed(fields.run_id.unwrap_or("").as_bytes());
    feed(fields.actor_canonical.as_bytes());
    feed(fields.kind.as_str().as_bytes());
    feed(fields.payload_json.as_bytes());
    feed(fields.prev_signature.as_bytes());

    let out = mac.finalize().into_bytes();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn verify(key: &SigningKey, fields: &SignedFields<'_>, expected: &str) -> bool {
    // Constant-time comparison: `String == String` short-circuits on the first differing
    // byte, which leaks position under timing. Cheap to do right.
    let actual = sign(key, fields);
    if actual.len() != expected.len() {
        return false;
    }
    actual
        .bytes()
        .zip(expected.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields<'a>(payload: &'a str, prev: &'a str, trace: &'a TraceId) -> SignedFields<'a> {
        SignedFields {
            seq: 1,
            ts: 1_780_000_000_000,
            trace_id: trace,
            session_id: Some("s-1"),
            run_id: None,
            actor_canonical: "harness",
            kind: EventKind::MemoryWritten,
            payload_json: payload,
            prev_signature: prev,
        }
    }

    #[test]
    fn signature_detects_a_changed_payload() {
        let key = SigningKey::from_bytes([7u8; 32]);
        let trace = TraceId::nil();
        let sig = sign(&key, &fields("{\"a\":1}", GENESIS, &trace));
        assert!(verify(&key, &fields("{\"a\":1}", GENESIS, &trace), &sig));
        assert!(
            !verify(&key, &fields("{\"a\":2}", GENESIS, &trace), &sig),
            "a payload edit must invalidate the signature"
        );
    }

    #[test]
    fn chaining_detects_a_removed_predecessor() {
        // The reason for mixing prev_signature in: deleting or reordering an earlier event
        // must break every signature after it, or forgetting could be achieved with DELETE.
        let key = SigningKey::from_bytes([7u8; 32]);
        let trace = TraceId::nil();
        let sig = sign(&key, &fields("{}", "abc123", &trace));
        assert!(
            !verify(&key, &fields("{}", GENESIS, &trace), &sig),
            "re-chaining an event onto a different predecessor must fail"
        );
    }

    #[test]
    fn field_framing_is_unambiguous() {
        // Without length prefixes these two would hash identically: the boundary between
        // session_id and run_id would be invisible.
        let key = SigningKey::from_bytes([9u8; 32]);
        let trace = TraceId::nil();
        let mk = |session: &'static str, run: &'static str| SignedFields {
            seq: 1,
            ts: 0,
            trace_id: &trace,
            session_id: Some(session),
            run_id: Some(run),
            actor_canonical: "harness",
            kind: EventKind::MemoryWritten,
            payload_json: "{}",
            prev_signature: GENESIS,
        };
        assert_ne!(sign(&key, &mk("ab", "c")), sign(&key, &mk("a", "bc")));
    }

    #[test]
    fn key_hex_round_trips() {
        let key = SigningKey::from_bytes([0xab; 32]);
        let restored = SigningKey::from_hex(&key.to_hex()).expect("valid hex");
        assert_eq!(key.to_hex(), restored.to_hex());
        assert!(SigningKey::from_hex("too short").is_none());
    }
}
