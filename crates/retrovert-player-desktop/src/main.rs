//! The desktop Retrovert player: the shared engine fed by the plugin catalog.
//!
//! One audio worker thread owns the [`PlayerBackend`] and its
//! [`PluginCatalog`], ticking the catalog every loop so the decoder set
//! follows the update channel: pulled, verified, and activated at the
//! mount/stop boundary without a restart. The main thread reads commands from
//! stdin and renders status; samples reach the device through cpal.
//!
//! ```text
//! retrovert-player-desktop [--root DIR] [--interval SECS] [--no-auto-apply] [FILES..]
//!
//! > play <path|index>   mount media (bare index picks from FILES)
//! > stop                drop the session
//! > status              catalog, updater, and playback state
//! > check               ask the channel now
//! > quit
//! ```

use std::collections::VecDeque;
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use retrovert_host::session::StreamFormat;
use retrovert_host::visualization::VisualizationConfig;
use retrovert_player::{PlaybackBackend, PlaybackStatus, PlayerBackend};
use retrovert_plugin_catalog::{CatalogConfig, ChannelConfig, PluginCatalog};

/// The dev channel this binary is built to follow.
const DEV_ROOT: &[u8] = include_bytes!("dev-root.json");
const DEV_METADATA: &str =
    "https://github.com/RetrovertApp/playback_plugins/releases/download/dev/channel-metadata/";
const DEV_ARTIFACTS: &str =
    "https://github.com/RetrovertApp/playback_plugins/releases/download/dev/v{version}/";

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const MAX_FRAMES: u32 = 4_096;
/// How much audio the worker keeps decoded ahead of the device.
const BUFFERED_SAMPLES: usize = (SAMPLE_RATE as usize / 5) * CHANNELS as usize;
/// The worker's pace between render top-ups.
const WORKER_SLEEP: Duration = Duration::from_millis(2);

enum Command {
    Play(PathBuf),
    Stop,
    Status,
    Check,
    Quit,
}

struct Options {
    root: PathBuf,
    interval: Duration,
    auto_apply: bool,
    files: Vec<PathBuf>,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let options = match parse_args() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    let Some(target) = CatalogConfig::native_target() else {
        eprintln!("this platform is not a published release target");
        std::process::exit(1);
    };
    println!(
        "following the dev channel for {target}; root {}",
        options.root.display()
    );

    // The device pulls from this ring; the worker keeps it topped up.
    let ring: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));
    let _stream = match start_output(&ring) {
        Ok(stream) => Some(stream),
        Err(message) => {
            eprintln!("no audio output ({message}); decoding continues silently");
            None
        }
    };

    let files = options.files.clone();
    let (commands, worker_commands) = channel();
    let worker = {
        let ring = Arc::clone(&ring);
        std::thread::Builder::new()
            .name("audio-worker".to_string())
            .spawn(move || worker_main(&options, &ring, &worker_commands))
    };
    let worker = match worker {
        Ok(worker) => worker,
        Err(e) => {
            eprintln!("could not start the audio worker: {e}");
            std::process::exit(1);
        }
    };

    repl(&commands, &files);
    let _ = commands.send(Command::Quit);
    let _ = worker.join();
}

fn parse_args() -> Result<Options, String> {
    let mut options = Options {
        root: default_root()?,
        interval: Duration::from_secs(30),
        auto_apply: true,
        files: Vec::new(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--root" => {
                options.root = PathBuf::from(
                    args.next()
                        .ok_or_else(|| "--root needs a path".to_string())?,
                );
            }
            "--interval" => {
                let seconds = args
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(|| "--interval needs whole seconds".to_string())?;
                options.interval = Duration::from_secs(seconds);
            }
            "--no-auto-apply" => options.auto_apply = false,
            "--help" | "-h" => {
                return Err(
                    "usage: retrovert-player-desktop [--root DIR] [--interval SECS] \
                     [--no-auto-apply] [FILES..]"
                        .to_string(),
                );
            }
            _ => options.files.push(PathBuf::from(argument)),
        }
    }
    Ok(options)
}

fn default_root() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| "no HOME to place --root under".to_string())?;
    Ok(PathBuf::from(home).join(".retrovert-player"))
}

