use psst_proto::{
    MAX_SELECTION_CHARS,
    api::{ApiError, ApiErrorCode, Rewrite, RewriteRequest},
};
use psst_wayland::{Command, MenuAction, MenuOutcome, Selected};

use super::*;

const SELECTED: &str = "hey can u send me the report";
const REWRITTEN: &str = "Could you send me the report?";

/// The menu open, once it shows what the selection read gave.
async fn open_menu(server: &FakeServer, selection: Option<&str>) -> (Daemon, Selected) {
    open_menu_with(server, selection, |_| {}).await
}

async fn open_menu_with(
    server: &FakeServer,
    selection: Option<&str>,
    tweak: impl FnOnce(&mut Deps),
) -> (Daemon, Selected) {
    let d = daemon(server, |_, deps| tweak(deps)).await;
    *d.wayland.selection.lock().unwrap() = selection.map(str::to_owned);
    assert!(d.run(Request::Menu).await.ok);
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Some(Some(spec)) = d.wayland.menus().last()
            && spec.selection != Selected::Reading
        {
            let selected = spec.selection.clone();
            return (d, selected);
        }
        assert!(tokio::time::Instant::now() < deadline, "the menu never read the selection");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn choose(d: &Daemon, command: Command) {
    d.wayland.send(WaylandEvent::Menu(MenuOutcome::Chose(MenuAction::Command(command))));
}

fn error_shown(d: &Daemon, message: &str) -> bool {
    d.wayland.showed(|s| matches!(s, OverlayState::Error { message: m } if m == message))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_from_the_menu_pastes_the_rewritten_selection_over_it() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_rewrite(Ok(REWRITTEN.into()));
    let (d, selected) = open_menu(&server, Some(SELECTED)).await;
    assert_eq!(selected, Selected::Text(SELECTED.into()), "the page shows what it would rewrite");

    choose(&d, Command::Style("polished".into()));
    wait_until(|| !d.wayland.inserted().is_empty()).await;
    d.idle().await;

    assert_eq!(d.wayland.inserted(), vec![paste(REWRITTEN)]);
    assert!(d.wayland.snapshots_precede_inserts(), "the clipboard comes back after the paste");
    let request = RewriteRequest { selection: SELECTED.into(), how: Rewrite::Style("polished".into()) };
    assert_eq!(server.rewrite_requests(), [request]);
    assert!(d.wayland.showed(|s| matches!(s, OverlayState::Flash { message } if message == "Rewriting\u{2026}")));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewrite_never_replaces_text_selected_since() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_rewrite(Ok(REWRITTEN.into()));
    let (d, _) = open_menu(&server, Some(SELECTED)).await;
    *d.wayland.selection.lock().unwrap() = Some("something else".into());

    choose(&d, Command::Instruction("make it polite".into()));
    wait_until(|| !d.wayland.inserted().is_empty()).await;
    d.idle().await;

    assert_eq!(d.wayland.inserted(), vec![Inserted::Copy(REWRITTEN.into())]);
    let note = "The selection changed: the rewrite is in the clipboard";
    assert!(d.wayland.showed(|s| matches!(s, OverlayState::Flash { message } if message == note)));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewrite_never_lands_in_another_window() {
    let dir = tempfile::tempdir().unwrap();
    let window = Arc::new(Mutex::new(serde_json::json!({ "id": 1, "app_id": "org.gnome.TextEditor" })));
    let niri = fake_niri(dir.path(), window.clone());
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_rewrite(Ok(REWRITTEN.into()));
    server.set_stalled(true);
    let (d, _) = open_menu_with(&server, Some(SELECTED), |deps| {
        deps.niri_socket = Some(niri.clone());
        deps.timing.niri_lookup = Duration::from_secs(5);
    })
    .await;

    choose(&d, Command::Instruction("make it polite".into()));
    wait_until(|| {
        d.wayland.showed(|s| matches!(s, OverlayState::Flash { message } if message.starts_with("Rewriting")))
    })
    .await;
    *window.lock().unwrap() = serde_json::json!({ "id": 2, "app_id": "org.gnome.TextEditor" });
    server.set_stalled(false);
    wait_until(|| !d.wayland.inserted().is_empty()).await;
    d.idle().await;

    assert_eq!(d.wayland.inserted(), vec![Inserted::Copy(REWRITTEN.into())]);
    let note = "The window changed: the rewrite is in the clipboard";
    assert!(d.wayland.showed(|s| matches!(s, OverlayState::Flash { message } if message == note)));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewrite_whose_paste_fails_goes_to_the_clipboard() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_rewrite(Ok(REWRITTEN.into()));
    let (d, _) = open_menu(&server, Some(SELECTED)).await;
    d.wayland.fail_inserts.store(true, Ordering::SeqCst);

    choose(&d, Command::Instruction("make it polite".into()));
    wait_until(|| d.wayland.inserted().len() == 2).await;
    assert_eq!(d.wayland.inserted(), vec![paste(REWRITTEN), Inserted::Copy(REWRITTEN.into())]);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_a_rewrite_on_its_way() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_rewrite(Ok(REWRITTEN.into()));
    server.set_stalled(true);
    let (d, _) = open_menu(&server, Some(SELECTED)).await;

    choose(&d, Command::Instruction("make it polite".into()));
    wait_until(|| {
        d.wayland.showed(|s| matches!(s, OverlayState::Flash { message } if message.starts_with("Rewriting")))
    })
    .await;
    assert_eq!(d.run(Request::Cancel).await.message, "cancelled the rewrite");
    server.set_stalled(false);
    wait_until(|| !server.rewrite_requests().is_empty()).await;
    d.idle().await;
    assert!(d.wayland.inserted().is_empty());
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_command_page_says_why_there_is_nothing_to_rewrite() {
    let server = FakeServer::start(FakeServerConfig::default());
    let (d, selected) = open_menu(&server, None).await;
    assert_eq!(selected, Selected::Missing("select the text first".into()));
    d.stop().await;

    let (d, selected) = open_menu(&server, Some(&"x".repeat(MAX_SELECTION_CHARS + 1))).await;
    assert_eq!(selected, Selected::Missing(format!("select at most {MAX_SELECTION_CHARS} characters")));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_rewrite_says_why_and_leaves_the_text_alone() {
    let server = FakeServer::start(FakeServerConfig::default());
    let refusal = "connect ChatGPT in the web UI to use commands";
    server.push_rewrite(Err(ApiError { code: ApiErrorCode::BadRequest, message: refusal.into() }));
    let (d, _) = open_menu(&server, Some(SELECTED)).await;

    choose(&d, Command::Instruction("make it polite".into()));
    wait_until(|| error_shown(&d, refusal)).await;
    d.idle().await;
    assert!(d.wayland.inserted().is_empty());
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_from_before_commands_asks_to_be_updated() {
    let server = FakeServer::start(FakeServerConfig::default());
    let (d, _) = open_menu(&server, Some(SELECTED)).await;

    choose(&d, Command::Instruction("make it polite".into()));
    wait_until(|| error_shown(&d, "update the server for commands")).await;
    assert!(d.wayland.inserted().is_empty());
    d.stop().await;
}
