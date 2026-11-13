use psst_engine_proto::fake::Script;
use psst_testkit::{
    TestServer,
    fake_tailscale::{FakeTailscale, TAILNET},
};
use serde_json::json;

use super::{access::pair, browser::*, harness::*, pages::logged_in};

fn tcp_server() -> TestServer {
    server(Script::default(), |cfg| cfg.tcp = true)
}

fn pair_text(server: &TestServer, extra: &[&str]) -> (String, String) {
    let out = admin(server, &[&["pair", "--name", "phone"], extra].concat());
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let code = text.lines().find_map(|line| line.strip_prefix("Pairing code: ")).expect("a code").to_owned();
    (text, code)
}

#[test]
fn pair_links_a_phone_to_the_servers_tailnet_name() {
    let tailscale = FakeTailscale::start();
    let server = server(Script::default(), |cfg| {
        cfg.tcp = true;
        cfg.server.tailscale_socket = tailscale.socket().into();
    });
    let address = format!("http://server.{TAILNET}:{}", server.tcp_addr().unwrap().port());

    let pairing = admin_json(&server, &["pair", "--name", "phone"]);
    let (text, code) = pair_text(&server, &[]);

    assert_eq!(pairing["server"], address.as_str());
    assert!(text.contains(&format!("\n{address}/pair?code={code}\n")), "{text}");
    assert!(text.contains(&format!("psst pair {address} {code}")), "{text}");
}

#[test]
fn without_tailscale_pair_links_to_the_host_name() {
    let server = tcp_server();
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap();

    let pairing = admin_json(&server, &["pair", "--name", "phone"]);

    assert_eq!(pairing["server"], format!("http://{}:{}", host.trim(), server.tcp_addr().unwrap().port()).as_str());
}

#[test]
fn an_address_given_to_pair_replaces_the_guess() {
    let server = tcp_server();

    let (text, code) = pair_text(&server, &["--server", "http://192.0.2.7:7341"]);
    let socket = admin(&server, &["pair", "--name", "phone", "--server", "/run/psst.sock"]);

    assert!(text.contains(&format!("\nhttp://192.0.2.7:7341/pair?code={code}\n")), "{text}");
    assert!(!socket.status.success());
    assert!(String::from_utf8_lossy(&socket.stderr).contains("http://host[:port]"));
}

#[tokio::test]
async fn the_pair_page_hands_the_app_the_code_and_the_address_it_was_opened_by() {
    let server = tcp_server();
    let addr = server.tcp_addr().unwrap();
    let code = admin_json(&server, &["pair", "--name", "phone"])["code"].as_str().unwrap().to_owned();
    let path = format!("/pair?code={code}");

    let page = request(addr, "GET", &path, &[("Host", "box.example:7341".into())], None).await;
    let port_80 = request(addr, "GET", &path, &[("Host", "box.example".into())], None).await;
    let without_code = request(addr, "GET", "/pair", &[], None).await;

    assert_eq!(page.status, 200, "{}", page.body);
    assert_eq!(
        open_psst(&page),
        format!(
            "intent://pair?server=http%3A%2F%2Fbox.example%3A7341&code={code}#Intent;scheme=psst;\
             package=io.github.figi44.psst;S.browser_fallback_url=https%3A%2F%2Fgithub.com%2Ffigi44%2Fpsst%2Freleases;end"
        )
    );
    // Browsers leave out port 80; the app's default port is psst's.
    assert!(open_psst(&port_80).starts_with("intent://pair?server=http%3A%2F%2Fbox.example%3A80&"));
    assert!(without_code.body.contains("psst-server pair"), "{}", without_code.body);
    // Only the app redeems the code: the page didn't use it up.
    let (status, body) =
        http(addr, "POST", "/api/pair/redeem", None, Some(json!({"code": code, "name": "phone"}))).await;
    assert_eq!(status, 200, "{body}");
}

