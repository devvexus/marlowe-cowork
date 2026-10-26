//! The memory models' first-run offer: the answer, and the download on its own thread.
//!
//! Onboarding asks once whether to install them. Yes fetches them with `marlowe_mind::download`
//! (progress and the outcome ride `MemoryStats`, the status the Wake memory line already reads) and
//! then has the Mind load them, no restart. No is remembered. A failed download is shown and not
//! retried; `marlowe models fetch` is the way back.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use marlowe_mind::download::{self, DownloadError, Net, Policy};
use marlowe_mind::Mind;
use marlowe_proto::{Event, MemoryStatsReport, ModelsState};
use serde::{Deserialize, Serialize};

use crate::daemon::Core;
use crate::lock;

/// File under the profile home holding the first-run answers that are not the user's name.
pub const ANSWERS_FILE: &str = "onboarding.json";

/// Progress is broadcast at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    Accepted,
    Declined,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Answers {
    #[serde(default)]
    memory_models: Option<Answer>,
}

/// The remembered answer to the memory-models offer, if there is one.
pub fn answer(home: &Path) -> Option<Answer> {
    let text = std::fs::read_to_string(home.join(ANSWERS_FILE)).ok()?;
    serde_json::from_str::<Answers>(&text).ok()?.memory_models
}

/// Remembers the answer (temp file + rename).
pub fn save_answer(home: &Path, a: Answer) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(&Answers { memory_models: Some(a) }).map_err(std::io::Error::other)?;
    let path = home.join(ANSWERS_FILE);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

/// What this daemon process knows about a download.
#[derive(Debug, Default)]
pub(crate) struct ModelsStatus {
    /// Bytes done and total while a download runs.
    installing: Option<(u64, u64)>,
    /// Why the last one stopped.
    failed: Option<String>,
}

/// The report with `models` as this daemon knows it: a running download, its failure, or the
/// remembered answer on top of what the Mind sees on disk.
pub(crate) fn overlay(core: &Core, mut s: MemoryStatsReport) -> MemoryStatsReport {
    let st = lock(&core.memory.models);
    s.models = match (&st.installing, &st.failed, std::mem::take(&mut s.models)) {
        (Some((done, total)), _, _) => ModelsState::Installing { done: *done, total: *total },
        (None, Some(reason), ModelsState::Missing { .. }) => ModelsState::Failed { reason: reason.clone() },
        (None, None, missing @ ModelsState::Missing { .. }) => match answer(&core.home) {
            Some(Answer::Declined) => ModelsState::Declined,
            Some(Answer::Accepted) => ModelsState::Failed { reason: "the earlier install did not finish".into() },
            None => missing,
        },
        (_, _, other) => other,
    };
    s
}

/// `ModelsDecline`: remembered, and the memory line says how to install later.
pub(crate) fn decline(core: &Arc<Core>, mind: &Mind) {
    if let Err(e) = save_answer(&core.home, Answer::Declined) {
        core.bus.publish(Event::Error { session: None, message: format!("could not save {ANSWERS_FILE}: {e}") });
    }
    publish(core, mind);
}

/// Broadcasts the Mind's stats with the daemon's view of the models.
fn publish(core: &Core, mind: &Mind) {
    if let Ok(s) = mind.stats() {
        core.bus.publish(Event::MemoryStats(overlay(core, s)));
    }
}

/// Clears the running state however the install ends, a panic included.
struct Running<'a>(&'a Core);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        lock(&self.0.memory.models).installing = None;
    }
}

/// `ModelsInstall`: runs on the request's worker thread until the download ends.
pub(crate) fn install(core: &Arc<Core>, mind: &Mind) {
    let base = download::base_url();
    let entries = download::manifest(download::WITH_CASCADE, &base);
    {
        let mut st = lock(&core.memory.models);
        if st.installing.is_some() {
            return;
        }
        st.failed = None;
        st.installing = Some((0, download::total_bytes(&entries)));
    }
    let _running = Running(core);
    if let Err(e) = save_answer(&core.home, Answer::Accepted) {
        core.bus.publish(Event::Error { session: None, message: format!("could not save {ANSWERS_FILE}: {e}") });
    }
    // The stats around the transfer are read once; progress only changes `models`.
    let shell = mind.stats().unwrap_or_default();
    core.bus.publish(Event::MemoryStats(overlay(core, shell.clone())));

    let result = match download::dest_dir() {
        None => Err("LOCALAPPDATA is not set, so there is nowhere to install them".to_string()),
        Some(dest) => {
            let mut last = Instant::now();
            download::download(&entries, &dest, &Net, &Policy::for_base(&base), &mut |p| {
                lock(&core.memory.models).installing = Some((p.done, p.total));
                if last.elapsed() >= PROGRESS_EVERY || p.done == p.total {
                    last = Instant::now();
                    core.bus.publish_ephemeral(Event::MemoryStats(overlay(core, shell.clone())));
                }
            })
            .map_err(|e: DownloadError| e.to_string())
        }
    };
    let loaded = result.and_then(|()| mind.reload_models().map_err(|e| e.to_string()));
    {
        let mut st = lock(&core.memory.models);
        st.installing = None;
        st.failed = loaded.err();
    }
    publish(core, mind);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let h = std::env::temp_dir().join(format!("marlowe-answers-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&h).unwrap();
        h
    }

    #[test]
    fn an_answer_is_remembered_and_a_missing_or_bad_file_is_none() {
        let h = home();
        assert_eq!(answer(&h), None);
        save_answer(&h, Answer::Declined).unwrap();
        assert_eq!(answer(&h), Some(Answer::Declined));
        assert!(!h.join("onboarding.json.tmp").exists());
        save_answer(&h, Answer::Accepted).unwrap();
        assert_eq!(answer(&h), Some(Answer::Accepted));
        std::fs::write(h.join(ANSWERS_FILE), "{not json").unwrap();
        assert_eq!(answer(&h), None);
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn the_name_file_and_the_answers_file_do_not_touch() {
        let h = home();
        crate::user::save(&h, &marlowe_agent::UserInfo { name: Some("Matthew".into()), address: None }).unwrap();
        save_answer(&h, Answer::Declined).unwrap();
        assert_eq!(crate::user::load(&h).and_then(|u| u.name).as_deref(), Some("Matthew"));
        assert_eq!(answer(&h), Some(Answer::Declined));
        let _ = std::fs::remove_dir_all(&h);
    }
}
