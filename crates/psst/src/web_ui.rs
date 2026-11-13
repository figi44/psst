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

/// `xdg-open` may stay running with the browser it started, so outlasting this counts as success.
const OPEN_WAIT: Duration = Duration::from_secs(2);

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

/// Own process group, so the browser it starts survives the terminal's Ctrl-C.
pub(crate) fn open(url: &str) -> anyhow::Result<()> {
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
