use std::{
    fs,
    net::{IpAddr, Ipv4Addr},
};

use psst_config::{ConfigFile, ServerConfig};
use psst_engine_proto::fake::Script;
use psst_proto::{ErrorCode, api::PairRedeemed};
use psst_testkit::{
    TestServer,
    fake_tailscale::{FakeTailscale, TAILNET},
};
use serde_json::Value;

use super::{browser::request, harness::*};

/// Every test connection reaches the server from here, so it's the calling machine's tailnet address.
const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

fn tailnet_server(auto_pair: &[&str]) -> (TestServer, FakeTailscale) {
    let tailscale = FakeTailscale::start();
    let server = server(Script::default(), |cfg| {
        cfg.tcp = true;
        cfg.server.auto_pair = auto_pair.iter().map(|name| name.to_string()).collect();
        cfg.server.tailscale_socket = tailscale.socket().into();
    });
    (server, tailscale)
}

async fn auto_pair(server: &TestServer) -> (u16, Value) {
    http(server.tcp_addr().unwrap(), "POST", "/api/pair/auto", None, None).await
}

async fn paired(server: &TestServer) -> PairRedeemed {
    let (status, body) = auto_pair(server).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_value(body).unwrap()
}

#[tokio::test]
async fn a_machine_in_auto_pair_pairs_itself_once() {
    let (server, tailscale) = tailnet_server(&["thinkpad"]);
    tailscale.add(LOOPBACK, "nThinkpadCNTRL", &format!("thinkpad.{TAILNET}"));

    let first = paired(&server).await;
    let client = Socket::tcp(server.tcp_addr().unwrap(), Some(first.token.as_str())).await.hello().await;
    assert_eq!(client.expect("welcome").welcome().device, first.device_id);
    let devices = admin_json(&server, &["devices", "list"]);
    let device = devices.as_array().unwrap().iter().find(|d| d["id"] == first.device_id.as_str());
    assert_eq!(device.map(|d| d["name"].clone()), Some("thinkpad".into()), "{devices}");

    // So nothing else on the machine, such as a container, can pair beside its daemon.
    let (status, body) = auto_pair(&server).await;
    assert_eq!(status, 403);
    let expected = format!("already paired as device {}", first.device_id);
    assert!(body["message"].as_str().unwrap().contains(&expected), "{body}");

    // How a machine that lost its token gets a new one.
    assert!(admin(&server, &["devices", "revoke", &first.device_id]).status.success());
    let second = paired(&server).await;
    assert_ne!(second.device_id, first.device_id);
}

#[tokio::test]
async fn a_machine_whose_token_never_arrived_pairs_again() {
    let (server, tailscale) = tailnet_server(&["thinkpad"]);
    tailscale.add(LOOPBACK, "nThinkpadCNTRL", &format!("thinkpad.{TAILNET}"));
    let lost = paired(&server).await;
    let again = paired(&server).await;

    let addr = server.tcp_addr().unwrap();
    let err = Socket::tcp(addr, Some(lost.token.as_str())).await.hello().await.err().expect("the lost token is void");
    assert_eq!(err.code, ErrorCode::Unauthorized);
    Socket::tcp(addr, Some(again.token.as_str())).await.hello().await.expect("welcome");
}

#[tokio::test]
async fn machines_tailscaled_does_not_vouch_for_are_refused() {
    let (server, tailscale) = tailnet_server(&["thinkpad"]);
    let other_machine = format!("pixel-9.{TAILNET}");
    let cases = [
        ("a machine auto_pair doesn't list", Some(other_machine.as_str()), "pixel-9 isn't in the server's auto_pair"),
        ("a namesake shared in from another tailnet", Some("thinkpad.tail9999.ts.net"), "isn't a machine on the"),
        ("an address that isn't on the tailnet", None, "isn't a machine on the server's tailnet"),
    ];
    for (case, name, expected) in cases {
        match name {
            Some(name) => tailscale.add(LOOPBACK, "nOtherCNTRL", name),
            None => tailscale.remove(LOOPBACK),
        }
        let (status, body) = auto_pair(&server).await;
        assert_eq!(status, 403, "{case}: {body}");
        assert!(body["message"].as_str().unwrap().contains(expected), "{case}: {body}");
    }

    tailscale.add(LOOPBACK, "nThinkpadCNTRL", &format!("thinkpad.{TAILNET}"));
    drop(tailscale);
    let (status, body) = auto_pair(&server).await;
    assert_eq!(status, 403, "without tailscaled, no machine pairs: {body}");
    let devices = admin_json(&server, &["devices", "list"]);
    assert!(devices.as_array().unwrap().iter().all(|d| d["kind"] != "token"), "{devices}");
}

#[tokio::test]
async fn a_machine_dropped_from_auto_pair_is_revoked_at_the_next_start() {
    let (mut server, tailscale) = tailnet_server(&["thinkpad", "pixel-9"]);
    tailscale.add(LOOPBACK, "nThinkpadCNTRL", &format!("thinkpad.{TAILNET}"));
    let thinkpad = paired(&server).await;

    let file = server.config_dir().join(ServerConfig::FILE_NAME);
    let mut config = ServerConfig::from_toml_str(&fs::read_to_string(&file).unwrap()).unwrap();
    config.auto_pair = vec!["pixel-9".into()];
    fs::write(&file, config.to_toml_string()).unwrap();
    server.restart();

    let addr = server.tcp_addr().unwrap();
    let err = Socket::tcp(addr, Some(thinkpad.token.as_str())).await.hello().await.err().expect("refused");
    assert_eq!(err.code, ErrorCode::Unauthorized);
    let (status, _) = auto_pair(&server).await;
    assert_eq!(status, 403, "nor does it pair again");
}

#[tokio::test]
async fn a_web_page_cannot_collect_a_token() {
    let (server, tailscale) = tailnet_server(&["thinkpad"]);
    tailscale.add(LOOPBACK, "nThinkpadCNTRL", &format!("thinkpad.{TAILNET}"));
    let addr = server.tcp_addr().unwrap();
    let origin = [("Origin", "http://rebound.example".to_owned())];
    let refused = request(addr, "POST", "/api/pair/auto", &origin, None).await;
    assert_eq!((refused.status, refused.json()["code"].as_str()), (403, Some("forbidden")), "{}", refused.body);

    let code = admin_json(&server, &["pair", "--name", "phone"])["code"].as_str().unwrap().to_owned();
    let body = serde_json::json!({"code": code, "name": "phone"}).to_string();
    let refused = request(addr, "POST", "/api/pair/redeem", &origin, Some(("application/json", body))).await;
    assert_eq!(refused.status, 403, "{}", refused.body);

    paired(&server).await;
    let (status, body) =
        http(addr, "POST", "/api/pair/redeem", None, Some(serde_json::json!({"code": code, "name": "phone"}))).await;
    assert_eq!(status, 200, "the app can still use the code: {body}");
}
