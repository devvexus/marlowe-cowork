//! What this machine knows that must never leave it: stored API keys, secret-named environment
//! variables, and who the user is. Gathered once per export; tests build [`ScrubInputs`] directly.

use std::path::Path;

use marlowe_brains::credentials::{CredentialStore, KeyringStore};
use marlowe_model::ProviderKind;

use super::config::ExportConfig;
use super::scrub::{Identity, ScrubInputs};

/// Environment variable names whose values are treated as known secrets.
fn secret_named(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    marlowe_os::child::SCRUBBED_ENV.contains(&n.as_str())
        || ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL", "PAT"]
            .iter()
            .any(|w| n.split('_').any(|part| part == *w) || n.ends_with(&format!("_{w}")))
}

/// A path or socket, not a secret: the name says so (`KEY_FILE`, `SSH_AUTH_SOCK`), or the value
/// is an absolute path that exists on this machine.
fn is_path_value(name: &str, v: &str) -> bool {
    let n = name.to_ascii_uppercase();
    if ["_PATH", "_DIR", "_FILE", "_SOCK", "_SOCKET"].iter().any(|s| n.ends_with(s)) {
        return true;
    }
    let b = v.as_bytes();
    let rooted = v.starts_with(['/', '\\'])
        || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/'));
    rooted && Path::new(v).exists()
}

/// Known secrets: every stored provider key plus secret-named environment values.
pub fn known_secrets(store: &dyn CredentialStore, env: impl Iterator<Item = (String, String)>) -> Vec<String> {
    let mut out: Vec<String> = ProviderKind::ALL
        .iter()
        .filter(|k| !k.is_local())
        .filter_map(|&k| store.get(k).ok().flatten())
        .collect();
    for (name, value) in env {
        let v = value.trim();
        // Inclusive: base64 secrets carry `/` and `+`, passphrases carry spaces. Only a value that
        // is clearly a path is left out.
        if secret_named(&name) && v.len() >= 8 && !is_path_value(&name, v) {
            out.push(v.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// `name` and `email` from a git config file's `[user]` section.
fn git_identity(text: &str) -> (Vec<String>, Vec<String>) {
    let (mut names, mut emails) = (Vec::new(), Vec::new());
    let mut in_user = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_user = l.trim_start_matches('[').trim_end_matches(']').trim().eq_ignore_ascii_case("user");
            continue;
        }
        if !in_user {
            continue;
        }
        if let Some((k, v)) = l.split_once('=') {
            let v = v.trim().trim_matches('"').to_string();
            match k.trim().to_ascii_lowercase().as_str() {
                "name" if !v.is_empty() => names.push(v),
                "email" if !v.is_empty() => emails.push(v),
                _ => {}
            }
        }
    }
    (names, emails)
}

/// The user's identity: `user.json`, the OS account, host, home directory and git identity.
pub fn identity(home: &Path) -> Identity {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let mut id = Identity::default();
    if let Some(u) = marlowe_daemon::user::load(home) {
        id.names.extend(u.name);
    }
    for k in ["USERNAME", "USER", "LOGNAME"] {
        id.accounts.extend(var(k));
    }
    for k in ["COMPUTERNAME", "HOSTNAME", "USERDOMAIN"] {
        id.hosts.extend(var(k));
    }
    let user_home = var("USERPROFILE").or_else(|| var("HOME"));
    if let Some(h) = &user_home {
        id.homes.push(h.clone());
        if let Some(last) = Path::new(h).file_name() {
            id.accounts.push(last.to_string_lossy().to_string());
        }
        for f in [".gitconfig", ".config/git/config"] {
            if let Ok(text) = std::fs::read_to_string(Path::new(h).join(f)) {
                let (n, e) = git_identity(&text);
                id.names.extend(n);
                id.emails.extend(e);
            }
        }
    }
    for v in [&mut id.names, &mut id.accounts, &mut id.hosts, &mut id.emails, &mut id.homes] {
        v.sort();
        v.dedup();
    }
    id
}

/// The scrubber's inputs for this machine and config.
pub fn gather(home: &Path, cfg: &ExportConfig) -> ScrubInputs {
    ScrubInputs {
        known_secrets: known_secrets(&KeyringStore::new(), std::env::vars()),
        identity: identity(home),
        redact_terms: cfg.redact_terms.clone(),
        allow_terms: cfg.allow_terms.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_brains::credentials::MemoryStore;

    #[test]
    fn known_secrets_take_stored_keys_and_secret_named_env_values() {
        let store = MemoryStore::default();
        store.set(ProviderKind::OpenRouter, "stored-openrouter-value-123").unwrap();
        let env = vec![
            ("WANDB_API_KEY".to_string(), "0123456789abcdef0123456789abcdef01234567".to_string()),
            ("GH_TOKEN".to_string(), "short".to_string()),
            ("MY_SERVICE_SECRET".to_string(), "Zq81mPq0vLx2".to_string()),
            ("PATH".to_string(), "C:\\Windows".to_string()),
            ("SSH_AUTH_SOCK".to_string(), "/tmp/agent.sock".to_string()),
            ("MONKEY".to_string(), "not-a-secret-name".to_string()),
        ];
        let tmp = std::env::temp_dir().to_string_lossy().to_string();
        let env = env.into_iter().chain([
            // About 70% of AWS secret keys hold a `/`.
            ("AWS_SECRET_ACCESS_KEY".to_string(), "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string()),
            ("DB_PASSWORD".to_string(), "correct horse battery staple".to_string()),
            ("SIGNING_KEY_FILE".to_string(), "keys/signing.pem".to_string()),
            ("API_TOKEN_DIR".to_string(), "/var/lib/tokens".to_string()),
            ("MY_SECRET".to_string(), tmp.clone()),
        ]);
        let k = known_secrets(&store, env);
        assert!(k.contains(&"wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string()));
        assert!(k.contains(&"correct horse battery staple".to_string()));
        assert!(!k.iter().any(|v| v.contains("signing.pem") || v.contains("/var/lib/tokens") || *v == tmp), "{k:?}");
        assert!(k.contains(&"stored-openrouter-value-123".to_string()));
        assert!(k.contains(&"0123456789abcdef0123456789abcdef01234567".to_string()));
        assert!(k.contains(&"Zq81mPq0vLx2".to_string()));
        assert!(!k.iter().any(|v| v == "short" || v.contains("Windows") || v.contains("agent.sock") || v.contains("not-a-secret")));
    }

    #[test]
    fn git_identity_reads_the_user_section_only() {
        let text = "[core]\n\tname = nope\n[user]\n\tname = Ada Lovelace\n\temail = \"ada@analytical.engine\"\n[alias]\n\temail = x\n";
        let (n, e) = git_identity(text);
        assert_eq!(n, vec!["Ada Lovelace"]);
        assert_eq!(e, vec!["ada@analytical.engine"]);
    }
}
