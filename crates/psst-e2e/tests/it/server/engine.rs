//! The engine supervisor: crashes, budgets, `oom`, failure, unload and the load
//! deadline.

use std::time::{Duration, Instant};

use psst_engine_proto::{
    EngineErrorCode,
    fake::{HelloScript, LoadScript, Script, TranscribeStep},
};
use psst_proto::{
    ErrorCode, SessionId,
    api::ModelState,
    ws::{Mode, ProgressState, ReadyModelState, ServerMsg},
};

use super::harness::*;

fn crash() -> TranscribeStep {
    TranscribeStep::Exit { code: 1, delay_ms: 0 }
}

fn oom() -> TranscribeStep {
    TranscribeStep::Error {
        code: EngineErrorCode::Oom,
        message: "CUDA out of memory".into(),
        retryable: true,
        delay_ms: 0,
    }
}

#[tokio::test]
async fn a_crashed_engine_is_restarted_and_the_request_tried_again() {
    let server = server(script(vec![crash()]), |_| {});
    let mut client = Client::connect(&server).await;
    client.upload(SessionId::new_v4(), SECOND).await;
    let (outcome, progress) = client.outcome().await;
    let ServerMsg::Final(fin) = outcome else { panic!("expected final, got {outcome:?}") };
    assert_eq!(fin.timings.attempts, 2);
    assert!(progress.iter().any(|p| p.state == ProgressState::Retrying), "{progress:?}");
    assert_eq!(requests_of(&server, "load").len(), 2);
    assert_eq!(wait_model(&server, ModelState::Loaded).await.error, None);
}

#[tokio::test]
async fn a_request_that_runs_out_of_its_budget_twice_fails_with_timeout() {
    let server = server(script(vec![TranscribeStep::Hang, TranscribeStep::Hang]), |_| {});
    wait_model(&server, ModelState::Loaded).await;
    let mut client = Client::connect(&server).await;
    let started = Instant::now();
    client.upload(SessionId::new_v4(), SECOND / 10).await;
    let err = client.error().await;
    assert_eq!((err.code, err.retryable), (ErrorCode::Timeout, true));
    // Two attempts of 5 s plus the 100 ms recording, and a restart in between.
    assert!(started.elapsed() >= Duration::from_millis(10_200), "{:?}", started.elapsed());
    assert_eq!(transcribes(&server).len(), 2);
    wait_model(&server, ModelState::Loaded).await;
}

