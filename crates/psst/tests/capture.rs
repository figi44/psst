//! Capture from the real default microphone through cpal (PipeWire when it's running). Needs a
//! microphone, so it only runs when asked: `cargo nextest run -p psst --run-ignored only`.

use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use psst::audio::{
    CpalSource,
    recorder::{self, RecEvent},
};

#[test]
#[ignore = "needs a microphone"]
fn the_default_microphone_records_16_khz_frames() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("mic.wav");
    let (tx, rx) = mpsc::channel();
    let started = Instant::now();
    let recorder = recorder::start(Arc::new(CpalSource { device: None }), wav.clone(), move |event| {
        let _ = tx.send((Instant::now(), event));
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(600));
    recorder.stop();
    let mut first_frame = None;
    let mut frames = 0;
    let finished = loop {
        let (at, event) = rx.recv_timeout(Duration::from_secs(5)).expect("the recorder finishes");
        match event {
            RecEvent::Frame { .. } => {
                frames += 1;
                first_frame.get_or_insert(at.duration_since(started));
            }
            RecEvent::Gap => {}
            other => break other,
        }
    };
    let RecEvent::Finished { samples, .. } = finished else { panic!("{finished:?}") };
    eprintln!("first frame after {first_frame:?}, {frames} frames, {samples} samples");
    assert!((6_400..=12_800).contains(&samples), "{samples} samples for 600 ms");
    assert_eq!(frames, samples.div_ceil(320));
    assert_eq!(psst::store::wav_samples(&wav), samples);
}
