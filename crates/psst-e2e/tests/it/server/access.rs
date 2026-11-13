use std::{
    fs,
    os::unix::fs::PermissionsExt,
    time::{Duration, Instant},
};

use psst_engine_proto::fake::Script;
use psst_proto::{
    ErrorCode, PROTOCOL_VERSION, SessionId,
    api::{ModelState, PairRedeemed},
    ws::{ClientInfo, HelloMsg},
};
use psst_testkit::TestServer;
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use super::harness::*;

fn tcp_server() -> TestServer {
    server(Script::default(), |cfg| cfg.tcp = true)
}

pub async fn pair(server: &TestServer) -> PairRedeemed {
    let pairing = admin_json(server, &["pair", "--name", "work-laptop"]);
    let code = pairing["code"].as_str().unwrap();
    assert_eq!(code.len(), 8);
    let body = json!({"code": code.to_lowercase(), "name": "laptop"});
    let (status, body) = http(server.tcp_addr().unwrap(), "POST", "/api/pair/redeem", None, Some(body)).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_value(body).unwrap()
}

#[tokio::test]
async fn a_paired_device_dictates_and_calls_the_api_with_its_token() {
    let server = tcp_server();
    let addr = server.tcp_addr().unwrap();
    let paired = pair(&server).await;
    assert!(paired.token.as_str().starts_with("psst_"));

    let mut client = Socket::tcp(addr, Some(paired.token.as_str())).await.hello().await.expect("welcome");
    assert_eq!(client.welcome().device, paired.device_id);
    let id = SessionId::new_v4();
    client.upload(id, SECOND).await;
    client.final_msg().await;

    let (status, body) = http(addr, "GET", "/api/model", Some(paired.token.as_str()), None).await;
    assert_eq!((status, body["name"].as_str()), (200, Some("large-v3-turbo")));
    let (status, body) = http(addr, "GET", "/api/model", None, None).await;
    assert_eq!((status, body["code"].as_str()), (401, Some("unauthorized")));

    let devices = admin_json(&server, &["devices", "list"]);
    let device = devices.as_array().unwrap().iter().find(|d| d["id"] == paired.device_id.as_str()).unwrap();
    assert_eq!((device["name"].as_str(), device["kind"].as_str()), (Some("work-laptop"), Some("token")));
    assert!(device["last_seen"].is_string());
    let history = admin_json(&server, &["history"]);
    assert_eq!(history[0]["device"], paired.device_id.as_str());
}

#[tokio::test]
async fn tcp_callers_without_a_valid_token_are_refused() {
    let server = tcp_server();
    let addr = server.tcp_addr().unwrap();
    for token in [None, Some("psst_not-a-token")] {
        let err = Socket::tcp(addr, token).await.hello().await.err().expect("refused");
        assert_eq!((err.session_id, err.code), (None, ErrorCode::Unauthorized), "token {token:?}");
        let (status, _) = http(addr, "GET", "/api/devices", token, None).await;
        assert_eq!(status, 401);
    }
    let (status, body) = http(addr, "GET", "/health", None, None).await;
    assert_eq!((status, body["version"].as_str()), (200, Some(env!("CARGO_PKG_VERSION"))));
}

#[tokio::test]
async fn a_paired_device_cannot_hand_out_pairing_codes() {
    let server = tcp_server();
    let paired = pair(&server).await;
    let body = json!({"name": "spare"});
    let (status, body) =
        http(server.tcp_addr().unwrap(), "POST", "/api/pairing", Some(paired.token.as_str()), Some(body)).await;
    assert_eq!((status, body["code"].as_str()), (403, Some("forbidden")), "{body}");
}

#[tokio::test]
async fn devices_outside_allow_from_get_nothing_but_a_refusal() {
    let server = server(Script::default(), |cfg| {
        cfg.tcp = true;
        cfg.server.allow_from = vec!["100.64.0.0/10".parse().unwrap()];
    });
    let addr = server.tcp_addr().unwrap();
    let code = admin_json(&server, &["pair", "--name", "work-laptop"])["code"].as_str().unwrap().to_owned();
    let (status, body) = http(addr, "POST", "/api/pair/redeem", None, Some(json!({"code": code, "name": "x"}))).await;
    assert_eq!(status, 403, "{body}");
    assert!(body["message"].as_str().unwrap().contains("allow_from"), "{body}");
    assert_eq!(http_status(addr, "/health").await, Some(403));
    assert_eq!(http_status(addr, "/login").await, Some(403));

    let err = Socket::tcp(addr, None).await.hello().await.err().expect("refused");
    assert_ne!(err.code, ErrorCode::Unauthorized, "pairing again wouldn't help");
    assert!(err.message.contains("allow_from"), "{}", err.message);
    Client::connect(&server).await;
}

