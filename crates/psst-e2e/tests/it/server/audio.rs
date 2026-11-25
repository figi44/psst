use psst_client::{
    AudioCodec, AudioFrame, OpusSettings, SessionEvent, StartOpts,
    api::{ApiClient, Method},
};
use psst_proto::{
    FRAME_SAMPLES, HistoryId, SessionId,
    api::{HistoryRow, ModelState, Setting, routes},
    ws::Mode,
};
use psst_testkit::{TestServer, test_audio};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

use super::{browser::Browser, harness::*, pages::logged_in};

/// 50.5 frames: the last one is padded.
const MS: u32 = 1_010;

/// A GET over the server's socket, whose callers are admins: (status, content type, body).
async fn get(server: &TestServer, path: &str) -> (u16, String, Vec<u8>) {
    let mut stream = UnixStream::connect(server.socket()).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let split = response.windows(4).position(|w| w == b"\r\n\r\n").expect("a complete response");
    let head = String::from_utf8_lossy(&response[..split]).to_ascii_lowercase();
    let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).expect("a status line");
    let content_type = head.lines().find_map(|line| line.strip_prefix("content-type: ")).unwrap_or("").to_owned();
    (status, content_type, response[split + 4..].to_vec())
}

fn audio_path(id: HistoryId) -> String {
    routes::with(routes::HISTORY_AUDIO, "id", &id.to_string())
}

/// The recording, once the server has kept it: it does so after sending the final.
async fn recording(server: &TestServer, id: HistoryId) -> Vec<u8> {
    let kept = async {
        loop {
            match get(server, &audio_path(id)).await {
                (200, content_type, ogg) => {
                    assert_eq!(content_type, "audio/ogg");
                    return ogg;
                }
                (404, ..) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
                (status, ..) => panic!("GET {} answered {status}", audio_path(id)),
            }
        }
    };
    tokio::time::timeout(MESSAGE_TIMEOUT, kept).await.expect("the recording is kept")
}

/// How long an Ogg Opus file plays: its last granule, less the decoder's pre-skip, at 48 kHz.
fn plays_ms(ogg: &[u8]) -> u64 {
    assert!(ogg.starts_with(b"OggS"), "not Ogg");
    let head = ogg.windows(8).position(|w| w == b"OpusHead").expect("an Opus stream");
    let pre_skip = u64::from(u16::from_le_bytes([ogg[head + 10], ogg[head + 11]]));
    let last = ogg.windows(4).rposition(|w| w == b"OggS").unwrap();
    let granule = u64::from_le_bytes(ogg[last + 6..last + 14].try_into().unwrap());
    (granule - pre_skip) / 48
}

async fn history(server: &TestServer) -> Vec<HistoryRow> {
    ApiClient::new(server.endpoint(), None).call(Method::Get, routes::HISTORY, None::<&()>).await.unwrap()
}

/// Over the harness's client, which sends PCM.
async fn dictate_pcm(server: &TestServer, ms: u32) {
    let mut client = Client::connect(server).await;
    client.upload(SessionId::new_v4(), ms as usize * 16).await;
    client.final_msg().await;
}

/// Over psst's own client, which sends Opus.
async fn dictate_opus(server: &TestServer) {
    let mut client = psst_client::Client::connect(&server.endpoint(), None).await.unwrap();
    let mut session = client.start(StartOpts {
        post_processing: None,
        target: None,
        session_id: SessionId::new_v4(),
        profile: None,
        mode: Mode::Live,
        resume: false,
        eval: None,
        codec: AudioCodec::Opus(OpusSettings::default()),
    });
    let audio = test_audio(MS);
    for chunk in audio.chunks(FRAME_SAMPLES) {
        let mut frame: AudioFrame = [0; FRAME_SAMPLES];
        frame[..chunk.len()].copy_from_slice(chunk);
        session.frames().try_send(frame).unwrap();
    }
    session.stop(audio.len() as u64).await.unwrap();
    loop {
        match tokio::time::timeout(MESSAGE_TIMEOUT, session.next_event()).await.expect("an event in time") {
            SessionEvent::Ready { .. } | SessionEvent::Progress { .. } => {}
            SessionEvent::Final(_) => return,
            other => panic!("expected a final, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_dictation_plays_back_as_long_as_it_was_sent_as_pcm_or_opus() {
    let server = default_server();
    wait_model(&server, ModelState::Loaded).await;
    // Whole frames: the end is decoded only if the encoder's lookahead was flushed.
    dictate_pcm(&server, 1_000).await;
    dictate_opus(&server).await;
    let [opus, pcm] = history(&server).await.try_into().unwrap();
    assert_eq!(plays_ms(&recording(&server, pcm.id).await), 1_000);
    assert_eq!(plays_ms(&recording(&server, opus.id).await), u64::from(MS));
}

#[tokio::test]
async fn with_audio_days_at_zero_recordings_go_and_history_stays() {
    let server = default_server();
    dictate_pcm(&server, MS).await;
    let id = history(&server).await[0].id;
    recording(&server, id).await;

    let api = ApiClient::new(server.endpoint(), None);
    let path = routes::with(routes::SETTING, "key", "audio_days");
    let _: Setting = api.call(Method::Put, &path, Some(&json!({ "value": "0" }))).await.unwrap();
    assert_eq!(get(&server, &audio_path(id)).await.0, 404);

    dictate_pcm(&server, MS).await;
    let rows = history(&server).await;
    assert_eq!(rows.len(), 2, "the history stays");
    assert_eq!(get(&server, &audio_path(rows[0].id)).await.0, 404, "a new dictation's recording isn't kept");
}

#[tokio::test]
async fn the_history_page_plays_a_kept_recording_to_logged_in_browsers_only() {
    let server = server(script(vec![]), |cfg| cfg.tcp = true);
    dictate_pcm(&server, MS).await;
    let id = history(&server).await[0].id;
    recording(&server, id).await;
    let play = format!("/history/{id}/audio");

    let page = logged_in(&server).await.get("/history").await;
    assert!(page.body.contains(&format!("href=\"{play}\"")), "{}", page.body);
    assert!(page.body.contains("with recordings for 30 days"), "{}", page.body);
    assert_ne!(Browser::new(server.tcp_addr().unwrap()).get(&play).await.status, 200);
}
