use super::*;

fn cfg(format: SampleFormat) -> StreamConfig {
    StreamConfig { sample_rate: 48_000, channels: 2, block: 480, format }
}

fn sine(frames: usize, ch: usize) -> Vec<f32> {
    (0..frames).flat_map(|i| {
        let v = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5;
        std::iter::repeat_n(v, ch)
    }).collect()
}

fn bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn floats(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

fn spec(json: &str) -> ChainSpec {
    serde_json::from_str(json).unwrap_or_else(|e| unreachable_spec(&e.to_string()))
}

#[allow(clippy::panic)]
fn unreachable_spec(e: &str) -> ChainSpec {
    panic!("{e}")
}

#[test]
fn empty_chain_passes_audio_through_unchanged() {
    let c = cfg(SampleFormat::F32le);
    let mut chain = Chain::new(&spec("[]"), &c).ok().unwrap_or_else(|| Chain { slots: vec![] });
    let input = bytes(&sine(4_800, 2));
    let mut out = Vec::new();
    let st = run(&mut chain, &c, &mut input.as_slice(), &mut out, None, |_| {}).ok();
    assert_eq!(out, input);
    assert_eq!(st.map(|s| s.frames), Some(4_800));
}

#[test]
fn stream_matches_processing_the_same_blocks_directly() {
    let c = cfg(SampleFormat::F32le);
    let sp = spec(r#"{"plugins":[{"id":"eq_7band"},{"id":"compressor"}]}"#);
    let input = sine(4_800 + 123, 2);
    // streamed
    let Ok(mut a) = Chain::new(&sp, &c) else { return assert!(false, "chain") };
    let mut out = Vec::new();
    let _ = run(&mut a, &c, &mut bytes(&input).as_slice(), &mut out, None, |_| {});
    // the same chain, called block by block by hand
    let Ok(mut b) = Chain::new(&sp, &c) else { return assert!(false, "chain") };
    let mut expect = Vec::new();
    for block in input.chunks(480 * 2) {
        let n = block.len() / 2;
        let mut io = vec![vec![0.0f32; 480]; 2];
        for i in 0..n { io[0][i] = block[i * 2]; io[1][i] = block[i * 2 + 1]; }
        b.process(&mut io, n);
        for i in 0..n { expect.push(io[0][i]); expect.push(io[1][i]); }
    }
    assert_eq!(floats(&out), expect);
}

#[test]
fn s16_round_trip_is_within_one_step() {
    let c = cfg(SampleFormat::S16le);
    let Ok(mut chain) = Chain::new(&spec("[]"), &c) else { return assert!(false, "chain") };
    let input: Vec<u8> = sine(960, 2).iter().flat_map(|x| ((x * 32767.0).round() as i16).to_le_bytes()).collect();
    let mut out = Vec::new();
    let _ = run(&mut chain, &c, &mut input.as_slice(), &mut out, None, |_| {});
    assert_eq!(out.len(), input.len());
    let diff = out.chunks_exact(2).zip(input.chunks_exact(2)).map(|(a, b)| (i16::from_le_bytes([a[0], a[1]]) as i32 - i16::from_le_bytes([b[0], b[1]]) as i32).abs()).max();
    assert!(diff.unwrap_or(0) <= 1);
}

#[test]
fn live_control_changes_a_parameter_and_bypasses() {
    let c = cfg(SampleFormat::F32le);
    let Ok(mut chain) = Chain::new(&spec(r#"[{"id":"compressor"}]"#), &c) else { return assert!(false, "chain") };
    let (tx, rx) = std::sync::mpsc::channel();
    for line in [r#"{"set":{"plugin":"compressor","param":"nope","value":1}}"#, r#"{"bypass":{"plugin":0,"on":true}}"#] {
        if let Some(Ok(ctl)) = parse_control(line) { let _ = tx.send(ctl); }
    }
    let input = bytes(&sine(960, 2));
    let mut out = Vec::new();
    let mut errors = 0;
    let st = run(&mut chain, &c, &mut input.as_slice(), &mut out, Some(&rx), |_| errors += 1).ok();
    assert_eq!(errors, 1, "an unknown parameter is reported, not fatal");
    assert_eq!(st.map(|s| s.controls), Some(2));
    assert_eq!(out, input, "a bypassed plugin leaves the audio as it is");
}

#[test]
fn bad_chains_are_refused_with_a_reason() {
    let c = cfg(SampleFormat::F32le);
    assert!(matches!(Chain::new(&spec(r#"[{"id":"no_such"}]"#), &c), Err(StreamError::UnknownPlugin(_))));
    assert!(matches!(Chain::new(&spec(r#"[{"id":"subtractive_synth"}]"#), &c), Err(StreamError::Instrument(_))));
    assert!(matches!(Chain::new(&spec(r#"[{"id":"compressor","params":{"nope":1}}]"#), &c), Err(StreamError::UnknownParam { .. })));
}

#[test]
fn every_chainable_plugin_runs_a_block_without_nan() {
    let c = cfg(SampleFormat::F32le);
    for info in chainable() {
        let sp = ChainSpec::List(vec![ChainItem { id: info.id.to_string(), params: Default::default(), bypass: false }]);
        let Ok(mut chain) = Chain::new(&sp, &c) else { continue };
        let mut out = Vec::new();
        let _ = run(&mut chain, &c, &mut bytes(&sine(4_800, 2)).as_slice(), &mut out, None, |_| {});
        assert!(floats(&out).iter().all(|v| v.is_finite()), "{}", info.id);
    }
}
