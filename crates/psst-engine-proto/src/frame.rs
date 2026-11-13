//! A frame over a limit, or a stream that ends inside a frame, means the stream is corrupt: the
//! server restarts the engine, and an engine exits.

use std::io::{self, Read, Write};

pub const PREFIX_LEN: usize = 8;

pub const MAX_HEADER_BYTES: u32 = 1 << 20;

pub const PAYLOAD_SLACK_BYTES: u64 = 1 << 20;

pub const AUDIO_BYTES_PER_SECOND: u64 = 64_000;

/// Enforced before allocating anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLimits {
    pub max_header: u32,
    pub max_payload: u32,
}

impl FrameLimits {
    pub fn for_max_audio_s(max_audio_s: u32) -> Self {
        let max_payload = u64::from(max_audio_s) * AUDIO_BYTES_PER_SECOND + PAYLOAD_SLACK_BYTES;
        Self { max_header: MAX_HEADER_BYTES, max_payload: max_payload.min(u64::from(u32::MAX)) as u32 }
    }
}

impl Default for FrameLimits {
    /// Before an engine's `hello` is known.
    fn default() -> Self {
        Self::for_max_audio_s(0)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawFrame {
    pub header: Vec<u8>,
    pub payload: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame header of {len} bytes is over the {max}-byte limit")]
    HeaderTooLarge { len: u64, max: u32 },
    #[error("frame payload of {len} bytes is over the {max}-byte limit")]
    PayloadTooLarge { len: u64, max: u32 },
    #[error("the stream ended inside a frame")]
    Truncated,
    #[error(transparent)]
    Io(#[from] io::Error),
}

fn parse_prefix(prefix: [u8; PREFIX_LEN], limits: &FrameLimits) -> Result<(usize, usize), FrameError> {
    let header_len = u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]);
    let payload_len = u32::from_le_bytes([prefix[4], prefix[5], prefix[6], prefix[7]]);
    if header_len > limits.max_header {
        return Err(FrameError::HeaderTooLarge { len: header_len.into(), max: limits.max_header });
    }
    if payload_len > limits.max_payload {
        return Err(FrameError::PayloadTooLarge { len: payload_len.into(), max: limits.max_payload });
    }
    Ok((header_len as usize, payload_len as usize))
}

fn prefix(header: &[u8], payload: &[u8]) -> Result<[u8; PREFIX_LEN], FrameError> {
    if header.len() > MAX_HEADER_BYTES as usize {
        return Err(FrameError::HeaderTooLarge { len: header.len() as u64, max: MAX_HEADER_BYTES });
    }
    let payload_len = u32::try_from(payload.len())
        .map_err(|_| FrameError::PayloadTooLarge { len: payload.len() as u64, max: u32::MAX })?;
    let mut prefix = [0; PREFIX_LEN];
    prefix[..4].copy_from_slice(&(header.len() as u32).to_le_bytes());
    prefix[4..].copy_from_slice(&payload_len.to_le_bytes());
    Ok(prefix)
}

fn truncated(err: io::Error) -> FrameError {
    match err.kind() {
        io::ErrorKind::UnexpectedEof => FrameError::Truncated,
        _ => FrameError::Io(err),
    }
}

pub fn read_frame<R: Read + ?Sized>(reader: &mut R, limits: &FrameLimits) -> Result<Option<RawFrame>, FrameError> {
    let mut prefix = [0; PREFIX_LEN];
    let mut filled = 0;
    while filled < PREFIX_LEN {
        match reader.read(&mut prefix[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(FrameError::Truncated),
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err.into()),
        }
    }
    let (header_len, payload_len) = parse_prefix(prefix, limits)?;
    let mut frame = RawFrame { header: vec![0; header_len], payload: vec![0; payload_len] };
    reader.read_exact(&mut frame.header).map_err(truncated)?;
    reader.read_exact(&mut frame.payload).map_err(truncated)?;
    Ok(Some(frame))
}

pub fn write_frame<W: Write + ?Sized>(writer: &mut W, header: &[u8], payload: &[u8]) -> Result<(), FrameError> {
    let prefix = prefix(header, payload)?;
    writer.write_all(&prefix)?;
    writer.write_all(header)?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

/// Not cancel-safe: run it in a task that owns the reader.
pub async fn read_frame_async<R: tokio::io::AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
    limits: &FrameLimits,
) -> Result<Option<RawFrame>, FrameError> {
    use tokio::io::AsyncReadExt;

    let mut prefix = [0; PREFIX_LEN];
    let mut filled = 0;
    while filled < PREFIX_LEN {
        match reader.read(&mut prefix[filled..]).await? {
            0 if filled == 0 => return Ok(None),
            0 => return Err(FrameError::Truncated),
            n => filled += n,
        }
    }
    let (header_len, payload_len) = parse_prefix(prefix, limits)?;
    let mut frame = RawFrame { header: vec![0; header_len], payload: vec![0; payload_len] };
    reader.read_exact(&mut frame.header).await.map_err(truncated)?;
    reader.read_exact(&mut frame.payload).await.map_err(truncated)?;
    Ok(Some(frame))
}

pub async fn write_frame_async<W: tokio::io::AsyncWrite + Unpin + ?Sized>(
    writer: &mut W,
    header: &[u8],
    payload: &[u8],
) -> Result<(), FrameError> {
    use tokio::io::AsyncWriteExt;

    let prefix = prefix(header, payload)?;
    writer.write_all(&prefix).await?;
    writer.write_all(header).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}
