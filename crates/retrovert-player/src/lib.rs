//! Headless playback coordination for Retrovert decoder plugins.

mod extensions;

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use retrovert_host::loader::{load_plugins, PluginError, PluginKind, PluginSet};
use retrovert_host::service::{ServiceHost, TrackMetadata};
use retrovert_host::session::{
    OwnedPlaybackRuntime, OwnedPreparedSession, OwnedSessionError, SessionError, StreamFormat,
};
use retrovert_host::visualization::{
    VisualizationConfig, VisualizationError, VizLayout, VizSnapshot,
};
use thiserror::Error;

const MEDIA_PROBE_BYTES: usize = 16_384;

/// Playback state observed by the host adapter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlaybackStatus {
    #[default]
    Idle,
    Loading,
    Playing,
    Paused,
    Error,
    Finished,
}

/// Failures while selecting, opening, rendering, or capturing a session.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BackendError {
    #[error("could not read media")]
    Media(#[source] std::io::Error),
    #[error("no playback plugin accepted the media")]
    NoPlugin,
    #[error("decode session failed")]
    Session(#[source] SessionError),
    #[error("visualization capture failed")]
    Visualization(#[source] VisualizationError),
    #[error("decode runtime failed")]
    Runtime(#[source] OwnedSessionError),
}

impl From<SessionError> for BackendError {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

impl From<OwnedSessionError> for BackendError {
    fn from(error: OwnedSessionError) -> Self {
        match error {
            OwnedSessionError::Session(error) => Self::Session(error),
            OwnedSessionError::Visualization(error) => Self::Visualization(error),
            // Required because retrovert-host may add owned-session failures.
            error => Self::Runtime(error),
        }
    }
}

impl From<VisualizationError> for BackendError {
    fn from(error: VisualizationError) -> Self {
        Self::Visualization(error)
    }
}

/// Synchronous decode-session operations driven by a host-owned run loop.
pub trait PlaybackBackend {
    /// Reports whether a media session is mounted.
    fn is_live(&self) -> bool;
    /// Replaces the current session with the selected media and subsong.
    fn mount(&mut self, path: &Path, subsong: u16) -> Result<(), BackendError>;
    /// Drops the current session.
    fn close(&mut self);
    /// Toggles a live session and reports whether it is now playing.
    fn toggle_pause(&mut self) -> Option<bool>;
    /// Reads static metadata through the selected decoder.
    fn probe_metadata(&self, path: &Path) -> Result<Option<TrackMetadata>, BackendError>;
    /// Checks a normalized or dot-prefixed media extension.
    fn can_handle_extension(&self, extension: &str) -> bool;
    /// Decodes at most `frames` and lends the produced interleaved samples.
    fn render(&mut self, frames: u32) -> Result<&[f32], BackendError>;
    /// Returns the layout required to allocate a caller-owned capture slot.
    fn visualization_layout(&self) -> Option<&Arc<VizLayout>>;
    /// Captures the current visualization into a compatible caller-owned slot.
    fn capture(&mut self, snapshot: &mut VizSnapshot) -> Result<(), BackendError>;
    /// Reports the current session status.
    fn status(&self) -> PlaybackStatus;
    /// Reports the selected decoder name, or an empty string when idle.
    fn plugin_name(&self) -> &str;
    /// Reports the mounted media extension, or an empty string when idle.
    fn media_extension(&self) -> &str;
}

/// The outcome of one decoder-set swap.
#[derive(Debug)]
pub struct SwapReport {
    /// How many playback plugins the replacement set holds.
    pub loaded: usize,
    /// The paths rejected while loading the replacement set.
    pub errors: Vec<PluginError>,
}

#[derive(Debug)]
struct Session {
    player: OwnedPreparedSession,
    plugin_name: String,
    media_extension: String,
    status: PlaybackStatus,
    output_frame: u64,
}

/// Decoder catalog and optional prepared session owned by one host thread.
#[derive(Debug)]
pub struct PlayerBackend {
    runtime: OwnedPlaybackRuntime,
    extensions: std::collections::HashSet<String>,
    visualization: VisualizationConfig,
    target: StreamFormat,
    max_frames: u32,
    session: Option<Session>,
}

impl PlayerBackend {
    /// Loads decoder plugins and fixes the render and visualization budgets.
    pub fn new(
        plugin_paths: &[PathBuf],
        visualization: VisualizationConfig,
        target: StreamFormat,
        max_frames: u32,
    ) -> Self {
        let report = load_plugins(plugin_paths);
        for error in &report.errors {
            log::error!("player: {error}");
        }
        let extensions = extensions::supported_extensions(&report.plugins);
        let runtime = OwnedPlaybackRuntime::new(report.plugins, ServiceHost::default());
        Self {
            runtime,
            extensions,
            visualization,
            target,
            max_frames,
            session: None,
        }
    }

    /// Replaces the decoder set and the extension index together.
    ///
    /// The current session and the old decoder set are fully dropped before
    /// anything from `plugin_paths` loads, so at most one set is ever
    /// resident. A rejected path rolls nothing back — the old set is already
    /// gone — so the caller judges the report and swaps back to the previous
    /// set's paths if it refuses the new one.
    pub fn swap_plugins(&mut self, plugin_paths: &[PathBuf]) -> SwapReport {
        self.session = None;
        self.extensions = std::collections::HashSet::new();
        self.runtime = OwnedPlaybackRuntime::new(PluginSet::default(), ServiceHost::default());
        let report = load_plugins(plugin_paths);
        let loaded = report.plugins.of_kind(PluginKind::Playback).count();
        self.extensions = extensions::supported_extensions(&report.plugins);
        self.runtime = OwnedPlaybackRuntime::new(report.plugins, ServiceHost::default());
        SwapReport {
            loaded,
            errors: report.errors,
        }
    }

    fn select(
        &self,
        path: &Path,
    ) -> Result<retrovert_host::session::OwnedPlaybackPlugin, BackendError> {
        let mut file = File::open(path).map_err(BackendError::Media)?;
        let total_size = file.metadata().map_err(BackendError::Media)?.len();
        let mut probe = [0_u8; MEDIA_PROBE_BYTES];
        let probe_size = file.read(&mut probe).map_err(BackendError::Media)?;
        let path_text = path.to_string_lossy();
        self.runtime
            .select(&probe[..probe_size], &path_text, total_size)
            .ok_or(BackendError::NoPlugin)
    }
}

impl PlaybackBackend for PlayerBackend {
    fn is_live(&self) -> bool {
        self.session.is_some()
    }

    fn mount(&mut self, path: &Path, subsong: u16) -> Result<(), BackendError> {
        self.session = None;
        let plugin = self.select(path)?;
        let plugin_name = plugin.name().to_owned();
        let media_extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let mut player = plugin.open_prepared_with_target(
            &path.to_string_lossy(),
            u32::from(subsong),
            self.target,
            self.max_frames,
            Some(self.visualization),
        )?;
        player.set_scope_enabled(true);
        self.session = Some(Session {
            player,
            plugin_name,
            media_extension,
            status: PlaybackStatus::Playing,
            output_frame: 0,
        });
        Ok(())
    }

    fn close(&mut self) {
        self.session = None;
    }

    fn toggle_pause(&mut self) -> Option<bool> {
        let session = self.session.as_mut()?;
        session.status = match session.status {
            PlaybackStatus::Playing => PlaybackStatus::Paused,
            PlaybackStatus::Paused => PlaybackStatus::Playing,
            PlaybackStatus::Idle
            | PlaybackStatus::Loading
            | PlaybackStatus::Error
            | PlaybackStatus::Finished => return None,
        };
        Some(session.status == PlaybackStatus::Playing)
    }

    fn probe_metadata(&self, path: &Path) -> Result<Option<TrackMetadata>, BackendError> {
        self.select(path)?
            .metadata(&path.to_string_lossy())
            .map_err(BackendError::from)
    }

    fn can_handle_extension(&self, extension: &str) -> bool {
        self.extensions.contains(
            extension
                .trim_start_matches('.')
                .to_ascii_lowercase()
                .as_str(),
        )
    }

    fn render(&mut self, frames: u32) -> Result<&[f32], BackendError> {
        let Some(session) = self.session.as_mut() else {
            return Ok(&[]);
        };
        if session.status != PlaybackStatus::Playing || frames == 0 {
            return Ok(&[]);
        }
        let chunk = session.player.read(frames.min(self.max_frames))?;
        let produced = chunk.frames() as u64;
        let finished = chunk.finished;
        session.output_frame = session.output_frame.saturating_add(produced);
        if finished {
            session.status = PlaybackStatus::Finished;
            log::info!("player: music playback finished");
        }
        Ok(chunk.samples)
    }

    fn visualization_layout(&self) -> Option<&Arc<VizLayout>> {
        self.session
            .as_ref()
            .and_then(|session| session.player.layout())
    }

    fn capture(&mut self, snapshot: &mut VizSnapshot) -> Result<(), BackendError> {
        let Some(session) = self.session.as_mut() else {
            return Ok(());
        };
        session
            .player
            .capture_visualization(session.output_frame, snapshot)
            .map_err(BackendError::from)
    }

    fn status(&self) -> PlaybackStatus {
        self.session
            .as_ref()
            .map_or(PlaybackStatus::Idle, |session| session.status)
    }

    fn plugin_name(&self) -> &str {
        self.session
            .as_ref()
            .map_or("", |session| session.plugin_name.as_str())
    }

    fn media_extension(&self) -> &str {
        self.session
            .as_ref()
            .map_or("", |session| session.media_extension.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment() -> (PathBuf, PathBuf) {
        let Some(build_dir) = std::env::var_os("BUILD_DIR") else {
            panic!("BUILD_DIR must identify the CMake build tree");
        };
        let Some(project_root) = std::env::var_os("PROJECT_ROOT") else {
            panic!("PROJECT_ROOT must identify the Replay source tree");
        };
        (PathBuf::from(build_dir), PathBuf::from(project_root))
    }

    fn backend(plugin_path: PathBuf) -> PlayerBackend {
        PlayerBackend::new(
            &[plugin_path],
            VisualizationConfig::default(),
            StreamFormat {
                sample_rate: 48_000,
                channels: 2,
            },
            4_096,
        )
    }

    fn check_decoder(plugin: &str, media: &str, expected_name: &str, expected_extension: &str) {
        let (build_dir, project_root) = environment();
        let plugin_path = build_dir
            .join("plugins/music_player/playback_plugins")
            .join(plugin);
        let media_path = project_root.join(media);
        assert!(plugin_path.is_file(), "{}", plugin_path.display());
        assert!(media_path.is_file(), "{}", media_path.display());

        let mut backend = backend(plugin_path);
        let metadata = backend.probe_metadata(&media_path);
        assert!(metadata.is_ok(), "{metadata:?}");
        assert!(metadata.ok().flatten().is_some());
        assert!(backend.can_handle_extension(expected_extension));
        assert!(backend.can_handle_extension(&format!(".{expected_extension}")));
        assert!(backend.can_handle_extension(&expected_extension.to_ascii_uppercase()));
        assert!(!backend.can_handle_extension("not-a-media-extension"));
        let mounted = backend.mount(&media_path, 0);
        assert!(mounted.is_ok(), "{mounted:?}");
        let rendered = backend.render(2_048);
        assert!(rendered.is_ok(), "{rendered:?}");
        assert!(rendered.is_ok_and(|samples| !samples.is_empty()));
        assert_eq!(backend.plugin_name(), expected_name);
        let Some(layout) = backend.visualization_layout().cloned() else {
            panic!("{expected_name} did not publish a visualization layout");
        };
        let mut snapshot = layout
            .new_snapshot()
            .unwrap_or_else(|error| panic!("{error}"));
        let captured = backend.capture(&mut snapshot);
        assert!(captured.is_ok(), "{captured:?}");
    }

    #[test]
    #[ignore = "requires the CMake-built OpenMPT playback plugin"]
    fn openmpt_decoder_drives_a_prepared_session() {
        let (build_dir, _) = environment();
        let media_path = build_dir.join("test_data/generated_player_engine.mod");
        let mut module = vec![0_u8; 1_084 + 1_024 + 4];
        module[..16].copy_from_slice(b"Replay PT oracle");
        module[42..44].copy_from_slice(&2_u16.to_be_bytes());
        module[45] = 64;
        module[48..50].copy_from_slice(&1_u16.to_be_bytes());
        module[950] = 1;
        module[1_080..1_084].copy_from_slice(b"M.K.");
        module[1_084..1_088].copy_from_slice(&[1, 172, 16, 0]);
        module[2_108..].copy_from_slice(&[0, 127, 0, 129]);
        let created = std::fs::create_dir_all(media_path.parent().unwrap_or(Path::new(".")));
        assert!(created.is_ok(), "{created:?}");
        let written = std::fs::write(&media_path, module);
        assert!(written.is_ok(), "{written:?}");
        let plugin_path =
            build_dir.join("plugins/music_player/playback_plugins/openmpt_playback.so");
        let mut backend = backend(plugin_path);
        let mounted = backend.mount(&media_path, 0);
        assert!(mounted.is_ok(), "{mounted:?}");
        assert!(backend
            .render(2_048)
            .is_ok_and(|samples| !samples.is_empty()));
    }

    #[test]
    #[ignore = "requires the CMake-built libvgm playback plugin"]
    fn libvgm_decoder_drives_a_prepared_session() {
        check_decoder(
            "libvgm_playback.so",
            "data/test_data/music/libvgm/earthworm_jim.vgm",
            "libvgm",
            "vgm",
        );
    }

    #[test]
    #[ignore = "requires the CMake-built sidplayfp playback plugin"]
    fn sidplayfp_decoder_drives_a_prepared_session() {
        check_decoder(
            "sidplayfp_playback.so",
            "data/test_data/music/sidplayfp/artefacts.sid",
            "sidplayfp",
            "sid",
        );
    }

    #[test]
    #[ignore = "requires the CMake-built TFMX playback plugin"]
    fn tfmx_decoder_drives_a_prepared_session() {
        check_decoder(
            "tfmx_playback.so",
            "data/test_data/music/tfmx/mdat.lovely_feelings",
            "tfmx",
            "mdat",
        );
    }
}
