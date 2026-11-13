use psst::pair::Credentials;
use psst_proto::{Endpoint, Token, ws::Mode};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        TcpListener, TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    time::Instant,
};

use super::*;

async fn slow_link(server: Endpoint, one_way: Duration) -> Endpoint {
    let Endpoint::Tcp { host, port } = server else { panic!("not TCP: {server}") };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = Endpoint::Tcp { host: "127.0.0.1".into(), port: listener.local_addr().unwrap().port() };
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let Ok(upstream) = TcpStream::connect((host.as_str(), port)).await else { continue };
            let (from_client, to_client) = client.into_split();
            let (from_server, to_server) = upstream.into_split();
            tokio::spawn(delayed(from_client, to_server, one_way));
            tokio::spawn(delayed(from_server, to_client, one_way));
        }
    });
    proxy
}

async fn delayed(mut from: OwnedReadHalf, mut to: OwnedWriteHalf, by: Duration) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut buf = vec![0; 64 * 1024];
        while let Ok(n @ 1..) = from.read(&mut buf).await {
            if tx.send((Instant::now() + by, buf[..n].to_vec())).is_err() {
                break;
            }
        }
    });
    while let Some((at, bytes)) = rx.recv().await {
        tokio::time::sleep_until(at).await;
        if to.write_all(&bytes).await.is_err() {
            break;
        }
    }
    let _ = to.shutdown().await;
}

// The current-thread runtime, so the test can move the daemon's clock past the 10 s window.
#[tokio::test]
async fn a_start_after_ten_quiet_seconds_on_a_slow_link_streams_live() {
    let server = FakeServer::start(FakeServerConfig { tcp: true, ..Default::default() });
    server.push_outcome(Outcome::Final("Over the tailnet. ".into()));
    // A 400 ms round trip: over the default 300 ms pong wait, and a reconnect doesn't fit in the
    // rest of the default 700 ms check.
    let link = slow_link(server.tcp_endpoint().unwrap(), Duration::from_millis(200)).await;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let creds = Credentials { server: link.clone(), device_id: "fake-device".into(), token: Token::new("fake-token") };
    creds.save(&state.join("credentials")).unwrap();
    let mut d = daemon_in(dir.path(), &server, |cfg, _| cfg.server = link).await;
    d._dir = Some(dir);
    tokio::time::timeout(WAIT, async {
        while !d.status().await.connection.starts_with("connected") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("connected through the slow link");

    // Nothing from the server for over 10 s, so `start` pings.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(11)).await;
    tokio::time::resume();
    let id = d.dictate(1500).await;
    d.idle().await;

    let offline = d.wayland.overlays().into_iter().flatten().find(|s| matches!(s, OverlayState::Offline { .. }));
    assert_eq!(offline, None);
    let seen = server.session(id).unwrap();
    assert_eq!(seen.starts.len(), 1, "{:?}", seen.starts);
    assert_eq!((seen.starts[0].mode, seen.starts[0].resume), (Mode::Live, false), "streamed live");
    assert_eq!(d.wayland.inserted(), vec![paste("Over the tailnet. ")]);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_token_is_replaced_by_pairing_on_a_slow_link() {
    let server = FakeServer::start(FakeServerConfig { tcp: true, ..Default::default() });
    server.push_outcome(Outcome::Final("Paired again. ".into()));
    // Through this link a connection takes 400 ms and a pairing 200 ms, so a 550 ms attempt fits
    // either one, never a refused connection followed by a pairing.
    let link = slow_link(server.tcp_endpoint().unwrap(), Duration::from_millis(100)).await;
    let dir = tempfile::tempdir().unwrap();
    let credentials = dir.path().join("state/credentials");
    std::fs::create_dir_all(dir.path().join("state")).unwrap();
    let revoked = Credentials { server: link.clone(), device_id: "revoked".into(), token: Token::new("revoked") };
    revoked.save(&credentials).unwrap();
    let budget = Duration::from_millis(550);
    let mut d = daemon_in(dir.path(), &server, |cfg, deps| {
        cfg.server = link;
        deps.timing.resume_budget = budget;
        deps.timing.start_budget = budget;
        deps.timing.attempt_budget = budget;
    })
    .await;

    d.dictate(300).await;
    d.idle().await;

    assert_eq!(d.wayland.inserted(), vec![paste("Paired again. ")]);
    assert_eq!(Credentials::load(&credentials).unwrap().unwrap().token, Token::new("fake-token"));
    d._dir = Some(dir);
    d.stop().await;
}
