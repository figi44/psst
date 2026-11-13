//! `psst ui`: asks the server for a one-time login link to its web UI and
//! opens it with `xdg-open`, or prints it with `--print`. The server is the one `client.toml`
//! names ([`crate::server_api`]); the link is valid for a minute and logs in once.

use std::{
    os::unix::process::CommandExt,
    process::{Command, ExitCode, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::anyhow;
use psst_client::api::Method;
use psst_proto::api::{LoginLink, routes};

use crate::server_api;

/// How long `xdg-open` may take to fail. It may stay running with the browser it started, which
/// is fine.
const OPEN_WAIT: Duration = Duration::from_secs(2);

/// Runs `psst ui`.
pub fn run(print: bool) -> anyhow::Result<ExitCode> {
    let api = server_api::connect()?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let link: LoginLink = runtime
        .block_on(api.call(Method::Post, routes::LOGIN_LINK, None::<&()>))
        .map_err(|err| server_api::describe(err, api.endpoint()))?;
    if print {
        println!("{}", link.url);
        return Ok(ExitCode::SUCCESS);
    }
    open(&link.url)?;
    println!("Opened psst's web UI in your browser. The link logs in once, until {}.", link.expires_at);
    Ok(ExitCode::SUCCESS)
}

/// Opens `url` with `xdg-open`, in its own process group so the browser it starts outlives this
/// command and its terminal's Ctrl-C.
fn open(url: &str) -> anyhow::Result<()> {
    let by_hand = || format!("open this link yourself within a minute:\n{url}");
    let mut child = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|err| anyhow!("cannot run xdg-open ({err}); {}", by_hand()))?;
    let deadline = Instant::now() + OPEN_WAIT;
    loop {
        match child.try_wait()? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(anyhow!("xdg-open failed ({status}); {}", by_hand())),
            None if Instant::now() >= deadline => return Ok(()),
            None => thread::sleep(Duration::from_millis(20)),
        }
    }
}
