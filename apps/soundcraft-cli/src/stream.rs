//! `soundcraft-cli stream` and `live`: the realtime plugins on a live stream (crates/stream does
//! the work).

use std::io::Write;

use super::args::{LiveArgs, StreamArgs};
use super::error::CliError;

pub(crate) fn stream(args: &StreamArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<(), CliError> {
    if args.list {
        outln!(out, "{}", soundcraft_stream::list_json());
        return Ok(());
    }
    let opts = soundcraft_stream::ServeOptions {
        chain: args.chain.clone().unwrap_or_default(),
        config: soundcraft_stream::StreamConfig { sample_rate: args.rate, channels: args.channels, block: args.block, format: args.format.into() },
        control: args.control.clone(),
    };
    soundcraft_stream::serve(&opts, err).map(|_| ()).map_err(CliError::Stream)
}

pub(crate) fn live(args: &LiveArgs, err: &mut dyn Write) -> Result<(), CliError> {
    let run = soundcraft_stream::LiveRun {
        chain: args.chain.clone(),
        input: args.input.clone(),
        input_format: args.input_format.clone(),
        output: args.output.clone(),
        control: args.control.clone(),
        sample_rate: args.rate,
        channels: args.channels,
        block: args.block,
        seconds: args.seconds,
        carry_video: !args.no_video,
        audio_bitrate_kbps: args.audio_bitrate,
    };
    soundcraft_stream::live(&run, err).map(|_| ()).map_err(CliError::Stream)
}