/// Open the default output device at the engine's fixed format.
fn start_output(ring: &Arc<Mutex<VecDeque<f32>>>) -> Result<cpal::Stream, String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "no default output device".to_string())?;
    let config = cpal::StreamConfig {
        channels: CHANNELS,
        sample_rate: cpal::SampleRate(SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };
    let ring = Arc::clone(ring);
    let stream = device
        .build_output_stream(
            &config,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut ring = match ring.lock() {
                    Ok(ring) => ring,
                    Err(poisoned) => poisoned.into_inner(),
                };
                for sample in out.iter_mut() {
                    *sample = ring.pop_front().unwrap_or(0.0);
                }
            },
            |e| log::error!("audio output: {e}"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

/// The audio worker: owns the backend and the catalog, drains commands,
/// keeps the ring fed, and closes finished sessions so the catalog can
/// activate at the stop boundary.
fn worker_main(options: &Options, ring: &Arc<Mutex<VecDeque<f32>>>, commands: &Receiver<Command>) {
    let mut backend = PlayerBackend::new(
        &[],
        VisualizationConfig::default(),
        StreamFormat {
            sample_rate: SAMPLE_RATE,
            channels: u32::from(CHANNELS),
        },
        MAX_FRAMES,
    );
    let mut config = CatalogConfig::beneath(
        &options.root,
        ChannelConfig {
            metadata_base_url: DEV_METADATA.to_string(),
            artifact_base_url: DEV_ARTIFACTS.to_string(),
        },
        DEV_ROOT.to_vec(),
        // Checked in main before the worker starts.
        CatalogConfig::native_target().unwrap_or("linux-x86_64"),
    );
    config.check_interval = options.interval;
    config.auto_apply = options.auto_apply;
    let mut catalog = match PluginCatalog::new(config) {
        Ok(catalog) => catalog,
        Err(e) => {
            eprintln!("the catalog could not be constructed: {e}");
            return;
        }
    };
    let mut announced: Option<String> = None;

    loop {
        match commands.try_recv() {
            Ok(Command::Play(path)) => match catalog.mount(&mut backend, &path, 0) {
                Ok(()) => println!(
                    "playing {} via {} (generation {})",
                    path.display(),
                    backend.plugin_name(),
                    catalog.active_generation().unwrap_or("none")
                ),
                Err(e) => eprintln!("could not play {}: {e}", path.display()),
            },
            Ok(Command::Stop) => backend.close(),
            Ok(Command::Status) => print_status(&backend, &catalog),
            Ok(Command::Check) => catalog.check_now(),
            Ok(Command::Quit) => return,
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return,
        }

        // A finished song leaves the boundary open for activation.
        if backend.status() == PlaybackStatus::Finished {
            println!("finished");
            backend.close();
        }

        catalog.tick(&mut backend);
        if catalog.active_generation() != announced.as_deref() {
            announced = catalog.active_generation().map(str::to_string);
            println!(
                "catalog: active generation is now {}",
                announced.as_deref().unwrap_or("none")
            );
        }

        top_up(&mut backend, ring);
        std::thread::sleep(WORKER_SLEEP);
    }
}

fn top_up(backend: &mut PlayerBackend, ring: &Arc<Mutex<VecDeque<f32>>>) {
    let buffered = match ring.lock() {
        Ok(ring) => ring.len(),
        Err(poisoned) => poisoned.into_inner().len(),
    };
    if buffered >= BUFFERED_SAMPLES {
        return;
    }
    let missing_frames = (BUFFERED_SAMPLES - buffered) / CHANNELS as usize;
    let frames = u32::try_from(missing_frames).unwrap_or(MAX_FRAMES);
    match backend.render(frames.min(MAX_FRAMES)) {
        Ok(samples) if !samples.is_empty() => {
            let mut ring = match ring.lock() {
                Ok(ring) => ring,
                Err(poisoned) => poisoned.into_inner(),
            };
            ring.extend(samples.iter().copied());
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("render failed: {e}");
            backend.close();
        }
    }
}

fn print_status(backend: &PlayerBackend, catalog: &PluginCatalog) {
    let updater = catalog.updater_status();
    println!(
        "playback: {:?} plugin '{}' media '{}'",
        backend.status(),
        backend.plugin_name(),
        backend.media_extension()
    );
    println!(
        "catalog: active {} pending {}",
        catalog.active_generation().unwrap_or("none"),
        catalog.pending_generation().unwrap_or("none"),
    );
    println!(
        "updater: {:?} progress {:.0}% available {}",
        updater.phase,
        f64::from(updater.progress) * 100.0,
        updater
            .available
            .map(|plan| plan.generation_id)
            .unwrap_or_else(|| "none".to_string()),
    );
    if let Some(check) = updater.last_check {
        println!("last check: {check:?}");
    }
    if let Some(error) = updater.last_error {
        println!("last error: {error}");
    }
    for event in catalog.history().iter().rev().take(5) {
        println!(
            "history: {:?} {} {}",
            event.what,
            event.generation,
            event.detail.as_deref().unwrap_or("")
        );
    }
}

/// Read commands until stdin closes or the user quits.
fn repl(commands: &Sender<Command>, files: &[PathBuf]) {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let mut words = line.split_whitespace();
        let command = match (words.next(), words.next()) {
            (Some("play"), Some(what)) => Command::Play(resolve_file(files, what)),
            (Some("stop"), _) => Command::Stop,
            (Some("status"), _) => Command::Status,
            (Some("check"), _) => Command::Check,
            (Some("quit") | Some("exit"), _) => break,
            (None, _) => continue,
            (Some(other), _) => {
                eprintln!("unknown command: {other}");
                continue;
            }
        };
        if commands.send(command).is_err() {
            break;
        }
    }
}

/// A bare index picks from the files given on the command line.
fn resolve_file(files: &[PathBuf], what: &str) -> PathBuf {
    if let Ok(index) = what.parse::<usize>() {
        if let Some(file) = files.get(index) {
            return file.clone();
        }
    }
    PathBuf::from(what)
}
