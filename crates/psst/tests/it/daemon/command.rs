use psst_proto::{
    MAX_SELECTION_CHARS,
    api::{ApiError, ApiErrorCode, Rewrite, RewriteRequest},
};
use psst_wayland::{Command, MenuAction, MenuOutcome, Selected, WaylandCmd};

use super::*;

const SELECTED: &str = "hey can u send me the report";
const REWRITTEN: &str = "Could you send me the report?";

/// The menu open, once it shows what the selection read gave.
async fn open_menu(server: &FakeServer, selection: Option<&str>) -> (Daemon, Selected) {
    open_menu_with(server, selection, |_, _| {}).await
}

async fn open_menu_with(
    server: &FakeServer,
    selection: Option<&str>,
    tweak: impl FnOnce(&mut ClientConfig, &mut Deps),
) -> (Daemon, Selected) {
    let d = daemon(server, tweak).await;
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
    let (d, _) = open_menu_with(&server, Some(SELECTED), |_, deps| {
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

/// Answers each `FocusedWindow` with the same window, `delay` late: time for the test to act
/// while a rewrite's checks wait on it.
fn slow_niri(dir: &Path, app_id: &'static str, delay: Duration) -> PathBuf {
    niri_after(dir, app_id, delay, &[])
}

/// As [`slow_niri`], but with no window focused for the answers numbered in `none` (from 0), as
/// while a layer surface such as the menu has the keyboard.
fn niri_after(dir: &Path, app_id: &'static str, delay: Duration, none: &'static [u64]) -> PathBuf {
    counted_niri(dir, app_id, delay, none, None).0
}

/// As [`niri_after`], with how many lookups it has been asked for so far: one counted is still
/// waiting out `delay`.
fn counted_niri(
    dir: &Path,
    app_id: &'static str,
    delay: Duration,
    none: &'static [u64],
    hold_from: Option<(u64, Arc<AtomicBool>)>,
) -> (PathBuf, Arc<AtomicU64>) {
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
    };
    let niri = dir.join("niri.sock");
    let listener = UnixListener::bind(&niri).unwrap();
    let asked = Arc::new(AtomicU64::new(0));
    let counted = asked.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (asked, hold_from) = (asked.clone(), hold_from.clone());
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                BufReader::new(read).read_line(&mut String::new()).await.unwrap();
                let number = asked.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(delay).await;
                if let Some((_, released)) = hold_from.filter(|(from, _)| number >= *from) {
                    while !released.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }
                let window = match none.contains(&number) {
                    true => serde_json::Value::Null,
                    false => serde_json::json!({ "id": 1, "app_id": app_id }),
                };
                let reply = serde_json::json!({ "Ok": { "FocusedWindow": window } });
                let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
            });
        }
    });
    (niri, counted)
}

/// Answers the first lookup, as a rewrite is asked for; holds every later one until the test
/// releases them, so it can act while the paste-time lookup is on its way.
fn held_niri(dir: &Path) -> (PathBuf, Arc<AtomicU64>, Arc<AtomicBool>) {
    let released = Arc::new(AtomicBool::new(false));
    let (niri, asked) = counted_niri(dir, "org.gnome.TextEditor", Duration::ZERO, &[], Some((1, released.clone())));
    (niri, asked, released)
}

