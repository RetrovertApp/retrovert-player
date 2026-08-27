//! Live plugin-catalog refresh for Retrovert hosts.
//!
//! The catalog owns its own [`retrovert_updater::Updater`] and drives the
//! whole check → apply → activate → retain pipeline from one [`tick`],
//! called from the audio worker's loop. Generations are serialized: at most
//! one plugin set is loaded per process, activated all-or-nothing at the
//! mount/stop boundary, and a refused set falls back to reloading the
//! previous one from disk. The persisted activation record and the
//! [`active_generation`] accessor replace any event channel.
//!
//! [`tick`]: PluginCatalog::tick
//! [`active_generation`]: PluginCatalog::active_generation

mod payload;
mod record;

use std::path::{Path, PathBuf};
use std::time::Duration;

use retrovert_player::{BackendError, PlaybackBackend, PlayerBackend, SwapReport};
use retrovert_updater::{Generation, Priority, Updater, UpdaterConfig};

use payload::PayloadStore;
use record::ActivationLog;

pub use record::{Event, EventKind};
pub use retrovert_updater::{ChannelConfig, StatusSnapshot, WorkerConfig};

/// Constructing the catalog failed; everything after construction is not
/// fatal and lands in the activation record instead.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the updater could not be constructed")]
    Updater(#[source] retrovert_updater::Error),
}

/// Everything one catalog is configured with.
#[derive(Debug, Clone)]
pub struct CatalogConfig {
    /// The directory the catalog owns: the updater's generations, the
    /// extracted payload trees, and the activation record all live beneath
    /// it.
    pub root: PathBuf,
    /// The updater's evictable download cache.
    pub cache_dir: PathBuf,
    /// The updater's trust state. Must survive upgrades and power loss, and
    /// must not sit inside any cache.
    pub trust_state_dir: PathBuf,
    /// The update channel to follow.
    pub channel: ChannelConfig,
    /// The TUF root this binary trusts the channel against.
    pub embedded_root: Vec<u8>,
    /// The release target whose artifacts this host takes, such as
    /// `linux-x86_64`. [`CatalogConfig::native_target`] names the running
    /// platform's.
    pub target: String,
    /// The shortest time between two channel checks.
    pub check_interval: Duration,
    /// Whether a check that finds an update flows straight into a download.
    /// This is the download-consent knob; activation itself has no gate.
    pub auto_apply: bool,
    /// Transfer worker count and placement.
    pub workers: WorkerConfig,
}

impl CatalogConfig {
    /// A config keeping cache and trust state beneath `root`, checking
    /// hourly, downloading automatically.
    #[must_use]
    pub fn beneath(
        root: impl Into<PathBuf>,
        channel: ChannelConfig,
        embedded_root: Vec<u8>,
        target: impl Into<String>,
    ) -> Self {
        let root = root.into();
        Self {
            cache_dir: root.join("cache"),
            trust_state_dir: root.join("trust-state"),
            root,
            channel,
            embedded_root,
            target: target.into(),
            check_interval: Duration::from_secs(60 * 60),
            auto_apply: true,
            workers: WorkerConfig::default(),
        }
    }

    /// The release target the running platform consumes, when it is one the
    /// pipeline publishes.
    #[must_use]
    pub fn native_target() -> Option<&'static str> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => Some("linux-x86_64"),
            ("linux", "aarch64") => Some("linux-arm64"),
            ("windows", "x86_64") => Some("windows-x86_64"),
            _ => None,
        }
    }
}

