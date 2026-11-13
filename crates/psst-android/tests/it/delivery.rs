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
fn an_unreachable_server_saves_the_dictation_and_its_text_waits_once_the_server_is_back() {
    let server = tcp_server(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("Kept for later.".into()));
    let mut phone = Phone::online(&address(&server));
    server.set_listening(false);
    phone.until_link(Link::Offline);
    let audio = test_audio(1_500);

    phone.send(Command::Start);
    phone.speak(&audio);
    phone.until_state("recording offline", |s| s.dictation_message.is_some());
    phone.send(Command::Stop);
    let id = phone.until_saved();
    let waiting = phone.until_state("idle with it saved", |s| s.dictation == Dictation::Idle);
    server.set_listening(true);
    phone.warm_until_online();
    let transcribed = phone.until_saved_list("its text", |saved| saved.first().is_some_and(|d| d.text.is_some()));
    phone.send(Command::Inserted { session_id: id });
    phone.until_saved_list("it gone once inserted", <[_]>::is_empty);

    assert_eq!(waiting.saved.len(), 1);
    assert_eq!((waiting.saved[0].session_id, waiting.saved[0].duration_ms, &waiting.saved[0].text), (id, 1_500, &None));
    assert_eq!((transcribed[0].session_id, transcribed[0].text.as_deref()), (id, Some("Kept for later.")));
    assert!(phone.finals().is_empty(), "never delivered as a final: its field is long gone");
    let shown = phone.dictations();
    assert_eq!(shown[shown.len() - 2..], [Dictation::WaitingForServer, Dictation::Idle], "{shown:?}");
    let seen = server.session(id).unwrap();
    assert_eq!(seen.starts.len(), 1);
    assert_eq!(seen.audio, padded(&audio));
}

#[test]
fn a_saved_dictation_can_be_discarded() {
    let server = tcp_server(FakeServerConfig::default());
    let mut phone = Phone::online(&address(&server));
    server.set_listening(false);
    phone.until_link(Link::Offline);

    phone.send(Command::Start);
    phone.speak(&test_audio(500));
    phone.send(Command::Stop);
    let id = phone.until_saved();
    phone.until_saved_list("it saved", |saved| saved.len() == 1);
    phone.send(Command::Discard { session_id: id });
    phone.until_saved_list("it gone", <[_]>::is_empty);

    assert!(phone.finals().is_empty());
    assert!(server.sessions().is_empty());
    assert_eq!(std::fs::read_dir(phone.dir().join("recordings")).unwrap().count(), 0);
}

#[test]
fn a_dictation_outlives_the_app_being_killed_and_is_sent_once_it_starts_again() {
    let server = tcp_server(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("Not lost.".into()));
    let dir = tempfile::tempdir().unwrap();
    let audio = test_audio(1_000);
    {
        let mut phone = Phone::at(dir.path());
        phone.send(Command::Configure { server: address(&server) });
        phone.until_link(Link::Online);
        server.set_listening(false);
        phone.until_link(Link::Offline);
        phone.send(Command::Start);
        phone.speak(&audio);
        // Dropped mid-dictation, as when Android kills the process.
    }
    server.set_listening(true);
    let mut phone = Phone::at(dir.path());
    phone.warm_until_online();

    let saved = phone.until_saved_list("its text", |saved| saved.first().is_some_and(|d| d.text.is_some()));
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].text.as_deref(), Some("Not lost."));
    assert_eq!(server.session(saved[0].session_id).unwrap().audio, padded(&audio));
}

#[test]
fn a_dictation_saved_for_one_server_never_goes_to_another() {
    let first = tcp_server(FakeServerConfig::default());
    let second = tcp_server(FakeServerConfig::default());
    let mut phone = Phone::online(&address(&first));
    first.set_listening(false);
    phone.until_link(Link::Offline);
    phone.send(Command::Start);
    phone.speak(&test_audio(1_000));
    phone.send(Command::Stop);
    let saved = phone.until_saved();

    phone.send(Command::Configure { server: address(&second) });
    phone.until_link(Link::Online);
    // A dictation of its own, so the phone has had every chance to send the saved one along.
    phone.send(Command::Start);
    phone.speak(&test_audio(500));
    phone.send(Command::Stop);
    let (id, _) = phone.until_final();
    assert!(second.session(id).is_some());
    assert!(second.session(saved).is_none(), "the first server's dictation went to the second");

    first.set_listening(true);
    phone.send(Command::Configure { server: address(&first) });
    phone.until_link(Link::Online);
    assert!(first.wait_until(WAIT, |s| s.session(saved).is_some_and(|s| s.stop_samples.is_some())));
}

#[test]
fn a_dictation_takes_the_connection_from_a_saved_one_going_in_the_background() {
    // Long enough a wait for its text that the dictation comes while the saved one holds the connection.
    let server = tcp_server(FakeServerConfig { final_delay: Duration::from_millis(800), ..Default::default() });
    let dir = tempfile::tempdir().unwrap();
    let store = psst_client::store::Store::open(dir.path()).unwrap();
    let waiting = SessionId::new_v4();
    let meta = psst_client::store::Meta {
        profile: String::new(),
        started_at: jiff::Timestamp::now(),
        samples: 8_000,
        gapped: false,
        state: psst_client::store::RecState::Complete,
        final_text: None,
        finalized_at: None,
        server: None,
    };
    store.write_meta(waiting, &meta).unwrap();
    psst_testkit::write_wav(&store.wav_path(waiting), &test_audio(500));
    let mut phone = Phone::at(dir.path());
    phone.send(Command::Configure { server: address(&server) });
    phone.until_link(Link::Online);
    assert!(server.wait_until(WAIT, |s| s.session(waiting).is_some_and(|s| s.stop_samples.is_some())));

    let audio = test_audio(700);
    let (before, after) = audio.split_at(audio.len() / 2);
    phone.send(Command::Start);
    phone.speak(before);
    streaming(&server, 2);
    phone.speak(after);
    phone.send(Command::Stop);
    let (id, text) = phone.until_final();
    let saved = phone.until_saved_list("the saved one's text", |saved| saved.first().is_some_and(|d| d.text.is_some()));

    assert!(text.starts_with("heard"), "{text}");
    assert_eq!(server.session(id).unwrap().starts[0].mode, Mode::Live, "streamed live");
    assert!(server.session(waiting).unwrap().cancelled, "the saved one made way");
    assert_eq!((saved[0].session_id, saved[0].text.as_deref()), (waiting, Some("heard 500 ms")));
    assert_eq!(phone.finals(), [text], "and was never delivered as a final");
}
