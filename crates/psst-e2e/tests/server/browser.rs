//! A stand-in for a browser at the web UI, over plain HTTP/1.1 on TCP: it keeps the session cookie
//! and sends `Origin` with its posts, as a browser on the UI's own pages does.

use std::net::SocketAddr;

use psst_client::api::{ApiClient, Method};
use psst_proto::{
    Endpoint, Token,
    api::{LoginLink, encode_segment, routes},
};
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

/// A complete HTTP response.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|err| panic!("not JSON ({err}): {}", self.body))
    }

    /// The value of the `name` field of the first form input on the page called `field`.
    pub fn input_value(&self, field: &str) -> Option<String> {
        let start = self.body.find(&format!("name=\"{field}\" value=\""))? + field.len() + 15;
        let end = start + self.body[start..].find('"')?;
        Some(self.body[start..end].to_owned())
    }
}

/// One HTTP exchange: `headers` as given, plus `Host`, `Content-Length` and `Connection: close`.
pub async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: Option<(&str, String)>,
) -> Response {
    let mut stream = TcpStream::connect(addr).await.expect("connect over TCP");
    let mut head = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    if !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("host")) {
        head.push_str(&format!("Host: {addr}\r\n"));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    let (content_type, body) = body.unwrap_or(("text/plain", String::new()));
    head.push_str(&format!("Content-Type: {content_type}\r\nContent-Length: {}\r\n\r\n", body.len()));
    stream.write_all(head.as_bytes()).await.expect("send the request");
    stream.write_all(body.as_bytes()).await.expect("send the body");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read the response");
    let raw = String::from_utf8(raw).expect("a UTF-8 response");
    let (head, body) = raw.split_once("\r\n\r\n").expect("a complete response");
    let mut lines = head.lines();
    let status = lines.next().and_then(|line| line.split_whitespace().nth(1)).and_then(|s| s.parse().ok());
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let chunked =
        headers.iter().any(|(name, value)| name.eq_ignore_ascii_case("transfer-encoding") && value == "chunked");
    let body = if chunked { dechunk(body) } else { body.to_owned() };
    Response { status: status.expect("a status line"), headers, body }
}

fn dechunk(mut body: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = body.split_once("\r\n") {
        let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        out.push_str(&rest[..size]);
        body = &rest[size + 2..];
    }
    out
}

/// `application/x-www-form-urlencoded`.
pub fn form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(name, value)| format!("{}={}", encode_segment(name), encode_segment(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// A login link, asked for at `endpoint` (with `token` over TCP).
pub async fn login_link(endpoint: Endpoint, token: Option<Token>) -> LoginLink {
    ApiClient::new(endpoint, token).call(Method::Post, routes::LOGIN_LINK, None::<&()>).await.expect("a login link")
}

/// A browser at `addr`, its own origin, and its session cookie once it has one.
pub struct Browser {
    pub addr: SocketAddr,
    pub cookie: Option<String>,
}

impl Browser {
    pub fn new(addr: SocketAddr) -> Browser {
        Browser { addr, cookie: None }
    }

    pub fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn cookie_header(&self) -> Vec<(&'static str, String)> {
        self.cookie.iter().map(|cookie| ("Cookie", format!("psst_session={cookie}"))).collect()
    }

    pub async fn get(&self, path: &str) -> Response {
        request(self.addr, "GET", path, &self.cookie_header(), None).await
    }

    /// Posts a form from the UI's own pages.
    pub async fn post(&self, path: &str, fields: &[(&str, &str)]) -> Response {
        self.post_with(path, &[("Origin", self.origin())], fields).await
    }

    /// Posts a form with `headers` (and the cookie) only.
    pub async fn post_with(&self, path: &str, headers: &[(&str, String)], fields: &[(&str, &str)]) -> Response {
        let mut all = self.cookie_header();
        all.extend(headers.iter().cloned());
        request(self.addr, "POST", path, &all, Some(("application/x-www-form-urlencoded", form(fields)))).await
    }

    /// Sends JSON from the UI's own origin, as a script on its pages would.
    pub async fn json(&self, method: &str, path: &str, body: &Value) -> Response {
        let mut headers = self.cookie_header();
        headers.push(("Origin", self.origin()));
        request(self.addr, method, path, &headers, Some(("application/json", body.to_string()))).await
    }

    /// Opens a login link and submits its form, as the page's script does; keeps the cookie.
    pub async fn log_in(&mut self, link: &LoginLink) -> Response {
        let path = link.url.split_once("/login").map(|(_, rest)| format!("/login{rest}")).expect("a /login link");
        let page = self.get(&path).await;
        assert_eq!(page.status, 200, "{}", page.body);
        assert!(page.header("set-cookie").is_none(), "opening the link changes nothing");
        let token = page.input_value("token").expect("the page's form carries the token");
        let redeemed = self.post(routes::LOGIN_REDEEM, &[("token", &token)]).await;
        if let Some(cookie) = redeemed.header("set-cookie") {
            let value = cookie.split(';').next().and_then(|pair| pair.strip_prefix("psst_session=")).unwrap();
            self.cookie = Some(value.to_owned());
        }
        redeemed
    }
}
