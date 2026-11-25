//! `soundcraft-cli stream`: the realtime plugins on a live stream (crates/stream does the work).

use std::io::Write;

use super::args::StreamArgs;
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
