//! The model library in `cowork.db`.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use marlowe_model::{Effort, ModelInfo, ModelRef};
use marlowe_proto::{LibraryModel, Slot, SlotAssignment};
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

    /// Removes a model and clears any slot pointing at it. Returns whether it existed.
    pub fn remove(&self, model: &ModelRef) -> Result<bool, BrainsError> {
        let key = model.to_string();
        self.conn.execute("UPDATE slots SET model_ref = NULL WHERE model_ref = ?1", [&key])?;
        Ok(self.conn.execute("DELETE FROM models WHERE ref = ?1", [&key])? > 0)
    }

    /// The default model: the first one added.
    pub fn default_model(&self) -> Result<Option<ModelRef>, BrainsError> {
        let s: Option<String> = self
            .conn
            .query_row("SELECT ref FROM models ORDER BY added_at, rowid LIMIT 1", [], |r| r.get(0))
            .optional()?;
        s.map(|s| s.parse::<ModelRef>().map_err(|e| BrainsError::Invalid(e.to_string())))
            .transpose()
    }

    /// Sets a slot. `model: None` makes it inherit the default.
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
        let default = self.default_model()?;
        let mut out = Vec::new();
        for slot in slots {
            let (model, effort) = self.stored(&slot)?;
            let inherited = model.is_none();
            out.push(SlotAssignment { slot, model: model.or_else(|| default.clone()), effort, inherited });
        }
        Ok(out)
    }

    /// The model and effort for `slot`; unset slots fall back to the default model.
    pub fn resolve(&self, slot: &Slot) -> Result<(ModelRef, Effort), BrainsError> {
        let (model, effort) = self.stored(slot)?;
        let model = match model {
            Some(m) => m,
            None => self.default_model()?.ok_or(BrainsError::NoModels)?,
        };
        Ok((model, effort))
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

fn now_ms() -> i64 {
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
