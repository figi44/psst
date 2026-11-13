use psst::follow::WaybarLine;
use psst_proto::PROTOCOL_VERSION;
use tokio::time::Instant;

use super::*;

fn error_shown(d: &Daemon, message: &str) -> usize {
    d.wayland
        .overlays()
        .iter()
        .flatten()
        .filter(|s| matches!(s, OverlayState::Error { message: m } if m == message))
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_newer_server_prompts_to_update_psst_and_keeps_the_dictation_for_retry() {
    const PROMPT: &str = "update psst";
    let server = FakeServer::start(FakeServerConfig::default());
    server.speak_protocol(PROTOCOL_VERSION + 1, true);
    server.push_outcome(Outcome::Final("after the update ".into()));
    let d = daemon(&server, |_, deps| deps.timing.notice_shown = Duration::from_millis(300)).await;

    wait_until(|| error_shown(&d, PROMPT) == 1).await;
    let status = d.status().await;
    assert_eq!(status.warnings, vec![PROMPT.to_string()]);
    let facts = format!("the server (psst fake) speaks protocol {}", PROTOCOL_VERSION + 1);
    assert!(status.connection.contains(&facts), "{status:?}");
    while d.status().await.error.is_some() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let id = d.dictate(300).await;
    let stopped = Instant::now();
    d.idle().await;
    assert!(stopped.elapsed() < Duration::from_secs(5), "saved after {:?}", stopped.elapsed());
    assert!(d.wayland.showed(|s| matches!(s, OverlayState::Offline { reason, .. } if reason == PROMPT)));
    assert_eq!(error_shown(&d, &format!("saved to History ({PROMPT})")), 1);
    assert_eq!(error_shown(&d, PROMPT), 1, "said once, not at every refusal");
    assert_eq!(d.store.read_meta(id).unwrap().state, RecState::Complete);
    assert!(d.wayland.inserted().is_empty());

    server.speak_protocol(PROTOCOL_VERSION, true);
    assert!(d.run(Request::Retry { session: None }).await.ok);
    d.idle().await;
    assert_eq!(d.wayland.inserted(), vec![paste("after the update ")]);
    assert!(d.status().await.warnings.is_empty());
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_older_server_prompts_to_update_the_server() {
    const PROMPT: &str = "update the server";
    let server = FakeServer::start(FakeServerConfig::default());
    // A refusal without the server's version, as older servers send it.
    server.speak_protocol(PROTOCOL_VERSION - 1, false);
    let d = daemon(&server, |_, _| {}).await;

    wait_until(|| error_shown(&d, PROMPT) == 1).await;
    let status = d.status().await;
    assert_eq!(status.warnings, vec![PROMPT.to_string()]);
    assert!(WaybarLine::new(&status).class.contains(&"warning"), "the status bar flags it");
    d.stop().await;
}
