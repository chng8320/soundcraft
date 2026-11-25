//! `soundcraft-cli live`: the realtime plugins on a live stream end to end. craft-live (FFmpeg)
//! reads the input (RTMP, SRT, RTSP, HLS, a device or a file) into raw samples, the chain
//! processes them block by block as in stream mode, and craft-live encodes them to the live
//! output with the input's video copied alongside. Control lines change the chain while it runs.
//! The plugins run on the CPU: a block takes milliseconds, so there is no device to choose.

use std::io::Write;

use crate::{Chain, Control, SampleFormat, StreamConfig, StreamError, StreamStats, load_chain, parse_control, run};

/// Everything `soundcraft-cli live` takes.
#[derive(Debug, Clone)]
pub struct LiveRun {
    /// The chain: a JSON file's path, or the JSON itself.
    pub chain: String,
    /// Where the stream comes from: a URL, a device or a file.
    pub input: String,
    /// FFmpeg's input format for a device (`alsa`, `pulse`, `dshow`, `avfoundation`).
    pub input_format: Option<String>,
    /// Where it goes: rtmp(s)://, srt://, rtsp://, a .m3u8 path (HLS) or a file.
    pub output: String,
    /// Control lines: `tcp:PORT`, or a file or FIFO.
    pub control: Option<String>,
    pub sample_rate: u32,
    pub channels: usize,
    /// Frames per block.
    pub block: usize,
    /// Stop after this many seconds; 0 runs until the input ends.
    pub seconds: u32,
    /// Copy the input's video to the output (default).
    pub carry_video: bool,
    pub audio_bitrate_kbps: Option<u32>,
}

/// Run until the input ends (or `seconds`): reports the chain as one JSON line on `log` when it
/// starts, control errors as they happen, and the totals at the end.
pub fn live(opts: &LiveRun, log: &mut dyn Write) -> Result<StreamStats, StreamError> {
    let spec = load_chain(&opts.chain)?;
    let cfg = StreamConfig { sample_rate: opts.sample_rate, channels: opts.channels, block: opts.block, format: SampleFormat::F32le };
    let mut chain = Chain::new(&spec, &cfg)?;
    let channels = u16::try_from(opts.channels).map_err(|_| StreamError::BadConfig("channels must be 1 to 32"))?;
    let mut lo =
        craft_live::LiveOptions::new(opts.input.clone(), Some(opts.output.clone()), craft_live::Raw::Audio { rate: opts.sample_rate, channels });
    lo.input_format = opts.input_format.clone();
    lo.seconds = opts.seconds;
    lo.carry_other = opts.carry_video;
    if let Some(k) = opts.audio_bitrate_kbps {
        lo.audio_bitrate_kbps = k;
    }
    let rx = match &opts.control {
        Some(spec) => {
            let lines = craft_live::control_lines(spec).map_err(|e| StreamError::Live(format!("control {spec}: {e}")))?;
            let (tx, rx) = std::sync::mpsc::channel::<Control>();
            std::thread::spawn(move || {
                for line in lines {
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
            Some(rx)
        }
        None => None,
    };
    let mut l = craft_live::Live::start(&lo).map_err(|e| StreamError::Live(e.to_string()))?;
    let ids: Vec<&str> = spec.items().iter().map(|i| i.id.as_str()).collect();
    let _ = writeln!(
        log,
        "{}",
        serde_json::json!({"live": "started", "input": opts.input, "output": opts.output, "plugins": ids,
            "latency_ms": chain.latency() as f64 * 1000.0 / f64::from(opts.sample_rate), "device": "CPU"})
    );
    let mut out = l.out.take().ok_or_else(|| StreamError::Live("the output did not start".into()))?;
    let mut errs = Vec::new();
    let result = run(&mut chain, &cfg, &mut l.frames, &mut out, rx.as_ref(), |e| errs.push(e.to_string()));
    drop(out);
    for e in &errs {
        let _ = writeln!(log, "{}", serde_json::json!({"control_error": e}));
    }
    if result.is_err() {
        l.stop();
    }
    let finished = l.finish().map_err(|e| StreamError::Live(e.to_string()));
    let stats = result?;
    finished?;
    let _ = writeln!(log, "{}", serde_json::json!({"live": "ended", "stats": stats}));
    Ok(stats)
}
