//! `psst-client` against a scripted server: connecting, live and upload sessions, and every way a
//! session can end (plan 5.1, 5.7).

use std::time::Duration;

use psst_client::{
    AudioFrame, Client, ClientError, FRAME_BUFFER, LostReason, SessionEvent, StartOpts, TrySendError, upload_file,
};
use psst_proto::{ErrorCode, FRAME_SAMPLES, Token, ws::Mode};
use psst_testkit::{
    fake_server::{FakeServer, FakeServerConfig, Outcome},
    test_audio, write_wav,
};
use tokio::time::timeout;
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(5);

fn opts(mode: Mode) -> StartOpts {
    StartOpts { session_id: Uuid::new_v4(), profile: None, mode, resume: false, eval: None }
}

fn frames(audio: &[i16]) -> Vec<AudioFrame> {
    audio
        .chunks(FRAME_SAMPLES)
        .map(|chunk| {
            let mut frame = [0; FRAME_SAMPLES];
            frame[..chunk.len()].copy_from_slice(chunk);
            frame
        })
        .collect()
}

/// The events up to and including the terminal one.
async fn until_terminal(session: &mut psst_client::Session) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    loop {
        let event = timeout(WAIT, session.next_event()).await.expect("an event in time");
        let terminal = event.is_terminal();
        events.push(event);
        if terminal {
            return events;
        }
    }
}

fn final_text(event: &SessionEvent) -> &str {
    match event {
        SessionEvent::Final(fin) => &fin.r#final.text,
        other => panic!("expected a final, got {other:?}"),
    }
}

#[tokio::test]
async fn connects_over_the_socket_and_reports_the_welcome() {
    let server = FakeServer::start(FakeServerConfig { max_recording_s: 300, ..Default::default() });
    let client = Client::connect(&server.endpoint(), None).await.unwrap();
    assert_eq!(client.welcome().device, "local:test");
    assert_eq!(client.welcome().limits.max_recording_s, 300);
    assert!(!client.is_closed());
}

#[tokio::test]
async fn tcp_needs_the_device_token() {
    let server = FakeServer::start(FakeServerConfig { tcp: true, ..Default::default() });
    let ep = server.tcp_endpoint().unwrap();

    let client = Client::connect(&ep, Some(&Token::new("fake-token"))).await.unwrap();
    assert_eq!(client.welcome().device, "fake-device");

    let refused = Client::connect(&ep, Some(&Token::new("wrong"))).await.unwrap_err();
    assert!(matches!(refused, ClientError::Refused(ref err) if err.code == ErrorCode::Unauthorized), "{refused:?}");
}

#[tokio::test]
async fn connecting_to_nothing_is_a_connect_error() {
    let dir = tempfile::tempdir().unwrap();
    let ep = psst_proto::Endpoint::Unix(dir.path().join("none.sock"));
    assert!(matches!(Client::connect(&ep, None).await, Err(ClientError::Connect { .. })));
}

#[tokio::test]
async fn a_live_session_sends_frames_from_before_ready_first_and_in_order() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("Push the fix.".into()));
    let mut client = Client::connect(&server.endpoint(), None).await.unwrap();
    let opts = opts(Mode::Live);
    let id = opts.session_id;
    let audio = test_audio(1_010); // 50.5 frames: the last one is padded
    let mut session = client.start(opts);
    let sender = session.frames();
    let frames = frames(&audio);
    // Half of the audio is queued before `ready` can have arrived.
    for frame in &frames[..25] {
        sender.try_send(*frame).unwrap();
    }
    assert!(matches!(session.next_event().await, SessionEvent::Ready { need_audio: true, resumed: false, .. }));
    for frame in &frames[25..] {
        sender.try_send(*frame).unwrap();
    }
    session.stop(audio.len() as u64).await.unwrap();

    let events = until_terminal(&mut session).await;
    assert_eq!(final_text(events.last().unwrap()), "Push the fix.");
    let seen = server.session(id).unwrap();
    assert_eq!(&seen.audio[..audio.len()], &audio[..]);
    assert_eq!(seen.audio.len(), 51 * FRAME_SAMPLES);
    assert_eq!(seen.stop_samples, Some(audio.len() as u64));
    // After the terminal event the session keeps saying how it ended.
    assert_eq!(final_text(&session.next_event().await), "Push the fix.");
}