/// Keeps one [`PlayerBackend`]'s decoder set current against a channel.
///
/// Owned by the same thread as the backend it feeds — the audio worker —
/// which calls [`PluginCatalog::tick`] every loop iteration and routes mounts
/// through [`PluginCatalog::mount`] so a pending generation activates at the
/// boundary the design fixes: nothing loads while a song is mounted, and an
/// active song finishes on the generation it started with.
pub struct PluginCatalog {
    updater: Updater,
    payloads: PayloadStore,
    log: ActivationLog,
    /// A completed generation waiting for the mount/stop boundary. At most
    /// one: a newer completion supersedes an unactivated older one.
    pending: Option<Generation>,
    /// Boot candidates still untried, newest last; only consulted while
    /// nothing has activated yet.
    boot_queue: Vec<Generation>,
    /// The generation whose plugin set the backend currently holds.
    loaded: Option<String>,
    booted: bool,
}

impl PluginCatalog {
    /// A catalog over `config`, fetching nothing yet.
    ///
    /// The backend starts empty; the first [`PluginCatalog::tick`] or
    /// [`PluginCatalog::mount`] re-probes the recorded active generation from
    /// disk, falling back to the newest one that loads clean.
    pub fn new(config: CatalogConfig) -> Result<Self, Error> {
        let payloads = PayloadStore::new(config.root.join("payloads"));
        let log = ActivationLog::load(config.root.join("activation.json"));
        let updater = Updater::new(UpdaterConfig {
            install_root: config.root,
            cache_dir: config.cache_dir,
            trust_state_dir: config.trust_state_dir,
            channel: config.channel,
            embedded_root: config.embedded_root,
            target: Some(config.target),
            check_interval: config.check_interval,
            auto_apply: config.auto_apply,
            workers: config.workers,
        })
        .map_err(Error::Updater)?;
        Ok(Self {
            updater,
            payloads,
            log,
            pending: None,
            boot_queue: Vec::new(),
            loaded: None,
            booted: false,
        })
    }

    /// One pump from the audio worker's loop.
    ///
    /// Costs a clock read when nothing is due. Checking and downloading run
    /// on the updater's own threads; only activation itself happens here, and
    /// only when no session is mounted.
    pub fn tick(&mut self, backend: &mut PlayerBackend) {
        self.boot_once();
        self.updater.tick();
        if let Some(generation) = self.updater.next_completed() {
            self.pending = Some(generation);
        }
        if self.pending.is_some() && !backend.is_live() {
            self.activate(backend);
        }
    }

    /// Replace the current session with the selected media, activating a
    /// pending generation at this boundary first.
    ///
    /// This is the mount consumers call instead of the backend's own: the
    /// old session and the old plugin set are fully dropped before the new
    /// generation loads, and the mount proceeds on whatever set is live.
    pub fn mount(
        &mut self,
        backend: &mut PlayerBackend,
        path: &Path,
        subsong: u16,
    ) -> Result<(), BackendError> {
        self.boot_once();
        if self.pending.is_some() {
            backend.close();
            self.activate(backend);
        }
        backend.mount(path, subsong)
    }

    /// Ask the channel now, whatever the check interval says.
    pub fn check_now(&self) {
        self.updater.check_now(Priority::User);
    }

    /// The generation whose plugin set is live, for the consumer's snapshot.
    #[must_use]
    pub fn active_generation(&self) -> Option<&str> {
        self.loaded.as_deref()
    }

    /// A completed generation waiting for the next mount/stop boundary.
    #[must_use]
    pub fn pending_generation(&self) -> Option<&str> {
        self.pending.as_ref().map(Generation::id)
    }

    /// Where the updater stands, for progress display.
    #[must_use]
    pub fn updater_status(&self) -> StatusSnapshot {
        self.updater.poll()
    }

    /// The activation history, oldest first.
    #[must_use]
    pub fn history(&self) -> &[Event] {
        self.log.history()
    }

    /// Seed the first activation from disk, once.
    fn boot_once(&mut self) {
        if self.booted {
            return;
        }
        self.booted = true;
        let mut generations = self.updater.generations().unwrap_or_default();
        generations.sort_by_key(|generation| modified(generation.dir()));
        // The recorded active generation re-probes first; the rest wait
        // newest-first in case it is gone or refuses to load.
        if let Some(position) = self
            .log
            .active()
            .and_then(|id| generations.iter().position(|g| g.id() == id))
        {
            let recorded = generations.remove(position);
            generations.push(recorded);
        }
        self.boot_queue = generations;
        self.pending = self.boot_queue.pop();
    }

