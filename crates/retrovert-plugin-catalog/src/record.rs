//! The catalog's persisted activation record.
//!
//! One JSON file naming the active generation, the one before it, and what
//! recently happened — activated, fell back, probe failed — so a boot can
//! re-probe what was running and diagnostics can read history without an
//! event channel. Writing it is best-effort: a record that cannot persist
//! costs the next boot its memory, never this session its playback.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The record schema this build reads and writes.
const SCHEMA: u32 = 1;

/// How much history the record keeps, newest last.
const HISTORY_CAP: usize = 64;

/// What one activation attempt came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    /// The generation went live.
    Activated,
    /// The generation was refused and the previous one was reloaded.
    FellBack,
    /// The generation was refused and nothing could be reloaded over it.
    ProbeFailed,
    /// The generation's archives would not extract into a loadable tree.
    ExtractFailed,
}

/// One entry in the activation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// What happened.
    pub what: EventKind,
    /// The generation it happened to.
    pub generation: String,
    /// Why, when there is more to say than the kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// When, as Unix milliseconds.
    pub at_unix_ms: i64,
}

#[derive(Debug, Serialize, Deserialize)]
struct RecordFile {
    schema: u32,
    #[serde(default)]
    active: Option<String>,
    #[serde(default)]
    previous: Option<String>,
    #[serde(default)]
    history: Vec<Event>,
}

impl Default for RecordFile {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            active: None,
            previous: None,
            history: Vec::new(),
        }
    }
}

/// The record, loaded once and rewritten on every event.
pub(crate) struct ActivationLog {
    path: PathBuf,
    record: RecordFile,
}

impl ActivationLog {
    /// Load the record at `path`, starting fresh when it is missing or
    /// unreadable — a corrupt record must not brick the catalog.
    pub(crate) fn load(path: PathBuf) -> Self {
        let record = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<RecordFile>(&bytes).ok())
            .filter(|record| record.schema == SCHEMA)
            .unwrap_or_default();
        Self { path, record }
    }

    /// The generation recorded as active, which is what a boot re-probes.
    pub(crate) fn active(&self) -> Option<&str> {
        self.record.active.as_deref()
    }

    /// The generation active before the current one: the fallback pin.
    pub(crate) fn previous(&self) -> Option<&str> {
        self.record.previous.as_deref()
    }

    /// The recorded history, oldest first.
    pub(crate) fn history(&self) -> &[Event] {
        &self.record.history
    }

    /// Record that `id` went live.
    pub(crate) fn activated(&mut self, id: &str) {
        if self.record.active.as_deref() != Some(id) {
            self.record.previous = self.record.active.take();
        }
        self.record.active = Some(id.to_string());
        self.push(EventKind::Activated, id, None);
    }

    /// Record that `refused` did not activate and `to` was reloaded.
    pub(crate) fn fell_back(&mut self, refused: &str, to: &str, detail: String) {
        self.push(
            EventKind::FellBack,
            refused,
            Some(format!("{detail}; reloaded {to}")),
        );
    }

    /// Record that `id` was refused with nothing reloaded over it.
    pub(crate) fn probe_failed(&mut self, id: &str, detail: String) {
        self.push(EventKind::ProbeFailed, id, Some(detail));
    }

    /// Record that `id` would not extract.
    pub(crate) fn extract_failed(&mut self, id: &str, detail: String) {
        self.push(EventKind::ExtractFailed, id, Some(detail));
    }

    fn push(&mut self, what: EventKind, generation: &str, detail: Option<String>) {
        self.record.history.push(Event {
            what,
            generation: generation.to_string(),
            detail,
            at_unix_ms: now_ms(),
        });
        if self.record.history.len() > HISTORY_CAP {
            let excess = self.record.history.len() - HISTORY_CAP;
            self.record.history.drain(..excess);
        }
        self.persist();
    }

    /// Write the record through a sibling and a rename, so a crash leaves the
    /// previous record rather than half of this one.
    fn persist(&self) {
        let staging = self.path.with_extension("json.next");
        let written = serde_json::to_vec_pretty(&self.record)
            .map_err(std::io::Error::other)
            .and_then(|bytes| fs::write(&staging, bytes))
            .and_then(|()| fs::rename(&staging, &self.path));
        if let Err(e) = written {
            log::warn!(
                "catalog: could not persist the activation record {}: {e}",
                self.path.display()
            );
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn activation_moves_the_pins_and_survives_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activation.json");

        let mut log = ActivationLog::load(path.clone());
        log.activated("gen-a");
        log.activated("gen-b");
        log.fell_back("gen-c", "gen-b", "one library refused".to_string());

        let reloaded = ActivationLog::load(path);
        assert_eq!(reloaded.active(), Some("gen-b"));
        assert_eq!(reloaded.previous(), Some("gen-a"));
        let kinds: Vec<EventKind> = reloaded.history().iter().map(|event| event.what).collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::Activated,
                EventKind::Activated,
                EventKind::FellBack
            ]
        );
    }

    #[test]
    fn re_activating_the_active_generation_keeps_the_previous_pin() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = ActivationLog::load(dir.path().join("activation.json"));
        log.activated("gen-a");
        log.activated("gen-b");
        log.activated("gen-b");
        assert_eq!(log.previous(), Some("gen-a"));
    }

    #[test]
    fn a_corrupt_record_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activation.json");
        fs::write(&path, b"not json at all").unwrap();
        let log = ActivationLog::load(path);
        assert_eq!(log.active(), None);
        assert!(log.history().is_empty());
    }
}
