//! Audio control for whoever is casting.
//!
//! ## Why the PulseAudio protocol, on a machine that does not run PulseAudio
//!
//! `pactl` is a client of a *protocol*, not of a particular server. PipeWire
//! implements that protocol through `pipewire-pulse`, so one code path covers
//! both the old and the new world — on the kiosk this talks to
//! "PulseAudio (on PipeWire 1.6.8)". The PipeWire-native tools (`wpctl`,
//! `pw-cli`) would be strictly *narrower*, and `amixer` is not even installed
//! there.
//!
//! Shelling out rather than binding libpulse: an FFI dependency is exactly the
//! kind of thing that made `aws-lc-rs` unusable for the armv7 cross build, and a
//! few milliseconds per click is not worth that risk.
//!
//! The one stack this does not reach is a machine with no sound server at all
//! (a Raspberry Pi OS Lite image, say). That is what `AudioBackend` is for: the
//! seam exists so an ALSA backend can be added without touching any caller. It
//! is deliberately not written yet — no device here needs it.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{debug, warn};

/// Everything the cast page can see and change.
#[derive(Debug, Serialize, Default)]
pub struct AudioState {
    pub available: bool,
    pub sinks: Vec<Sink>,
    pub streams: Vec<Stream>,
}

#[derive(Debug, Serialize)]
pub struct Sink {
    pub name: String,
    pub description: String,
    pub volume: u32,
    pub muted: bool,
    pub is_default: bool,
}

