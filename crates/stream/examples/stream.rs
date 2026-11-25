//! `cargo run --example stream -- CHAIN [RATE CHANNELS BLOCK FORMAT [CONTROL]]`: the same as
//! `soundcraft-cli stream`, without the rest of the CLI (for testing pipes on their own).
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.first().map(String::as_str) == Some("--list") {
        println!("{}", soundcraft_stream::list_json());
        return;
    }
    let num = |i: usize, d: u32| a.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    let format = match a.get(4).map(String::as_str) {
        Some("s16le") => soundcraft_stream::SampleFormat::S16le,
        _ => soundcraft_stream::SampleFormat::F32le,
    };
    let opts = soundcraft_stream::ServeOptions {
        chain: a.first().cloned().unwrap_or_else(|| "[]".into()),
        config: soundcraft_stream::StreamConfig { sample_rate: num(1, 48_000), channels: num(2, 2) as usize, block: num(3, 480) as usize, format },
        control: a.get(5).map(std::path::PathBuf::from),
    };
    if let Err(e) = soundcraft_stream::serve(&opts, &mut std::io::stderr()) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