#[tokio::test]
async fn progress_arrives_while_the_server_works() {
    let server = FakeServer::start(FakeServerConfig {
        progress_interval: Duration::from_millis(100),
        final_delay: Duration::from_millis(450),
        ..Default::default()
    });
    let mut client = Client::connect(&server.endpoint(), None).await.unwrap();
    let mut session = client.start(opts(Mode::Live));
    session.stop(0).await.unwrap();
    let events = until_terminal(&mut session).await;
    assert!(events.iter().filter(|e| matches!(e, SessionEvent::Progress { .. })).count() >= 2, "{events:?}");
    assert!(matches!(events.last(), Some(SessionEvent::Final(_))));
}

#[tokio::test]
async fn an_error_before_ready_is_a_rejection_and_after_ready_an_error() {
    let server = FakeServer::start(FakeServerConfig::default());
    let mut client = Client::connect(&server.endpoint(), None).await.unwrap();

    server.reject_next(ErrorCode::Busy);
    let mut rejected = client.start(opts(Mode::Live));
    assert!(
        matches!(until_terminal(&mut rejected).await.as_slice(), [SessionEvent::Rejected(err)] if err.code == ErrorCode::Busy)
    );

    server.push_outcome(Outcome::Error(ErrorCode::EngineFailed));
    let mut failed = client.start(opts(Mode::Live));
    failed.stop(0).await.unwrap();
    let events = until_terminal(&mut failed).await;
    assert!(matches!(events.first(), Some(SessionEvent::Ready { .. })));
    assert!(
        matches!(events.last(), Some(SessionEvent::Error(err)) if err.code == ErrorCode::EngineFailed && err.retryable)
    );
}

#[tokio::test]
async fn stop_with_the_wrong_sample_count_is_bad_audio() {
    let server = FakeServer::start(FakeServerConfig::default());
    let mut client = Client::connect(&server.endpoint(), None).await.unwrap();
    let mut session = client.start(opts(Mode::Live));
    session.stop(16_000).await.unwrap();
    let events = until_terminal(&mut session).await;
    assert!(matches!(events.last(), Some(SessionEvent::Error(err)) if err.code == ErrorCode::BadAudio));
}

#[tokio::test]
async fn cancel_ends_a_session_on_the_server() {
    let server = FakeServer::start(FakeServerConfig::default());
    let mut client = Client::connect(&server.endpoint(), None).await.unwrap();
    let opts = opts(Mode::Live);
    let id = opts.session_id;
    let mut session = client.start(opts);
    session.frames().try_send([1; FRAME_SAMPLES]).unwrap();
    assert!(matches!(session.next_event().await, SessionEvent::Ready { .. }));
    session.cancel().await.unwrap();
    assert!(matches!(until_terminal(&mut session).await.last(), Some(SessionEvent::Cancelled)));
    assert!(server.session(id).unwrap().cancelled);
    assert_eq!(session.frames().try_send([0; FRAME_SAMPLES]), Err(TrySendError::Closed));
}

#[tokio::test]
async fn a_second_start_waits_for_the_first_session_and_can_be_cancelled_meanwhile() {
    let server = FakeServer::start(FakeServerConfig { final_delay: Duration::from_millis(200), ..Default::default() });
    server.push_outcome(Outcome::Final("one".into()));
    server.push_outcome(Outcome::Final("three".into()));
    let mut client = Client::connect(&server.endpoint(), None).await.unwrap();

    let mut first = client.start(opts(Mode::Live));
    first.stop(0).await.unwrap();
    let second_opts = opts(Mode::Live);
    let second_id = second_opts.session_id;
    let mut second = client.start(second_opts);
    let mut third = client.start(opts(Mode::Live));
    // The second never reached the server, so cancelling it is local.
    second.cancel().await.unwrap();
    assert!(matches!(until_terminal(&mut second).await.as_slice(), [SessionEvent::Cancelled]));
    third.stop(0).await.unwrap();

    assert_eq!(final_text(until_terminal(&mut first).await.last().unwrap()), "one");
    assert_eq!(final_text(until_terminal(&mut third).await.last().unwrap()), "three");
    assert!(server.session(second_id).is_none());
}

