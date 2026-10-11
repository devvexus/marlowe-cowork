//! The profile root: what a journal lives in, and how it is opened.
//!
//! **There is deliberately no `open_or_init`.** That function is the single most tempting
//! default in this crate and the most dangerous: it makes a wiped, missing, or corrupt
//! journal indistinguishable from a brand-new profile. Every number downstream would still
//! look plausible — an empty memory scores badly but scores *cleanly* — so the failure would
//! present as a retrieval-quality problem and send investigation to the cues and the gate,
//! neither of which was at fault.
//!
//! So: [`Profile::init`] creates, [`Profile::open`] requires, and neither one falls back to
//! the other.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::JournalError;
use crate::signature::SigningKey;

pub const JOURNAL_DB: &str = "journal.db";
pub const KEY_FILE: &str = "profile.key";
pub const MANIFEST_FILE: &str = "profile.json";

/// The version of the *derivation* that turns the event stream into the belief store.
///
/// This is the mechanism ADR-009 rests on. The belief store is a materialized view, so
/// adding a derived per-entry field later is a version bump plus a rebuild — never a journal
/// migration, and never a contract major bump. Recording the version is what makes "rebuild
/// when it changes" checkable instead of remembered.
///
/// **2 — Session H.** `MemoryWrittenPayload` gained `occurred_at_ms`, the per-turn time §4.6
/// carries on the wire and the implementation had been discarding. The field is required rather
/// than defaulted, so a version-1 journal cannot replay: it would decode into entries whose
/// `occurred_at_ms` is 0, which puts every turn in one derived session and makes session pruning
/// silently degenerate. This bump is what turns that into a named refusal at open time.
pub const DERIVATION_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub contract_version: String,
    pub derivation_version: u32,
}

/// The name of the exclusivity lock inside a profile root.
const LOCK_FILE: &str = "profile.lock";

/// An exclusive hold on a profile root, released when the `Profile` is dropped.
///
/// # Why the journal cannot tolerate a second writer
///
/// The journal is a **hash chain**: every entry signs over the previous entry's signature, and
/// `seq` is `last_seq + 1` from an in-memory counter loaded at open. Two processes on one profile
/// therefore do not merely race — they **fork the chain**, and `verify_chain` is right to reject
/// the result. This is not a concurrency bug to tune away; concurrent writers are incompatible
/// with the design, so the honest mechanism is to refuse the second one.
///
/// **Observed, not theorised.** A `marlowe --ask` running beside a `marlowe --serve` on the same
/// profile produced `UNIQUE constraint failed: journal.seq` on every append the daemon attempted,
/// for a whole session, while the other process wrote happily. The audit log — invariant 7 — was
/// silently taking writes from one process and refusing them from the other.
#[derive(Debug)]
struct ProfileLock {
    path: PathBuf,
    _file: fs::File,
}

impl ProfileLock {
    /// Take the lock, or say who has it. **Refuses at load time**, which is the whole point: a
    /// second daemon that started and then failed every append would look like a broken journal
    /// rather than a second daemon.
    fn acquire(root: &Path) -> Result<Self, JournalError> {
        let path = root.join(LOCK_FILE);
        let file = Self::exclusive(&path).map_err(|e| JournalError::ProfileLocked {
            root: root.to_path_buf(),
            detail: e.to_string(),
        })?;
        Ok(Self { path, _file: file })
    }

    #[cfg(windows)]
    fn exclusive(path: &Path) -> std::io::Result<fs::File> {
        use std::os::windows::fs::OpenOptionsExt;
        // Share nothing: a second opener fails with a sharing violation, which is exactly the
        // answer wanted. The handle is held for the process's lifetime.
        fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .share_mode(0)
            .open(path)
    }

    #[cfg(not(windows))]
    fn exclusive(path: &Path) -> std::io::Result<fs::File> {
        use std::io::Write;
        // POSIX has no mandatory locking, so this uses `O_EXCL` on a lock file plus a liveness
        // check: a stale lock from a crashed process must not wedge the profile forever.
        match fs::OpenOptions::new().create_new(true).write(true).open(path) {
            Ok(mut f) => {
                let _ = write!(f, "{}", std::process::id());
                Ok(f)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = fs::read_to_string(path).unwrap_or_default();
                let alive = holder
                    .trim()
                    .parse::<i32>()
                    .map(|pid| Path::new(&format!("/proc/{pid}")).exists())
                    .unwrap_or(false);
                if alive {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        format!("held by pid {}", holder.trim()),
                    ));
                }
                // Stale: the holder is gone. Reclaim rather than refuse forever.
                let _ = fs::remove_file(path);
                let mut f = fs::OpenOptions::new().create_new(true).write(true).open(path)?;
                let _ = write!(f, "{}", std::process::id());
                Ok(f)
            }
            Err(e) => Err(e),
        }
    }
}

impl Drop for ProfileLock {
    fn drop(&mut self) {
        // Best effort. A leftover file is reclaimed by the liveness check above; on Windows the
        // handle closing is what releases it and the file itself is harmless.
        let _ = fs::remove_file(&self.path);
    }
}

pub struct Profile {
    root: PathBuf,
    key: SigningKey,
    manifest: Manifest,
    /// **Held for as long as the profile is.** `Journal::open` takes a `&Profile`, so there is no
    /// way to reach the journal without having taken this — the validating-constructor pattern
    /// applied to a resource rather than a value.
    _lock: ProfileLock,
}

impl Profile {
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn key(&self) -> &SigningKey {
        &self.key
    }
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    pub fn journal_path(&self) -> PathBuf {
        self.root.join(JOURNAL_DB)
    }

