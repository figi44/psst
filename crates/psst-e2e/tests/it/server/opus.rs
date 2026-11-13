//! Audio as Opus on `/v1/client`: the server decodes each packet into the PCM it transcribes.

use psst_client::{AudioCodec, AudioFrame, OpusSettings, SessionEvent, StartOpts, upload_file};
use psst_engine_proto::fake::{HelloScript, Script};
use psst_proto::{
    Endpoint, ErrorCode, FRAME_SAMPLES, SessionId,
    api::ModelState,
    ws::{AudioFormat, Codec, FinalMsg, Mode, StartMsg},
};
use psst_testkit::{test_audio, write_wav};

use super::harness::*;

fn opus_opts(mode: Mode) -> StartOpts {
    StartOpts {
        session_id: SessionId::new_v4(),
        profile: None,
        mode,
        resume: false,
        eval: None,
        codec: AudioCodec::Opus(OpusSettings::default()),
    }
}

async fn final_of(session: &mut psst_client::Session) -> Box<FinalMsg> {
    loop {
        match tokio::time::timeout(MESSAGE_TIMEOUT, session.next_event()).await.expect("an event in time") {
            SessionEvent::Ready { .. } | SessionEvent::Progress { .. } => {}
            SessionEvent::Final(fin) => return fin,
            other => panic!("expected a final, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn live_and_uploaded_opus_reach_the_engine_as_every_sample() {
    let server = default_server();
    wait_model(&server, ModelState::Loaded).await;
    let endpoint = Endpoint::Unix(server.socket().to_owned());
    let mut client = psst_client::Client::connect(&endpoint, None).await.unwrap();
    let audio = test_audio(1_010); // 50.5 frames: the last one is padded

    let mut live = client.start(opus_opts(Mode::Live));
    assert_eq!(live.codec(), Codec::Opus);
    for chunk in audio.chunks(FRAME_SAMPLES) {
        let mut frame: AudioFrame = [0; FRAME_SAMPLES];
        frame[..chunk.len()].copy_from_slice(chunk);
        live.frames().try_send(frame).unwrap();
    }
    live.stop(audio.len() as u64).await.unwrap();
    let fin = final_of(&mut live).await;
    assert_eq!((fin.r#final.raw_text.as_str(), fin.timings.audio_ms), ("fake transcript", 1_010));

    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("rec.wav");
    write_wav(&wav, &audio);
    let mut upload = upload_file(&mut client, opus_opts(Mode::Upload), &wav);
    assert_eq!(upload.codec(), Codec::Opus);
    let fin = final_of(&mut upload).await;
    assert_eq!((fin.r#final.raw_text.as_str(), fin.timings.audio_ms), ("fake transcript", 1_010));

    let samples: Vec<usize> = transcribes(&server).iter().map(|request| request.samples).collect();
    assert_eq!(samples, [audio.len(), audio.len()]);
}

#[tokio::test]
async fn a_live_opus_session_is_streamed_to_the_engine_as_decoded_pcm() {
    let hello = HelloScript { stream: true, ..HelloScript::default() };
    let server = server(Script { hello, ..Script::default() }, |_| {});
    wait_model(&server, ModelState::Loaded).await;
    let endpoint = Endpoint::Unix(server.socket().to_owned());
    let mut client = psst_client::Client::connect(&endpoint, None).await.unwrap();
    let audio = test_audio(1_010); // 50.5 frames: the last one is padded

    let mut live = client.start(opus_opts(Mode::Live));
    for chunk in audio.chunks(FRAME_SAMPLES) {
        let mut frame: AudioFrame = [0; FRAME_SAMPLES];
        frame[..chunk.len()].copy_from_slice(chunk);
        live.frames().try_send(frame).unwrap();
    }
    live.stop(audio.len() as u64).await.unwrap();
    let fin = final_of(&mut live).await;
    assert_eq!(fin.timings.audio_ms, 1_010);

    let closed = requests_of(&server, "close");
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].samples, audio.len(), "the stream gets the decoded samples, without the padding");
    assert!(transcribes(&server).is_empty());
}

#[tokio::test]
async fn a_message_that_is_not_a_20_ms_opus_packet_is_bad_audio() {
    let server = default_server();
    let mut client = Client::connect(&server).await;
    assert_eq!(client.welcome().codecs, [Codec::PcmS16le, Codec::Opus]);

    // Empty; a code 3 packet without its frame count; a 40 ms packet; a 10 ms packet.
    for packet in [&[][..], &[0xff], &[0x10], &[0x00]] {
        let id = SessionId::new_v4();
        let audio = AudioFormat { codec: Codec::Opus, ..AudioFormat::default() };
        client
            .start_with(StartMsg { session_id: id, profile: None, audio, mode: Mode::Live, resume: false, eval: None })
            .await;
        client.ready().await;
        client.send_binary(packet).await;
        let err = client.error().await;
        assert_eq!((err.session_id, err.code), (Some(id), ErrorCode::BadAudio), "packet {packet:?}: {err:?}");
    }
    assert!(transcribes(&server).is_empty());
}
