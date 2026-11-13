//! Reading the recordings the daemon writes: 16 kHz mono 16-bit WAV, whose length comes from
//! the file size rather than from a header that may never have been finalised.

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};

use psst_proto::SAMPLE_RATE;

/// Where a WAV file's samples are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavInfo {
    /// Byte offset of the first sample.
    pub data_offset: u64,
    /// Samples in the file, from its size.
    pub samples: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum WavError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("not a 16 kHz mono 16-bit PCM WAV file: {0}")]
    Format(String),
}

/// Reads a WAV header and counts the samples after it from the file size.
pub fn read_wav_info(path: &Path) -> Result<WavInfo, WavError> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let info = parse_header(&mut file, len)?;
    Ok(info)
}

pub(crate) fn parse_header(file: &mut (impl Read + Seek), len: u64) -> Result<WavInfo, WavError> {
    let format = |msg: &str| WavError::Format(msg.into());
    let mut riff = [0u8; 12];
    file.read_exact(&mut riff).map_err(|_| format("too short"))?;
    if &riff[0..4] != b"RIFF" || &riff[8..12] != b"WAVE" {
        return Err(format("no RIFF/WAVE header"));
    }
    let mut pos = 12u64;
    let mut fmt_ok = false;
    loop {
        let mut chunk = [0u8; 8];
        file.read_exact(&mut chunk).map_err(|_| format("no data chunk"))?;
        let size = u32::from_le_bytes(chunk[4..8].try_into().expect("4 bytes")) as u64;
        pos += 8;
        match &chunk[0..4] {
            b"fmt " => {
                let mut fmt = vec![0u8; size.min(64) as usize];
                file.read_exact(&mut fmt).map_err(|_| format("short fmt chunk"))?;
                if fmt.len() < 16 {
                    return Err(format("short fmt chunk"));
                }
                let tag = u16::from_le_bytes([fmt[0], fmt[1]]);
                let channels = u16::from_le_bytes([fmt[2], fmt[3]]);
                let rate = u32::from_le_bytes(fmt[4..8].try_into().expect("4 bytes"));
                let bits = u16::from_le_bytes([fmt[14], fmt[15]]);
                if !matches!(tag, 1 | 0xfffe) || channels != 1 || rate != SAMPLE_RATE || bits != 16 {
                    return Err(WavError::Format(format!("{channels} channels, {rate} Hz, {bits} bits")));
                }
                fmt_ok = true;
                pos += size + (size & 1);
                file.seek(SeekFrom::Start(pos))?;
            }
            b"data" => {
                if !fmt_ok {
                    return Err(format("data before fmt"));
                }
                let samples = len.saturating_sub(pos) / 2;
                return Ok(WavInfo { data_offset: pos, samples });
            }
            _ => {
                pos += size + (size & 1);
                file.seek(SeekFrom::Start(pos))?;
            }
        }
    }
}