#[tokio::test]
async fn the_frame_buffer_holds_thirty_seconds_before_ready() {
    let server = FakeServer::start(FakeServerConfig { final_delay: Duration::from_millis(300), ..Default::default() });
    let mut client = Client::connect(&server.endpoint(), None).await.unwrap();
    // The first session holds the connection, so the second gets no `ready` yet.
    let mut first = client.start(opts(Mode::Live));
    first.stop(0).await.unwrap();
    let second_opts = opts(Mode::Live);
    let second_id = second_opts.session_id;
    let mut second = client.start(second_opts);
    let sender = second.frames();
    let audio = test_audio(30_000);
    for frame in frames(&audio) {
        sender.try_send(frame).unwrap();
    }
    assert_eq!(frames(&audio).len(), FRAME_BUFFER);
    assert_eq!(sender.try_send([0; FRAME_SAMPLES]), Err(TrySendError::Full));

    until_terminal(&mut first).await;
    assert!(matches!(second.next_event().await, SessionEvent::Ready { .. }));
    second.stop(audio.len() as u64).await.unwrap();
    assert!(matches!(until_terminal(&mut second).await.last(), Some(SessionEvent::Final(_))));
    assert_eq!(server.session(second_id).unwrap().audio, audio);
}

#[tokio::test]
async fn progress_silence_loses_the_session_and_ends_the_connection() {
    let server = FakeServer::start(FakeServerConfig::default());
    let mut client = Client::connect(&server.endpoint(), None).await.unwrap();
    let mut session = client.start(opts(Mode::Live));
    assert!(matches!(session.next_event().await, SessionEvent::Ready { .. }));
    server.set_stalled(true);
    let started = std::time::Instant::now();
    let events = until_terminal(&mut session).await;
    assert_eq!(events.last(), Some(&SessionEvent::Lost(LostReason::ProgressTimeout)));
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    timeout(WAIT, client.closed()).await.unwrap();
    assert!(client.is_closed());
    // A start on a closed connection is lost at once.
    let mut late = client.start(opts(Mode::Live));
    assert!(matches!(late.next_event().await, SessionEvent::Lost(LostReason::ConnectionClosed { .. })));
}

#[tokio::test]
async fn a_closed_connection_loses_the_session() {
    let server = FakeServer::start(FakeServerConfig::default());
    let mut client = Client::connect(&server.endpoint(), None).await.unwrap();
    let mut session = client.start(opts(Mode::Live));
    assert!(matches!(session.next_event().await, SessionEvent::Ready { .. }));
    server.disconnect_all();
    let events = until_terminal(&mut session).await;
    assert!(matches!(events.last(), Some(SessionEvent::Lost(LostReason::ConnectionClosed { .. }))), "{events:?}");
    timeout(WAIT, client.closed()).await.unwrap();
}

#[tokio::test]
async fn check_alive_pings_only_after_ten_quiet_seconds() {
    let server = FakeServer::start(FakeServerConfig::default());
    let mut client = Client::connect(&server.endpoint(), None).await.unwrap();

    // Heard from just now: no ping, so even a stalled server passes.
    server.set_stalled(true);
    client.check_alive(Duration::from_millis(300)).await.unwrap();

    // Quiet for over 10 s: the ping goes unanswered.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(11)).await;
    tokio::time::resume();
    assert!(matches!(client.check_alive(Duration::from_millis(300)).await, Err(ClientError::Timeout)));

    // Answered once the server reads again.
    server.set_stalled(false);
    client.check_alive(Duration::from_millis(700)).await.unwrap();

    client.close();
    timeout(WAIT, client.closed()).await.unwrap();
    assert!(matches!(client.check_alive(Duration::from_millis(300)).await, Err(ClientError::Closed)));
}

#[tokio::test]
async fn upload_file_streams_the_whole_recording_even_with_an_unfinished_header() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("uploaded".into()));
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("rec.wav");
    let audio = test_audio(7_003);
    write_wav(&wav, &audio);
    // The daemon's header may still say 0 bytes of data.
    let mut bytes = std::fs::read(&wav).unwrap();
    bytes[40..44].copy_from_slice(&0u32.to_le_bytes());
    std::fs::write(&wav, bytes).unwrap();

    let mut client = Client::connect(&server.endpoint(), None).await.unwrap();
    let opts = StartOpts { resume: true, ..opts(Mode::Live) };
    let id = opts.session_id;
    let mut session = upload_file(&mut client, opts, &wav);
    let events = until_terminal(&mut session).await;
    assert_eq!(final_text(events.last().unwrap()), "uploaded");
    let seen = server.session(id).unwrap();
    assert_eq!(seen.starts[0].mode, Mode::Upload);
    assert!(seen.starts[0].resume);
    assert_eq!(seen.stop_samples, Some(audio.len() as u64));
    assert_eq!(&seen.audio[..audio.len()], &audio[..]);
}

