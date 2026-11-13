use psst_proto::{Final, api::HistoryRow};
use psst_testkit::{fake_server::Outcome, test_audio};

use super::*;

/// The device the fake server pairs every phone as.
const PHONE: &str = "fake-device";

fn row(device: &str, text: Option<&str>, created_at: &str) -> HistoryRow {
    HistoryRow {
        id: 1,
        session_id: SessionId::new_v4(),
        device: device.into(),
        profile: "default".into(),
        created_at: created_at.parse().unwrap(),
        model: None,
        revision: None,
        r#final: text.map(|text| Final {
            text: text.into(),
            raw_text: text.into(),
            edits: vec![],
            language: "en".into(),
        }),
        server_timings: None,
        client_timings: None,
    }
}

fn item(text: &str, created_ms: i64) -> HistoryItem {
    HistoryItem { text: text.into(), created_ms }
}

#[test]
fn history_lists_this_phones_dictations_that_have_text_newest_first() {
    let server = tcp_server(FakeServerConfig::default());
    // As a server that predates the device filter answers: every device's rows.
    server.set_history(vec![
        row(PHONE, Some("  Newest.\n"), "2026-11-10T08:40:00Z"),
        row("local:alice", Some("From the laptop."), "2026-11-10T08:39:00Z"),
        row(PHONE, Some(" "), "2026-11-10T08:38:00Z"),
        row(PHONE, None, "2026-11-10T08:37:00Z"),
        row(PHONE, Some("Older."), "2026-11-10T08:36:00.250Z"),
    ]);
    let mut phone = Phone::online(&address(&server));

    phone.send(Command::History);
    let (items, error) = phone.until_history();

    assert_eq!(error, None);
    assert_eq!(items, [item("Newest.", 1_794_300_000_000), item("Older.", 1_794_299_760_250)]);
    assert_eq!(server.history_requests(), [format!("/api/history?limit=20&device={PHONE}")]);
}

#[test]
fn history_keeps_at_most_twenty() {
    let server = tcp_server(FakeServerConfig::default());
    server.set_history((0..25).map(|_| row(PHONE, Some("Again."), "2026-11-10T08:40:00Z")).collect());
    let mut phone = Phone::online(&address(&server));

    phone.send(Command::History);
    let (items, _) = phone.until_history();

    assert_eq!(items.len(), 20);
}

#[test]
fn history_that_cannot_be_had_says_why_and_lists_nothing() {
    let server = tcp_server(FakeServerConfig::default());
    server.set_history(vec![row(PHONE, Some("Hello."), "2026-11-10T08:40:00Z")]);
    let proxy = Proxy::to(&server);
    proxy.refuse_auto_pair();
    let mut phone = Phone::new();

    phone.send(Command::History);
    let unconfigured = phone.until_history();
    phone.send(Command::Configure { server: proxy.address.clone() });
    phone.until_link(Link::NeedsPairing);
    phone.send(Command::History);
    let unpaired = phone.until_history();
    phone.send(Command::Configure { server: address(&server) });
    phone.until_link(Link::Online);
    server.set_stalled(true);
    phone.send(Command::History);
    let no_answer = phone.until_history();
    server.set_stalled(false);
    server.set_listening(false);
    phone.send(Command::History);
    let unreachable = phone.until_history();

    let answers = [unconfigured, unpaired, no_answer, unreachable];
    for (items, error) in &answers {
        assert!(items.is_empty() && error.is_some(), "{items:?} {error:?}");
    }
    let reasons: std::collections::HashSet<_> = answers.iter().map(|(_, error)| error).collect();
    assert_eq!(reasons.len(), answers.len(), "{answers:#?}");
}

#[test]
fn asking_for_history_during_a_dictation_does_not_disturb_it() {
    let server = tcp_server(FakeServerConfig::default());
    server.set_history(vec![row(PHONE, Some("Earlier."), "2026-11-10T08:40:00Z")]);
    server.push_outcome(Outcome::Final("Hello world.".into()));
    let mut phone = Phone::online(&address(&server));
    let audio = test_audio(2_000);
    let (before, after) = audio.split_at(audio.len() / 2);

    phone.send(Command::Start);
    phone.speak(before);
    streaming(&server, 1);
    phone.send(Command::History);
    phone.speak(after);
    phone.send(Command::Stop);
    let (id, text) = phone.until_final();
    phone.until_dictation(Dictation::Idle);
    // It may have come before the final.
    if phone.histories().is_empty() {
        phone.until_history();
    }

    assert_eq!(text, "Hello world.");
    assert_eq!(phone.dictations(), [Dictation::Idle, Dictation::Recording, Dictation::Transcribing, Dictation::Idle]);
    assert_eq!(phone.histories(), [(vec![item("Earlier.", 1_794_300_000_000)], None)]);
    let seen = server.session(id).unwrap();
    assert_eq!(seen.starts.len(), 1);
    assert_eq!(seen.audio, padded(&audio));
}
