//! The user's name and address form: `<home>\user.json`, held live in `Core` so a `SetUser`
//! reaches the next turn and the next brief without a restart.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use marlowe_agent::UserInfo;
use marlowe_proto::{clean_user_field, Event};
use serde::{Deserialize, Serialize};

use crate::daemon::Core;
use crate::lock;

/// File name under the profile home.
pub const USER_FILE: &str = "user.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct UserFile {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    address: Option<String>,
}

pub fn path(home: &Path) -> PathBuf {
    home.join(USER_FILE)
}

/// What `user.json` says; `None` when there is no readable file. A file that exists is the whole
/// answer, so a stored "no address" is not overridden by anything else.
pub fn load(home: &Path) -> Option<UserInfo> {
    let f: UserFile = serde_json::from_str(&std::fs::read_to_string(path(home)).ok()?).ok()?;
    Some(UserInfo { name: f.name, address: f.address })
}

/// Writes `user.json` atomically (temp file + rename).
pub fn save(home: &Path, user: &UserInfo) -> std::io::Result<()> {
    let f = UserFile { name: user.name.clone(), address: user.address.clone() };
    let json = serde_json::to_string_pretty(&f).map_err(std::io::Error::other)?;
    let p = path(home);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &p)
}

/// The live user. `set` is false until something has named the user.
pub(crate) struct UserCell {
    inner: Mutex<(UserInfo, bool)>,
}

impl UserCell {
    pub fn new(info: UserInfo, set: bool) -> Self {
        Self { inner: Mutex::new((info, set)) }
    }

    pub fn get(&self) -> UserInfo {
        lock(&self.inner).0.clone()
    }

    pub fn event(&self) -> Event {
        let g = lock(&self.inner);
        Event::User { name: g.0.name.clone(), address: g.0.address.clone(), set: g.1 }
    }

    fn replace(&self, info: UserInfo) {
        *lock(&self.inner) = (info, true);
    }
}

/// Handles `SetUser`: validates, writes `user.json`, updates the live user, broadcasts `User`.
pub(crate) fn set(core: &Arc<Core>, name: Option<String>, address: Option<String>) -> Result<(), String> {
    let info = UserInfo {
        name: clean_user_field("name", name.as_deref())?,
        address: clean_user_field("address", address.as_deref())?,
    };
    // One writer at a time, so the file and the live value cannot disagree.
    let _g = lock(&core.user_write);
    save(&core.home, &info).map_err(|e| format!("could not save {USER_FILE}: {e}"))?;
    core.user.replace(info);
    core.bus.publish(core.user.event());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_load_round_trips_and_a_missing_or_bad_file_is_none() {
        let home = std::env::temp_dir().join(format!("marlowe-user-{}-{}", std::process::id(), crate::now_ms()));
        std::fs::create_dir_all(&home).unwrap();
        assert_eq!(load(&home), None);
        let u = UserInfo { name: Some("Matthew".into()), address: None };
        save(&home, &u).unwrap();
        assert_eq!(load(&home), Some(u));
        assert!(!home.join("user.json.tmp").exists());
        std::fs::write(path(&home), "{not json").unwrap();
        assert_eq!(load(&home), None);
        let _ = std::fs::remove_dir_all(&home);
    }
}
