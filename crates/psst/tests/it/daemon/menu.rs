use std::time::{Duration, Instant};

use psst::{
    daemon::Sound,
    ipc::{Request, Status},
};
use psst_config::{ClientConfig, MenuConfig};
use psst_proto::SessionId;
use psst_testkit::fake_server::{FakeServer, FakeServerConfig, Outcome};
use psst_wayland::{MenuAction, MenuOutcome, OverlayState, WaylandCmd, WaylandEvent};

use super::{Daemon, WAIT, daemon, daemon_in, paste, wait_until};

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
