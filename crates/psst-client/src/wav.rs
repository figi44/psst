use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

use psst_proto::SAMPLE_RATE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavInfo {
    pub data_offset: u64,
    pub samples: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum WavError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("not a 16 kHz mono 16-bit PCM WAV file: {0}")]
    Format(String),
}

pub fn read_wav_info(path: &Path) -> Result<WavInfo, WavError> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let info = parse_header(&mut file, len)?;
    Ok(info)
}

/// Counts the samples from the file size, not the header: the daemon's recordings may never have
/// had their header finalised.
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

/// A 16 kHz mono 16-bit WAV, written as it's recorded. Its header gets the length only at
/// `finish`: until then, [`read_wav_info`] counts the samples from the file size.
pub struct WavWriter {
    file: File,
    pub samples: u64,
}

impl WavWriter {
    pub fn create(path: &Path) -> io::Result<WavWriter> {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
        file.write_all(&header(0))?;
        Ok(WavWriter { file, samples: 0 })
    }

    pub fn write(&mut self, samples: &[i16]) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        self.file.write_all(&bytes)?;
        self.samples += samples.len() as u64;
        Ok(())
    }

    pub fn finish(&mut self) -> io::Result<()> {
        let data_len = u32::try_from(self.samples * 2).unwrap_or(u32::MAX);
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header(data_len))?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(())
    }
}

fn header(data_len: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&data_len.saturating_add(36).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&1u16.to_le_bytes());
    h[24..28].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    h[28..32].copy_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    h[32..34].copy_from_slice(&2u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}
