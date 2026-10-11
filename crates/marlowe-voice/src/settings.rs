//! Voice settings, persisted as `<home>\voice.json`. Files written before the engine choice
//! existed (`{"enabled":…,"voice":…}`) load as the WinRT engine.
//!
//! The file is hand-edited (to switch engines), so it is read field by field: a field that does
//! not parse keeps its default and is reported, and a file with such problems is never
//! overwritten ([`Loaded::problems`]), so one typo cannot cost the rest of it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// Which engine synthesizes speech. Playback is WinRT's either way. Read with
/// [`Engine::parse`] (any case, aliases); written lowercase.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    /// WinRT `SpeechSynthesizer`: the default, and the fallback for every other engine.
    #[default]
    Winrt,
    /// The Chatterbox voice-clone sidecar (`tools/voice/chatterbox_sidecar.py`). A placeholder
    /// until the M6 voice.
    Chatterbox,
}

impl Engine {
    fn is_winrt(&self) -> bool {
        *self == Engine::Winrt
    }

    /// `winrt` or `chatterbox`, any case.
    pub fn parse(s: &str) -> Option<Engine> {
        match s.trim().to_ascii_lowercase().as_str() {
            "winrt" | "windows" | "default" => Some(Engine::Winrt),
            "chatterbox" | "clone" => Some(Engine::Chatterbox),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for Engine {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Engine, D::Error> {
        let name = String::deserialize(d)?;
        Engine::parse(&name).ok_or_else(|| serde::de::Error::custom(format!("unknown engine '{name}' (winrt or chatterbox)")))
    }
}

/// Whole seconds from any non-negative JSON number (`300`, `300.0`).
fn secs<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = f64::deserialize(d)?;
    if v.is_finite() && v >= 0.0 {
        Ok(v.round().min(u64::MAX as f64) as u64)
    } else {
        Err(serde::de::Error::custom(format!("expected a number of seconds, found {v}")))
    }
}

/// The Chatterbox sidecar: which interpreter runs it, which sample it clones, and the
/// generation parameters of the reference `speak.py`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatterboxSettings {
    /// A Python with `chatterbox-tts` installed (a venv's `Scripts\python.exe`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub python: Option<PathBuf>,
    /// The voice sample to clone (wav).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<PathBuf>,
    /// The sidecar script. `None`: the copy built into this binary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<PathBuf>,
    /// `auto` (CUDA when available), `cuda` or `cpu`.
    pub device: String,
    pub exaggeration: f32,
    pub cfg_weight: f32,
    pub temperature: f32,
    /// Scales every punctuation pause.
    pub pause_scale: f32,
    /// The sidecar exits after this long without a request, releasing its VRAM. 0: never.
    #[serde(deserialize_with = "secs")]
    pub idle_timeout_s: u64,
    /// How long a cold start (interpreter + model load) may take.
    #[serde(deserialize_with = "secs")]
    pub load_timeout_s: u64,
    /// How long a line waits for a model that is still loading before the Windows voice says it
    /// instead (the load carries on for the next line).
    #[serde(deserialize_with = "secs")]
    pub ready_grace_s: u64,
    /// Base time one part of an utterance (a clause) may take; longer parts get more.
    #[serde(deserialize_with = "secs")]
    pub request_timeout_s: u64,
    /// GPU memory the loaded clone holds, MiB. A GPU start needs this much free beside what is
    /// loaded; otherwise the Windows voice speaks.
    pub vram_mib: u64,
}

impl Default for ChatterboxSettings {
    fn default() -> Self {
        Self {
            python: None,
            reference: None,
            script: None,
            device: "auto".into(),
            exaggeration: 0.5,
            cfg_weight: 1.0,
            temperature: 0.6,
            pause_scale: 0.7,
            idle_timeout_s: 300,
            load_timeout_s: 120,
            ready_grace_s: 5,
            request_timeout_s: 60,
            vram_mib: 4_608,
        }
    }
}

/// On/off, the chosen voice name and the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceSettings {
    pub enabled: bool,
    /// `None`: pick automatically. With the Chatterbox engine this is the WinRT fallback voice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(default, skip_serializing_if = "Engine::is_winrt")]
    pub engine: Engine,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chatterbox: Option<ChatterboxSettings>,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self { enabled: true, voice: None, engine: Engine::Winrt, chatterbox: None }
    }
}

/// Environment overrides for quick testing; never written back to `voice.json`.
pub const ENV_ENGINE: &str = "MARLOWE_VOICE_ENGINE";
pub const ENV_PYTHON: &str = "MARLOWE_VOICE_PYTHON";
pub const ENV_REF: &str = "MARLOWE_VOICE_REF";
pub const ENV_SCRIPT: &str = "MARLOWE_VOICE_SCRIPT";
pub const ENV_DEVICE: &str = "MARLOWE_VOICE_DEVICE";

