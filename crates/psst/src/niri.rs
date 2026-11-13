//! The focused window's app id from niri's IPC socket (`$NIRI_SOCKET`), for choosing the paste
//! chord. One request per connection, as `niri msg` does, so nothing stays open.

use std::{path::Path, time::Duration};

use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use tracing::debug;

/// The default for how long the lookup may take before the paste goes ahead with the fallback
/// chord (`Timing::niri_lookup`).
pub const TIMEOUT: Duration = Duration::from_millis(150);

/// The focused window's app id; `None` without niri, without a focused window, or on any error.
pub async fn focused_app_id(socket: &Path, timeout: Duration) -> Option<String> {
    match tokio::time::timeout(timeout, query(socket)).await {
        Ok(Ok(app_id)) => app_id,
        Ok(Err(err)) => {
            debug!("niri: {err}");
            None
        }
        Err(_) => {
            debug!("niri didn't answer within {timeout:?}");
            None
        }
    }
}

async fn query(socket: &Path) -> std::io::Result<Option<String>> {
    let mut stream = UnixStream::connect(socket).await?;
    stream.write_all(b"\"FocusedWindow\"\n").await?;
    stream.shutdown().await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    let reply: Value = serde_json::from_str(&line).map_err(std::io::Error::other)?;
    // {"Ok":{"FocusedWindow":{"app_id":"…", …}}}, or null when nothing is focused.
    Ok(reply.pointer("/Ok/FocusedWindow/app_id").and_then(Value::as_str).map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use tokio::net::UnixListener;

    use super::*;

    async fn fake_niri(reply: &'static str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("niri.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut request = String::new();
            BufReader::new(read).read_line(&mut request).await.unwrap();
            assert_eq!(request, "\"FocusedWindow\"\n");
            write.write_all(reply.as_bytes()).await.unwrap();
        });
        (dir, path)
    }

    #[tokio::test]
    async fn reads_the_focused_app_id() {
        let (_dir, path) = fake_niri(
            "{\"Ok\":{\"FocusedWindow\":{\"id\":3,\"title\":\"~\",\"app_id\":\"com.mitchellh.ghostty\",\"pid\":1}}}\n",
        )
        .await;
        assert_eq!(focused_app_id(&path, TIMEOUT).await.as_deref(), Some("com.mitchellh.ghostty"));
    }

    #[tokio::test]
    async fn no_focused_window_or_no_niri_is_none() {
        let (_dir, path) = fake_niri("{\"Ok\":{\"FocusedWindow\":null}}\n").await;
        assert_eq!(focused_app_id(&path, TIMEOUT).await, None);
        assert_eq!(focused_app_id(Path::new("/nonexistent/niri.sock"), TIMEOUT).await, None);
    }
}
