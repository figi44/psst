//! Sessions on `/v1/client` (plan 5.1): the normal flow, resume, cancel, disconnects and limits.

use std::time::{Duration, Instant};

use psst_config::TermSpec;
use psst_engine_proto::fake::{Script, TranscribeStep};
use psst_proto::{
    ClientTimings, ErrorCode, SessionId,
    api::{ModelState, Stats},
    ws::{AudioFormat, ClientMsg, Mode, ProgressState, ReadyModelState, ReadyMsg, ReportMsg, ServerMsg, StartMsg},
};
use serde_json::json;

use super::harness::*;

fn new_id() -> SessionId {
    SessionId::new_v4()
}

#[tokio::test]
async fn a_live_session_streams_audio_and_ends_with_its_final() {
    let server = server(Script::default(), |cfg| {
        cfg.profiles.profiles.get_mut("default").unwrap().prompt_style = "Hello there.".into();
        cfg.dictionary.terms = vec![TermSpec::new("Hyprland"), TermSpec { pin: true, ..TermSpec::new("psst") }];
    });
    wait_model(&server, ModelState::Loaded).await;
    let mut client = Client::connect(&server).await;
    assert!(client.welcome().device.starts_with("local:"), "{:?}", client.welcome());
    assert_eq!(client.welcome().limits.max_recording_s, 600);

    let id = new_id();
    client.start(id, Mode::Live, false).await;
    let ready = client.ready().await;
    assert_eq!(
        ready,
        ReadyMsg { session_id: id, model_state: ReadyModelState::Loaded, resumed: false, need_audio: true }
    );
    client.audio(SECOND / 2).await;
    let progress = client.progress_until(|p| p.received_ms >= 500).await;
    assert_eq!((progress.session_id, progress.state), (id, ProgressState::Recording));
    client.audio(SECOND / 2 - 100).await;
    client.stop(id, 15_900).await;
    let fin = client.final_msg().await;

    assert_eq!(fin.session_id, id);
    assert_eq!(fin.r#final.raw_text, "fake transcript");
    assert_eq!((fin.timings.audio_ms, fin.timings.attempts), (993, 1));
    assert!(fin.trace.is_none(), "trace comes only with eval_mode");
    let requests = transcribes(&server);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].samples, 15_900, "padding is trimmed");
    assert_eq!(requests[0].header["language"], "en");
    assert_eq!(requests[0].header["bias"], json!({"style": "Hello there.", "terms": ["psst", "Hyprland"]}));

    // The client's report lands in history, and stats cover both sides.
    let timings = ClientTimings { stop_to_final_ms: Some(fin.timings.stop_to_final_ms + 7), ..Default::default() };
    client.send(ClientMsg::Report(ReportMsg { session_id: id, history_id: fin.history_id, timings })).await;
    let deadline = Instant::now() + MESSAGE_TIMEOUT;
    let row = loop {
        let history = admin_json(&server, &["history"]);
        let row = history.as_array().unwrap().iter().find(|row| row["id"] == fin.history_id).unwrap().clone();
        if !row["client_timings"].is_null() || Instant::now() > deadline {
            break row;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(row["session_id"], id.to_string());
    assert_eq!(row["final"]["raw_text"], "fake transcript");
    assert_eq!(row["revision"], "fake-revision");
    assert_eq!(row["client_timings"]["stop_to_final_ms"], fin.timings.stop_to_final_ms + 7);
    let stats: Stats = serde_json::from_value(admin_json(&server, &["stats"])).unwrap();
    assert_eq!(stats.sessions, 1);
    assert_eq!(stats.timings["network_estimate_ms"].p50, 7);
    assert_eq!(stats.timings["server.audio_ms"].p95, 993);

    let devices = admin_json(&server, &["devices", "list"]);
    assert_eq!(devices[0]["id"], client.welcome().device);
    assert_eq!(devices[0]["kind"], "local");
}

#[tokio::test]
async fn a_finished_session_is_resumed_from_memory_and_after_a_restart_from_history() {
    let mut server = default_server();
    let mut client = Client::connect(&server).await;
    let id = new_id();
    client.upload(id, SECOND).await;
    let fin = client.final_msg().await;

    client.start(id, Mode::Upload, false).await;
    let err = client.error().await;
    assert_eq!((err.session_id, err.code), (Some(id), ErrorCode::BadRequest), "reusing an id needs resume");

    let mut other = Client::connect(&server).await;
    other.start(id, Mode::Upload, true).await;
    let ready = other.ready().await;
    assert!(ready.resumed && !ready.need_audio, "{ready:?}");
    assert_eq!(other.final_msg().await, fin);

    server.restart();
    let mut third = Client::connect(&server).await;
    third.start(id, Mode::Upload, true).await;
    let ready = third.ready().await;
    assert!(ready.resumed && !ready.need_audio, "{ready:?}");
    let again = third.final_msg().await;
    assert_eq!((again.history_id, &again.r#final, again.timings), (fin.history_id, &fin.r#final, fin.timings));
    assert_eq!(transcribes(&server).len(), 1, "a finished session is never transcribed again");
}

#[tokio::test]
async fn a_disconnect_does_not_cancel_and_a_resume_attaches_to_the_running_request() {
    let server = server(script(vec![result("slow words", 1500)]), |_| {});
    wait_model(&server, ModelState::Loaded).await;
    let mut client = Client::connect(&server).await;
    let id = new_id();
    client.upload(id, SECOND).await;
    client.progress_until(|p| p.state == ProgressState::Transcribing).await;
    drop(client);

    let mut other = Client::connect(&server).await;
    other.start(id, Mode::Upload, true).await;
    let ready = other.ready().await;
    assert!(ready.resumed && !ready.need_audio, "{ready:?}");
    let (outcome, progress) = other.outcome().await;
    let ServerMsg::Final(fin) = outcome else { panic!("expected final, got {outcome:?}") };
    assert_eq!(fin.r#final.raw_text, "slow words");
    assert!(progress.iter().all(|p| p.state == ProgressState::Transcribing && p.received_ms == 1000), "{progress:?}");
    assert_eq!(transcribes(&server).len(), 1, "a session is never transcribed twice");
}

#[tokio::test]
async fn resuming_on_another_connection_closes_the_one_that_held_the_session() {
    let server = server(script(vec![result("moved", 1500)]), |_| {});
    wait_model(&server, ModelState::Loaded).await;
    let mut first = Client::connect(&server).await;
    let id = new_id();
    first.upload(id, SECOND).await;
    first.progress_until(|p| p.state == ProgressState::Transcribing).await;

    let mut second = Client::connect(&server).await;
    second.start(id, Mode::Upload, true).await;
    assert!(second.ready().await.resumed);
    assert!(first.closed().await, "the connection that lost the session is closed");
    assert_eq!(second.final_msg().await.r#final.raw_text, "moved");
}

#[tokio::test]
async fn resuming_an_unknown_session_asks_for_its_audio() {
    let server = default_server();
    let mut client = Client::connect(&server).await;
    let id = new_id();
    client.start(id, Mode::Upload, true).await;
    let ready = client.ready().await;
    assert!(!ready.resumed && ready.need_audio, "{ready:?}");
    client.audio(SECOND).await;
    client.stop(id, SECOND as u64).await;
    assert_eq!(client.final_msg().await.session_id, id);
}

#[tokio::test]
async fn resuming_a_recording_session_discards_its_old_audio() {
    let server = default_server();
    let mut client = Client::connect(&server).await;
    let id = new_id();
    client.start(id, Mode::Live, false).await;
    client.ready().await;
    client.audio(SECOND).await;
    drop(client);

    let mut other = Client::connect(&server).await;
    other.start(id, Mode::Upload, true).await;
    let ready = other.ready().await;
    assert!(!ready.resumed && ready.need_audio, "{ready:?}");
    other.audio(SECOND / 2).await;
    other.stop(id, SECOND as u64 / 2).await;
    other.final_msg().await;
    let requests = transcribes(&server);
    assert_eq!(requests.iter().map(|r| r.samples).collect::<Vec<_>>(), [SECOND / 2]);
}

#[tokio::test]
async fn cancel_during_upload_ends_the_session_without_transcribing() {
    let server = default_server();
    let mut client = Client::connect(&server).await;
    let id = new_id();
    client.start(id, Mode::Upload, false).await;
    client.ready().await;
    client.audio(SECOND).await;
    client.cancel(id).await;
    assert!(matches!(client.outcome().await.0, ServerMsg::Cancelled(c) if c.session_id == id));
    client.cancel(id).await; // a late cancel is ignored

    // A cancelled session is removed, so resuming it asks for the audio again.
    client.start(id, Mode::Upload, true).await;
    assert!(client.ready().await.need_audio);
    assert!(transcribes(&server).is_empty());
    client.audio(SECOND / 4).await;
    client.stop(id, SECOND as u64 / 4).await;
    client.final_msg().await;
    assert_eq!(transcribes(&server).len(), 1);
}

#[tokio::test]
async fn cancel_during_transcription_answers_at_once_and_restarts_a_stuck_engine() {
    let server = server(script(vec![result("too slow", 20_000)]), |_| {});
    wait_model(&server, ModelState::Loaded).await;
    let mut client = Client::connect(&server).await;
    let id = new_id();
    client.upload(id, SECOND).await;
    client.progress_until(|p| p.state == ProgressState::Transcribing).await;
    let asked = Instant::now();
    client.cancel(id).await;
    assert!(matches!(client.outcome().await.0, ServerMsg::Cancelled(c) if c.session_id == id));
    assert!(asked.elapsed() < Duration::from_secs(1), "cancel is answered at once");

    // The next dictation runs on a restarted engine once the 2 s grace is over.
    let next = new_id();
    client.upload(next, SECOND).await;
    let fin = client.final_msg().await;
    assert_eq!((fin.session_id, fin.r#final.raw_text.as_str()), (next, "fake transcript"));
    assert!(asked.elapsed() >= Duration::from_secs(2), "the cancelled request had 2 s to finish");
    assert_eq!(requests_of(&server, "load").len(), 2, "the stuck engine was restarted");
    assert_eq!(client.try_recv(Duration::from_millis(300)).await, None, "the cancelled result is never sent");
}

#[tokio::test]
async fn a_recording_without_its_connection_is_dropped_after_10_s_without_audio() {
    let server = server(Script::default(), |cfg| cfg.server.limits.max_active_sessions = 1);
    let mut client = Client::connect(&server).await;
    client.start(new_id(), Mode::Live, false).await;
    client.ready().await;
    client.audio(320).await;
    drop(client);
    let dropped = Instant::now();

    let mut other = Client::connect(&server).await;
    other.start(new_id(), Mode::Live, false).await;
    let err = other.error().await;
    assert_eq!((err.code, err.retryable), (ErrorCode::Busy, true), "the orphaned recording still counts");
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(dropped.elapsed() < Duration::from_secs(20), "the orphaned recording was never dropped");
        other.start(new_id(), Mode::Live, false).await;
        match other.recv().await {
            ServerMsg::Ready(_) => break,
            ServerMsg::Error(err) if err.code == ErrorCode::Busy => {}
            msg => panic!("unexpected {msg:?}"),
        }
    }
    assert!(dropped.elapsed() >= Duration::from_secs(9), "dropped too early: {:?}", dropped.elapsed());
}

#[tokio::test]
async fn stop_with_a_sample_count_that_does_not_match_the_audio_is_bad_audio() {
    let server = default_server();
    let mut client = Client::connect(&server).await;
    for claimed in [900, 2000] {
        let id = new_id();
        client.start(id, Mode::Live, false).await;
        client.ready().await;
        client.audio(1000).await; // four frames: 1280 samples arrive
        client.stop(id, claimed).await;
        let err = client.error().await;
        assert_eq!(
            (err.session_id, err.code, err.retryable),
            (Some(id), ErrorCode::BadAudio, true),
            "claimed {claimed}"
        );
    }
    let id = new_id();
    client.start(id, Mode::Live, false).await;
    client.ready().await;
    client.audio(1000).await;
    client.stop(id, 1000).await;
    client.final_msg().await;
    assert_eq!(transcribes(&server).iter().map(|r| r.samples).collect::<Vec<_>>(), [1000]);
}

#[tokio::test]
async fn audio_more_than_5_s_past_max_recording_s_is_too_long() {
    let server = server(Script::default(), |cfg| cfg.server.limits.max_recording_s = 1);
    let mut client = Client::connect(&server).await;
    assert_eq!(client.welcome().limits.max_recording_s, 1);
    let id = new_id();
    client.start(id, Mode::Live, false).await;
    client.ready().await;
    client.audio(6 * SECOND).await;
    assert_eq!(client.try_recv(Duration::from_millis(200)).await, None, "6 s is within the 5 s grace");
    client.audio(320).await;
    let err = client.error().await;
    assert_eq!((err.session_id, err.code, err.retryable), (Some(id), ErrorCode::TooLong, false));
}

#[tokio::test]
async fn more_than_max_active_sessions_are_busy() {
    let server = server(Script::default(), |cfg| cfg.server.limits.max_active_sessions = 2);
    let mut clients = Vec::new();
    for _ in 0..2 {
        let mut client = Client::connect(&server).await;
        let id = new_id();
        client.start(id, Mode::Live, false).await;
        client.ready().await;
        clients.push((client, id));
    }
    let mut third = Client::connect(&server).await;
    third.start(new_id(), Mode::Live, false).await;
    let err = third.error().await;
    assert_eq!((err.code, err.retryable), (ErrorCode::Busy, true));

    let (first, id) = &mut clients[0];
    first.cancel(*id).await;
    assert!(matches!(first.outcome().await.0, ServerMsg::Cancelled(_)));
    third.start(new_id(), Mode::Live, false).await;
    third.ready().await;
}

#[tokio::test]
async fn a_connection_has_one_active_session_at_a_time() {
    let server = default_server();
    let mut client = Client::connect(&server).await;
    let (first, second) = (new_id(), new_id());
    client.start(first, Mode::Live, false).await;
    client.ready().await;
    client.start(second, Mode::Live, false).await;
    let err = client.error().await;
    assert_eq!((err.session_id, err.code), (Some(second), ErrorCode::BadRequest));
    client.audio(SECOND).await;
    client.stop(first, SECOND as u64).await;
    assert_eq!(client.final_msg().await.session_id, first);
}

#[tokio::test]
async fn malformed_messages_and_unknown_profiles_are_bad_requests() {
    let server = default_server();
    let mut client = Client::connect(&server).await;
    client.send_text(r#"{"type": "frobnicate"}"#).await;
    let err = client.error().await;
    assert_eq!((err.session_id, err.code), (None, ErrorCode::BadRequest));

    let id = new_id();
    client
        .start_with(StartMsg {
            session_id: id,
            profile: Some("klingon".into()),
            audio: AudioFormat::default(),
            mode: Mode::Live,
            resume: false,
            eval: None,
        })
        .await;
    let err = client.error().await;
    assert_eq!((err.session_id, err.code), (Some(id), ErrorCode::BadRequest));
    assert!(err.message.contains("klingon"), "{err:?}");

    client.upload(new_id(), SECOND).await;
    client.final_msg().await;
}

#[tokio::test]
async fn an_incompatible_protocol_version_is_refused() {
    let server = default_server();
    let err = Socket::unix(server.socket()).await.hello_with(99).await.err().expect("refused");
    assert_eq!((err.session_id, err.code), (None, ErrorCode::Protocol));
}

#[tokio::test]
async fn eval_needs_eval_mode() {
    let server = default_server();
    let mut client = Client::connect(&server).await;
    let id = new_id();
    client.start_eval(id, false).await;
    let err = client.error().await;
    assert_eq!((err.session_id, err.code), (Some(id), ErrorCode::BadRequest));
}

#[tokio::test]
async fn eval_mode_sends_the_trace_and_bias_false_sends_no_prompt() {
    let server = server(Script::default(), |cfg| {
        cfg.server.eval_mode = true;
        cfg.profiles.profiles.get_mut("default").unwrap().prompt_style = "Style.".into();
        cfg.dictionary.terms = vec![TermSpec::new("psst")];
    });
    let mut client = Client::connect(&server).await;
    let id = new_id();
    client.start_eval(id, false).await;
    client.ready().await;
    client.audio(SECOND).await;
    client.stop(id, SECOND as u64).await;
    let fin = client.final_msg().await;
    let trace = fin.trace.expect("eval_mode sends the trace");
    assert_eq!(trace.prompt, "");

    client.upload(new_id(), SECOND).await;
    assert_eq!(client.final_msg().await.trace.expect("a trace").prompt, "Style. psst.");
    let biases: Vec<_> = transcribes(&server).into_iter().map(|r| r.header["bias"].clone()).collect();
    assert_eq!(biases, [json!({"style": "", "terms": []}), json!({"style": "Style.", "terms": ["psst"]})]);
}

#[tokio::test]
async fn a_rejected_start_leaves_no_session_behind() {
    // An engine error that isn't retryable fails the session at once, and the id can be resumed.
    let step = TranscribeStep::Error {
        code: psst_engine_proto::EngineErrorCode::Internal,
        message: "broken".into(),
        retryable: false,
        delay_ms: 0,
    };
    let server = server(script(vec![step]), |_| {});
    let mut client = Client::connect(&server).await;
    let id = new_id();
    client.upload(id, SECOND).await;
    let err = client.error().await;
    assert_eq!((err.session_id, err.code), (Some(id), ErrorCode::Internal));
    client.start(id, Mode::Upload, true).await;
    assert!(client.ready().await.need_audio, "an errored session asks for its audio again");
    client.audio(SECOND).await;
    client.stop(id, SECOND as u64).await;
    client.final_msg().await;
}
