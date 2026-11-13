use std::io::{Cursor, Read};

use psst_engine_proto::{
    FrameError, FrameLimits, RawFrame, read_frame, read_frame_async, write_frame, write_frame_async,
};

fn frame_bytes(header: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    write_frame(&mut out, header, payload).unwrap();
    out
}

fn prefix(header_len: u32, payload_len: u32) -> Vec<u8> {
    [header_len.to_le_bytes(), payload_len.to_le_bytes()].concat()
}

#[test]
fn frames_are_length_prefixed_little_endian() {
    let bytes = frame_bytes(br#"{"type":"shutdown"}"#, &[1, 2, 3, 4]);
    assert_eq!(&bytes[..8], &prefix(19, 4)[..]);
    assert_eq!(&bytes[8..27], br#"{"type":"shutdown"}"#);
    assert_eq!(&bytes[27..], &[1, 2, 3, 4]);
}

#[test]
fn reads_consecutive_frames_then_a_clean_end() {
    let mut stream = frame_bytes(b"{\"a\":1}", b"");
    stream.extend(frame_bytes(b"{\"b\":2}", &[9; 8]));
    let mut reader = Cursor::new(stream);
    let limits = FrameLimits::default();

    let first = read_frame(&mut reader, &limits).unwrap().unwrap();
    assert_eq!(first, RawFrame { header: b"{\"a\":1}".to_vec(), payload: vec![] });
    let second = read_frame(&mut reader, &limits).unwrap().unwrap();
    assert_eq!(second, RawFrame { header: b"{\"b\":2}".to_vec(), payload: vec![9; 8] });
    assert!(read_frame(&mut reader, &limits).unwrap().is_none());
}

#[test]
fn a_stream_ending_inside_a_frame_is_truncated() {
    let whole = frame_bytes(b"{\"a\":1}", &[0; 16]);
    for cut in 1..whole.len() {
        let result = read_frame(&mut Cursor::new(&whole[..cut]), &FrameLimits::default());
        assert!(matches!(result, Err(FrameError::Truncated)), "cut at {cut}: {result:?}");
    }
}

#[test]
fn rejects_a_header_over_one_mebibyte_before_reading_it() {
    let mut reader = Cursor::new(prefix((1 << 20) + 1, 0));
    let result = read_frame(&mut reader, &FrameLimits::for_max_audio_s(900));
    assert!(matches!(result, Err(FrameError::HeaderTooLarge { len: 1_048_577, max: 1_048_576 })), "{result:?}");
}

#[test]
fn payload_limit_is_max_audio_seconds_times_64000_plus_one_mebibyte() {
    let limits = FrameLimits::for_max_audio_s(10);
    let max = 10 * 64_000 + (1 << 20);
    assert_eq!(limits.max_payload, max);

    let at_limit = read_frame(&mut Cursor::new(prefix(0, max)).chain(Cursor::new(vec![0; max as usize])), &limits);
    assert_eq!(at_limit.unwrap().unwrap().payload.len(), max as usize);

    let over = read_frame(&mut Cursor::new(prefix(0, max + 1)), &limits);
    assert!(matches!(over, Err(FrameError::PayloadTooLarge { .. })), "{over:?}");
}

#[test]
fn limits_before_hello_allow_one_mebibyte_payloads() {
    assert_eq!(FrameLimits::default(), FrameLimits { max_header: 1 << 20, max_payload: 1 << 20 });
}

#[test]
fn writer_refuses_an_oversized_header() {
    let result = write_frame(&mut Vec::new(), &vec![b' '; (1 << 20) + 1], b"");
    assert!(matches!(result, Err(FrameError::HeaderTooLarge { .. })), "{result:?}");
}

#[tokio::test]
async fn async_reader_and_writer_match_the_blocking_ones() {
    let mut stream = Vec::new();
    write_frame_async(&mut stream, b"{\"a\":1}", &[7; 12]).await.unwrap();
    assert_eq!(stream, frame_bytes(b"{\"a\":1}", &[7; 12]));

    let mut reader = &stream[..];
    let frame = read_frame_async(&mut reader, &FrameLimits::default()).await.unwrap().unwrap();
    assert_eq!(frame, RawFrame { header: b"{\"a\":1}".to_vec(), payload: vec![7; 12] });
    assert!(read_frame_async(&mut reader, &FrameLimits::default()).await.unwrap().is_none());

    let mut truncated = &stream[..10];
    let result = read_frame_async(&mut truncated, &FrameLimits::default()).await;
    assert!(matches!(result, Err(FrameError::Truncated)), "{result:?}");

    let mut oversized = &prefix(0, (1 << 20) + 1)[..];
    let result = read_frame_async(&mut oversized, &FrameLimits::default()).await;
    assert!(matches!(result, Err(FrameError::PayloadTooLarge { .. })), "{result:?}");
}