/// `voice.json` as read: the settings, and what in the file could not be used (each such field
/// kept its default). A file with problems is not overwritten.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Loaded {
    pub settings: VoiceSettings,
    pub problems: Vec<String>,
}

/// `fields` laid over `base` one at a time; a field that does not parse is reported (named with
/// `prefix`) and leaves `base`'s value.
fn overlay<T: Serialize + serde::de::DeserializeOwned>(base: T, fields: Map<String, Value>, prefix: &str, problems: &mut Vec<String>) -> T {
    let mut cur = base;
    for (k, v) in fields {
        let Ok(Value::Object(mut probe)) = serde_json::to_value(&cur) else { break };
        probe.insert(k.clone(), v);
        match serde_json::from_value::<T>(Value::Object(probe)) {
            Ok(next) => cur = next,
            Err(e) => problems.push(format!("{prefix}{k}: {e}")),
        }
    }
    cur
}

impl VoiceSettings {
    pub fn path(home: &Path) -> PathBuf {
        home.join("voice.json")
    }

    /// Reads the settings, logging whatever in the file could not be used.
    pub fn load(home: &Path) -> VoiceSettings {
        let loaded = Self::read(home);
        for p in &loaded.problems {
            eprintln!("marlowe: voice: {}: {p}", Self::path(home).display());
        }
        loaded.settings
    }