/// One thing making sound. Not only the cast: a signage box may also be running
/// AirPlay or a music daemon, and a presenter needs to be able to turn those
/// down without hunting for whoever started them.
#[derive(Debug, Serialize)]
pub struct Stream {
    pub index: u32,
    pub label: String,
    pub volume: u32,
    pub muted: bool,
    /// The display browser's own audio, i.e. what the caster is sending.
    pub is_cast: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum AudioCommand {
    /// The output device as a whole.
    Sink {
        #[serde(flatten)]
        action: SinkAction,
    },
    /// One playback stream.
    Stream {
        index: u32,
        #[serde(flatten)]
        action: StreamAction,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SinkAction {
    Volume { value: u32 },
    Mute { value: bool },
    /// Switch the output device.
    Select { name: String },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum StreamAction {
    Volume { value: u32 },
    Mute { value: bool },
}

/// What a sound server has to do for us.
///
/// Split out so a second implementation can be dropped in for a machine without
/// a sound server. Dispatch goes through `Backend` rather than `dyn` — there is
/// exactly one backend per process, picked once at startup, so dynamic dispatch
/// would only buy an `async-trait` dependency.
trait AudioBackend {
    async fn state(&self, cast_pids: &[u32]) -> Option<AudioState>;
    async fn apply(&self, command: &AudioCommand, cast_stream: Option<u32>) -> bool;
}

pub enum Backend {
    Pactl(Pactl),
    /// No usable sound server. Every call reports "unavailable" so the page can
    /// hide its controls instead of showing dead ones.
    Unavailable,
}

impl Backend {
    /// Pick a backend once, at startup.
    pub async fn detect() -> Self {
        if Pactl::usable().await {
            debug!("Audio control via pactl");
            Backend::Pactl(Pactl)
        } else {
            warn!("No usable audio control found (pactl missing or no sound server)");
            Backend::Unavailable
        }
    }

    pub async fn state(&self, cast_pids: &[u32]) -> AudioState {
        match self {
            Backend::Pactl(backend) => backend.state(cast_pids).await.unwrap_or_default(),
            Backend::Unavailable => AudioState::default(),
        }
    }

    pub async fn apply(&self, command: &AudioCommand, cast_stream: Option<u32>) -> bool {
        match self {
            Backend::Pactl(backend) => backend.apply(command, cast_stream).await,
            Backend::Unavailable => false,
        }
    }
}

pub struct Pactl;

async fn run(args: &[&str]) -> Option<String> {
    let output = Command::new("pactl").args(args).output().await.ok()?;
    if !output.status.success() {
        debug!("pactl {:?} failed: {}", args, output.status);
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Percentages come back as `"75%"`.
fn parse_percent(raw: Option<&str>) -> u32 {
    raw.and_then(|text| text.trim().trim_end_matches('%').parse().ok())
        .unwrap_or(0)
}

/// A volume has one entry per channel; they are set together, so the first is
/// representative.
fn first_channel_volume(value: &serde_json::Value) -> u32 {
    value
        .get("volume")
        .and_then(|v| v.as_object())
        .and_then(|channels| channels.values().next())
        .and_then(|channel| channel.get("value_percent"))
        .and_then(|p| p.as_str())
        .map(|p| parse_percent(Some(p)))
        .unwrap_or(0)
}

impl Pactl {
    async fn usable() -> bool {
        run(&["info"]).await.is_some()
    }

    async fn default_sink(&self) -> Option<String> {
        run(&["get-default-sink"])
            .await
            .map(|raw| raw.trim().to_string())
            .filter(|name| !name.is_empty())
    }

    async fn json(&self, what: &str) -> Option<Vec<serde_json::Value>> {
        let raw = run(&["--format=json", "list", what]).await?;
        serde_json::from_str(&raw).ok()
    }
}

impl AudioBackend for Pactl {
    async fn state(&self, cast_pids: &[u32]) -> Option<AudioState> {
        let default = self.default_sink().await;

        let sinks = self
            .json("sinks")
            .await?
            .iter()
            .filter_map(|sink| {
                let name = sink.get("name")?.as_str()?.to_string();
                let description = sink
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or(&name)
                    .to_string();
                Some(Sink {
                    is_default: default.as_deref() == Some(name.as_str()),
                    volume: first_channel_volume(sink),
                    muted: sink.get("mute").and_then(|m| m.as_bool()).unwrap_or(false),
                    name,
                    description,
                })
            })
            .collect();

        let streams = self
            .json("sink-inputs")
            .await?
            .iter()
            .filter_map(|input| {
                let index = input.get("index")?.as_u64()? as u32;
                let props = input.get("properties");
                let get = |key: &str| {
                    props
                        .and_then(|p| p.get(key))
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                };
                let pid = get("application.process.id").and_then(|p| p.parse::<u32>().ok());

                // Ours if the stream belongs to a process under the browser we
                // started. Names cannot decide this: with two displays both would
                // just say "Chromium".
                let is_cast = match pid {
                    Some(pid) => cast_pids.contains(&pid),
                    None => false,
                };

                let app = get("application.name").unwrap_or_else(|| "?".to_string());
                let media = get("media.name");
                let label = match media {
                    Some(media) if media != app => format!("{} – {}", app, media),
                    _ => app,
                };

                Some(Stream {
                    index,
                    label,
                    volume: first_channel_volume(input),
                    muted: input.get("mute").and_then(|m| m.as_bool()).unwrap_or(false),
                    is_cast,
                })
            })
            .collect();

        Some(AudioState {
            available: true,
            sinks,
            streams,
        })
    }

    async fn apply(&self, command: &AudioCommand, cast_stream: Option<u32>) -> bool {
        // Above 100% pactl happily distorts; nobody wants that from a web slider.
        let clamp = |value: u32| value.min(100).to_string() + "%";

        match command {
            AudioCommand::Sink { action } => match action {
                SinkAction::Volume { value } => {
                    let Some(sink) = self.default_sink().await else {
                        return false;
                    };
                    run(&["set-sink-volume", &sink, &clamp(*value)]).await.is_some()
                }
                SinkAction::Mute { value } => {
                    let Some(sink) = self.default_sink().await else {
                        return false;
                    };
                    let flag = if *value { "1" } else { "0" };
                    run(&["set-sink-mute", &sink, flag]).await.is_some()
                }
                SinkAction::Select { name } => {
                    if run(&["set-default-sink", name]).await.is_none() {
                        return false;
                    }
                    // set-default-sink only affects *new* streams. Without moving
                    // the cast stream too, switching the output appears to do
                    // nothing: the audio keeps playing out of the old device.
                    if let Some(stream) = cast_stream {
                        let index = stream.to_string();
                        if run(&["move-sink-input", &index, name]).await.is_none() {
                            warn!("Switched default sink but could not move the cast stream");
                        }
                    }
                    true
                }
            },
            AudioCommand::Stream { index, action } => {
                let index = index.to_string();
                match action {
                    StreamAction::Volume { value } => {
                        run(&["set-sink-input-volume", &index, &clamp(*value)])
                            .await
                            .is_some()
                    }
                    StreamAction::Mute { value } => {
                        let flag = if *value { "1" } else { "0" };
                        run(&["set-sink-input-mute", &index, flag]).await.is_some()
                    }
                }
            }
        }
    }
}

// ------------------------------------------------------------------ process

/// Every process descending from `root`, itself included.
///
/// Chromium's audio comes from a child process, not the one we spawned, so
/// matching a stream to "our" browser means matching against the whole subtree.
/// Read straight from /proc: one shot, no dependency, and this only runs when
/// the audio panel is polled.
pub fn descendants(root: u32) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();

    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            // "pid (comm) state ppid ...". `comm` may contain spaces and even
            // parentheses, so the fields only become countable after the last ')'.
            let Some(close) = stat.rfind(')') else { continue };
            let mut fields = stat[close + 1..].split_whitespace();
            let (Some(_state), Some(ppid)) = (fields.next(), fields.next()) else {
                continue;
            };
            if let Ok(ppid) = ppid.parse::<u32>() {
                children.entry(ppid).or_default().push(pid);
            }
        }
    }

    let mut all = vec![root];
    let mut queue = vec![root];
    while let Some(parent) = queue.pop() {
        for child in children.remove(&parent).unwrap_or_default() {
            all.push(child);
            queue.push(child);
        }
    }
    all
}