    /// Load the pending generation into the backend, all-or-nothing.
    ///
    /// Called only when nothing is mounted. The old set is dropped before the
    /// new one loads — at most one generation is ever resident — so a refusal
    /// recovers by reloading the previous set's files, which retention keeps
    /// on disk until a newer activation has succeeded.
    fn activate(&mut self, backend: &mut PlayerBackend) {
        let Some(generation) = self.pending.take() else {
            return;
        };
        if self.loaded.as_deref() == Some(generation.id()) {
            self.boot_queue.clear();
            return;
        }
        let libraries =
            match self
                .payloads
                .ensure(generation.id(), generation.dir(), generation.artifacts())
            {
                Ok(libraries) => libraries,
                Err(detail) => {
                    log::error!("catalog: {} would not extract: {detail}", generation.id());
                    self.log.extract_failed(generation.id(), detail);
                    self.recover(backend, generation.id());
                    return;
                }
            };

        let report = backend.swap_plugins(&libraries);
        if accepted(&report, generation.artifacts().len()) {
            log::info!(
                "catalog: activated {} ({} plugins)",
                generation.id(),
                report.loaded
            );
            self.loaded = Some(generation.id().to_string());
            self.log.activated(generation.id());
            self.boot_queue.clear();
            self.clean_up(&generation);
        } else {
            let detail = describe(&report, generation.artifacts().len());
            log::error!("catalog: {} was refused: {detail}", generation.id());
            self.log.probe_failed(generation.id(), detail);
            self.recover(backend, generation.id());
        }
    }

    /// Put a working set back after `refused` did not activate.
    fn recover(&mut self, backend: &mut PlayerBackend, refused: &str) {
        let Some(previous) = self.loaded.clone() else {
            // Still booting: nothing was resident to fall back to. The refused
            // set is still loaded, so drop it before trying the next newest —
            // leaving it in place would let `mount` play on a refused set.
            let _ = backend.swap_plugins(&[]);
            self.pending = self.boot_queue.pop();
            return;
        };
        let reloaded = self.find(&previous).and_then(|generation| {
            let libraries = self
                .payloads
                .ensure(generation.id(), generation.dir(), generation.artifacts())
                .ok()?;
            let report = backend.swap_plugins(&libraries);
            accepted(&report, generation.artifacts().len()).then_some(())
        });
        if reloaded.is_some() {
            self.log
                .fell_back(refused, &previous, "the previous set was reloaded".into());
        } else {
            log::error!("catalog: the previous generation {previous} would not reload");
            self.log
                .probe_failed(&previous, "the previous generation would not reload".into());
            let _ = backend.swap_plugins(&[]);
            self.loaded = None;
        }
    }

    /// Collect what the activation of `activated` retired.
    ///
    /// Live is the active generation and the fallback pin; everything else —
    /// older generations, their payload trees, and the archives the download
    /// cache still holds for this set — goes. Runs at the activation
    /// boundary, where nothing renders.
    fn clean_up(&mut self, activated: &Generation) {
        let mut live = vec![activated.id().to_string()];
        if let Some(previous) = self.log.previous() {
            live.push(previous.to_string());
        }
        match self.updater.retain(&live) {
            Ok(removed) if !removed.is_empty() => {
                log::info!("catalog: retired {}", removed.join(", "));
            }
            Ok(_) => {}
            Err(e) => log::warn!("catalog: retention failed: {e}"),
        }
        self.payloads.retain(&live);
        for artifact in activated.artifacts() {
            self.updater.evict_cached(&artifact.sha256);
        }
    }

