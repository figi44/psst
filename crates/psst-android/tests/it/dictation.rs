use psst_proto::ws::{Mode, ReadyModelState};
use psst_testkit::{fake_server::Outcome, test_audio};

use super::*;

#[test]
fn a_dictation_streams_while_the_user_talks_and_its_text_comes_after_stop() {
    let server = tcp_server(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("Hello world.".into()));
    let mut phone = Phone::online(&address(&server));
    // Not a whole number of 20 ms frames.
    let audio = test_audio(2_345);
    let (before, after) = audio.split_at(audio.len() / 2);

    phone.send(Command::Start);
    phone.speak(before);
    streaming(&server, 1);
    phone.speak(after);
    phone.send(Command::Stop);
    let (id, text) = phone.until_final();
    phone.until_dictation(Dictation::Idle);

    assert_eq!(text, "Hello world.");
    assert_eq!(phone.dictations(), [Dictation::Idle, Dictation::Recording, Dictation::Transcribing, Dictation::Idle]);
    let seen = server.session(id).unwrap();
    assert_eq!(seen.starts.len(), 1);
    assert_eq!(seen.starts[0].mode, Mode::Live);
    assert_eq!(seen.audio, padded(&audio));
    assert_eq!(seen.stop_samples, Some(audio.len() as u64));
}

#[test]
fn inserting_the_text_reports_the_phones_timings() {
    let server = tcp_server(FakeServerConfig::default());
    let mut phone = Phone::online(&address(&server));
    phone.send(Command::Start);
    phone.speak(&test_audio(500));
    phone.send(Command::Stop);
    let (id, _) = phone.until_final();

    phone.send(Command::Inserted { session_id: id });

    assert!(server.wait_until(WAIT, |s| !s.reports().is_empty()), "no report");
    let report = server.reports()[0];
    assert_eq!(report.session_id, id);
    assert!(report.timings.toggle_to_first_frame_ms.is_some(), "{report:?}");
    assert!(report.timings.stop_to_final_ms.is_some(), "{report:?}");
    assert!(report.timings.stop_to_dispatch_ms.is_some(), "{report:?}");
}

#[test]
fn a_recording_counts_down_its_last_ten_seconds_and_ends_itself_at_the_limit() {
    let server = tcp_server(FakeServerConfig { max_recording_s: 12, ..Default::default() });
    let mut phone = Phone::online(&address(&server));

    phone.send(Command::Start);
    phone.speak(&test_audio(100));
    streaming(&server, 1);
    phone.speak(&test_audio(13_000));
    let (id, _) = phone.until_final();
    // The recorder's own `stop`, after the core ended the recording, changes nothing.
    phone.send(Command::Stop);
    phone.until_dictation(Dictation::Idle);

    let mut countdown: Vec<u32> = phone
        .events
        .iter()
        .filter_map(|event| match event {
            Event::State { state } => state.seconds_left,
            _ => None,
        })
        .collect();
    countdown.dedup();
    assert_eq!(countdown, (1..=10).rev().collect::<Vec<u32>>());
    assert_eq!(server.session(id).unwrap().stop_samples, Some(12 * 16_000));
}

#[test]
fn a_model_still_loading_shows_until_the_text_comes() {
    let server = tcp_server(FakeServerConfig {
        model_state: ReadyModelState::Loading,
        final_delay: Duration::from_millis(200),
        ..Default::default()
    });
    let mut phone = Phone::online(&address(&server));

    phone.send(Command::Start);
    phone.speak(&test_audio(300));
    streaming(&server, 1);
    phone.send(Command::Stop);
    phone.until_final();

    // Before `ready` says the model is loading, the dictation shows as transcribing.
    let shown = phone.dictations();
    assert!(shown.ends_with(&[Dictation::LoadingModel]), "{shown:?}");
}

#[test]
fn a_cancelled_dictation_never_delivers_its_text() {
    let server = tcp_server(FakeServerConfig { final_delay: Duration::from_millis(300), ..Default::default() });
    server.push_outcome(Outcome::Final("Cancelled while transcribing.".into()));
    server.push_outcome(Outcome::Final("Delivered.".into()));
    let mut phone = Phone::online(&address(&server));

    phone.send(Command::Start);
    phone.speak(&test_audio(300));
    streaming(&server, 1);
    phone.send(Command::Cancel);
    phone.until_dictation(Dictation::Idle);

    phone.send(Command::Start);
    phone.speak(&test_audio(300));
    streaming(&server, 2);
    phone.send(Command::Stop);
    assert!(server.wait_until(WAIT, |s| s.sessions()[1].stop_samples.is_some()), "no stop on the server");
    phone.send(Command::Cancel);
    phone.until_dictation(Dictation::Idle);

    phone.send(Command::Start);
    phone.speak(&test_audio(300));
    phone.send(Command::Stop);
    let (_, text) = phone.until_final();

    assert_eq!(text, "Delivered.");
    assert_eq!(phone.finals(), ["Delivered."]);
    let sessions = server.sessions();
    assert!(sessions[0].cancelled && sessions[1].cancelled, "{sessions:#?}");
}
