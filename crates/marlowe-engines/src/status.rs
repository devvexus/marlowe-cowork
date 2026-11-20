//! What the backends window shows for one backend: the installed copy, the newest release and
//! build upstream (from the last lookup), and how far behind the installed copy is.

use std::path::Path;

use crate::backend::Backend;
use crate::installed::{self, Installed, Kind};
use crate::remote::{self, release_cmp, Remote};
use crate::state;

/// Whether the installed copy is the newest release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    NotInstalled,
    UpToDate,
    UpdateAvailable,
    /// Installed, with no release known to compare against.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub backend: Backend,
    pub selected: bool,
    pub installed: Option<Installed>,
    pub rollback: Option<String>,
    /// `(tag, published)` of the newest release and the newest build with this platform's files.
    pub latest_release: Option<(String, String)>,
    pub newest_build: Option<(String, String)>,
    pub update: Update,
    pub builds_newer: Option<u32>,
    pub last_checked: Option<u64>,
    pub dont_ask: bool,
}

/// The status of `backend` from what is on disk.
pub fn status(root: &Path, backend: Backend) -> Status {
    let st = state::load(root);
    let own = st.of(backend);
    let inst = installed::current(root, backend);
    let remote = remote::load(root, backend);
    let (update, builds_newer) = compare(backend, inst.as_ref(), remote.as_ref());
    Status {
        backend,
        selected: st.selected() == backend,
        installed: inst,
        rollback: own.previous.filter(|t| installed::read(root, backend, t).is_some()),
        latest_release: remote.as_ref().and_then(|r| r.latest_release.as_ref()).map(|r| (r.tag.clone(), r.published.clone())),
        newest_build: remote.as_ref().and_then(|r| r.newest_build.as_ref()).map(|r| (r.tag.clone(), r.published.clone())),
        update,
        builds_newer,
        last_checked: remote.as_ref().map(|r| r.fetched).filter(|f| *f > 0),
        dont_ask: own.dont_ask,
    }
}

/// The update state and the builds newer upstream than `inst`.
pub fn compare(backend: Backend, inst: Option<&Installed>, remote: Option<&Remote>) -> (Update, Option<u32>) {
    let Some(inst) = inst else { return (Update::NotInstalled, None) };
    let Some(remote) = remote else { return (Update::Unknown, None) };
    let update = match remote.latest_release.as_ref() {
        None => Update::Unknown,
        Some(latest) => {
            if backend.releases_point_at_builds() {
                match (inst.build_number, remote.release_build) {
                    (Some(have), Some(release)) if have >= release => Update::UpToDate,
                    (Some(_), Some(_)) => Update::UpdateAvailable,
                    _ => by_release_tag(inst, &latest.tag),
                }
            } else {
                by_release_tag(inst, &latest.tag)
            }
        }
    };
    (update, builds_newer(inst, remote))
}

fn by_release_tag(inst: &Installed, latest: &str) -> Update {
    match inst.release.as_deref().or((inst.kind == Kind::Release).then_some(inst.tag.as_str())) {
        Some(have) if release_cmp(have, latest) == std::cmp::Ordering::Less => Update::UpdateAvailable,
        Some(_) => Update::UpToDate,
        None => Update::Unknown,
    }
}