/// The button's link, as a browser reads it.
fn open_psst(page: &Response) -> String {
    let (_, rest) = page.body.split_once(r#"id="open" href=""#).unwrap_or_else(|| panic!("no button: {}", page.body));
    let (href, _) = rest.split_once('"').unwrap();
    href.replace("&#x2f;", "/").replace("&amp;", "&")
}

#[tokio::test]
async fn the_devices_page_shows_a_qr_code_that_pairs_a_phone() {
    let server = tcp_server();
    let addr = server.tcp_addr().unwrap();
    let browser = logged_in(&server).await;
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap();

    let devices = browser.get("/devices").await;
    assert!(devices.body.contains(r#"action="/devices/pair""#), "{}", devices.body);
    let page = browser.post("/devices/pair", &[("name", "pixel")]).await;

    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains(r#"<svg class="qr""#), "{}", page.body);
    // The browser came by loopback, which no phone can use.
    let code = linked_code(&page, &format!("http://{}:{}", host.trim(), addr.port()));
    let text = page.body.replace("&#x2f;", "/");
    assert!(text.contains(&format!("psst pair http://{}:{} {code}", host.trim(), addr.port())), "{text}");
    let (status, body) =
        http(addr, "POST", "/api/pair/redeem", None, Some(json!({"code": code, "name": "Pixel 9"}))).await;
    assert_eq!(status, 200, "{body}");
    assert!(browser.get("/devices").await.body.contains(">pixel<"), "named as the form said");
}

#[tokio::test]
async fn the_qr_code_links_to_the_address_a_browser_on_another_machine_used() {
    let server = tcp_server();
    let browser = logged_in(&server).await;
    let headers = [("Host", "box.example:7341".to_owned()), ("Origin", "http://box.example:7341".to_owned())];

    let page = browser.post_with("/devices/pair", &headers, &[("name", "phone")]).await;

    assert_eq!(page.status, 200, "{}", page.body);
    linked_code(&page, "http://box.example:7341");
}

#[tokio::test]
async fn the_qr_code_links_to_the_servers_tailnet_name() {
    let tailscale = FakeTailscale::start();
    let server = server(Script::default(), |cfg| {
        cfg.tcp = true;
        cfg.server.tailscale_socket = tailscale.socket().into();
    });
    let browser = logged_in(&server).await;
    let headers = [("Host", "box.example:7341".to_owned()), ("Origin", "http://box.example:7341".to_owned())];

    let page = browser.post_with("/devices/pair", &headers, &[("name", "phone")]).await;

    linked_code(&page, &format!("http://server.{TAILNET}:{}", server.tcp_addr().unwrap().port()));
}

#[tokio::test]
async fn only_this_machines_users_pair_from_the_web_ui() {
    let server = tcp_server();
    let addr = server.tcp_addr().unwrap();
    let phone = pair(&server).await;
    let mut through_phone = Browser::new(addr);
    let tcp = psst_proto::Endpoint::Tcp { host: addr.ip().to_string(), port: addr.port() };
    through_phone.log_in(&login_link(tcp, Some(phone.token.clone())).await).await;
    let local = logged_in(&server).await;

    let devices = through_phone.get("/devices").await;
    let refused = through_phone.post("/devices/pair", &[("name", "spare")]).await;
    let cross_site = local.post_with("/devices/pair", &[("Origin", "http://evil.example".into())], &[]).await;

    assert!(!devices.body.contains("/devices/pair"), "{}", devices.body);
    assert_eq!(refused.status, 403, "{}", refused.body);
    assert!(refused.body.contains("psst-server pair"), "{}", refused.body);
    assert!(!refused.body.contains("?code="), "{}", refused.body);
    assert_eq!(cross_site.status, 403, "{}", cross_site.body);
    assert!(!cross_site.body.contains("?code="), "{}", cross_site.body);
}

/// The code in the page's link to `server`'s pair page.
fn linked_code(page: &Response, server: &str) -> String {
    let body = page.body.replace("&#x2f;", "/");
    let (_, rest) = body
        .split_once(&format!("href=\"{server}/pair?code="))
        .unwrap_or_else(|| panic!("no link to {server}: {body}"));
    rest.split_once('"').unwrap().0.to_owned()
}
