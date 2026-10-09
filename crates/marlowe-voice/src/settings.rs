//! Voice settings, persisted as `<home>\voice.json`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// On/off and the chosen voice name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceSettings {
    pub enabled: bool,
    /// `None`: pick automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self { enabled: true, voice: None }
    }
}

impl VoiceSettings {
    pub fn path(home: &Path) -> PathBuf {
        home.join("voice.json")
    }

    /// Reads the settings; a missing or unreadable file gives the defaults.
    pub fn load(home: &Path) -> VoiceSettings {
        std::fs::read_to_string(Self::path(home))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Writes the settings (temp file + rename).
    pub fn save(&self, home: &Path) -> std::io::Result<()> {
        let path = Self::path(home);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }
}
