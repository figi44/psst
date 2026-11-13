use futures_util::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, info};

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Manager {
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

pub async fn watch(resumed: mpsc::UnboundedSender<()>) {
    if let Err(err) = run(&resumed).await {
        debug!("not watching for resume: {err}");
    }
}

async fn run(resumed: &mpsc::UnboundedSender<()>) -> zbus::Result<()> {
    let connection = zbus::Connection::system().await?;
    let manager = ManagerProxy::new(&connection).await?;
    let mut signals = manager.receive_prepare_for_sleep().await?;
    while let Some(signal) = signals.next().await {
        if !signal.args()?.start {
            info!("resumed from suspend");
            if resumed.send(()).is_err() {
                break;
            }
        }
    }
    Ok(())
}
