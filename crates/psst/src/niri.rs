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
    /// niri's, unique while the window lives.
    pub id: Option<u64>,
    pub app_id: String,
    pub title: Option<String>,
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

/// While a layer surface has the keyboard, psst's own menu included, niri reports no focused
/// window: this asks again, as the menu goes, until `timeout` runs out.
pub async fn focused_window_after_layers(socket: &Path, timeout: Duration) -> Option<Window> {
    let settled = async {
        loop {
            match query(socket).await {
                Ok(None) => tokio::time::sleep(Duration::from_millis(50)).await,
                Ok(window) => return window,
                Err(err) => {
                    debug!("niri: {err}");
                    return None;
                }
            }
        }
    };
    match tokio::time::timeout(timeout, settled).await {
        Ok(window) => window,
        Err(_) => {
            debug!("niri showed no focused window within {timeout:?}");
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
    if let Some(err) = reply.get("Err") {
        return Err(std::io::Error::other(format!("niri refused: {err}")));
    }
    let Some(window) = reply.pointer("/Ok/FocusedWindow") else { return Ok(None) };
    let text = |key: &str| window.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    let id = window.get("id").and_then(Value::as_u64);
    Ok(text("app_id").map(|app_id| Window { id, app_id, title: text("title") }))
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
        let window = Window { id: Some(3), app_id: "com.mitchellh.ghostty".into(), title: Some("~".into()) };
        assert_eq!(focused_window(&path, TIMEOUT).await, Some(window));
    }

    #[tokio::test]
    async fn a_window_without_a_title_still_has_its_app() {
        let (_dir, path) =
            fake_niri("{\"Ok\":{\"FocusedWindow\":{\"id\":3,\"title\":null,\"app_id\":\"Slack\"}}}\n").await;
        assert_eq!(focused_window(&path, TIMEOUT).await.map(|w| w.app_id).as_deref(), Some("Slack"));
    }

    #[tokio::test]
    async fn no_focused_window_or_no_niri_is_none() {
        let (_dir, path) = fake_niri("{\"Ok\":{\"FocusedWindow\":null}}\n").await;
        assert_eq!(focused_window(&path, TIMEOUT).await, None);
        assert_eq!(focused_window(Path::new("/nonexistent/niri.sock"), TIMEOUT).await, None);
    }

    /// Answers every request alike, for as long as the test runs.
    async fn steady_niri(reply: &'static str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("niri.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (read, mut write) = stream.into_split();
                BufReader::new(read).read_line(&mut String::new()).await.unwrap();
                let _ = write.write_all(reply.as_bytes()).await;
            }
        });
        (dir, path)
    }

    #[tokio::test]
    async fn waiting_out_a_layer_gives_up_at_the_deadline() {
        let (_dir, path) = steady_niri("{\"Ok\":{\"FocusedWindow\":null}}\n").await;
        let started = std::time::Instant::now();
        assert_eq!(focused_window_after_layers(&path, Duration::from_millis(200)).await, None);
        let waited = started.elapsed();
        assert!((Duration::from_millis(200)..Duration::from_millis(600)).contains(&waited), "{waited:?}");
    }

    #[tokio::test]
    async fn a_refusal_from_niri_ends_the_wait_at_once() {
        let (_dir, path) = steady_niri("{\"Err\":\"no such request\"}\n").await;
        let started = std::time::Instant::now();
        assert_eq!(focused_window_after_layers(&path, Duration::from_secs(5)).await, None);
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
    }
}
