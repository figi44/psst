use jiff::Timestamp;
use psst_client::api::{ApiCallError, ApiClient, Method};
use psst_engine_proto::fake::Script;
use psst_proto::{
    Endpoint,
    api::{LoginLink, routes},
};
use psst_testkit::TestServer;
use serde_json::json;

use super::{access::pair, browser::*, harness::*};

fn tcp_server() -> TestServer {
    server(Script::default(), |cfg| cfg.tcp = true)
}

async fn logged_in(server: &TestServer) -> Browser {
    let mut browser = Browser::new(server.tcp_addr().unwrap());
    let link = login_link(server.endpoint(), None).await;
    assert_eq!(browser.log_in(&link).await.status, 303);
    browser
}

#[tokio::test]
async fn a_login_link_logs_a_browser_in_once() {
    let server = tcp_server();
    let addr = server.tcp_addr().unwrap();
    let link = login_link(server.endpoint(), None).await;
    let prefix = format!("http://127.0.0.1:{}/login?token=", addr.port());
    assert!(link.url.starts_with(&prefix), "a socket caller gets a loopback link: {}", link.url);
    let valid_for = link.expires_at.duration_since(Timestamp::now()).as_secs();
    assert!((55..=60).contains(&valid_for), "valid for a minute: {valid_for} s");

    let mut browser = Browser::new(addr);
    let redeemed = browser.log_in(&link).await;
    assert_eq!((redeemed.status, redeemed.header("location")), (303, Some("/history")));
    let cookie = redeemed.header("set-cookie").unwrap();
    for attribute in ["HttpOnly", "SameSite=Strict", "Path=/", "Max-Age=604800"] {
        assert!(cookie.split("; ").any(|a| a == attribute), "{attribute} in {cookie}");
    }
    assert!(!cookie.contains("Secure"), "plain HTTP can't keep a Secure cookie: {cookie}");
    let model = browser.get(routes::MODEL).await;
    assert_eq!((model.status, model.json()["name"].as_str()), (200, Some("large-v3-turbo")));

    let token = link.url.strip_prefix(&prefix).unwrap();
    let again = Browser::new(addr).post(routes::LOGIN_REDEEM, &[("token", token)]).await;
    assert_eq!(again.status, 403, "a link logs in once");
    assert!(again.body.contains("psst ui"), "{}", again.body);
}

#[tokio::test]
async fn a_link_asked_for_over_tcp_starts_with_the_address_the_client_reached() {
    let server = tcp_server();
    let addr = server.tcp_addr().unwrap();
    let paired = pair(&server).await;
    let host =
        [("Host", "desktop.example:7341".to_owned()), ("Authorization", format!("Bearer {}", paired.token.as_str()))];
    let link = request(addr, "POST", routes::LOGIN_LINK, &host, None).await;
    assert_eq!(link.status, 200, "{}", link.body);
    assert!(
        link.json()["url"].as_str().unwrap().starts_with("http://desktop.example:7341/login?token="),
        "{}",
        link.body
    );

    let tcp = Endpoint::Tcp { host: addr.ip().to_string(), port: addr.port() };
    let mut browser = Browser::new(addr);
    assert_eq!(browser.log_in(&login_link(tcp, Some(paired.token.clone())).await).await.status, 303);

    // A browser session can't extend itself, and a caller needs credentials.
    let from_browser = browser.json("POST", routes::LOGIN_LINK, &json!({})).await;
    assert_eq!((from_browser.status, from_browser.json()["code"].as_str()), (403, Some("forbidden")));
    assert_eq!(request(addr, "POST", routes::LOGIN_LINK, &[], None).await.status, 401);
}

#[tokio::test]
async fn web_url_starts_every_link_and_makes_the_cookie_secure_for_https() {
    let server = server(Script::default(), |cfg| {
        cfg.tcp = true;
        cfg.server.web_url = Some("https://desktop.tail1234.ts.net/".into());
    });
    let link = login_link(server.endpoint(), None).await;
    assert!(link.url.starts_with("https://desktop.tail1234.ts.net/login?token="), "{}", link.url);

    // Behind a proxy for that name, the browser's origin is web_url, whatever Host it forwards.
    let browser = Browser::new(server.tcp_addr().unwrap());
    let token = link.url.split_once("token=").unwrap().1;
    let headers = [("Origin", "https://desktop.tail1234.ts.net".to_owned())];
    let redeemed = browser.post_with(routes::LOGIN_REDEEM, &headers, &[("token", token)]).await;
    assert_eq!(redeemed.status, 303, "{}", redeemed.body);
    assert!(redeemed.header("set-cookie").unwrap().ends_with("; Secure"));
}

