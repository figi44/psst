use std::{io::SeekFrom, path::PathBuf};

use psst_proto::{FRAME_BYTES, FRAME_SAMPLES, SessionId};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, BufReader},
    sync::{mpsc, watch},
};

use crate::{
    AudioFrame,
    connection::{Cmd, Outgoing},
    wav,
};

pub(crate) async fn run(
    id: SessionId,
    path: PathBuf,
    frames: mpsc::Sender<Outgoing>,
    mut ready: watch::Receiver<Option<bool>>,
    cmd: mpsc::UnboundedSender<Cmd>,
) {
    // `cmd` keeps the connection open until the audio is queued, even if every handle is dropped.
    let need_audio = match ready.wait_for(Option::is_some).await {
        Ok(need_audio) => need_audio.expect("waited for Some"),
        Err(_) => return,
    };
    if !need_audio {
        return;
    }
    if let Err(message) = stream(&path, &frames).await {
        let _ = cmd.send(Cmd::Fail(id, message));
    }
}

/// `Ok` also when the session stopped taking audio (it was cancelled or ended), since there's
/// nothing left to report then.
async fn stream(path: &PathBuf, frames: &mpsc::Sender<Outgoing>) -> Result<(), String> {
    let fail = |err: &dyn std::fmt::Display| format!("cannot read {}: {err}", path.display());
    let mut file = File::open(path).await.map_err(|e| fail(&e))?;
    let len = file.metadata().await.map_err(|e| fail(&e))?.len();
    let mut header = vec![0u8; 4096.min(len as usize)];
    file.read_exact(&mut header).await.map_err(|e| fail(&e))?;
    let info = wav::parse_header(&mut std::io::Cursor::new(&header), len).map_err(|e| fail(&e))?;
    file.seek(SeekFrom::Start(info.data_offset)).await.map_err(|e| fail(&e))?;
    let mut file = BufReader::with_capacity(64 * 1024, file);

    let mut remaining = info.samples;
    let mut buf = vec![0u8; FRAME_BYTES];
    while remaining > 0 {
        let take = remaining.min(FRAME_SAMPLES as u64) as usize;
        file.read_exact(&mut buf[..take * 2]).await.map_err(|e| fail(&e))?;
        let mut frame: AudioFrame = [0; FRAME_SAMPLES];
        for (sample, bytes) in frame.iter_mut().zip(buf[..take * 2].as_chunks::<2>().0) {
            *sample = i16::from_le_bytes(*bytes);
        }
        if frames.send(Outgoing::Frame(Box::new(frame))).await.is_err() {
            return Ok(());
        }
        remaining -= take as u64;
    }
    let _ = frames.send(Outgoing::Stop(info.samples)).await;
    Ok(())
}