/// Builds upstream newer than `inst`: by build number where there is one, else by publish date.
/// `None` until a builds lookup has happened.
fn builds_newer(inst: &Installed, remote: &Remote) -> Option<u32> {
    if remote.builds.is_empty() {
        return None;
    }
    if let (Some(have), Some(newest)) = (inst.build_number, remote.builds.iter().filter_map(|b| b.number).max()) {
        return Some(newest.saturating_sub(have));
    }
    let after = inst.published.as_deref().unwrap_or("");
    Some(remote.builds.iter().filter(|b| b.published.as_str() > after).count() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{lay_out, record, scratch};
    use crate::remote::{BuildRef, Release};

    fn rel(tag: &str) -> Release {
        Release { tag: tag.into(), published: "2026-11-13T00:00:00Z".into(), assets: Vec::new() }
    }

    fn upstream_remote(release: &str, release_build: u32, newest: u32) -> Remote {
        Remote {
            fetched: 9,
            builds_fetched: 9,
            latest_release: Some(rel(release)),
            release_build: Some(release_build),
            newest_build: Some(rel(&format!("b{newest}"))),
            builds: vec![BuildRef { tag: format!("b{newest}"), published: "2026-11-20T00:00:00Z".into(), number: Some(newest) }],
        }
    }

    #[test]
    fn a_release_install_is_up_to_date_until_a_newer_release_names_a_later_build() {
        let mut inst = record(Backend::LlamaCpp, "v1.0.0", Kind::Release, 1);
        inst.release = Some("v1.0.0".into());
        inst.build_number = Some(12359);
        let (u, newer) = compare(Backend::LlamaCpp, Some(&inst), Some(&upstream_remote("v1.0.0", 12359, 12556)));
        assert_eq!((u, newer), (Update::UpToDate, Some(197)));
        let (u, _) = compare(Backend::LlamaCpp, Some(&inst), Some(&upstream_remote("v1.1.0", 12700, 12710)));
        assert_eq!(u, Update::UpdateAvailable);
    }

    #[test]
    fn a_build_ahead_of_the_release_is_up_to_date_and_builds_never_make_an_update() {
        let mut inst = record(Backend::LlamaCpp, "b12500", Kind::Build, 1);
        inst.build_number = Some(12500);
        inst.release = Some("v1.0.0".into());
        let (u, newer) = compare(Backend::LlamaCpp, Some(&inst), Some(&upstream_remote("v1.0.0", 12359, 12556)));
        assert_eq!((u, newer), (Update::UpToDate, Some(56)));
        let (_, newer) = compare(Backend::LlamaCpp, Some(&inst), Some(&upstream_remote("v1.0.0", 12359, 12500)));
        assert_eq!(newer, Some(0));
    }

    #[test]
    fn the_fork_compares_release_tags_and_counts_builds_by_date() {
        let mut inst = record(Backend::MsLlamaCpp, "ms-v1.0", Kind::Release, 1);
        inst.release = Some("ms-v1.0".into());
        inst.published = Some("2026-11-17T22:30:33Z".into());
        let remote = Remote {
            fetched: 3,
            latest_release: Some(rel("ms-v1.1")),
            builds: vec![
                BuildRef { tag: "ms-b2".into(), published: "2026-11-19T00:00:00Z".into(), number: None },
                BuildRef { tag: "ms-b1".into(), published: "2026-11-18T00:00:00Z".into(), number: None },
                BuildRef { tag: "ms-b0".into(), published: "2026-11-01T00:00:00Z".into(), number: None },
            ],
            ..Default::default()
        };
        assert_eq!(compare(Backend::MsLlamaCpp, Some(&inst), Some(&remote)), (Update::UpdateAvailable, Some(2)));
    }

    #[test]
    fn nothing_installed_or_nothing_looked_up_is_said_plainly() {
        assert_eq!(compare(Backend::LlamaCpp, None, None), (Update::NotInstalled, None));
        let inst = record(Backend::LlamaCpp, "v1.0.0", Kind::Release, 1);
        assert_eq!(compare(Backend::LlamaCpp, Some(&inst), None), (Update::Unknown, None));
    }

    #[test]
    fn status_reads_the_disk_and_reports_only_a_rollback_that_still_exists() {
        let root = scratch("status");
        let mut inst = record(Backend::LlamaCpp, "v1.0.0", Kind::Release, 5);
        inst.release = Some("v1.0.0".into());
        inst.build_number = Some(12359);
        lay_out(&root, &inst, false);
        state::update(&root, |s| {
            s.entry(Backend::LlamaCpp).current = Some("v1.0.0".into());
            s.entry(Backend::LlamaCpp).previous = Some("v0.9.0".into());
        })
        .unwrap();
        remote::save(&root, Backend::LlamaCpp, &upstream_remote("v1.0.0", 12359, 12556)).unwrap();
        let s = status(&root, Backend::LlamaCpp);
        assert!(s.selected && s.installed.is_some());
        assert_eq!((s.update, s.builds_newer, s.rollback.as_deref()), (Update::UpToDate, Some(197), None));
        assert_eq!(s.latest_release.unwrap().0, "v1.0.0");
        let ms = status(&root, Backend::MsLlamaCpp);
        assert!(!ms.selected && ms.update == Update::NotInstalled);
        let _ = std::fs::remove_dir_all(&root);
    }
}
