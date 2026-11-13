//! Restarting the engine after the machine wakes up (plan 5.3, step 5): logind's
//! `PrepareForSleep(false)` on the system bus.

use futures_util::StreamExt;
use tokio::sync::watch;
use zbus::{MatchRule, MessageStream, message::Type};

use crate::engine::Supervisor;

/// Watches for resume until shutdown. Without a system bus, logs it and returns.
pub async fn watch_resume(engine: Supervisor, mut shutdown: watch::Receiver<bool>) {
    let mut stream = match subscribe().await {
        Ok(stream) => stream,
        Err(err) => {
            tracing::info!("not watching for resume from sleep: {err}");
            return;
        }
    };
    loop {
        tokio::select! {
            msg = stream.next() => match msg {
                Some(Ok(msg)) => {
                    if let Ok(false) = msg.body().deserialize::<bool>() {
                        engine.resume();
                    }
                }
                Some(Err(err)) => tracing::warn!("logind signal: {err}"),
                None => return,
            },
            _ = shutdown.wait_for(|stop| *stop) => return,
        }
    }
}

async fn subscribe() -> zbus::Result<MessageStream> {
    let conn = zbus::Connection::system().await?;
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender("org.freedesktop.login1")
        .interface("org.freedesktop.login1.Manager")
        .member("PrepareForSleep")
        .path("/org/freedesktop/login1")
        .build()?;
    MessageStream::for_match_rule(rule, &conn, None).await
}