#[test]
fn only_the_servers_own_user_can_read_its_state() {
    let server = tcp_server();
    let mode = fs::metadata(server.state_dir()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700, "{mode:o}");
}

#[tokio::test]
async fn one_machine_cannot_hold_every_connection() {
    let server = tcp_server();
    let addr = server.tcp_addr().unwrap();
    let mut idle = Vec::new();
    for _ in 0..64 {
        idle.push(TcpStream::connect(addr).await.expect("connect over TCP"));
    }
    let mut extra = TcpStream::connect(addr).await.expect("the kernel accepts it");
    let _ = extra.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
    let mut response = Vec::new();
    let _ = extra.read_to_end(&mut response).await;
    assert!(response.is_empty(), "{}", String::from_utf8_lossy(&response));

    drop(idle);
    let deadline = Instant::now() + Duration::from_secs(5);
    while http_status(addr, "/health").await != Some(200) {
        assert!(Instant::now() < deadline, "the server never took connections again");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Unlike `http`, `None` when the server closes without answering.
async fn http_status(addr: std::net::SocketAddr, path: &str) -> Option<u16> {
    let mut stream = TcpStream::connect(addr).await.ok()?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.ok()?;
    String::from_utf8_lossy(&response).split_whitespace().nth(1)?.parse().ok()
}

#[tokio::test]
async fn the_server_hangs_up_on_an_oversized_message() {
    let server = tcp_server();
    let addr = server.tcp_addr().unwrap();
    let token = pair(&server).await.token;
    let client = ClientInfo { name: "psst-e2e".into(), version: "0".into() };
    let config = Some(json!({"padding": "x".repeat(1024 * 1024)}));
    let hello = HelloMsg { protocol: PROTOCOL_VERSION, client, accepts: Vec::new(), config };
    assert!(Socket::tcp(addr, Some(token.as_str())).await.hangs_up_on(hello).await);
    Socket::tcp(addr, Some(token.as_str())).await.hello().await.expect("a hello of the usual size is welcome");
}

#[tokio::test]
async fn wrong_guesses_leave_the_real_code_working() {
    let server = tcp_server();
    let addr = server.tcp_addr().unwrap();
    let code = admin_json(&server, &["pair", "--name", "work-laptop"])["code"].as_str().unwrap().to_owned();
    for _ in 0..9 {
        let (status, body) =
            http(addr, "POST", "/api/pair/redeem", None, Some(json!({"code": "WRONG234", "name": "x"}))).await;
        assert_eq!((status, body["code"].as_str()), (403, Some("forbidden")));
    }
    let (status, body) = http(addr, "POST", "/api/pair/redeem", None, Some(json!({"code": code, "name": "x"}))).await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
async fn redeeming_codes_is_rate_limited() {
    let server = tcp_server();
    let addr = server.tcp_addr().unwrap();
    let mut statuses = Vec::new();
    for _ in 0..11 {
        statuses
            .push(http(addr, "POST", "/api/pair/redeem", None, Some(json!({"code": "WRONG234", "name": "x"}))).await.0);
    }
    assert_eq!(statuses, [403, 403, 403, 403, 403, 403, 403, 403, 403, 403, 429]);
}

#[tokio::test]
async fn revoking_a_device_closes_its_connection_and_refuses_its_token() {
    let server = tcp_server();
    let addr = server.tcp_addr().unwrap();
    let paired = pair(&server).await;
    let mut client = Socket::tcp(addr, Some(paired.token.as_str())).await.hello().await.expect("welcome");

    let out = admin(&server, &["devices", "revoke", &paired.device_id]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(client.closed().await, "the revoked device's connection is closed");
    let err = Socket::tcp(addr, Some(paired.token.as_str())).await.hello().await.err().expect("refused");
    assert_eq!(err.code, ErrorCode::Unauthorized);
    let devices = admin_json(&server, &["devices", "list"]);
    assert!(devices.as_array().unwrap().iter().all(|d| d["id"] != paired.device_id.as_str()));

    let out = admin(&server, &["devices", "revoke", "nope"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no device nope"));
}

#[tokio::test]
async fn a_device_revoked_before_its_hello_is_refused() {
    let server = tcp_server();
    let paired = pair(&server).await;
    let socket = Socket::tcp(server.tcp_addr().unwrap(), Some(paired.token.as_str())).await;
    assert!(admin(&server, &["devices", "revoke", &paired.device_id]).status.success());
    let err = socket.hello().await.err().expect("refused");
    assert_eq!(err.code, ErrorCode::Unauthorized);
}

#[tokio::test]
async fn a_device_that_connects_again_replaces_its_older_connection() {
    let server = tcp_server();
    let addr = server.tcp_addr().unwrap();
    let paired = pair(&server).await;
    let mut old = Socket::tcp(addr, Some(paired.token.as_str())).await.hello().await.expect("welcome");
    let mut new = Socket::tcp(addr, Some(paired.token.as_str())).await.hello().await.expect("welcome");
    assert!(old.closed().await, "the older connection is closed");
    new.upload(SessionId::new_v4(), SECOND).await;
    new.final_msg().await;
}

#[tokio::test]
async fn local_users_outside_local_users_are_refused() {
    let server = server(Script::default(), |cfg| cfg.server.local_users = vec!["psst-e2e-nobody".into()]);
    let err = Socket::unix(server.socket()).await.hello().await.err().expect("refused");
    assert_eq!((err.session_id, err.code), (None, ErrorCode::Unauthorized));
    let out = admin(&server, &["model", "status"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("local_users") && stderr.contains("403"), "{stderr}");
}

#[tokio::test]
async fn the_servers_own_user_is_an_admin_on_the_socket_when_local_users_is_empty() {
    let server = default_server();
    assert_eq!(wait_model(&server, ModelState::Loaded).await.state, ModelState::Loaded);
    let pairing = admin_json(&server, &["pair", "--name", "phone"]);
    assert!(pairing["expires_at"].is_string());
}

struct Activated {
    child: std::process::Child,
}

impl Activated {
    fn start(dir: &std::path::Path, local_users: &[String]) -> Activated {
        use std::{fs, os::fd::OwnedFd, process::Stdio};

        let config = dir.join("config");
        fs::create_dir(&config).unwrap();
        let script = dir.join("engine.json");
        fs::write(&script, "{}").unwrap();
        fs::write(
            config.join("server.toml"),
            format!(
                "listen = []\nlocal_users = {local_users:?}\n[model]\ncommand = [{SERVER:?}, \"__fake-engine\", {:?}]\n",
                script.display().to_string()
            ),
        )
        .unwrap();
        let listener = std::os::unix::net::UnixListener::bind(dir.join("client.sock")).unwrap();
        // The listener arrives as stdin; the shell moves it to fd 3 and execs the server in its
        // own process, so LISTEN_PID matches.
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(r#"exec 3<&0 </dev/null; LISTEN_PID=$$ LISTEN_FDS=1 exec "$0" serve --config-dir "$1" --state-dir "$2" --cache-dir "$3""#)
            .arg(SERVER)
            .arg(&config)
            .arg(dir.join("state"))
            .arg(dir.join("cache"))
            .stdin(Stdio::from(OwnedFd::from(listener)))
            .stderr(fs::File::create(dir.join("server.log")).unwrap())
            .spawn()
            .unwrap();
        Activated { child }
    }
}

impl Drop for Activated {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn a_socket_from_systemd_admits_only_the_users_in_local_users() {
    let me = nix::unistd::User::from_uid(nix::unistd::getuid()).unwrap().expect("a passwd entry").name;

    let dir = tempfile::tempdir().unwrap();
    let _server = Activated::start(dir.path(), &[]);
    let err = Socket::unix(&dir.path().join("client.sock")).await.hello().await.err().expect("refused");
    assert_eq!(err.code, ErrorCode::Unauthorized, "under systemd, empty local_users admits nobody");

    let dir = tempfile::tempdir().unwrap();
    let mut server = Activated::start(dir.path(), std::slice::from_ref(&me));
    let socket = dir.path().join("client.sock");
    let client = Socket::unix(&socket).await.hello().await.expect("welcome");
    assert_eq!(client.welcome().device, format!("local:{me}"));
    drop(client);

    nix::sys::signal::kill(nix::unistd::Pid::from_raw(server.child.id() as i32), nix::sys::signal::Signal::SIGTERM)
        .unwrap();
    assert!(server.child.wait().unwrap().success());
    assert!(socket.exists(), "the socket belongs to systemd and stays");
}