async fn with_niri(server: &FakeServer, niri: PathBuf) -> Daemon {
    let (d, _) = open_menu_with(server, Some(SELECTED), |cfg, deps| {
        let copy_only = AppRule { paste: Some(PasteAction::CopyOnly), paste_delay_ms: 0 };
        cfg.apps.insert("org.keepassxc.KeePassXC".into(), copy_only);
        deps.niri_socket = Some(niri);
        deps.timing.niri_lookup = Duration::from_secs(5);
    })
    .await;
    d
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewrite_in_a_terminal_goes_to_the_clipboard() {
    let dir = tempfile::tempdir().unwrap();
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_rewrite(Ok(REWRITTEN.into()));
    let d = with_niri(&server, slow_niri(dir.path(), "com.mitchellh.ghostty", Duration::ZERO)).await;

    choose(&d, Command::Instruction("make it polite".into()));
    wait_until(|| !d.wayland.inserted().is_empty()).await;
    d.idle().await;

    assert_eq!(d.wayland.inserted(), vec![Inserted::Copy(REWRITTEN.into())], "not typed at the prompt");
    let note = "Pasting here can't replace a selection: the rewrite is in the clipboard";
    assert!(d.wayland.showed(|s| matches!(s, OverlayState::Flash { message } if message == note)));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewrite_waits_for_the_menu_to_give_its_window_back() {
    let dir = tempfile::tempdir().unwrap();
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_rewrite(Ok(REWRITTEN.into()));
    let d = with_niri(&server, niri_after(dir.path(), "org.gnome.TextEditor", Duration::ZERO, &[0, 1])).await;

    choose(&d, Command::Instruction("make it polite".into()));
    wait_until(|| !d.wayland.inserted().is_empty()).await;
    d.idle().await;

    assert_eq!(d.wayland.inserted(), vec![paste(REWRITTEN)], "not to the clipboard for want of a window");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewrite_waits_out_a_layer_as_it_goes_in_too() {
    let dir = tempfile::tempdir().unwrap();
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_rewrite(Ok(REWRITTEN.into()));
    // Asked from the window; as it goes in, a layer surface briefly has the keyboard.
    let d = with_niri(&server, niri_after(dir.path(), "org.gnome.TextEditor", Duration::ZERO, &[1])).await;

    choose(&d, Command::Instruction("make it polite".into()));
    wait_until(|| !d.wayland.inserted().is_empty()).await;
    d.idle().await;

    assert_eq!(d.wayland.inserted(), vec![paste(REWRITTEN)]);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewrite_waits_longer_than_a_dictation_for_a_busy_niri() {
    let dir = tempfile::tempdir().unwrap();
    let niri = slow_niri(dir.path(), "org.gnome.TextEditor", Duration::from_millis(300));
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_rewrite(Ok(REWRITTEN.into()));
    // The default lookup, 150 ms: too short for this niri.
    let (d, _) = open_menu_with(&server, Some(SELECTED), |_, deps| deps.niri_socket = Some(niri)).await;

    choose(&d, Command::Instruction("make it polite".into()));
    wait_until(|| !d.wayland.inserted().is_empty()).await;
    d.idle().await;

    assert_eq!(d.wayland.inserted(), vec![paste(REWRITTEN)]);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewrite_for_an_app_that_only_gets_copies_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_rewrite(Ok(REWRITTEN.into()));
    let d = with_niri(&server, slow_niri(dir.path(), "org.keepassxc.KeePassXC", Duration::ZERO)).await;

    choose(&d, Command::Instruction("make it polite".into()));
    wait_until(|| !d.wayland.inserted().is_empty()).await;
    d.idle().await;

    assert_eq!(d.wayland.inserted(), vec![Inserted::Copy(REWRITTEN.into())]);
    let note = "Pasting is off for this app: the rewrite is in the clipboard";
    assert!(d.wayland.showed(|s| matches!(s, OverlayState::Flash { message } if message == note)));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewrite_waits_out_a_menu_opened_while_it_was_checked() {
    let dir = tempfile::tempdir().unwrap();
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_rewrite(Ok(REWRITTEN.into()));
    let (niri, asked, released) = held_niri(dir.path());
    let d = with_niri(&server, niri).await;
    let reads =
        || d.wayland.cmds.lock().unwrap().iter().filter(|c| matches!(c, WaylandCmd::ReadSelection { .. })).count();

    choose(&d, Command::Instruction("make it polite".into()));
    // Asked for, answered, and now waiting on the window lookup before it goes in.
    wait_until(|| asked.load(Ordering::SeqCst) == 2).await;
    assert!(d.run(Request::Menu).await.ok);
    released.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(d.wayland.inserted().is_empty(), "nothing goes in while the menu is open");

    d.wayland.send(WaylandEvent::Menu(MenuOutcome::Closed));
    wait_until(|| !d.wayland.inserted().is_empty()).await;
    assert_eq!(d.wayland.inserted(), vec![paste(REWRITTEN)]);
    assert_eq!(asked.load(Ordering::SeqCst), 3, "the window looked up again after the menu closed");
    assert_eq!(reads(), 3, "the menu's two reads, then the selection's check, last");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_selection_is_checked_last_after_a_slow_window_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_rewrite(Ok(REWRITTEN.into()));
    let (niri, asked, released) = held_niri(dir.path());
    let d = with_niri(&server, niri).await;

    choose(&d, Command::Instruction("make it polite".into()));
    wait_until(|| asked.load(Ordering::SeqCst) == 2).await;
    // Selected anew in the same window while its lookup was on its way.
    *d.wayland.selection.lock().unwrap() = Some("something else".into());
    released.store(true, Ordering::SeqCst);
    wait_until(|| !d.wayland.inserted().is_empty()).await;
    d.idle().await;

    assert_eq!(d.wayland.inserted(), vec![Inserted::Copy(REWRITTEN.into())]);
    let note = "The selection changed: the rewrite is in the clipboard";
    assert!(d.wayland.showed(|s| matches!(s, OverlayState::Flash { message } if message == note)));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_a_rewrite_while_it_is_checked() {
    let dir = tempfile::tempdir().unwrap();
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_rewrite(Ok(REWRITTEN.into()));
    let (niri, asked, released) = held_niri(dir.path());
    let d = with_niri(&server, niri).await;

    choose(&d, Command::Instruction("make it polite".into()));
    wait_until(|| asked.load(Ordering::SeqCst) == 2).await;
    assert_eq!(d.run(Request::Cancel).await.message, "cancelled the rewrite");
    released.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
    d.idle().await;
    assert!(d.wayland.inserted().is_empty());
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