/// An engine that replies `oom` has made sure it can run again, so the retry
/// goes to the same process.
#[tokio::test]
async fn out_of_memory_gets_one_more_attempt_in_the_same_engine_with_batch_size_one() {
    let server = server(script(vec![oom(), result("fits now", 0), oom(), oom(), oom(), oom()]), |_| {});
    let mut client = Client::connect(&server).await;
    client.upload(SessionId::new_v4(), SECOND).await;
    let fin = client.final_msg().await;
    assert_eq!((fin.r#final.raw_text.as_str(), fin.timings.attempts), ("fits now", 2));
    let batch_sizes: Vec<_> = transcribes(&server).iter().map(|r| r.header["options"]["batch_size"].clone()).collect();
    assert_eq!(batch_sizes, [8, 1], "the configured batch size, then 1");

    // Running out on both attempts is an `oom` error and never a crash: four in a row leave the
    // model loaded in the first engine.
    for _ in 0..2 {
        client.upload(SessionId::new_v4(), SECOND).await;
        let err = client.error().await;
        assert_eq!((err.code, err.retryable), (ErrorCode::Oom, true));
    }
    assert_eq!(model_status(&server).state, ModelState::Loaded);
    client.upload(SessionId::new_v4(), SECOND).await;
    client.final_msg().await;
    assert_eq!(requests_of(&server, "load").len(), 1, "the engine was never restarted");
}

#[tokio::test]
async fn three_crashes_in_a_row_mark_the_model_failed_until_it_is_loaded_again() {
    let server = server(script(vec![crash(), crash(), crash()]), |_| {});
    let mut client = Client::connect(&server).await;
    client.upload(SessionId::new_v4(), SECOND).await;
    assert_eq!(client.error().await.code, ErrorCode::EngineFailed, "two crashes use up both attempts");
    client.upload(SessionId::new_v4(), SECOND).await;
    assert_eq!(client.error().await.code, ErrorCode::EngineFailed);

    let status = model_status(&server);
    assert_eq!(status.state, ModelState::Failed);
    assert!(status.error.as_deref().unwrap_or_default().contains("3 times"), "{status:?}");
    let id = SessionId::new_v4();
    client.start(id, Mode::Upload, false).await;
    let err = client.error().await;
    assert_eq!((err.session_id, err.code), (Some(id), ErrorCode::ModelUnavailable));
    assert_eq!(err.detail.as_ref().unwrap()["reason"], "failed");

    assert_eq!(admin_json(&server, &["model", "load"])["state"], "loaded");
    client.upload(SessionId::new_v4(), SECOND).await;
    client.final_msg().await;
}

#[tokio::test]
async fn an_unload_is_sticky_until_model_load_even_across_server_restarts() {
    let mut server = default_server();
    wait_model(&server, ModelState::Loaded).await;
    assert_eq!(admin_json(&server, &["model", "unload"])["state"], "unloaded");
    let mut client = Client::connect(&server).await;
    let id = SessionId::new_v4();
    client.start(id, Mode::Live, false).await;
    let err = client.error().await;
    assert_eq!((err.session_id, err.code, err.retryable), (Some(id), ErrorCode::ModelUnloaded, false));

    server.restart();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(model_status(&server).state, ModelState::Unloaded);
    assert_eq!(requests_of(&server, "load").len(), 1, "the restarted server didn't load the model");

    assert_eq!(admin_json(&server, &["model", "load"])["state"], "loaded");
    let mut client = Client::connect(&server).await;
    client.upload(SessionId::new_v4(), SECOND).await;
    client.final_msg().await;

    server.restart();
    wait_model(&server, ModelState::Loaded).await;
}

#[tokio::test]
async fn unloading_fails_the_sessions_waiting_for_the_engine() {
    let server = server(script(vec![result("never", 10_000)]), |_| {});
    wait_model(&server, ModelState::Loaded).await;
    let mut client = Client::connect(&server).await;
    let id = SessionId::new_v4();
    client.upload(id, SECOND).await;
    client.progress_until(|p| p.state == ProgressState::Transcribing).await;
    let unloaded = Instant::now();
    assert_eq!(admin_json(&server, &["model", "unload"])["state"], "unloaded");
    let err = client.error().await;
    assert_eq!((err.session_id, err.code), (Some(id), ErrorCode::ModelUnloaded));
    assert!(unloaded.elapsed() < Duration::from_secs(3), "a running attempt is killed, not waited for");
}

#[tokio::test]
async fn a_session_waits_at_most_load_deadline_s_for_the_model() {
    let load = LoadScript { delay_ms: 5000, ..LoadScript::default() };
    let server = server(Script { load, ..Script::default() }, |cfg| cfg.server.load_deadline_s = 1);
    let mut client = Client::connect(&server).await;
    let id = SessionId::new_v4();
    client.start(id, Mode::Live, false).await;
    assert_eq!(client.ready().await.model_state, ReadyModelState::Loading);
    client.audio(SECOND).await;
    assert_eq!(client.progress_until(|_| true).await.state, ProgressState::Loading);
    let stopped = Instant::now();
    client.stop(id, SECOND as u64).await;
    let err = client.error().await;
    assert_eq!((err.code, err.retryable), (ErrorCode::Timeout, true));
    assert!(stopped.elapsed() < Duration::from_secs(3), "{:?}", stopped.elapsed());
}

#[tokio::test]
async fn an_engine_that_accepts_less_than_max_recording_s_fails_to_load() {
    let hello = HelloScript { max_audio_s: 300, ..HelloScript::default() };
    let server = server(Script { hello, ..Script::default() }, |_| {});
    let status = wait_model(&server, ModelState::Failed).await;
    assert!(status.error.as_deref().unwrap_or_default().contains("max_audio_s = 300"), "{status:?}");
    let mut client = Client::connect(&server).await;
    client.start(SessionId::new_v4(), Mode::Live, false).await;
    assert_eq!(client.error().await.code, ErrorCode::ModelUnavailable);
    assert!(requests_of(&server, "load").is_empty(), "the load is never sent");
}

#[tokio::test]
async fn a_load_error_marks_the_model_failed() {
    let load = LoadScript {
        outcome: psst_engine_proto::fake::LoadOutcome::Error {
            code: EngineErrorCode::LoadFailed,
            message: "no such model".into(),
        },
        ..LoadScript::default()
    };
    let server = server(Script { load, ..Script::default() }, |_| {});
    let status = wait_model(&server, ModelState::Failed).await;
    assert!(status.error.as_deref().unwrap_or_default().contains("no such model"), "{status:?}");
    let out = admin(&server, &["model", "load"]);
    assert!(!out.status.success(), "`model load` fails when loading fails");
    assert!(String::from_utf8_lossy(&out.stdout).contains("failed: loading the model failed: no such model"));
}
