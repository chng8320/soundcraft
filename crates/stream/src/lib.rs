#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
//! SoundCraft stream mode.
//!
//! Runs a chain of SoundCraft's realtime plugins (the same ids and parameters as `soundcraft-cli
//! plugins` and the engine's commands) over a live audio stream: interleaved raw PCM in on one
//! reader, the processed PCM out on one writer, block by block, so FFmpeg, GStreamer or a LiveKit
//! agent can sit on either side. Parameters can change while it runs, through control messages
//! (one JSON object per line). The plugins keep their own contract: `process` never allocates, so
//! the per-block loop allocates nothing once it has started.

use std::io::{self, Read, Write};
use std::sync::mpsc::Receiver;

use serde::{Deserialize, Serialize};
use soundcraft_dsp::{Plugin, PluginInfo};

mod live;
pub use live::{LiveRun, live};

/// Sample format of the raw stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SampleFormat {
    /// 32-bit float, little endian (FFmpeg `-f f32le`).
    F32le,
    /// 16-bit signed integer, little endian (FFmpeg `-f s16le`).
    S16le,
}

impl SampleFormat {
    pub fn bytes(self) -> usize {
        match self {
            Self::F32le => 4,
            Self::S16le => 2,
        }
    }
}

/// One plugin of a chain: its id and the parameters to set.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ChainItem {
    pub id: String,
    #[serde(default)]
    pub params: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub bypass: bool,
}

/// A chain: `{"plugins": [...]}` or just the list.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum ChainSpec {
    Object { plugins: Vec<ChainItem> },
    List(Vec<ChainItem>),
}

impl ChainSpec {
    pub fn items(&self) -> &[ChainItem] {
        match self {
            Self::Object { plugins } | Self::List(plugins) => plugins,
        }
    }
}

/// Stream settings.
#[derive(Debug, Clone, Copy)]
pub struct StreamConfig {
    pub sample_rate: u32,
    pub channels: usize,
    /// Frames per block (480 = 10 ms at 48 kHz).
    pub block: usize,
    pub format: SampleFormat,
}

/// A live change: `{"set": {"plugin": 0 | "eq_7band", "param": "...", "value": 3.0}}`,
/// `{"bypass": {"plugin": 1, "on": true}}` or `{"reset": true}`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Control {
    Set { plugin: PluginRef, param: String, value: f32 },
    Bypass { plugin: PluginRef, on: bool },
    Reset(bool),
}

/// A plugin in the chain, by position or by id (the first with that id).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum PluginRef {
    Index(usize),
    Id(String),
}

