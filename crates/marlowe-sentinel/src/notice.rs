//! Notice drafts, ranking, and when to speak.

use marlowe_proto::{Notice, NoticeSource, QuietHours, Severity};

/// A notice before it is recorded (the store assigns `id` and `at_ms`).
#[derive(Debug, Clone, PartialEq)]
pub struct NoticeDraft {
    pub source: NoticeSource,
    pub severity: Severity,
    /// e.g. `finished`, `stalled`, `failed`, `nan`, `oom`, `diverged`, `plateau`, `best`,
    /// `new_commits`, `new_model`.
    pub kind: String,
    pub title: String,
    pub detail: String,
    pub subject: String,
}

impl NoticeDraft {
    pub fn new(source: NoticeSource, severity: Severity, kind: &str, title: impl Into<String>, detail: impl Into<String>, subject: impl Into<String>) -> Self {
        Self { source, severity, kind: kind.into(), title: title.into(), detail: detail.into(), subject: subject.into() }
    }

    /// The dedup key.
    pub fn key(&self) -> (NoticeSource, &str, &str) {
        (self.source, &self.subject, &self.kind)
    }
}

/// Sorts most important first: severity descending, then newest.
pub fn rank(notices: &mut [Notice]) {
    notices.sort_by(|a, b| b.severity.cmp(&a.severity).then(b.at_ms.cmp(&a.at_ms)));
}

/// Whether a notice is spoken: urgent and outside quiet hours (`local_minute` after midnight).
/// Quiet hours never stop recording.
pub fn should_speak(notice: &Notice, quiet: QuietHours, local_minute: u16) -> bool {
    notice.severity.is_urgent() && !quiet.contains(local_minute)
}