    /// Reads the settings field by field. A missing file gives the defaults and no problem.
    pub fn read(home: &Path) -> Loaded {
        let failed = |why: String| Loaded { settings: VoiceSettings::default(), problems: vec![why] };
        let text = match std::fs::read_to_string(Self::path(home)) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Loaded::default(),
            Err(e) => return failed(format!("unreadable: {e}")),
        };
        let map = match serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) {
            Ok(Value::Object(m)) => m,
            Ok(_) => return failed("not a JSON object".into()),
            Err(e) => return failed(format!("not JSON: {e}")),
        };
        let (mut top, mut section) = (Map::new(), None);
        for (k, v) in map {
            match (k.as_str(), v) {
                ("chatterbox", Value::Object(m)) => section = Some(m),
                (_, v) => {
                    top.insert(k, v);
                }
            }
        }
        let mut problems = Vec::new();
        let mut settings = overlay(VoiceSettings::default(), top, "", &mut problems);
        if let Some(m) = section {
            settings.chatterbox = Some(overlay(ChatterboxSettings::default(), m, "chatterbox.", &mut problems));
        }
        Loaded { settings, problems }
    }

    /// Writes the settings (temp file + rename).
    pub fn save(&self, home: &Path) -> std::io::Result<()> {
        let path = Self::path(home);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }

    /// These settings with the `MARLOWE_VOICE_*` overrides `env` supplies (blank values are
    /// ignored, as is an unknown engine name). Any Chatterbox override fills in a Chatterbox
    /// section, so `MARLOWE_VOICE_ENGINE=chatterbox` plus the python and sample paths is enough.
    pub fn with_env(&self, env: &dyn Fn(&str) -> Option<String>) -> VoiceSettings {
        let get = |k: &str| env(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let mut s = self.clone();
        if let Some(e) = get(ENV_ENGINE).as_deref().and_then(Engine::parse) {
            s.engine = e;
        }
        let mut cb = s.chatterbox.clone().unwrap_or_default();
        let mut touched = false;
        for (var, slot) in [(ENV_PYTHON, &mut cb.python), (ENV_REF, &mut cb.reference), (ENV_SCRIPT, &mut cb.script)] {
            if let Some(v) = get(var) {
                *slot = Some(PathBuf::from(v));
                touched = true;
            }
        }
        if let Some(d) = get(ENV_DEVICE) {
            cb.device = d;
            touched = true;
        }
        if touched || s.engine == Engine::Chatterbox {
            s.chatterbox = Some(cb);
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn a_file_from_before_the_engine_choice_loads_as_winrt() {
        let s: VoiceSettings = serde_json::from_str(r#"{"enabled":false,"voice":"George (en-GB)"}"#).unwrap();
        assert_eq!(s, VoiceSettings { enabled: false, voice: Some("George (en-GB)".into()), engine: Engine::Winrt, chatterbox: None });
        // And writes back the shape it had: no engine keys appear.
        assert_eq!(serde_json::to_string(&s).unwrap(), r#"{"enabled":false,"voice":"George (en-GB)"}"#);
    }

    #[test]
    fn chatterbox_settings_round_trip_and_missing_fields_take_defaults() {
        let s: VoiceSettings = serde_json::from_str(
            r#"{"enabled":true,"engine":"chatterbox","chatterbox":{"python":"C:\\v\\python.exe","reference":"C:\\v\\me.wav","temperature":0.4}}"#,
        )
        .unwrap();
        assert_eq!(s.engine, Engine::Chatterbox);
        let cb = s.chatterbox.clone().unwrap();
        assert_eq!(cb.python.as_deref(), Some(Path::new(r"C:\v\python.exe")));
        assert_eq!(cb.temperature, 0.4);
        assert_eq!((cb.exaggeration, cb.cfg_weight, cb.pause_scale, cb.idle_timeout_s), (0.5, 1.0, 0.7, 300));
        assert_eq!(cb.device, "auto");
        let back: VoiceSettings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn save_and_load_round_trip_on_disk() {
        let dir = std::env::temp_dir().join(format!("marlowe-voice-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let s = VoiceSettings {
            engine: Engine::Chatterbox,
            chatterbox: Some(ChatterboxSettings { reference: Some("me.wav".into()), ..Default::default() }),
            ..Default::default()
        };
        s.save(&dir).unwrap();
        assert_eq!(VoiceSettings::load(&dir), s);
        std::fs::write(VoiceSettings::path(&dir), "not json").unwrap();
        let bad = VoiceSettings::read(&dir);
        assert_eq!(bad.settings, VoiceSettings::default());
        assert!(bad.problems[0].contains("not JSON"), "{:?}", bad.problems);
        assert_eq!(VoiceSettings::read(&dir.join("missing")), Loaded::default());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_hand_edited_file_keeps_every_field_that_parses() {
        let dir = std::env::temp_dir().join(format!("marlowe-voice-lenient-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = VoiceSettings::path(&dir);
        // Wrong case and a float timeout are read as meant.
        std::fs::write(&path, r#"{"enabled":false,"engine":"Chatterbox","chatterbox":{"python":"C:\\v\\python.exe","idle_timeout_s":600.0}}"#).unwrap();
        let ok = VoiceSettings::read(&dir);
        assert!(ok.problems.is_empty(), "{:?}", ok.problems);
        assert_eq!((ok.settings.enabled, ok.settings.engine), (false, Engine::Chatterbox));
        assert_eq!(ok.settings.chatterbox.as_ref().unwrap().idle_timeout_s, 600);

        // An unknown engine, a negative timeout and a word for a number are problems; every
        // other field survives, the voice stays off.
        std::fs::write(&path, r#"{"enabled":false,"engine":"kokoro","chatterbox":{"reference":"C:\\v\\me.wav","load_timeout_s":-5,"temperature":"hot"}}"#).unwrap();
        let bad = VoiceSettings::read(&dir);
        assert!(!bad.settings.enabled);
        assert_eq!(bad.settings.engine, Engine::Winrt);
        let cb = bad.settings.chatterbox.unwrap();
        assert_eq!(cb.reference.as_deref(), Some(Path::new(r"C:\v\me.wav")));
        assert_eq!((cb.load_timeout_s, cb.temperature), (120, 0.6));
        assert_eq!(bad.problems.len(), 3, "{:?}", bad.problems);
        assert!(bad.problems.iter().any(|p| p.starts_with("engine:") && p.contains("kokoro")), "{:?}", bad.problems);
        assert!(bad.problems.iter().any(|p| p.starts_with("chatterbox.load_timeout_s")), "{:?}", bad.problems);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn env_overrides_select_the_engine_and_fill_the_paths() {
        assert_eq!(VoiceSettings::default().with_env(&no_env), VoiceSettings::default());
        let env = |k: &str| match k {
            ENV_ENGINE => Some(" Chatterbox ".to_string()),
            ENV_PYTHON => Some(r"C:\v\python.exe".to_string()),
            ENV_REF => Some(r"C:\v\me.wav".to_string()),
            ENV_DEVICE => Some("   ".to_string()),
            _ => None,
        };
        let s = VoiceSettings::default().with_env(&env);
        assert_eq!(s.engine, Engine::Chatterbox);
        let cb = s.chatterbox.unwrap();
        assert_eq!(cb.python.as_deref(), Some(Path::new(r"C:\v\python.exe")));
        assert_eq!(cb.reference.as_deref(), Some(Path::new(r"C:\v\me.wav")));
        assert_eq!(cb.device, "auto", "a blank override is ignored");

        // The environment can switch a configured clone back to WinRT; unknown names change nothing.
        let file = VoiceSettings { engine: Engine::Chatterbox, ..Default::default() };
        assert_eq!(file.with_env(&|k| (k == ENV_ENGINE).then(|| "winrt".into())).engine, Engine::Winrt);
        assert_eq!(file.with_env(&|k| (k == ENV_ENGINE).then(|| "kokoro".into())).engine, Engine::Chatterbox);
        // Chatterbox selected with no section still gets one (with defaults) to report from.
        assert_eq!(file.with_env(&no_env).chatterbox, Some(ChatterboxSettings::default()));
    }
}