    /// Create a new profile. **The directory must not already contain one.**
    ///
    /// The eval adapter calls this on every spawn with a fresh temporary directory, because
    /// the harness spawns a target once per corpus and four more times in the clock probe
    /// alone. State surviving between spawns would make the probe compare contaminated runs
    /// while reporting a clean verdict.
    pub fn init(root: impl AsRef<Path>) -> Result<Self, JournalError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;

        let existing: Vec<_> = fs::read_dir(&root)?.filter_map(Result::ok).collect();
        if !existing.is_empty() {
            return Err(JournalError::ProfileRootNotEmpty {
                root: root.clone(),
                entries: existing.len(),
            });
        }

        let key = SigningKey::generate().map_err(|e| JournalError::KeyGeneration(e.to_string()))?;
        fs::write(root.join(KEY_FILE), key.to_hex())?;

        let manifest = Manifest {
            contract_version: marlowe_contract::CONTRACT_VERSION.to_string(),
            derivation_version: DERIVATION_VERSION,
        };
        fs::write(
            root.join(MANIFEST_FILE),
            serde_json::to_string_pretty(&manifest)?,
        )?;

        crate::store::create_schema(&root.join(JOURNAL_DB))?;

        let _lock = ProfileLock::acquire(&root)?;
        Ok(Self { root, key, manifest, _lock })
    }

    /// Open an existing profile. Every part must be present and consistent.
    ///
    /// A missing key, a missing journal, a manifest from a future contract, or a derivation
    /// version this build does not know are each a **startup error**. The system must be
    /// unable to start against a profile it cannot fully account for, rather than start and
    /// produce numbers about a state it half-understands.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, JournalError> {
        let root = root.as_ref().to_path_buf();

        let key_path = root.join(KEY_FILE);
        let manifest_path = root.join(MANIFEST_FILE);
        let db_path = root.join(JOURNAL_DB);

        for required in [&key_path, &manifest_path, &db_path] {
            if !required.exists() {
                return Err(JournalError::ProfileIncomplete {
                    root: root.clone(),
                    missing: required.clone(),
                });
            }
        }

        let key = SigningKey::from_hex(&fs::read_to_string(&key_path)?)
            .ok_or_else(|| JournalError::MalformedKey { path: key_path })?;

        let manifest: Manifest = serde_json::from_str(&fs::read_to_string(&manifest_path)?)?;
        if manifest.contract_version != marlowe_contract::CONTRACT_VERSION {
            return Err(JournalError::ContractVersionMismatch {
                found: manifest.contract_version,
                expected: marlowe_contract::CONTRACT_VERSION.to_string(),
            });
        }
        if manifest.derivation_version != DERIVATION_VERSION {
            return Err(JournalError::DerivationVersionMismatch {
                found: manifest.derivation_version,
                expected: DERIVATION_VERSION,
            });
        }

        let _lock = ProfileLock::acquire(&root)?;
        Ok(Self { root, key, manifest, _lock })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("marlowe-profile-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn init_then_open_round_trips() {
        let dir = tmp("roundtrip");
        // **The first profile is dropped before the second opens**, and that is the lock working
        // rather than a test workaround: two live `Profile` values on one root is exactly the
        // configuration that forked the journal's hash chain and produced
        // `UNIQUE constraint failed: journal.seq` on every append from the losing process.
        let created_key = {
            let created = Profile::init(&dir).expect("init");
            created.key().to_hex()
        };
        let opened = Profile::open(&dir).expect("open");
        assert_eq!(created_key, opened.key().to_hex());
        assert_eq!(opened.manifest().derivation_version, DERIVATION_VERSION);
        drop(opened);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The lock, asserted directly. **A second holder is refused by name.**
    #[test]
    fn a_second_process_cannot_open_a_profile_that_is_already_held() {
        let dir = tmp("locked");
        let first = Profile::init(&dir).expect("init");

        let second = Profile::open(&dir);
        match second {
            Err(JournalError::ProfileLocked { .. }) => {}
            Err(e) => panic!("refused for the wrong reason: {e}"),
            Ok(_) => panic!(
                "two live profiles on one root. The journal is a signed hash chain with a                  per-process sequence counter, so the second writer forks it — which is what                  `UNIQUE constraint failed: journal.seq` looked like from the inside."
            ),
        }

        // And it is released, so a crash or a restart does not wedge the profile.
        drop(first);
        let reopened = Profile::open(&dir);
        assert!(reopened.is_ok(), "the lock must release on drop: {:?}", reopened.err());
        drop(reopened);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_refuses_a_non_empty_root() {
        let dir = tmp("nonempty");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("leftover.txt"), "from a previous run").unwrap();
        assert!(matches!(
            Profile::init(&dir),
            Err(JournalError::ProfileRootNotEmpty { .. })
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_refuses_an_incomplete_profile() {
        // The case that matters: a journal that was deleted. Without this error the system
        // would start empty and every downstream number would look clean.
        let dir = tmp("incomplete");
        Profile::init(&dir).expect("init");
        fs::remove_file(dir.join(JOURNAL_DB)).unwrap();
        assert!(matches!(
            Profile::open(&dir),
            Err(JournalError::ProfileIncomplete { .. })
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn there_is_no_open_or_init() {
        // Executable documentation. If someone adds the convenience function this test is
        // the thing that has to be deleted first, and deleting a test with this name is a
        // decision rather than a slip.
        let dir = tmp("absent");
        assert!(
            Profile::open(&dir).is_err(),
            "opening a nonexistent profile must fail, never silently create one"
        );
    }
}
