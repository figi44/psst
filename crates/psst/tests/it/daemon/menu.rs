use std::time::{Duration, Instant};

use psst::{
    daemon::Sound,
    ipc::{Request, Status},
    store::{Meta, RecState, Store},
};
use psst_config::{ClientConfig, MenuConfig};
use psst_proto::{Final, SessionId, api::HistoryRow};
use psst_testkit::{
    fake_server::{FakeServer, FakeServerConfig, Outcome},
    test_audio, write_wav,
};
use psst_wayland::{HistoryEntry, Listing, MenuAction, MenuOutcome, MenuSpec, OverlayState, WaylandCmd, WaylandEvent};

use super::{Daemon, Inserted, WAIT, daemon, daemon_in, paste, wait_until};

async fn until(d: &Daemon, what: &str, check: impl Fn(&Status) -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let status = d.status().await;
        if check(&status) {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "never {what}: {status:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_double_tap_opens_the_menu_without_a_trace_of_the_recording_it_began() {
    let server = FakeServer::start(FakeServerConfig::default());
    let d = daemon(&server, |cfg, _| cfg.menu = MenuConfig::default()).await;

    let first = d.toggle().await;
    let first: SessionId = first.message.rsplit(' ').next().unwrap().parse().expect("recording");
    let tapped = d.toggle().await;
    assert_eq!(tapped.message, "opened the menu");
    let status = d.status().await;
    assert_eq!((status.state.as_str(), &status.recording, status.retained.len()), ("idle", &None, 0), "{status:?}");
    wait_until(|| d.wayland.menus().last().is_some_and(Option::is_some)).await;
    let spec = d.wayland.menus().last().cloned().flatten().unwrap();
    assert_eq!(spec.profile, "default");

    assert_eq!(d.toggle().await.message, "closed the menu", "the hotkey closes it again");
    wait_until(|| d.wayland.menus().last() == Some(&None)).await;
    assert_eq!(d.sounds(), [], "no chime");
    assert!(!d.wayland.showed(|_| true), "no overlay: {:?}", d.wayland.overlays());
    assert!(d.toggle().await.ok);
    assert!(!d.run(Request::Menu).await.ok, "not over a live microphone");
    assert!(d.run(Request::Cancel).await.ok);
    // The server's answers about profiles and the model may have updated it while it was open.
    let menus = d.wayland.menus().len();
    d.dictate(500).await;
    d.idle().await;
    assert_eq!(d.wayland.menus().len(), menus, "slow enough to dictate: no menu");
    assert_eq!(server.session(first), None, "the server never heard of it");
    d.stop().await;

    let d = daemon(&server, |_, _| {}).await;
    assert!(d.toggle().await.ok);
    let stopped = d.toggle().await;
    assert!(stopped.ok && stopped.message.starts_with("stop"), "double-tapping is off: {stopped:?}");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_single_tap_records_from_the_tap_but_chimes_only_once_no_second_can_come() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("every word ".into()));
    let window = Duration::from_millis(400);
    let d = daemon(&server, |cfg, _| cfg.menu = MenuConfig { double_tap_ms: window.as_millis() as u32 }).await;

    let tapped = Instant::now();
    let started = d.toggle().await;
    let id: SessionId = started.message.rsplit(' ').next().unwrap().parse().expect("recording");
    wait_until(|| d.wayland.showed(|_| true)).await;
    assert!(tapped.elapsed() >= window, "the overlay came after {:?}", tapped.elapsed());
    wait_until(|| d.sounds() == [Sound::Start]).await;
    assert!(tapped.elapsed() >= window, "the chime came after {:?}", tapped.elapsed());
    assert!(d.wayland.showed(|s| matches!(s, OverlayState::Recording { .. })), "{:?}", d.wayland.overlays());

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(d.toggle().await.message.starts_with("stopped"));
    d.idle().await;
    assert_eq!(d.wayland.inserted(), vec![paste("every word ")]);
    let seen = server.session(id).unwrap();
    let samples = seen.stop_samples.unwrap() as usize;
    assert!(samples >= 16 * 700, "{samples}");
    assert_eq!(&seen.audio[..samples], &d.audio[..samples], "what was said before the chime is kept");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paste_waits_for_the_menu_to_close() {
    let server = FakeServer::start(FakeServerConfig { final_delay: Duration::from_millis(300), ..Default::default() });
    server.push_outcome(Outcome::Final("after the menu ".into()));
    let d = daemon(&server, |_, _| {}).await;

    d.dictate(500).await;
    assert_eq!(d.run(Request::Menu).await.message, "opened the menu");
    until(&d, "transcribed", |s| s.finishing.first().is_some_and(|f| f.phase == "waiting to be inserted")).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(d.wayland.inserted(), vec![], "the menu has the keyboard");

    d.wayland.send(WaylandEvent::Menu(MenuOutcome::Closed));
    d.idle().await;
    assert_eq!(d.wayland.inserted(), vec![paste("after the menu ")]);
    let cmds = d.wayland.cmds.lock().unwrap().clone();
    let closed = cmds.iter().rposition(|cmd| matches!(cmd, WaylandCmd::Menu(None)));
    let pasted = cmds.iter().position(|cmd| matches!(cmd, WaylandCmd::Paste { .. }));
    assert!(
        matches!((closed, pasted), (Some(closed), Some(pasted)) if closed < pasted),
        "closed again, in case an update had reopened it, before the paste: {cmds:?}"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_profile_chosen_in_the_menu_is_used_until_client_toml_names_another() {
    let server = FakeServer::start(FakeServerConfig::default());
    let dir = tempfile::tempdir().unwrap();
    let d = daemon_in(dir.path(), &server, |_, _| {}).await;

    assert!(d.run(Request::Menu).await.ok);
    wait_until(|| d.wayland.menus().last().is_some_and(Option::is_some)).await;
    d.wayland.send(WaylandEvent::Menu(MenuOutcome::Chose(MenuAction::SwitchProfile("cs".into()))));
    until(&d, "switched", |s| s.profile == "cs").await;
    let id = d.dictate(400).await;
    d.idle().await;
    assert_eq!(server.session(id).unwrap().starts[0].profile.as_deref(), Some("cs"));
    d.stop().await;

    let restarted = daemon_in(dir.path(), &server, |_, _| {}).await;
    assert_eq!(restarted.status().await.profile, "cs", "kept across restarts");
    restarted.stop().await;

    let configured = |cfg: &mut ClientConfig, _: &mut _| cfg.profile = "work".into();
    let changed = daemon_in(dir.path(), &server, configured).await;
    assert_eq!(changed.status().await.profile, "work", "a new profile in client.toml wins");
    changed.stop().await;
    let back = daemon_in(dir.path(), &server, |_, _| {}).await;
    assert_eq!(back.status().await.profile, "default", "and the menu's choice is forgotten");
    back.stop().await;
}

/// The spec of the open menu once it passes `check`.
async fn menu_until(d: &Daemon, what: &str, check: impl Fn(&MenuSpec) -> bool) -> MenuSpec {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Some(Some(spec)) = d.wayland.menus().last()
            && check(spec)
        {
            return spec.clone();
        }
        assert!(tokio::time::Instant::now() < deadline, "the menu never showed {what}: {:?}", d.wayland.menus().last());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn saved(store: &Store, state: RecState, text: Option<&str>) -> SessionId {
    let id = SessionId::new_v4();
    let meta = Meta {
        profile: "default".into(),
        started_at: jiff::Timestamp::now(),
        samples: 8_000,
        gapped: false,
        state,
        final_text: text.map(String::from),
        finalized_at: None,
    };
    store.write_meta(id, &meta).unwrap();
    write_wav(&store.wav_path(id), &test_audio(500));
    id
}

fn row(device: &str, text: &str) -> HistoryRow {
    HistoryRow {
        id: 1,
        session_id: SessionId::new_v4(),
        device: device.into(),
        profile: "default".into(),
        created_at: jiff::Timestamp::now(),
        model: None,
        revision: None,
        r#final: Some(Final { text: text.into(), raw_text: text.into(), edits: vec![], language: "en".into() }),
        server_timings: None,
        client_timings: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_saved_dictation_is_transcribed_in_the_background_and_pasted_from_the_history() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("Written later. ".into()));
    let d = daemon(&server, |_, deps| deps.timing.stop_window = Duration::from_millis(500)).await;
    server.set_listening(false);
    let id = d.dictate(300).await;
    d.idle().await;
    assert!(
        d.wayland.showed(|s| matches!(s, OverlayState::Error { message } if message.starts_with("saved to History")))
    );

    server.set_listening(true);
    assert_eq!(d.run(Request::Menu).await.message, "opened the menu", "which connects");
    let spec = menu_until(&d, "the text", |spec| spec.saved.first().is_some_and(|e| e.text.is_some())).await;
    assert_eq!(spec.saved.len(), 1);
    assert_eq!((spec.saved[0].key.clone(), spec.saved[0].text.as_deref()), (id.to_string(), Some("Written later.")));
    assert_eq!(d.status().await.retained[0].state, "transcribed");
    assert!(d.wayland.inserted().is_empty(), "never pasted on its own");
    assert!(server.session(id).unwrap().starts.last().unwrap().resume);

    d.wayland.send(WaylandEvent::Menu(MenuOutcome::Chose(MenuAction::Paste(id.to_string()))));
    wait_until(|| d.store.list().is_empty()).await;
    assert_eq!(d.wayland.inserted(), vec![paste("Written later. ")]);
    assert!(d.wayland.snapshots_precede_inserts());
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dictation_takes_the_connection_from_a_saved_one_going_in_the_background() {
    // Long enough a wait for its text that the dictation comes while the saved one holds the connection.
    let server = FakeServer::start(FakeServerConfig { final_delay: Duration::from_millis(800), ..Default::default() });
    let dir = tempfile::tempdir().unwrap();
    let waiting = saved(&Store::open(&dir.path().join("state")).unwrap(), RecState::Complete, None);
    let d = daemon_in(dir.path(), &server, |_, _| {}).await;
    wait_until(|| server.session(waiting).is_some_and(|s| s.stop_samples.is_some())).await;

    let id = d.dictate(400).await;
    d.idle().await;
    let pasted = d.wayland.inserted();
    assert!(matches!(&pasted[..], [Inserted::Paste(text, _)] if text.starts_with("heard")), "{pasted:?}");
    assert_eq!(server.session(id).unwrap().starts[0].mode, psst_proto::ws::Mode::Live, "streamed live");
    assert!(server.session(waiting).unwrap().cancelled, "the saved one made way");

    super::wait_until(|| d.store.read_meta(waiting).is_ok_and(|m| m.state == RecState::Transcribed)).await;
    assert_eq!(d.store.read_meta(waiting).unwrap().final_text.as_deref(), Some("heard 500 ms"));
    assert_eq!(d.wayland.inserted(), pasted, "and was never pasted");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_history_lists_this_machines_dictations_from_the_server_after_the_saved_ones() {
    let server = FakeServer::start(FakeServerConfig::default());
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("state")).unwrap();
    let ready = saved(&store, RecState::Transcribed, Some("Never went in. "));
    let earlier = row("local:test", "From the server. ");
    let mut ready_row = row("local:test", "Never went in. ");
    ready_row.session_id = ready;
    server.set_history(vec![ready_row, earlier.clone(), row("phone", "From the phone.")]);
    let d = daemon_in(dir.path(), &server, |_, _| {}).await;

    assert!(d.run(Request::Menu).await.ok);
    let spec = menu_until(&d, "the server's history", |spec| matches!(spec.history, Listing::Ready(_))).await;
    assert_eq!(spec.saved.iter().map(|e| e.text.as_deref()).collect::<Vec<_>>(), [Some("Never went in.")]);
    let key = earlier.session_id.to_string();
    let Listing::Ready(history) = &spec.history else { unreachable!() };
    assert_eq!(
        history,
        &[HistoryEntry { key: key.clone(), when: history[0].when.clone(), text: Some("From the server.".into()) }]
    );
    assert!(server.history_requests()[0].ends_with("&device=local%3Atest"), "{:?}", server.history_requests());

    d.wayland.send(WaylandEvent::Menu(MenuOutcome::Kept(MenuAction::Discard(ready.to_string()))));
    menu_until(&d, "it gone", |spec| spec.saved.is_empty()).await;
    assert!(d.store.list().is_empty());
    d.wayland.send(WaylandEvent::Menu(MenuOutcome::Chose(MenuAction::Copy(key))));
    wait_until(|| !d.wayland.inserted().is_empty()).await;
    assert_eq!(d.wayland.inserted(), vec![Inserted::Copy("From the server. ".into())]);
    d.stop().await;
}
