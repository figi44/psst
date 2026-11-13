use std::{path::Path, time::Duration};

use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use tracing::debug;

pub const TIMEOUT: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub app_id: String,
    pub title: Option<String>,
}

pub async fn focused_app_id(socket: &Path, timeout: Duration) -> Option<String> {
    focused_window(socket, timeout).await.map(|window| window.app_id)
}

pub async fn focused_window(socket: &Path, timeout: Duration) -> Option<Window> {
    match tokio::time::timeout(timeout, query(socket)).await {
        Ok(Ok(window)) => window,
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

async fn query(socket: &Path) -> std::io::Result<Option<Window>> {
    let mut stream = UnixStream::connect(socket).await?;
    stream.write_all(b"\"FocusedWindow\"\n").await?;
    stream.shutdown().await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    let reply: Value = serde_json::from_str(&line).map_err(std::io::Error::other)?;
    let Some(window) = reply.pointer("/Ok/FocusedWindow") else { return Ok(None) };
    let text = |key: &str| window.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    Ok(text("app_id").map(|app_id| Window { app_id, title: text("title") }))
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
    async fn reads_the_focused_window() {
        let (_dir, path) = fake_niri(
            "{\"Ok\":{\"FocusedWindow\":{\"id\":3,\"title\":\"~\",\"app_id\":\"com.mitchellh.ghostty\",\"pid\":1}}}\n",
        )
        .await;
        let window = Window { app_id: "com.mitchellh.ghostty".into(), title: Some("~".into()) };
        assert_eq!(focused_window(&path, TIMEOUT).await, Some(window));
    }

    #[tokio::test]
    async fn a_window_without_a_title_still_has_its_app() {
        let (_dir, path) =
            fake_niri("{\"Ok\":{\"FocusedWindow\":{\"id\":3,\"title\":null,\"app_id\":\"Slack\"}}}\n").await;
        assert_eq!(focused_app_id(&path, TIMEOUT).await.as_deref(), Some("Slack"));
    }

    #[tokio::test]
    async fn no_focused_window_or_no_niri_is_none() {
        let (_dir, path) = fake_niri("{\"Ok\":{\"FocusedWindow\":null}}\n").await;
        assert_eq!(focused_app_id(&path, TIMEOUT).await, None);
        assert_eq!(focused_app_id(Path::new("/nonexistent/niri.sock"), TIMEOUT).await, None);
    }
}
