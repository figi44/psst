//! `psst ui` as built (`psst-e2e-daemon ui`): a login link from the server `client.toml` names,
//! opened with `xdg-open` or printed, over the socket and over TCP with the paired token.

use std::{ffi::OsStr, fs, os::unix::fs::PermissionsExt};

use psst_engine_proto::fake::Script;
use psst_proto::api::LoginLink;

use crate::{browser::Browser, harness::*};

/// What a browser makes of the link: it logs in once.
async fn logs_in(server: &psst_testkit::TestServer, url: &str) {
    let link = LoginLink { url: url.trim().to_owned(), expires_at: jiff::Timestamp::now() };
    let mut browser = Browser::new(server.tcp_addr().unwrap());
    assert_eq!(browser.log_in(&link).await.status, 303, "{url}");
    assert_eq!(browser.get("/dictionary").await.status, 200);
}

#[tokio::test]
async fn psst_ui_opens_a_login_link_with_xdg_open() {
    let server = server(Script::default(), |cfg| cfg.tcp = true);
    let home = Home::new(server.endpoint());
    let bin = home.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let opener = bin.join("xdg-open");
    fs::write(&opener, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$HOME/opened\"\n").unwrap();
    fs::set_permissions(&opener, fs::Permissions::from_mode(0o755)).unwrap();

    let out = home.psst_with(&["ui"], &[("PATH", bin.as_os_str())]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("Opened psst's web UI in your browser."));
    let opened = fs::read_to_string(home.path().join("opened")).expect("xdg-open ran");
    let port = server.tcp_addr().unwrap().port();
    assert!(opened.starts_with(&format!("http://127.0.0.1:{port}/login?token=")), "{opened}");
    logs_in(&server, &opened).await;

    // Without xdg-open, the link is still there to open by hand.
    let out = home.psst_with(&["ui"], &[("PATH", OsStr::new("/nonexistent"))]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cannot run xdg-open") && err.contains("/login?token="), "{err}");
}

#[tokio::test]
async fn psst_ui_prints_the_link_for_another_machines_browser() {
    let server = server(Script::default(), |cfg| cfg.tcp = true);
    let endpoint = server.tcp_endpoint().unwrap();
    let home = Home::new(endpoint.clone());
    let code = admin_json(&server, &["pair", "--name", "thinkpad"])["code"].as_str().unwrap().to_owned();
    home.ok(&["pair", &endpoint.to_string(), &code]);

    let url = home.ok(&["ui", "--print"]);
    assert!(url.starts_with(&format!("{endpoint}/login?token=")), "where the laptop reached the server: {url}");
    logs_in(&server, &url).await;
}
