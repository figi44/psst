//! How a session's frames go on the wire: as `pcm_s16le`, or as one Opus packet each
//! (`psst_proto::ws`).

use opusic_c::{Application, Bitrate, Channels, InbandFec, SampleRate};
use psst_proto::{FRAME_BYTES, ws::Codec};

use crate::AudioFrame;

/// The most a 20 ms Opus packet of one frame can take.
const MAX_PACKET: usize = 1275;

/// The codec a session asks for. Opus is used only when the server's `welcome` lists it; a
/// server that doesn't get PCM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    /// `pcm_s16le`: 640 bytes a frame.
    Pcm,
    Opus(OpusSettings),
}

/// The Opus encoder's settings. The default is VOIP at 16 kbit/s (VBR, no DTX, no FEC), chosen with
/// `psst-eval audio --codec opus` as below; the other settings exist to measure against it.
///
/// # How it was chosen
///
/// Each setting transcribed the 125 eval clips (89 English, 36 Czech, 27 must-pass) through the
/// real engine, and was compared per language with a PCM run of the same clips. The engine is
/// deterministic: a second PCM run changed nothing. Two lossless controls, run as PCM, show what
/// any change to the audio does: the clips delayed by 6.5 ms (exactly Opus's lookahead), and with
/// ±1 LSB of noise. Changes against PCM, in points:
///
/// | Audio | Output WER en / cs | Term recall en / cs | Outputs changed | Must-pass changed |
/// |---|---|---|---|---|
/// | PCM | 4.28 / 8.50 | 87.69 / 86.21 | | |
/// | PCM delayed 6.5 ms | −0.07 / +0.79 | −4.62 / +3.45 | 24 | 8 |
/// | PCM ±1 LSB noise | +0.07 / +0.59 | +1.54 / ±0.00 | 9 | 5 |
/// | **Opus VOIP 16 kbit/s** | −0.07 / −0.79 | ±0.00 / +3.45 | 29 | 11 |
/// | Opus VOIP 24 kbit/s | −0.42 / +0.20 | −1.54 / +3.45 | 29 | 11 |
/// | Opus VOIP 32 kbit/s | +0.21 / −0.79 | −1.54 / −3.45 | 24 | 10 |
/// | Opus VOIP 48 kbit/s | +0.07 / ±0.00 | −3.08 / +6.90 | 24 | 9 |
///
/// AUDIO at the same bitrates lands in the same range. Nothing passes the eval gate's tolerances
/// (WER up by at most 0.2 points, term recall down by at most 0.5, no must-pass output changed),
/// not even the lossless controls: one term is 1.54 recall points in English and 3.45 in Czech,
/// and Whisper changes long clips' outputs on any change to the audio. Judged against the lossless
/// controls instead, Opus is within noise, so the daemon sends it by default over TCP. VOIP at
/// 16 kbit/s is the lowest bitrate tried, and the only setting within the WER and recall
/// tolerances in both languages.
///
/// On the wire it averages 33 bytes a frame: 1.9 KB/s with WebSocket framing, against PCM's
/// 32.4 KB/s. A live session through WireGuard, one TCP segment per frame, takes about 64 kbit/s
/// against 306, mostly headers. Encoding takes about 0.2 ms a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusSettings {
    /// Target bitrate in bits per second; libopus takes 500 to 512,000.
    pub bitrate: u32,
    pub application: OpusApplication,
}

impl Default for OpusSettings {
    fn default() -> Self {
        Self { bitrate: 16_000, application: OpusApplication::Voip }
    }
}

/// libopus's coding mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpusApplication {
    /// Tuned for speech intelligibility.
    Voip,
    /// Tuned to stay close to the input.
    Audio,
}

/// One session's encoder.
pub(crate) enum Encoder {
    Pcm,
    Opus(Box<opusic_c::Encoder>),
}

impl Encoder {
    /// An encoder for `codec`, or for PCM when the server doesn't list Opus in `accepted`.
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

    /// What `start.audio.codec` says.
    pub fn codec(&self) -> Codec {
        match self {
            Self::Pcm => Codec::PcmS16le,
            Self::Opus(_) => Codec::Opus,
        }
    }

    /// One binary message for `frame`.
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