#[derive(Debug)]
pub enum StreamError {
    UnknownPlugin(String),
    Instrument(String),
    UnknownParam {
        plugin: String,
        param: String,
    },
    BadParam {
        plugin: String,
        param: String,
    },
    BadConfig(&'static str),
    Io(io::Error),
    /// The live input or output (FFmpeg) failed.
    Live(String),
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownPlugin(id) => write!(f, "unknown plugin {id:?} (soundcraft-cli plugins lists them)"),
            Self::Instrument(id) => write!(f, "{id:?} is an instrument: it makes sound, it doesn't process a stream"),
            Self::UnknownParam { plugin, param } => write!(f, "{plugin} has no parameter {param:?}"),
            Self::BadParam { plugin, param } => write!(f, "{plugin}.{param} must be a number (or true/false)"),
            Self::BadConfig(why) => write!(f, "{why}"),
            Self::Io(e) => write!(f, "{e}"),
            Self::Live(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for StreamError {}

impl From<io::Error> for StreamError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

struct Slot {
    plugin: Box<dyn Plugin>,
    bypass: bool,
}

/// A prepared chain of plugins.
pub struct Chain {
    slots: Vec<Slot>,
}

impl Chain {
    /// Builds and prepares a chain for the stream settings.
    pub fn new(spec: &ChainSpec, cfg: &StreamConfig) -> Result<Self, StreamError> {
        if cfg.channels == 0 || cfg.channels > 32 {
            return Err(StreamError::BadConfig("channels must be 1 to 32"));
        }
        if cfg.block == 0 || cfg.block > 65_536 {
            return Err(StreamError::BadConfig("block must be 1 to 65536 frames"));
        }
        if !(8_000..=384_000).contains(&cfg.sample_rate) {
            return Err(StreamError::BadConfig("sample rate must be 8000 to 384000 Hz"));
        }
        let mut slots = Vec::with_capacity(spec.items().len());
        for item in spec.items() {
            let info = soundcraft_dsp::plugin_info(&item.id).ok_or_else(|| StreamError::UnknownPlugin(item.id.clone()))?;
            if info.is_instrument || soundcraft_dsp::is_generator(&item.id) {
                return Err(StreamError::Instrument(item.id.clone()));
            }
            let mut plugin = soundcraft_dsp::create(&item.id).ok_or_else(|| StreamError::UnknownPlugin(item.id.clone()))?;
            plugin.set_offline(false);
            plugin.prepare(cfg.sample_rate as f32, cfg.block, cfg.channels);
            for (param, value) in &item.params {
                let v = match value {
                    serde_json::Value::Number(n) => n.as_f64().map(|x| x as f32),
                    serde_json::Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
                    serde_json::Value::String(s) => choice_index(info, param, s),
                    _ => None,
                }
                .ok_or_else(|| StreamError::BadParam { plugin: item.id.clone(), param: param.clone() })?;
                if !plugin.set_param(param, v) {
                    return Err(StreamError::UnknownParam { plugin: item.id.clone(), param: param.clone() });
                }
            }
            slots.push(Slot { plugin, bypass: item.bypass });
        }
        Ok(Self { slots })
    }

    /// Total latency of the active plugins, in samples (to keep picture and sound together).
    pub fn latency(&self) -> usize {
        self.slots.iter().filter(|s| !s.bypass).map(|s| s.plugin.latency()).sum()
    }

    /// Processes one block in place (planar `io[channel][frame]`).
    pub fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        for s in self.slots.iter_mut().filter(|s| !s.bypass) {
            s.plugin.process(io, frames);
        }
    }

    /// Applies a live change. Unknown plugins or parameters are reported, not fatal.
    pub fn apply(&mut self, c: &Control) -> Result<(), String> {
        match c {
            Control::Set { plugin, param, value } => {
                let s = self.slot(plugin).ok_or_else(|| format!("no plugin {plugin:?} in the chain"))?;
                if s.plugin.set_param(param, *value) { Ok(()) } else { Err(format!("{} has no parameter {param:?}", s.plugin.info().id)) }
            }
            Control::Bypass { plugin, on } => {
                let s = self.slot(plugin).ok_or_else(|| format!("no plugin {plugin:?} in the chain"))?;
                s.bypass = *on;
                Ok(())
            }
            Control::Reset(true) => {
                for s in &mut self.slots {
                    s.plugin.reset();
                }
                Ok(())
            }
            Control::Reset(false) => Ok(()),
        }
    }

    fn slot(&mut self, r: &PluginRef) -> Option<&mut Slot> {
        match r {
            PluginRef::Index(i) => self.slots.get_mut(*i),
            PluginRef::Id(id) => self.slots.iter_mut().find(|s| s.plugin.info().id == id),
        }
    }
}

fn choice_index(info: &PluginInfo, param: &str, label: &str) -> Option<f32> {
    let p = info.param(param)?;
    p.choices.iter().position(|c| c.eq_ignore_ascii_case(label)).map(|i| i as f32)
}

/// The plugins a stream can use, with their parameters (for presets and the Windmill scripts).
pub fn chainable() -> Vec<&'static PluginInfo> {
    soundcraft_dsp::plugins().iter().copied().filter(|p| !p.is_instrument && !soundcraft_dsp::is_generator(p.id)).collect()
}

/// Counts of what a stream did.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct StreamStats {
    pub blocks: u64,
    pub frames: u64,
    pub controls: u64,
    pub control_errors: u64,
}

/// Runs the chain until the input ends. A last partial block is processed too; a trailing
/// partial sample (an odd byte count) is dropped. Control messages are applied between blocks.
pub fn run(
    chain: &mut Chain,
    cfg: &StreamConfig,
    input: &mut dyn Read,
    output: &mut dyn Write,
    controls: Option<&Receiver<Control>>,
    mut on_control_error: impl FnMut(&str),
) -> Result<StreamStats, StreamError> {
    let ch = cfg.channels;
    let bytes_per_frame = ch * cfg.format.bytes();
    let mut raw = vec![0u8; cfg.block * bytes_per_frame];
    let mut io: Vec<Vec<f32>> = vec![vec![0.0; cfg.block]; ch];
    let mut stats = StreamStats::default();
    loop {
        if let Some(rx) = controls {
            while let Ok(c) = rx.try_recv() {
                stats.controls += 1;
                if let Err(e) = chain.apply(&c) {
                    stats.control_errors += 1;
                    on_control_error(&e);
                }
            }
        }
        let got = read_full(input, &mut raw)?;
        let frames = got / bytes_per_frame;
        if frames == 0 {
            break;
        }
        decode(&raw[..frames * bytes_per_frame], cfg.format, &mut io, frames);
        chain.process(&mut io, frames);
        encode(&io, frames, cfg.format, &mut raw[..frames * bytes_per_frame]);
        output.write_all(&raw[..frames * bytes_per_frame])?;
        output.flush()?;
        stats.blocks += 1;
        stats.frames += frames as u64;
        if got < raw.len() {
            break;
        }
    }
    Ok(stats)
}

