use opusic_c::{Application, Bitrate, Channels, InbandFec, SampleRate};
use psst_proto::{FRAME_BYTES, ws::Codec};

use crate::AudioFrame;

/// Opus's largest packet.
const MAX_PACKET: usize = 1275;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Pcm,
    Opus(OpusSettings),
}

/// The default, VOIP at 16 kbit/s, was chosen with `psst-eval audio --codec opus` on the 125 eval
/// clips (English and Czech); the other settings exist to measure against it. Against PCM nothing
/// passes the eval gate, not even lossless controls (PCM delayed 6.5 ms, or with ±1 LSB of noise):
/// Whisper changes outputs on any change to the audio. Against those controls Opus is within
/// noise, so the daemon sends it by default over TCP. 16 kbit/s, the lowest bitrate tried (16 to
/// 48), is the only one within the WER and term-recall tolerances in both languages.
///
/// It averages 33 bytes a frame: 1.9 KB/s with WebSocket framing against PCM's 32.4 KB/s, and about
/// 64 kbit/s against 306 for a live session through WireGuard. Encoding takes about 0.2 ms a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusSettings {
    /// Bits per second; libopus takes 500 to 512,000.
    pub bitrate: u32,
    pub application: OpusApplication,
}

impl Default for OpusSettings {
    fn default() -> Self {
        Self { bitrate: 16_000, application: OpusApplication::Voip }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpusApplication {
    Voip,
    Audio,
}

pub(crate) enum Encoder {
    Pcm,
    Opus(Box<opusic_c::Encoder>),
}

impl Encoder {
    pub fn new(codec: AudioCodec, accepted: &[Codec]) -> Result<Encoder, String> {
        let settings = match codec {
            AudioCodec::Opus(settings) if accepted.contains(&Codec::Opus) => settings,
            _ => return Ok(Encoder::Pcm),
        };
        let fail = |what: &str, err: opusic_c::ErrorCode| format!("cannot {what}: {}", err.message());
        let application = match settings.application {
            OpusApplication::Voip => Application::Voip,
            OpusApplication::Audio => Application::Audio,
        };
        let mut encoder = opusic_c::Encoder::new(Channels::Mono, SampleRate::Hz16000, application)
            .map_err(|err| fail("create an Opus encoder", err))?;
        encoder.set_bitrate(Bitrate::Value(settings.bitrate)).map_err(|err| fail("set the Opus bitrate", err))?;
        // The server counts samples, so every frame needs its packet; TCP loses none, so no FEC.
        encoder.set_dtx(false).map_err(|err| fail("turn Opus DTX off", err))?;
        encoder.set_inband_fec(InbandFec::Off).map_err(|err| fail("turn Opus FEC off", err))?;
        Ok(Encoder::Opus(Box::new(encoder)))
    }

    pub fn codec(&self) -> Codec {
        match self {
            Self::Pcm => Codec::PcmS16le,
            Self::Opus(_) => Codec::Opus,
        }
    }

    pub fn encode(&mut self, frame: &AudioFrame) -> Result<Vec<u8>, String> {
        match self {
            Self::Pcm => {
                let mut bytes = Vec::with_capacity(FRAME_BYTES);
                for sample in frame {
                    bytes.extend_from_slice(&sample.to_le_bytes());
                }
                Ok(bytes)
            }
            Self::Opus(encoder) => {
                // libopus takes 16-bit samples as `u16`.
                let samples = frame.map(|sample| sample as u16);
                let mut packet = [0u8; MAX_PACKET];
                let len = encoder
                    .encode_to_slice(&samples, &mut packet)
                    .map_err(|err| format!("cannot encode a frame with Opus: {}", err.message()))?;
                Ok(packet[..len].to_vec())
            }
        }
    }
}
