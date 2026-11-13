use psst_proto::ws::Mode;
use psst_testkit::{fake_server::Outcome, test_audio};

use super::*;

#[test]
fn a_connection_lost_while_recording_still_delivers_everything_said() {
    let server = tcp_server(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("Every word.".into()));
    let mut phone = Phone::online(&address(&server));
    let audio = test_audio(3_000);
    let (before, after) = audio.split_at(audio.len() / 3);

    phone.send(Command::Start);
    phone.speak(before);
    streaming(&server, 1);
    server.disconnect_all();
    let offline = phone.until_state("recording offline", |s| s.dictation_message.is_some());
    phone.speak(after);
    phone.send(Command::Stop);
    let (id, text) = phone.until_final();

    assert_eq!(offline.dictation, Dictation::Recording);
    assert_eq!(text, "Every word.");
    let seen = server.session(id).unwrap();
    assert_eq!(seen.starts.last().map(|start| (start.mode, start.resume)), Some((Mode::Upload, true)));
    assert_eq!(seen.audio, padded(&audio));
    assert_eq!(seen.stop_samples, Some(audio.len() as u64));
}

#[test]
fn a_connection_lost_while_transcribing_still_delivers_the_text() {
    let server = tcp_server(FakeServerConfig { final_delay: Duration::from_millis(400), ..Default::default() });
    server.push_outcome(Outcome::Final("Still here.".into()));
    let mut phone = Phone::online(&address(&server));

    phone.send(Command::Start);
    phone.speak(&test_audio(1_000));
    streaming(&server, 1);
    phone.send(Command::Stop);
    phone.until_dictation(Dictation::Transcribing);
    server.disconnect_all();
    let (id, text) = phone.until_final();

    assert_eq!(text, "Still here.");
    assert!(phone.dictations().contains(&Dictation::Sending), "{:?}", phone.dictations());
    let seen = server.session(id).unwrap();
    assert_eq!(seen.starts.len(), 2, "{seen:#?}");
    assert!(seen.starts[1].resume);
}

#[test]
fn an_unreachable_server_fails_the_dictation_and_retry_delivers_it_once_it_is_back() {
    let server = tcp_server(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("Kept for later.".into()));
    let mut phone = Phone::online(&address(&server));
    server.set_listening(false);
    phone.until_link(Link::Offline);
    let audio = test_audio(1_500);

    phone.send(Command::Start);
    phone.speak(&audio);
    let recording = phone.until_state("recording offline", |s| s.dictation_message.is_some());
    phone.send(Command::Stop);
    let failed = phone.until_dictation(Dictation::Failed);
    server.set_listening(true);
    phone.send(Command::Retry);
    let (id, text) = phone.until_final();

    assert_eq!(recording.dictation, Dictation::Recording);
    assert!(failed.dictation_message.is_some(), "{failed:?}");
    assert_eq!(text, "Kept for later.");
    let shown = phone.dictations();
    let failing = shown.iter().position(|d| *d == Dictation::WaitingForServer).expect("waiting for the server");
    assert_eq!(shown[failing + 1], Dictation::Failed, "{shown:?}");
    let seen = server.session(id).unwrap();
    assert_eq!(seen.starts.len(), 1);
    assert_eq!(seen.audio, padded(&audio));
}

#[test]
fn a_failed_dictation_can_be_discarded() {
    let server = tcp_server(FakeServerConfig::default());
    let mut phone = Phone::online(&address(&server));
    server.set_listening(false);
    phone.until_link(Link::Offline);

    phone.send(Command::Start);
    phone.speak(&test_audio(500));
    phone.send(Command::Stop);
    phone.until_dictation(Dictation::Failed);
    phone.send(Command::Discard);
    let idle = phone.until_dictation(Dictation::Idle);

    assert_eq!(idle.dictation_message, None);
    assert!(phone.finals().is_empty());
    assert!(server.sessions().is_empty());
}
