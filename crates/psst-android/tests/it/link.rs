use std::os::unix::fs::PermissionsExt;

use psst_proto::PROTOCOL_VERSION;

use super::*;

#[test]
fn configuring_a_bare_address_pairs_by_tailnet_identity_and_connects() {
    let server = tcp_server(FakeServerConfig::default());
    let mut phone = Phone::new();
    let first = phone.until_state("the first state", |_| true);

    phone.send(Command::Configure { server: address(&server).trim_start_matches("http://").into() });
    let online = phone.until_link(Link::Online);

    assert_eq!(first.link, Link::Unconfigured);
    assert_eq!(online.server, Some(address(&server)));
    assert_eq!(online.server_version.as_deref(), Some("fake"));
    assert_eq!(online.link_message, None);
}

#[test]
fn a_server_that_does_not_know_the_phone_asks_for_a_code_and_takes_the_right_one() {
    let server = tcp_server(FakeServerConfig::default());
    let proxy = Proxy::to(&server);
    proxy.refuse_auto_pair();
    let mut phone = Phone::new();

    phone.send(Command::Configure { server: proxy.address.clone() });
    let unknown = phone.until_link(Link::NeedsPairing);
    phone.send(Command::Pair { code: "WRONG".into() });
    phone.until_link(Link::Connecting);
    let refused = phone.until_link(Link::NeedsPairing);
    phone.send(Command::Pair { code: "PAIRCODE".into() });
    let online = phone.until_link(Link::Online);

    assert!(unknown.link_message.is_some(), "{unknown:?}");
    assert!(refused.link_message.is_some() && refused.link_message != unknown.link_message, "{refused:?}");
    assert_eq!(online.link_message, None);
}

#[test]
fn a_server_that_forgets_the_phone_asks_for_a_code_again() {
    let first = tcp_server(FakeServerConfig { token: "first".into(), ..Default::default() });
    let second = tcp_server(FakeServerConfig { token: "second".into(), ..Default::default() });
    let proxy = Proxy::to(&first);
    let mut phone = Phone::online(&proxy.address);

    proxy.point_at(&second);
    proxy.refuse_auto_pair();
    first.disconnect_all();
    phone.until_link(Link::Offline);
    phone.send(Command::Warm);
    let refused = phone.until_link(Link::NeedsPairing);
    phone.send(Command::Pair { code: "PAIRCODE".into() });
    phone.until_link(Link::Online);

    assert!(refused.link_message.is_some(), "{refused:?}");
}

#[test]
fn a_protocol_mismatch_says_which_side_to_update() {
    let newer = tcp_server(FakeServerConfig::default());
    newer.speak_protocol(PROTOCOL_VERSION + 1, true);
    let older = tcp_server(FakeServerConfig::default());
    older.speak_protocol(PROTOCOL_VERSION - 1, true);
    let mut phone = Phone::new();

    phone.send(Command::Configure { server: address(&newer) });
    let update_app = phone.until_link(Link::UpdateApp);
    phone.send(Command::Configure { server: address(&older) });
    let update_server = phone.until_link(Link::UpdateServer);

    assert!(update_app.link_message.is_some(), "{update_app:?}");
    assert!(update_server.link_message.is_some(), "{update_server:?}");
}

#[test]
fn the_server_and_the_pairing_survive_a_restart() {
    let server = tcp_server(FakeServerConfig::default());
    let proxy = Proxy::to(&server);
    let dir = tempfile::tempdir().unwrap();
    {
        let mut phone = Phone::at(dir.path());
        phone.send(Command::Configure { server: proxy.address.clone() });
        phone.until_link(Link::Online);
    }
    // Online again only with the token saved before.
    proxy.refuse_auto_pair();

    let mut phone = Phone::at(dir.path());
    let online = phone.until_link(Link::Online);

    assert_eq!(online.server, Some(proxy.address.clone()));
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let entry = entry.unwrap();
        let mode = entry.metadata().unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{} is {mode:o}", entry.path().display());
    }
}

#[test]
fn an_address_that_is_not_one_says_so() {
    let mut phone = Phone::new();

    phone.send(Command::Configure { server: "https://psst.example".into() });
    let offline = phone.until_link(Link::Offline);
    phone.send(Command::Configure { server: String::new() });
    let unconfigured = phone.until_link(Link::Unconfigured);

    assert_eq!(offline.server.as_deref(), Some("https://psst.example"));
    assert!(offline.link_message.is_some(), "{offline:?}");
    assert_eq!(unconfigured.server, None);
}

#[test]
fn a_server_name_the_phone_cannot_find_points_at_tailscale() {
    let mut phone = Phone::new();

    phone.send(Command::Configure { server: "http://psst-server.invalid:7341".into() });
    let offline = phone.until_link(Link::Offline);

    let message = offline.link_message.unwrap();
    assert!(message.contains("cannot find psst-server.invalid"), "{message}");
    assert!(message.contains("turn on Tailscale") && message.contains("split tunneling"), "{message}");
}