/// Reads until `buf` is full or the input ends; returns the bytes read.
fn read_full(r: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

fn decode(raw: &[u8], f: SampleFormat, io: &mut [Vec<f32>], frames: usize) {
    let ch = io.len();
    for i in 0..frames {
        for (c, chan) in io.iter_mut().enumerate() {
            let k = i * ch + c;
            chan[i] = match f {
                SampleFormat::F32le => {
                    let b = [raw[k * 4], raw[k * 4 + 1], raw[k * 4 + 2], raw[k * 4 + 3]];
                    let v = f32::from_le_bytes(b);
                    if v.is_finite() { v } else { 0.0 }
                }
                SampleFormat::S16le => f32::from(i16::from_le_bytes([raw[k * 2], raw[k * 2 + 1]])) / 32768.0,
            };
        }
    }
}

fn encode(io: &[Vec<f32>], frames: usize, f: SampleFormat, raw: &mut [u8]) {
    let ch = io.len();
    for i in 0..frames {
        for (c, chan) in io.iter().enumerate() {
            let k = i * ch + c;
            let v = if chan[i].is_finite() { chan[i] } else { 0.0 };
            match f {
                SampleFormat::F32le => raw[k * 4..k * 4 + 4].copy_from_slice(&v.to_le_bytes()),
                SampleFormat::S16le => {
                    let s = (v.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                    raw[k * 2..k * 2 + 2].copy_from_slice(&s.to_le_bytes());
                }
            }
        }
    }
}

/// Parses one control line; blank lines and `#` comments are `None`.
pub fn parse_control(line: &str) -> Option<Result<Control, String>> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    Some(serde_json::from_str::<Control>(t).map_err(|e| format!("bad control {t:?}: {e}")))
}

/// Everything `soundcraft-cli stream` takes.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// The chain: a JSON file's path, or the JSON itself.
    pub chain: String,
    pub config: StreamConfig,
    /// A file or named pipe (FIFO) of control lines, read while the stream runs.
    pub control: Option<std::path::PathBuf>,
}

/// Runs a stream on stdin and stdout: reports the chain (plugins and latency) as one JSON line on
/// `log`, applies control lines as they come, and reports the totals when the input ends.
pub fn serve(opts: &ServeOptions, log: &mut dyn Write) -> Result<StreamStats, StreamError> {
    let spec = load_chain(&opts.chain)?;
    let mut chain = Chain::new(&spec, &opts.config)?;
    let ids: Vec<&str> = spec.items().iter().map(|i| i.id.as_str()).collect();
    let _ = writeln!(
        log,
        "{}",
        serde_json::json!({"stream": "started", "plugins": ids, "latency_samples": chain.latency(),
        "latency_ms": chain.latency() as f64 * 1000.0 / f64::from(opts.config.sample_rate), "block": opts.config.block})
    );
    let rx = opts.control.as_ref().map(|path| {
        let (tx, rx) = std::sync::mpsc::channel();
        let path = path.clone();
        std::thread::spawn(move || {
            // A FIFO blocks here until a writer opens it; a plain file is read once.
            let Ok(f) = std::fs::File::open(&path) else { return };
            for line in io::BufRead::lines(io::BufReader::new(f)).map_while(Result::ok) {
                match parse_control(&line) {
                    Some(Ok(c)) => {
                        if tx.send(c).is_err() {
                            break;
                        }
                    }
                    Some(Err(e)) => log::warn!("{e}"),
                    None => {}
                }
            }
        });
        rx
    });
    let stdin = io::stdin();
    let stdout = io::stdout();
    let (mut i, mut o) = (stdin.lock(), stdout.lock());
    let mut errs = Vec::new();
    let stats = run(&mut chain, &opts.config, &mut i, &mut o, rx.as_ref(), |e| errs.push(e.to_string()))?;
    for e in &errs {
        let _ = writeln!(log, "{}", serde_json::json!({"control_error": e}));
    }
    let _ = writeln!(log, "{}", serde_json::json!({"stream": "ended", "stats": stats}));
    Ok(stats)
}

/// A chain from a JSON file's path or the JSON itself.
pub fn load_chain(chain: &str) -> Result<ChainSpec, StreamError> {
    let text = if chain.trim_start().starts_with(['[', '{']) { chain.to_string() } else { std::fs::read_to_string(chain)? };
    serde_json::from_str(&text).map_err(|e| StreamError::Io(io::Error::new(io::ErrorKind::InvalidData, format!("chain: {e}"))))
}

/// The chainable plugins and their parameters, as JSON (`soundcraft-cli stream --list`).
pub fn list_json() -> serde_json::Value {
    serde_json::Value::Array(chainable().iter().map(|p| serde_json::json!({
        "id": p.id, "name": p.name, "category": p.category, "realtime": true,
        "params": p.params.iter().map(|q| serde_json::json!({"id": q.id, "name": q.name, "min": q.min, "max": q.max, "default": q.default, "unit": q.unit, "choices": q.choices})).collect::<Vec<_>>()
    })).collect())
}

#[cfg(test)]
mod tests;
