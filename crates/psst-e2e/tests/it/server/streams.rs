use std::time::Duration;

use psst_engine_proto::{
    EngineErrorCode,
    fake::{HelloScript, LoggedRequest, Script, TranscribeStep},
};
use psst_proto::{
    SessionId,
    api::ModelState,
    ws::{Mode, ServerMsg},
};
use serde_json::json;

use psst_testkit::TestServer;

use super::harness::*;

fn streaming(transcribe: Vec<TranscribeStep>) -> TestServer {
    let hello = HelloScript { stream: true, ..HelloScript::default() };
    server(Script { hello, transcribe, ..Script::default() }, |_| {})
}

async fn dictate(client: &mut Client) -> psst_proto::ws::FinalMsg {
    let id = SessionId::new_v4();
    client.start(id, Mode::Live, false).await;
    client.ready().await;
    client.audio(SECOND / 2).await;
    client.audio(SECOND / 2 - 100).await;
    client.stop(id, 15_900).await;
    client.final_msg().await
}

async fn eventually(server: &TestServer, kind: &str, count: usize) -> Vec<LoggedRequest> {
    let deadline = tokio::time::Instant::now() + MESSAGE_TIMEOUT;
    loop {
        let requests = requests_of(server, kind);
        if requests.len() >= count {
            return requests;
        }
        assert!(tokio::time::Instant::now() < deadline, "the engine got {} `{kind}`, not {count}", requests.len());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_live_session_is_streamed_and_closed_at_stop_but_an_upload_is_sent_whole() {
    let server = streaming(Vec::new());
    wait_model(&server, ModelState::Loaded).await;
    let mut client = Client::connect(&server).await;

    let fin = dictate(&mut client).await;
    assert_eq!((fin.r#final.raw_text.as_str(), fin.timings.attempts), ("fake transcript", 1));
    let opened = requests_of(&server, "open");
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].header["language"], "en");
    assert_eq!(opened[0].header["options"]["path"], "batched");
    let closed = requests_of(&server, "close");
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].header, json!({"type": "close", "id": opened[0].header["id"]}));
    assert_eq!(closed[0].samples, 15_900, "the stream gets the real samples, without the last frame's padding");
    assert!(transcribes(&server).is_empty());

    client.upload(SessionId::new_v4(), SECOND).await;
    client.final_msg().await;
    assert_eq!(requests_of(&server, "open").len(), 1);
    assert_eq!(transcribes(&server).len(), 1);
}

#[tokio::test]
async fn a_stream_lost_with_its_engine_is_sent_whole_at_stop() {
    let server = streaming(Vec::new());
    wait_model(&server, ModelState::Loaded).await;
    let mut client = Client::connect(&server).await;
    let id = SessionId::new_v4();
    client.start(id, Mode::Live, false).await;
    client.ready().await;
    client.audio(SECOND / 2).await;
    eventually(&server, "open", 1).await;

    assert_eq!(admin_json(&server, &["model", "unload"])["state"], "unloaded");
    admin_json(&server, &["model", "load"]);
    wait_model(&server, ModelState::Loaded).await;
    client.audio(SECOND / 2 - 100).await;
    client.stop(id, 15_900).await;
    let fin = client.final_msg().await;

    assert_eq!(fin.timings.attempts, 1);
    assert!(requests_of(&server, "close").is_empty());
    let whole = transcribes(&server);
    assert_eq!(whole.len(), 1);
    assert_eq!(whole[0].samples, 15_900);
}

#[tokio::test]
async fn a_stream_the_engine_cannot_close_is_sent_whole_as_the_next_attempt() {
    let refused = TranscribeStep::Error {
        code: EngineErrorCode::BadRequest,
        message: "no open stream".into(),
        retryable: false,
        delay_ms: 0,
    };
    let server = streaming(vec![refused]);
    wait_model(&server, ModelState::Loaded).await;
    let mut client = Client::connect(&server).await;

    let fin = dictate(&mut client).await;
    assert_eq!((fin.r#final.raw_text.as_str(), fin.timings.attempts), ("fake transcript", 2));
    assert_eq!(requests_of(&server, "close").len(), 1);
    let whole = transcribes(&server);
    assert_eq!(whole.len(), 1);
    assert_eq!(whole[0].samples, 15_900);
}

#[tokio::test]
async fn cancelling_or_losing_a_live_session_discards_its_stream() {
    let server = streaming(Vec::new());
    wait_model(&server, ModelState::Loaded).await;

    let mut client = Client::connect(&server).await;
    let id = SessionId::new_v4();
    client.start(id, Mode::Live, false).await;
    client.ready().await;
    client.audio(SECOND / 4).await;
    client.cancel(id).await;
    let (outcome, _) = client.outcome().await;
    assert!(matches!(outcome, ServerMsg::Cancelled(_)), "{outcome:?}");
    let discarded = eventually(&server, "discard", 1).await;
    assert_eq!(discarded[0].header["id"], requests_of(&server, "open")[0].header["id"]);

    let mut client = Client::connect(&server).await;
    client.start(SessionId::new_v4(), Mode::Live, false).await;
    client.ready().await;
    client.audio(SECOND / 4).await;
    drop(client);
    let discarded = eventually(&server, "discard", 2).await;
    assert_eq!(discarded[1].header["id"], requests_of(&server, "open")[1].header["id"]);
    assert!(requests_of(&server, "close").is_empty());
}