    fn find(&self, id: &str) -> Option<Generation> {
        self.updater
            .generations()
            .ok()?
            .into_iter()
            .find(|generation| generation.id() == id)
    }

    #[cfg(test)]
    fn inject_pending(&mut self, generation: Generation) {
        self.booted = true;
        self.pending = Some(generation);
    }
}

/// Whether a swap took every plugin the generation names.
fn accepted(report: &SwapReport, expected: usize) -> bool {
    report.errors.is_empty() && report.loaded == expected
}

fn describe(report: &SwapReport, expected: usize) -> String {
    if report.errors.is_empty() {
        return format!("{} of {expected} plugins loaded", report.loaded);
    }
    let errors: Vec<String> = report.errors.iter().map(ToString::to_string).collect();
    format!(
        "{} of {expected} plugins loaded: {}",
        report.loaded,
        errors.join("; ")
    )
}

fn modified(dir: &Path) -> std::time::SystemTime {
    std::fs::metadata(dir)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    use std::fs;

    use retrovert_host::session::StreamFormat;
    use retrovert_host::visualization::VisualizationConfig;

    /// The dev channel's public trust anchor; only parsed at construction,
    /// never fetched against in these tests.
    const DEV_ROOT: &[u8] = include_bytes!("../tests/fixtures/dev-root.json");

    fn backend() -> PlayerBackend {
        PlayerBackend::new(
            &[],
            VisualizationConfig::default(),
            StreamFormat {
                sample_rate: 48_000,
                channels: 2,
            },
            4_096,
        )
    }

    fn catalog(root: &Path) -> PluginCatalog {
        let channel = ChannelConfig {
            // Nothing listens here: construction parses the root and fetches
            // nothing, and a check that does run is refused instantly.
            metadata_base_url: "https://127.0.0.1:1/metadata/".to_string(),
            artifact_base_url: "https://127.0.0.1:1/artifacts/".to_string(),
        };
        let mut config = CatalogConfig::beneath(root, channel, DEV_ROOT.to_vec(), "linux-x86_64");
        // Keep the schedule from ever coming due under test.
        config.check_interval = Duration::from_secs(1_000_000);
        config.auto_apply = false;
        PluginCatalog::new(config).expect("a catalog")
    }

    fn generation_id(seed: &str) -> String {
        // Digest-shaped, as the applier requires of ids reaching the disk.
        let mut id = format!("{seed:x>4}").repeat(16);
        id.truncate(64);
        id
    }

    /// Publish a generation the way the updater would: files first, record
    /// last, beneath `<root>/generations/`.
    fn install(root: &Path, id: &str, artifacts: &[(&str, Vec<u8>)]) {
        let generations = root.join("generations");
        let dir = generations.join(id);
        fs::create_dir_all(&dir).unwrap();
        let mut installed = Vec::new();
        for (name, archive) in artifacts {
            let path = format!("{name}.tar.zst");
            fs::write(dir.join(&path), archive).unwrap();
            installed.push(serde_json::json!({
                "name": name,
                "revision": "abc1234",
                "sha256": "ab".repeat(32),
                "size": archive.len(),
                "path": path,
            }));
        }
        let record = serde_json::json!({
            "schema": 1,
            "generation": id,
            "artifacts": installed,
        });
        fs::write(
            generations.join(format!("{id}.generation.json")),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
    }

    fn garbage_plugin_archive(name: &str) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let lib = format!("{name}_playback{}", std::env::consts::DLL_SUFFIX);
        let bytes = b"not a loadable library";
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, lib, bytes.as_slice())
            .unwrap();
        let tarred = builder.into_inner().unwrap();
        zstd::stream::encode_all(tarred.as_slice(), 0).unwrap()
    }

    #[test]
    fn boot_activates_the_newest_generation_and_records_it() {
        let dir = tempfile::tempdir().unwrap();
        let id = generation_id("a");
        install(dir.path(), &id, &[]);

        let mut catalog = catalog(dir.path());
        let mut backend = backend();
        catalog.tick(&mut backend);

        assert_eq!(catalog.active_generation(), Some(id.as_str()));
        assert_eq!(
            catalog.history().last().map(|e| e.what),
            Some(EventKind::Activated)
        );
    }

    #[test]
    fn boot_reprobes_the_recorded_active_generation_over_a_newer_one() {
        let dir = tempfile::tempdir().unwrap();
        let older = generation_id("a");
        let newer = generation_id("b");
        install(dir.path(), &older, &[]);
        std::thread::sleep(Duration::from_millis(20));
        install(dir.path(), &newer, &[]);
        fs::write(
            dir.path().join("activation.json"),
            serde_json::json!({ "schema": 1, "active": older }).to_string(),
        )
        .unwrap();

        let mut catalog = catalog(dir.path());
        let mut backend = backend();
        catalog.tick(&mut backend);

        assert_eq!(catalog.active_generation(), Some(older.as_str()));
    }

    #[test]
    fn a_boot_generation_that_refuses_to_load_falls_back_to_the_next_newest() {
        let dir = tempfile::tempdir().unwrap();
        let good = generation_id("a");
        let bad = generation_id("b");
        install(dir.path(), &good, &[]);
        std::thread::sleep(Duration::from_millis(20));
        install(dir.path(), &bad, &[("spu", garbage_plugin_archive("spu"))]);

        let mut catalog = catalog(dir.path());
        let mut backend = backend();
        catalog.tick(&mut backend);
        // The refusal re-arms pending; the next boundary tries the fallback.
        catalog.tick(&mut backend);

        assert_eq!(catalog.active_generation(), Some(good.as_str()));
        let kinds: Vec<EventKind> = catalog.history().iter().map(|e| e.what).collect();
        assert_eq!(kinds, vec![EventKind::ProbeFailed, EventKind::Activated]);
    }

    #[test]
    fn a_refused_update_reloads_the_previous_generation() {
        let dir = tempfile::tempdir().unwrap();
        let good = generation_id("a");
        install(dir.path(), &good, &[]);

        let mut catalog = catalog(dir.path());
        let mut backend = backend();
        catalog.tick(&mut backend);
        assert_eq!(catalog.active_generation(), Some(good.as_str()));

        let bad = generation_id("b");
        install(dir.path(), &bad, &[("spu", garbage_plugin_archive("spu"))]);
        let refused = catalog.find(&bad).expect("the published update");
        catalog.inject_pending(refused);
        catalog.tick(&mut backend);

        assert_eq!(catalog.active_generation(), Some(good.as_str()));
        let last = catalog.history().last().expect("an event");
        assert_eq!(last.what, EventKind::FellBack);
        assert_eq!(last.generation, bad);
    }

    #[test]
    fn activation_retires_everything_but_the_active_and_previous_generations() {
        let dir = tempfile::tempdir().unwrap();
        let oldest = generation_id("a");
        let old = generation_id("b");
        install(dir.path(), &oldest, &[]);
        std::thread::sleep(Duration::from_millis(20));
        install(dir.path(), &old, &[]);

        let mut catalog = catalog(dir.path());
        let mut backend = backend();
        catalog.tick(&mut backend);
        assert_eq!(catalog.active_generation(), Some(old.as_str()));

        let newest = generation_id("c");
        install(dir.path(), &newest, &[]);
        let update = catalog.find(&newest).expect("the published update");
        catalog.inject_pending(update);
        catalog.tick(&mut backend);

        assert_eq!(catalog.active_generation(), Some(newest.as_str()));
        let mut remaining: Vec<String> = catalog
            .updater
            .generations()
            .unwrap()
            .into_iter()
            .map(|g| g.id().to_string())
            .collect();
        remaining.sort();
        let mut expected = vec![old.clone(), newest.clone()];
        expected.sort();
        assert_eq!(remaining, expected, "active and previous survive");
    }
}
