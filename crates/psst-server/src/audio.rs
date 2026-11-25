//! Dictations' audio, kept for `audio_days` as Ogg Opus: browsers and ffmpeg play it as it is.

use anyhow::{Result, anyhow};
use axum::{
    http::header,
    response::{IntoResponse, Response},
};
use opusic_c::{Application, Bitrate, Channels, Encoder, InbandFec, SampleRate};
use psst_proto::FRAME_SAMPLES;

/// What the client sent. Its Opus packets are kept as they came, so the audio kept is exactly the
/// audio transcribed; PCM is encoded as the client would have.
pub enum Recorded {
    Opus(Vec<Vec<u8>>),
    Pcm(Vec<u8>),
}

/// The client's default, which its evals found transcribes as well as PCM.
const BITRATE: u32 = 16_000;
const MAX_PACKET: usize = 1275;
/// Ogg Opus counts time at 48 kHz whatever the input rate.
const TO_48K: u64 = 3;
const PACKETS_PER_PAGE: usize = 50;

fn encoder() -> Result<Encoder> {
    let fail = |what: &str, err: opusic_c::ErrorCode| anyhow!("cannot {what}: {}", err.message());
    let mut encoder = Encoder::new(Channels::Mono, SampleRate::Hz16000, Application::Voip)
        .map_err(|err| fail("create an Opus encoder", err))?;
    encoder.set_bitrate(Bitrate::Value(BITRATE)).map_err(|err| fail("set the Opus bitrate", err))?;
    encoder.set_dtx(false).map_err(|err| fail("turn Opus DTX off", err))?;
    encoder.set_inband_fec(InbandFec::Off).map_err(|err| fail("turn Opus FEC off", err))?;
    Ok(encoder)
}

/// `samples` long at 16 kHz: the packets' last frame is padded past it.
pub fn ogg(recorded: Recorded, samples: u64, serial: u32) -> Result<Vec<u8>> {
    let mut encoder = encoder()?;
    // The client's encoder has the same settings, so the same lookahead.
    let pre_skip = u64::from(encoder.get_look_ahead().map_err(|err| anyhow!("{}", err.message()))?) * TO_48K;
    let packets = match recorded {
        Recorded::Opus(packets) => packets,
        Recorded::Pcm(pcm) => {
            let samples: Vec<u16> = pcm.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect();
            let mut packets = Vec::with_capacity(samples.len().div_ceil(FRAME_SAMPLES));
            // A frame of silence past the end flushes the lookahead, so the audio's end is decoded.
            let silence = [0u16; FRAME_SAMPLES];
            for chunk in samples.chunks(FRAME_SAMPLES).chain([&silence[..]]) {
                let mut frame = [0u16; FRAME_SAMPLES];
                frame[..chunk.len()].copy_from_slice(chunk);
                let mut packet = [0u8; MAX_PACKET];
                let len = encoder
                    .encode_to_slice(&frame, &mut packet)
                    .map_err(|err| anyhow!("cannot encode a frame with Opus: {}", err.message()))?;
                packets.push(packet[..len].to_vec());
            }
            packets
        }
    };

    let mut out = Vec::new();
    let mut page = Page { serial, sequence: 0 };
    let mut head = b"OpusHead".to_vec();
    head.push(1);
    head.push(1);
    head.extend_from_slice(&u16::try_from(pre_skip)?.to_le_bytes());
    head.extend_from_slice(&16_000u32.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes());
    head.push(0);
    page.write(&mut out, &[&head], 0, BOS);
    let vendor = b"psst";
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&u32::try_from(vendor.len())?.to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes());
    page.write(&mut out, &[&tags], 0, 0);

    // Decoding gives the lookahead first, then the audio: the last granule ends it at `samples`,
    // but can't claim more than the packets hold.
    let decoded = packets.len() as u64 * FRAME_SAMPLES as u64 * TO_48K;
    let end = (pre_skip + samples * TO_48K).min(decoded);
    let pages = packets.chunks(PACKETS_PER_PAGE).collect::<Vec<_>>();
    for (i, chunk) in pages.iter().enumerate() {
        let last = i + 1 == pages.len();
        let granule = if last { end } else { ((i + 1) * PACKETS_PER_PAGE * FRAME_SAMPLES) as u64 * TO_48K };
        let packets: Vec<&[u8]> = chunk.iter().map(Vec::as_slice).collect();
        page.write(&mut out, &packets, granule, if last { EOS } else { 0 });
    }
    Ok(out)
}

pub fn response(ogg: Vec<u8>) -> Response {
    ([(header::CONTENT_TYPE, "audio/ogg"), (header::CACHE_CONTROL, "private, max-age=86400")], ogg).into_response()
}

const BOS: u8 = 0x02;
const EOS: u8 = 0x04;

struct Page {
    serial: u32,
    sequence: u32,
}

impl Page {
    /// Every packet whole on this page: at most 50 packets of at most 1275 bytes fit its 255
    /// segments.
    fn write(&mut self, out: &mut Vec<u8>, packets: &[&[u8]], granule: u64, flags: u8) {
        let start = out.len();
        out.extend_from_slice(b"OggS");
        out.push(0);
        out.push(flags);
        out.extend_from_slice(&granule.to_le_bytes());
        out.extend_from_slice(&self.serial.to_le_bytes());
        out.extend_from_slice(&self.sequence.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        let lacing: Vec<u8> = packets
            .iter()
            .flat_map(|packet| {
                let full = packet.len() / 255;
                std::iter::repeat_n(255u8, full).chain([(packet.len() % 255) as u8])
            })
            .collect();
        out.push(u8::try_from(lacing.len()).expect("a page's packets fit 255 segments"));
        out.extend_from_slice(&lacing);
        for packet in packets {
            out.extend_from_slice(packet);
        }
        let crc = crc(&out[start..]);
        out[start + 22..start + 26].copy_from_slice(&crc.to_le_bytes());
        self.sequence += 1;
    }
}

/// Ogg's CRC-32: polynomial 0x04c11db7, unreflected, starting from 0.
fn crc(bytes: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &byte in bytes {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04c1_1db7 } else { crc << 1 };
        }
    }
    crc
}
