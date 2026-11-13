use std::path::PathBuf;

use psst_proto::Endpoint;

fn tcp(host: &str, port: u16) -> Endpoint {
    Endpoint::Tcp { host: host.into(), port }
}

#[test]
fn parses_socket_paths_and_http_addresses() {
    let cases = [
        ("/run/psst/client.sock", Endpoint::Unix(PathBuf::from("/run/psst/client.sock"))),
        ("http://desktop:7341", tcp("desktop", 7341)),
        ("http://desktop", tcp("desktop", 7341)),
        ("http://desktop/", tcp("desktop", 7341)),
        ("http://100.64.0.1:8000", tcp("100.64.0.1", 8000)),
        ("http://[fd7a:115c::1]:7341", tcp("fd7a:115c::1", 7341)),
        ("http://[::1]", tcp("::1", 7341)),
    ];
    for (input, expected) in cases {
        assert_eq!(input.parse::<Endpoint>().unwrap(), expected, "{input}");
    }
}

#[test]
fn rejects_anything_else() {
    for input in [
        "",
        "desktop:7341",
        "relative/client.sock",
        "https://desktop",
        "ws://desktop:7341",
        "http://",
        "http://:7341",
        "http://desktop:port",
        "http://desktop:70000",
        "http://desktop/v1/client",
        "http://user@desktop",
        "http://[::1",
    ] {
        assert!(input.parse::<Endpoint>().is_err(), "{input:?} should be rejected");
    }
}

#[test]
fn display_round_trips() {
    for input in ["/run/psst/client.sock", "http://desktop:7341", "http://[fd7a:115c::1]:7341"] {
        let endpoint: Endpoint = input.parse().unwrap();
        assert_eq!(endpoint.to_string(), input);
        assert_eq!(endpoint.to_string().parse::<Endpoint>().unwrap(), endpoint);
    }
}

#[test]
fn serialises_as_a_string() {
    let endpoint: Endpoint = serde_json::from_str(r#""http://desktop""#).unwrap();
    assert_eq!(endpoint, tcp("desktop", 7341));
    assert_eq!(serde_json::to_string(&endpoint).unwrap(), r#""http://desktop:7341""#);
    assert!(serde_json::from_str::<Endpoint>(r#""desktop""#).is_err());
}
