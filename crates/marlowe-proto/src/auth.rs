//! The connection token: minted once per profile home, compared in constant time.

use std::io;

/// Minimum accepted token length (hex chars).
pub const MIN_TOKEN_LEN: usize = 32;

/// The refusal sent before closing an unauthenticated connection.
pub const REFUSED: &str = "unauthenticated: this daemon serves a different profile home";

/// A fresh 256-bit token as 64 hex chars.
pub fn mint_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Constant-time comparison. A token shorter than [`MIN_TOKEN_LEN`] never matches.
pub fn token_matches(expected: &str, offered: &str) -> bool {
    if expected.len() < MIN_TOKEN_LEN || expected.len() != offered.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in expected.bytes().zip(offered.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_unique_and_match_only_themselves() {
        let a = mint_token().unwrap();
        let b = mint_token().unwrap();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(token_matches(&a, &a));
        assert!(!token_matches(&a, &b));
        assert!(!token_matches(&a, ""));
        assert!(!token_matches(&a, &a[..63]));
        assert!(!token_matches("short", "short"), "a short expected token never authenticates");
    }
}