#[tokio::test]
async fn without_a_tcp_listener_there_is_no_link_to_give() {
    let server = default_server();
    let api = ApiClient::new(server.endpoint(), None);
    let (status, body) = match api.call::<LoginLink>(Method::Post, routes::LOGIN_LINK, None::<&()>).await {
        Err(ApiCallError::Api { status, error }) => (status, error.message),
        other => panic!("expected a refusal, got {other:?}"),
    };
    assert_eq!(status, 400);
    assert!(body.contains("listen = []"), "{body}");
}

#[tokio::test]
async fn redeeming_login_links_is_rate_limited() {
    let server = tcp_server();
    let browser = Browser::new(server.tcp_addr().unwrap());
    let mut statuses = Vec::new();
    for _ in 0..11 {
        statuses.push(browser.post(routes::LOGIN_REDEEM, &[("token", "not-a-token")]).await.status);
    }
    assert_eq!(statuses, [403, 403, 403, 403, 403, 403, 403, 403, 403, 403, 429]);
    let link = login_link(server.endpoint(), None).await;
    assert_eq!(Browser::new(server.tcp_addr().unwrap()).log_in(&link).await.status, 429, "even with a good token");
}

#[tokio::test]
async fn the_session_cookie_authenticates_the_api_over_tcp_until_logout() {
    let server = tcp_server();
    let addr = server.tcp_addr().unwrap();
    let mut browser = Browser::new(addr);
    assert_eq!(browser.get(routes::DEVICES).await.status, 401);
    browser.cookie = Some("not-a-session".into());
    assert_eq!(browser.get(routes::DEVICES).await.status, 401);

    let browser = logged_in(&server).await;
    assert_eq!(browser.get(routes::DEVICES).await.status, 200);
    let notes = json!({"language": "en", "rules": []});
    let put = browser.json("PUT", &routes::with_name(routes::PROFILE, "notes"), &notes).await;
    assert_eq!((put.status, put.json()["profile"]["layer"].as_str()), (200, Some("added")), "{}", put.body);

    let out = browser.post("/logout", &[]).await;
    assert_eq!(out.status, 200);
    assert!(out.header("set-cookie").unwrap().starts_with("psst_session=; Max-Age=0;"), "{:?}", out.headers);
    assert_eq!(browser.get(routes::DEVICES).await.status, 401, "logging out ends the session");
}

#[tokio::test]
async fn a_browser_session_cannot_ask_for_a_pairing_code() {
    let server = tcp_server();
    let browser = logged_in(&server).await;
    let response = browser.json("POST", routes::PAIRING, &json!({"name": "spare"})).await;
    assert_eq!((response.status, response.json()["code"].as_str()), (403, Some("forbidden")), "{}", response.body);
}

#[tokio::test]
async fn cookie_requests_from_other_origins_are_refused_before_they_change_anything() {
    let server = tcp_server();
    let browser = logged_in(&server).await;
    let path = routes::with_name(routes::PROFILE, "notes");
    let body = json!({"language": "en"}).to_string();
    let cross_site: [&[(&str, String)]; 4] = [
        &[("Origin", "http://evil.example".into())],
        &[("Origin", "null".into())],
        &[("Sec-Fetch-Site", "cross-site".into())],
        &[],
    ];
    for headers in cross_site {
        let mut all = vec![("Cookie", format!("psst_session={}", browser.cookie.as_ref().unwrap()))];
        all.extend(headers.iter().cloned());
        let response = request(browser.addr, "PUT", &path, &all, Some(("application/json", body.clone()))).await;
        assert_eq!((response.status, response.json()["code"].as_str()), (403, Some("forbidden")), "{headers:?}");
    }
    let names: Vec<_> = browser.get(routes::PROFILES).await.json()["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["default"], "nothing was stored");

    // Without an Origin, the browser's own Sec-Fetch-Site will do.
    let same = browser.post_with("/logout", &[("Sec-Fetch-Site", "same-origin".into())], &[]).await;
    assert_eq!(same.status, 200);

    // A login form posted from elsewhere doesn't use the token up.
    let link = login_link(server.endpoint(), None).await;
    let token = link.url.split_once("token=").unwrap().1;
    let mut other = Browser::new(server.tcp_addr().unwrap());
    let refused =
        other.post_with(routes::LOGIN_REDEEM, &[("Origin", "http://evil.example".into())], &[("token", token)]).await;
    assert_eq!(refused.status, 403);
    assert_eq!(other.log_in(&link).await.status, 303);
}

#[tokio::test]
async fn revoking_a_device_ends_the_browser_sessions_it_started() {
    let server = tcp_server();
    let addr = server.tcp_addr().unwrap();
    let paired = pair(&server).await;
    let tcp = Endpoint::Tcp { host: addr.ip().to_string(), port: addr.port() };
    let mut browser = Browser::new(addr);
    browser.log_in(&login_link(tcp, Some(paired.token.clone())).await).await;
    assert_eq!(browser.get(routes::MODEL).await.status, 200);
    assert!(admin(&server, &["devices", "revoke", &paired.device_id]).status.success());
    assert_eq!(browser.get(routes::MODEL).await.status, 401);
}
