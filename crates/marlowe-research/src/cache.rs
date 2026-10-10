//! The on-disk research cache: `<home>\cache\research\<channel>\<blake3>.bin`. Channel answers
//! and fetched pages are cached by content-addressed key with a per-read TTL; writes are atomic
//! (temp file + rename) so a crash never leaves a torn entry.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::de::DeserializeOwned;
use serde::Serialize;

use marlowe_proto::ChannelId;

/// A cache key: blake3 of the key parts, hex.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CacheKey(String);

impl CacheKey {
    /// Hashes `parts` with a separator no part can forge.
    pub fn of<S: AsRef<str>>(parts: &[S]) -> Self {
        let mut h = blake3::Hasher::new();
        for p in parts {
            let p = p.as_ref().as_bytes();
            h.update(&(p.len() as u64).to_le_bytes());
            h.update(p);
        }
        CacheKey(h.finalize().to_hex().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The research cache under a profile home.
#[derive(Debug, Clone)]
pub struct ResearchCache {
    root: PathBuf,
}

impl ResearchCache {
    /// `<home>\cache\research`.
    pub fn at_home(home: &Path) -> Self {
        Self::new(home.join("cache").join("research"))
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where `key` lives for `bucket` (a channel name, or `pages` for fetched documents).
    pub fn path(&self, bucket: &str, key: &CacheKey) -> PathBuf {
        self.root.join(bucket).join(format!("{}.bin", key.as_str()))
    }

    /// The cached bytes if present and younger than `ttl`.
    pub fn get(&self, bucket: &str, key: &CacheKey, ttl: Duration) -> Option<Vec<u8>> {
        let p = self.path(bucket, key);
        let age = std::fs::metadata(&p).ok()?.modified().ok()?.elapsed().unwrap_or(Duration::ZERO);
        if age > ttl {
            return None;
        }
        std::fs::read(p).ok()
    }

    pub fn put(&self, bucket: &str, key: &CacheKey, bytes: &[u8]) -> std::io::Result<()> {
        let p = self.path(bucket, key);
        let dir = p.parent().unwrap_or(&self.root);
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{}.{}.tmp", key.as_str(), std::process::id()));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &p).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }

    pub fn get_json<T: DeserializeOwned>(&self, channel: ChannelId, key: &CacheKey, ttl: Duration) -> Option<T> {
        serde_json::from_slice(&self.get(channel.as_str(), key, ttl)?).ok()
    }

    pub fn put_json<T: Serialize>(&self, channel: ChannelId, key: &CacheKey, value: &T) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
        self.put(channel.as_str(), key, &bytes)
    }

    /// Removes entries older than `age` under every bucket; returns how many.
    pub fn purge_older_than(&self, age: Duration) -> std::io::Result<usize> {
        let Ok(buckets) = std::fs::read_dir(&self.root) else { return Ok(0) };
        let cutoff = SystemTime::now().checked_sub(age).unwrap_or(SystemTime::UNIX_EPOCH);
        let mut n = 0;
        for b in buckets.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())) {
            for f in std::fs::read_dir(b.path())?.flatten() {
                let old = f.metadata().and_then(|m| m.modified()).is_ok_and(|m| m < cutoff);
                if old && std::fs::remove_file(f.path()).is_ok() {
                    n += 1;
                }
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SourceRef;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("marlowe-research-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn keys_are_stable_and_unforgeable_by_joining() {
        assert_eq!(CacheKey::of(&["a", "b"]), CacheKey::of(&["a", "b"]));
        assert_ne!(CacheKey::of(&["ab", ""]), CacheKey::of(&["a", "b"]));
        assert_eq!(CacheKey::of(&["x"]).as_str().len(), 64);
    }

    #[test]
    fn entries_round_trip_expire_and_purge() {
        let home = tmp("cache");
        let c = ResearchCache::at_home(&home);
        assert!(c.root().ends_with(Path::new("cache").join("research")));
        let k = CacheKey::of(&["arxiv", "lora rank"]);
        assert_eq!(c.get("arxiv", &k, Duration::from_secs(60)), None);
        let hits = vec![SourceRef::new(ChannelId::Arxiv, "https://arxiv.org/abs/2106.09685", "LoRA")];
        c.put_json(ChannelId::Arxiv, &k, &hits).unwrap();
        assert_eq!(c.get_json::<Vec<SourceRef>>(ChannelId::Arxiv, &k, Duration::from_secs(60)), Some(hits));
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(c.get("arxiv", &k, Duration::from_millis(1)), None, "expired");
        assert_eq!(c.purge_older_than(Duration::from_millis(1)).unwrap(), 1);
        assert_eq!(c.get("arxiv", &k, Duration::from_secs(60)), None);
        let _ = std::fs::remove_dir_all(&home);
    }
}
