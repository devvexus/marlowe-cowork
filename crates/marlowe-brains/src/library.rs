//! The model library in `cowork.db`.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use marlowe_model::{Effort, ModelInfo, ModelRef};
use marlowe_proto::{ContextPolicy, KvType, LibraryModel, PolicySource, Slot, SlotAssignment};
use rusqlite::{params, Connection, OptionalExtension};

use crate::error::BrainsError;

/// Library tables. `info` is the `ModelInfo` as JSON; `added_at` is unix ms.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS models (
    ref       TEXT PRIMARY KEY,
    provider  TEXT NOT NULL,
    model_id  TEXT NOT NULL,
    display   TEXT NOT NULL,
    info      TEXT NOT NULL,
    added_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS slots (
    slot      TEXT PRIMARY KEY,
    model_ref TEXT,
    effort    TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS context_policy (
    model_ref  TEXT PRIMARY KEY,
    max_ctx    INTEGER NOT NULL CHECK (max_ctx >= 2048),
    kv_type    TEXT NOT NULL CHECK (kv_type IN ('f16','q8_0','q4_0')),
    parallel   INTEGER NOT NULL CHECK (parallel BETWEEN 1 AND 16),
    source     TEXT NOT NULL CHECK (source IN ('auto','user')),
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS hw_calibration (
    key         TEXT PRIMARY KEY,
    value       REAL NOT NULL,
    observed_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS edit_formats (
    model_ref   TEXT PRIMARY KEY,
    formats     TEXT NOT NULL,
    measured_at INTEGER NOT NULL
);
";

/// The library over one sqlite connection. Not `Sync`; wrap in a `Mutex` to share.
pub struct Library {
    conn: Connection,
}

type Row = (String, String, i64);

impl Library {
    /// Opens (creating if needed) the library in `cowork.db`, WAL mode, schema applied.
    pub fn open(path: &Path) -> Result<Self, BrainsError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::from_connection(conn)
    }

    /// An in-memory library (tests).
    pub fn open_in_memory() -> Result<Self, BrainsError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    /// Wraps an existing connection and applies the schema.
    pub fn from_connection(conn: Connection) -> Result<Self, BrainsError> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Every model, oldest first.
    pub fn models(&self) -> Result<Vec<LibraryModel>, BrainsError> {
        let mut st = self
            .conn
            .prepare("SELECT ref, info, added_at FROM models ORDER BY added_at, rowid")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(decode(row?)?);
        }
        Ok(out)
    }

    /// One model by ref.
    pub fn get(&self, model: &ModelRef) -> Result<Option<LibraryModel>, BrainsError> {
        let row: Option<Row> = self
            .conn
            .query_row(
                "SELECT ref, info, added_at FROM models WHERE ref = ?1",
                [model.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        row.map(decode).transpose()
    }

    /// Adds a model, or refreshes its info if present. Returns its ref.
    pub fn add(&self, info: &ModelInfo) -> Result<ModelRef, BrainsError> {
        let model = info.model_ref();
        let json = serde_json::to_string(info).map_err(|e| BrainsError::Invalid(e.to_string()))?;
        self.conn.execute(
            "INSERT INTO models (ref, provider, model_id, display, info, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(ref) DO UPDATE SET display = excluded.display, info = excluded.info",
            params![model.to_string(), info.provider.as_str(), info.id, info.display, json, now_ms()],
        )?;
        Ok(model)
    }

    /// Removes a model, clears any slot pointing at it and deletes its context policy. Returns
    /// whether it existed.
    pub fn remove(&self, model: &ModelRef) -> Result<bool, BrainsError> {
        let key = model.to_string();
        self.conn.execute("UPDATE slots SET model_ref = NULL WHERE model_ref = ?1", [&key])?;
        self.conn.execute("DELETE FROM context_policy WHERE model_ref = ?1", [&key])?;
        self.conn.execute("DELETE FROM edit_formats WHERE model_ref = ?1", [&key])?;
        Ok(self.conn.execute("DELETE FROM models WHERE ref = ?1", [&key])? > 0)
    }

    /// The first model added: the Conductor's model until one is assigned (onboarding).
    pub fn default_model(&self) -> Result<Option<ModelRef>, BrainsError> {
        let s: Option<String> = self
            .conn
            .query_row("SELECT ref FROM models ORDER BY added_at, rowid LIMIT 1", [], |r| r.get(0))
            .optional()?;
        s.map(|s| s.parse::<ModelRef>().map_err(|e| BrainsError::Invalid(e.to_string())))
            .transpose()
    }

    /// Sets a slot. `model: None` makes it inherit (see [`Library::resolve`]).
    pub fn set_slot(&self, slot: &Slot, model: Option<&ModelRef>, effort: Effort) -> Result<(), BrainsError> {
        if let Some(m) = model {
            if self.get(m)?.is_none() {
                return Err(BrainsError::UnknownModel(m.clone()));
            }
        }
        self.conn.execute(
            "INSERT INTO slots (slot, model_ref, effort) VALUES (?1, ?2, ?3)
             ON CONFLICT(slot) DO UPDATE SET model_ref = excluded.model_ref, effort = excluded.effort",
            params![slot.to_string(), model.map(|m| m.to_string()), effort.as_str()],
        )?;
        Ok(())
    }

    /// Conductor, Reflection, Quick, then every stored crew slot, each resolved.
    pub fn slots(&self) -> Result<Vec<SlotAssignment>, BrainsError> {
        let mut slots: Vec<Slot> = Slot::FIXED.to_vec();
        let mut st = self.conn.prepare("SELECT slot FROM slots ORDER BY slot")?;
        let names = st.query_map([], |r| r.get::<_, String>(0))?;
        for name in names {
            if let Ok(slot @ Slot::Crew(_)) = name?.parse::<Slot>() {
                slots.push(slot);
            }
        }
        let inherit = self.inherited_model()?;
        let mut out = Vec::new();
        for slot in slots {
            let (model, effort) = self.stored(&slot)?;
            let inherited = model.is_none();
            let model = match (model, &slot) {
                (Some(m), _) => Some(m),
                (None, Slot::Conductor) => self.default_model()?,
                (None, _) => inherit.clone(),
            };
            out.push(SlotAssignment { slot, model, effort, inherited });
        }
        Ok(out)
    }

    /// The model and effort for `slot`. An unset Conductor takes the first model added; every
    /// other unset slot takes the Conductor's model, never a different (perhaps larger) one that
    /// happens to have been added first.
    pub fn resolve(&self, slot: &Slot) -> Result<(ModelRef, Effort), BrainsError> {
        let (model, effort) = self.stored(slot)?;
        let model = match model {
            Some(m) => m,
            None if *slot == Slot::Conductor => self.default_model()?.ok_or(BrainsError::NoModels)?,
            None => self.inherited_model()?.ok_or(BrainsError::NoModels)?,
        };
        Ok((model, effort))
    }

    /// What an unset non-Conductor slot runs: the Conductor's model.
    fn inherited_model(&self) -> Result<Option<ModelRef>, BrainsError> {
        match self.stored(&Slot::Conductor)?.0 {
            Some(m) => Ok(Some(m)),
            None => self.default_model(),
        }
    }

    /// Stored (model, effort) for a slot; defaults when absent.
    fn stored(&self, slot: &Slot) -> Result<(Option<ModelRef>, Effort), BrainsError> {
        let row: Option<(Option<String>, String)> = self
            .conn
            .query_row("SELECT model_ref, effort FROM slots WHERE slot = ?1", [slot.to_string()], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let Some((model, effort)) = row else { return Ok((None, Effort::default())) };
        let model = model
            .map(|s| s.parse::<ModelRef>().map_err(|e| BrainsError::Invalid(e.to_string())))
            .transpose()?;
        Ok((model, effort.parse().unwrap_or_default()))
    }
}

impl Library {
    /// The saved context policy of `model`, if any.
    pub fn policy(&self, model: &ModelRef) -> Result<Option<ContextPolicy>, BrainsError> {
        let row: Option<(i64, String, i64, String, i64)> = self
            .conn
            .query_row(
                "SELECT max_ctx, kv_type, parallel, source, updated_at FROM context_policy WHERE model_ref = ?1",
                [model.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        Ok(row.and_then(|(max_ctx, kv, parallel, source, at)| {
            Some(ContextPolicy {
                max_ctx: max_ctx as u32,
                kv_type: KvType::parse(&kv)?,
                parallel: parallel as u16,
                source: if source == "user" { PolicySource::User } else { PolicySource::Auto },
                updated_ms: at,
            })
        }))
    }

    /// Every saved policy.
    pub fn policies(&self) -> Result<Vec<(ModelRef, ContextPolicy)>, BrainsError> {
        let mut st = self.conn.prepare("SELECT model_ref FROM context_policy ORDER BY model_ref")?;
        let refs: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
        let mut out = Vec::new();
        for r in refs {
            let Ok(m) = r.parse::<ModelRef>() else { continue };
            if let Some(p) = self.policy(&m)? {
                out.push((m, p));
            }
        }
        Ok(out)
    }

    /// Saves a policy (the caller has validated it).
    pub fn set_policy(&self, model: &ModelRef, p: &ContextPolicy) -> Result<(), BrainsError> {
        let source = match p.source {
            PolicySource::User => "user",
            PolicySource::Auto => "auto",
        };
        self.conn.execute(
            "INSERT INTO context_policy (model_ref, max_ctx, kv_type, parallel, source, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(model_ref) DO UPDATE SET max_ctx = excluded.max_ctx, kv_type = excluded.kv_type,
               parallel = excluded.parallel, source = excluded.source, updated_at = excluded.updated_at",
            params![model.to_string(), p.max_ctx, p.kv_type.ollama_name(), p.parallel, source, p.updated_ms],
        )?;
        Ok(())
    }

    /// The edit formats `marlowe bench --apply` measured for `model`, best first (`edit`,
    /// `edit_lines`, `write`), or `None` when it was never benched (the session then uses the
    /// model class's default, design 3.2).
    pub fn edit_formats(&self, model: &ModelRef) -> Result<Option<Vec<String>>, BrainsError> {
        let json: Option<String> = self
            .conn
            .query_row("SELECT formats FROM edit_formats WHERE model_ref = ?1", [model.to_string()], |r| r.get(0))
            .optional()?;
        Ok(json.and_then(|j| serde_json::from_str(&j).ok()))
    }

    /// Records the measured edit formats of `model`, best first. An empty list clears them.
    pub fn set_edit_formats(&self, model: &ModelRef, formats: &[String]) -> Result<(), BrainsError> {
        if formats.is_empty() {
            self.conn.execute("DELETE FROM edit_formats WHERE model_ref = ?1", [model.to_string()])?;
            return Ok(());
        }
        if self.get(model)?.is_none() {
            return Err(BrainsError::UnknownModel(model.clone()));
        }
        let json = serde_json::to_string(formats).map_err(|e| BrainsError::Invalid(e.to_string()))?;
        self.conn.execute(
            "INSERT INTO edit_formats (model_ref, formats, measured_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(model_ref) DO UPDATE SET formats = excluded.formats, measured_at = excluded.measured_at",
            params![model.to_string(), json, now_ms()],
        )?;
        Ok(())
    }

    pub fn calibration(&self, key: &str) -> Result<Option<f64>, BrainsError> {
        Ok(self.conn.query_row("SELECT value FROM hw_calibration WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    /// The newest calibration whose key starts with `prefix`, as (key, value).
    pub fn calibration_like(&self, prefix: &str) -> Result<Option<(String, f64)>, BrainsError> {
        Ok(self
            .conn
            .query_row(
                "SELECT key, value FROM hw_calibration WHERE substr(key, 1, length(?1)) = ?1 ORDER BY observed_at DESC LIMIT 1",
                [prefix],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Forgets every calibration whose key starts with `prefix`; how many went.
    pub fn forget_calibration(&self, prefix: &str) -> Result<usize, BrainsError> {
        Ok(self.conn.execute("DELETE FROM hw_calibration WHERE substr(key, 1, length(?1)) = ?1", [prefix])?)
    }

    pub fn set_calibration(&self, key: &str, value: f64) -> Result<(), BrainsError> {
        self.conn.execute(
            "INSERT INTO hw_calibration (key, value, observed_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, observed_at = excluded.observed_at",
            params![key, value, now_ms()],
        )?;
        Ok(())
    }
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn decode((_, info, added): Row) -> Result<LibraryModel, BrainsError> {
    let info: ModelInfo = serde_json::from_str(&info).map_err(|e| BrainsError::Invalid(e.to_string()))?;
    Ok(LibraryModel { model: info.model_ref(), info, added_ms: added })
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_model::ProviderKind;

    fn info(id: &str) -> ModelInfo {
        ModelInfo::bare(ProviderKind::Ollama, id)
    }

    #[test]
    fn first_added_is_default_and_slots_inherit() {
        let lib = Library::open_in_memory().unwrap();
        assert!(matches!(lib.resolve(&Slot::Conductor), Err(BrainsError::NoModels)));
        let a = lib.add(&info("a")).unwrap();
        let b = lib.add(&info("b")).unwrap();
        assert_eq!(lib.default_model().unwrap(), Some(a.clone()));
        assert_eq!(lib.resolve(&Slot::Quick).unwrap().0, a);
        lib.set_slot(&Slot::Quick, Some(&b), Effort::High).unwrap();
        assert_eq!(lib.resolve(&Slot::Quick).unwrap(), (b.clone(), Effort::High));
        let slots = lib.slots().unwrap();
        assert_eq!(slots.len(), 3);
        assert!(slots[0].inherited && !slots[2].inherited);
    }

    #[test]
    fn unset_slots_follow_the_conductor_not_the_first_model_added() {
        let lib = Library::open_in_memory().unwrap();
        let big = lib.add(&info("marlowe-valiance:22b")).unwrap();
        let small = lib.add(&info("marlowe-dawn:9b")).unwrap();
        lib.set_slot(&Slot::Conductor, Some(&small), Effort::Medium).unwrap();
        lib.set_slot(&Slot::Reflection, None, Effort::Low).unwrap();
        for slot in [Slot::Reflection, Slot::Quick, Slot::Crew("general".into())] {
            assert_eq!(lib.resolve(&slot).unwrap().0, small, "{slot}");
        }
        assert!(lib.slots().unwrap().iter().all(|a| a.model.as_ref() == Some(&small)));
        assert_eq!(lib.default_model().unwrap(), Some(big));
    }

    #[test]
    fn edit_formats_are_measured_per_model_and_go_with_it() {
        let lib = Library::open_in_memory().unwrap();
        let a = lib.add(&info("a")).unwrap();
        assert_eq!(lib.edit_formats(&a).unwrap(), None);
        lib.set_edit_formats(&a, &["edit_lines".into(), "edit".into()]).unwrap();
        assert_eq!(lib.edit_formats(&a).unwrap(), Some(vec!["edit_lines".to_string(), "edit".to_string()]));
        lib.set_edit_formats(&a, &["edit".into()]).unwrap();
        assert_eq!(lib.edit_formats(&a).unwrap(), Some(vec!["edit".to_string()]));
        // A model the library does not hold cannot get formats, and an empty list clears.
        let ghost = ModelRef::new(ProviderKind::Ollama, "ghost");
        assert!(lib.set_edit_formats(&ghost, &["edit".into()]).is_err());
        lib.set_edit_formats(&a, &[]).unwrap();
        assert_eq!(lib.edit_formats(&a).unwrap(), None);
        lib.set_edit_formats(&a, &["write".into()]).unwrap();
        lib.remove(&a).unwrap();
        assert_eq!(lib.edit_formats(&a).unwrap(), None);
    }

    #[test]
    fn add_is_idempotent_and_remove_clears_slots() {
        let lib = Library::open_in_memory().unwrap();
        let a = lib.add(&info("a")).unwrap();
        let b = lib.add(&info("b")).unwrap();
        lib.add(&info("a")).unwrap();
        assert_eq!(lib.models().unwrap().len(), 2);
        lib.set_slot(&Slot::Crew("coder".into()), Some(&b), Effort::Low).unwrap();
        assert_eq!(lib.slots().unwrap().len(), 4);
        assert!(lib.remove(&b).unwrap());
        assert!(!lib.remove(&b).unwrap());
        assert_eq!(lib.resolve(&Slot::Crew("coder".into())).unwrap(), (a, Effort::Low));
        assert!(lib.set_slot(&Slot::Quick, Some(&b), Effort::Low).is_err());
    }

    #[test]
    fn policies_persist_validate_in_sql_and_go_with_their_model() {
        let lib = Library::open_in_memory().unwrap();
        let a = lib.add(&info("a")).unwrap();
        let p = ContextPolicy { max_ctx: 47_104, kv_type: KvType::Q4_0, parallel: 2, source: PolicySource::User, updated_ms: 5 };
        lib.set_policy(&a, &p).unwrap();
        assert_eq!(lib.policy(&a).unwrap(), Some(p));
        assert_eq!(lib.policies().unwrap(), vec![(a.clone(), p)]);
        // The table's CHECKs are the last line: a window under 2048 or 17 agents never lands.
        assert!(lib.set_policy(&a, &ContextPolicy { max_ctx: 1024, ..p }).is_err());
        assert!(lib.set_policy(&a, &ContextPolicy { parallel: 17, ..p }).is_err());
        lib.remove(&a).unwrap();
        assert_eq!(lib.policy(&a).unwrap(), None);
        lib.set_calibration("compute|blob|32768|512", 370.3).unwrap();
        assert_eq!(lib.calibration_like("compute|blob|").unwrap().map(|c| c.1), Some(370.3));
    }

    #[test]
    fn persists_on_disk() {
        let dir = std::env::temp_dir().join(format!("brains-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cowork.db");
        {
            let lib = Library::open(&path).unwrap();
            lib.add(&info("a")).unwrap();
        }
        assert_eq!(Library::open(&path).unwrap().models().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