#[tokio::test]
async fn resuming_a_finished_session_sends_no_audio() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("first time".into()));
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("rec.wav");
    write_wav(&wav, &test_audio(2_000));
    let mut client = Client::connect(&server.endpoint(), None).await.unwrap();
    let opts = StartOpts { resume: true, ..opts(Mode::Upload) };
    let id = opts.session_id;
    let mut first = upload_file(&mut client, opts.clone(), &wav);
    assert_eq!(final_text(until_terminal(&mut first).await.last().unwrap()), "first time");

    let mut again = upload_file(&mut client, opts, &wav);
    let events = until_terminal(&mut again).await;
    assert!(matches!(events[0], SessionEvent::Ready { resumed: true, need_audio: false, .. }));
    assert_eq!(final_text(events.last().unwrap()), "first time");
    assert_eq!(server.session(id).unwrap().starts.len(), 2);
}

#[tokio::test]
async fn a_dropped_upload_still_completes_on_the_server() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("kept".into()));
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("rec.wav");
    write_wav(&wav, &test_audio(3_000));
    let mut client = Client::connect(&server.endpoint(), None).await.unwrap();
    let opts = StartOpts { resume: true, ..opts(Mode::Upload) };
    let id = opts.session_id;
    drop(upload_file(&mut client, opts, &wav));
    drop(client);
    let seen = move |server: &FakeServer| server.session(id).is_some_and(|s| s.stop_samples == Some(48_000));
    assert!(tokio::task::spawn_blocking(move || server.wait_until(WAIT, seen)).await.unwrap());
}

#[tokio::test]
async fn an_upload_can_be_cancelled_before_its_audio_is_sent() {
    let server = FakeServer::start(FakeServerConfig::default());
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("rec.wav");
    write_wav(&wav, &test_audio(60_000));
    let mut client = Client::connect(&server.endpoint(), None).await.unwrap();
    let opts = StartOpts { resume: true, ..opts(Mode::Upload) };
    let id = opts.session_id;
    let mut session = upload_file(&mut client, opts, &wav);
    session.cancel().await.unwrap();
    assert!(matches!(until_terminal(&mut session).await.last(), Some(SessionEvent::Cancelled)));
    let seen = server.session(id).unwrap();
    assert!(seen.cancelled);
    assert_eq!(seen.stop_samples, None);
}

#[tokio::test]
async fn an_unreadable_upload_ends_in_an_error_and_is_cancelled_on_the_server() {
    let server = FakeServer::start(FakeServerConfig::default());
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("missing.wav");
    let mut client = Client::connect(&server.endpoint(), None).await.unwrap();
    let opts = StartOpts { resume: true, ..opts(Mode::Upload) };
    let id = opts.session_id;
    let mut session = upload_file(&mut client, opts, &wav);
    let events = until_terminal(&mut session).await;
    assert!(matches!(events.last(), Some(SessionEvent::Error(err)) if err.code == ErrorCode::Internal), "{events:?}");
    let cancelled = move |server: &FakeServer| server.session(id).is_some_and(|s| s.cancelled);
    assert!(tokio::task::spawn_blocking(move || server.wait_until(WAIT, cancelled)).await.unwrap());
}

#[tokio::test]
async fn report_reaches_the_server() {
    let server = FakeServer::start(FakeServerConfig::default());
    let mut client = Client::connect(&server.endpoint(), None).await.unwrap();
    let opts = opts(Mode::Live);
    let id = opts.session_id;
    let mut session = client.start(opts);
    session.stop(0).await.unwrap();
    let events = until_terminal(&mut session).await;
    let SessionEvent::Final(fin) = events.last().unwrap() else { panic!("{events:?}") };
    let timings = psst_proto::ClientTimings { stop_to_final_ms: Some(12), ..Default::default() };
    client.report(id, fin.history_id, timings);
    let reported =
        move |server: &FakeServer| server.reports().iter().any(|r| r.session_id == id && r.timings == timings);
    assert!(tokio::task::spawn_blocking(move || server.wait_until(WAIT, reported)).await.unwrap());
}
