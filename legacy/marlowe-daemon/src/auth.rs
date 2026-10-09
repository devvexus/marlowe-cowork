//! Connection authentication for the daemon socket.
//!
//! # What this defends against, stated precisely
//!
//! **Not** "malware running as you". A process running as your user does not need Marlowe — it can
//! read your files and spawn its own shell directly, and going through the agent is a strictly
//! worse path for it. That threat is out of scope and no token changes it.
//!
//! What it defends is **other users on the same machine**. Loopback is per-machine, not per-user:
//! on a shared box, an RDP host or a terminal server, user B can connect to user A's daemon port
//! and drive an agent holding user A's filesystem and shell access. That is a genuine cross-user
//! privilege escalation, and it is the case this closes.
//!
//! # Why a token and not a random port
//!
//! A random port is discovery, not authentication — 65,536 ports scan in seconds. The token lives
//! in the profile root, which is under `%LOCALAPPDATA%` (Windows) or the XDG data dir (POSIX):
//! directories the OS already makes unreadable to other users. So the secret inherits an ACL that
//! is enforced by the kernel rather than by us.
//!
//! # What it deliberately is not
//!
//! Not a defence against a debugger, another process running as the same user, or anyone who can
//! read the profile root. Those all have the token by construction, and if they can read the
//! profile root they can also read `profile.key` and the journal — so a token would be the least
//! of it. The boundary this draws is exactly the one the filesystem already draws.

use std::io;
use std::path::{Path, PathBuf};

/// How a refusal is recognised on the wire.
///
/// Defined once and read by both halves — the daemon prefixes its refusal event with it and
/// [`crate::client::Client`] matches on it. A sentence matched by eye in two files is a string
/// that drifts the first time someone improves the wording, and the half that stops matching
/// fails open into "some unexplained error" rather than "you were refused".
pub const REFUSED: &str = "unauthenticated:";

/// What the user is told when the token does not match.
pub fn refusal() -> String {
    format!(
        "{REFUSED} this daemon is serving a different profile, or a different user. \
         A client authenticates with the token in its own profile root; run `marlowe --serve` \
         for this profile, or point the client at the right one with --profile-root."
    )
}

/// Where the token lives, beside the journal it protects access to.
pub fn token_path(profile_root: &Path) -> PathBuf {
    profile_root.join("daemon.token")
}

/// Read the existing token, or mint one.
///
/// **Minted per profile, not per start.** A client that reconnects to a running daemon has to be
/// able to read the same value, and a token that rotated on every start would break the reconnect
/// path that invariant 6 exists to provide.
pub fn ensure_token(profile_root: &Path) -> io::Result<String> {
    let path = token_path(profile_root);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim().to_string();
        if trimmed.len() >= 32 {
            return Ok(trimmed);
        }
    }
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &token)?;
    restrict(&path)?;
    Ok(token)
}

/// Read a token without creating one. `None` means no daemon has ever run for this profile.
pub fn read_token(profile_root: &Path) -> Option<String> {
    std::fs::read_to_string(token_path(profile_root))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| s.len() >= 32)
}

/// Owner-only permissions where the platform expresses them in the file itself.
///
/// On Windows the containing directory is already per-user, and setting a DACL here would need
/// `windows-sys`, which this crate does not depend on — so the guarantee there is the parent
/// directory's ACL, which is the same one protecting `profile.key`.
#[cfg(unix)]
fn restrict(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Constant-time equality.
///
/// The comparison is on a 64-character hex string, so a short-circuiting `==` leaks a prefix length
/// through timing. That is a small channel over loopback and it costs nothing to close.
pub fn matches(expected: &str, offered: &str) -> bool {
    if expected.len() != offered.len() {
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

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("marlowe-auth-{name}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn a_token_is_minted_once_and_reread_thereafter() {
        let root = tmp("mint");
        let a = ensure_token(&root).unwrap();
        let b = ensure_token(&root).unwrap();
        assert_eq!(a, b, "a rotating token would break the reconnect path");
        assert_eq!(a.len(), 64);
        assert_eq!(read_token(&root).as_deref(), Some(a.as_str()));
    }

    #[test]
    fn two_profiles_get_different_tokens() {
        assert_ne!(
            ensure_token(&tmp("p1")).unwrap(),
            ensure_token(&tmp("p2")).unwrap()
        );
    }

    #[test]
    fn no_token_means_no_daemon_has_run_here() {
        assert!(read_token(&tmp("empty")).is_none());
    }

    #[test]
    fn a_truncated_token_file_is_replaced_rather_than_accepted() {
        let root = tmp("short");
        std::fs::write(token_path(&root), "deadbeef").unwrap();
        assert!(read_token(&root).is_none(), "a short value must not authenticate");
        assert_eq!(ensure_token(&root).unwrap().len(), 64);
    }

    #[test]
    fn comparison_rejects_every_near_miss() {
        let t = ensure_token(&tmp("cmp")).unwrap();
        assert!(matches(&t, &t));
        assert!(!matches(&t, ""));
        assert!(!matches(&t, &t[..63]));
        let mut wrong = t.clone();
        wrong.pop();
        wrong.push(if t.ends_with('a') { 'b' } else { 'a' });
        assert!(!matches(&t, &wrong), "one differing character must fail");
    }
}
