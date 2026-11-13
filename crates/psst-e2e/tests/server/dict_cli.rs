//! `psst dict` as built (`psst-e2e-daemon dict …`), against the real server: over the socket, and
//! over TCP with the token `psst pair` stored.

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use psst_config::{ClientConfig, ConfigFile, DictionaryFile};
use psst_engine_proto::fake::Script;
use psst_proto::{Endpoint, api::DictionaryView};
use psst_testkit::TestServer;
use tempfile::TempDir;

use crate::harness::*;

const PSST: &str = env!("CARGO_BIN_EXE_psst-e2e-daemon");

/// A home for `psst` with a `client.toml` naming `server`, and nothing from this process's
/// environment.
struct Home(TempDir);

impl Home {
    fn new(server: Endpoint) -> Home {
        let home = Home(tempfile::tempdir().unwrap());
        let config = home.0.path().join("config/psst");
        fs::create_dir_all(&config).unwrap();
        let client = ClientConfig { server, ..ClientConfig::default() };
        fs::write(config.join(ClientConfig::FILE_NAME), client.to_toml_string()).unwrap();
        home
    }

    fn psst(&self, args: &[&str]) -> Output {
        let root = self.0.path();
        Command::new(PSST)
            .args(args)
            .env_clear()
            .env("HOME", root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .output()
            .expect("run psst")
    }

    /// Runs `psst <args>`; panics unless it succeeds, and returns its stdout.
    #[track_caller]
    fn ok(&self, args: &[&str]) -> String {
        let out = self.psst(args);
        assert!(out.status.success(), "psst {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    /// Runs `psst <args>`; panics unless it fails, and returns its stderr.
    #[track_caller]
    fn fails(&self, args: &[&str]) -> String {
        let out = self.psst(args);
        assert_eq!(out.status.code(), Some(1), "psst {args:?}: {}", String::from_utf8_lossy(&out.stdout));
        String::from_utf8(out.stderr).unwrap()
    }
}

fn dictionary_server(tcp: bool) -> TestServer {
    server(Script::default(), |cfg| {
        cfg.tcp = tcp;
        cfg.dictionary = DictionaryFile::from_toml_str(
            "[[terms]]\ntext = \"Hyprland\"\nsounds_like = [\"hyper land\"]\n\n[[terms]]\ntext = \"Go\"\nlanguages = \"en\"\n\n[[terms]]\ntext = \"Kubernetes\"\n",
        )
        .unwrap();
    })
}

#[tokio::test]
async fn psst_dict_changes_the_dictionary_and_says_what_it_did() {
    let server = dictionary_server(false);
    let home = Home::new(server.endpoint());

    assert_eq!(home.ok(&["dict", "add", "t3code", "--sounds-like", "t3 code", "--pin"]), "`t3code` added\n");
    assert_eq!(
        home.fails(&["dict", "add", "Hypr", "--sounds-like", "hyper land"]),
        "psst: terms `Hyprland` and `Hypr` both claim the spelling `hyper land` in every language\n"
    );
    assert!(home.fails(&["dict", "add", "hyprland"]).contains("`hyprland` is already in the dictionary"));
    assert_eq!(
        home.ok(&["dict", "add", "Hyprland", "--sounds-like", "hyper land", "--sounds-like", "hyperland", "--replace"]),
        "`Hyprland` replaces dictionary.toml's version\n"
    );
    assert_eq!(home.ok(&["dict", "add", "Wayland", "--replace", "--lang", "en", "--no-fuzzy"]), "`Wayland` added\n");
    assert_eq!(home.ok(&["dict", "rm", "Go"]), "`Go` is hidden; `psst dict reset` brings it back\n");
    assert_eq!(
        home.ok(&["dict", "never", "Kubernetes", "cube nettie"]),
        "never rule 1: `kubernetes` never replaces \"cube nettie\"\n"
    );
    assert_eq!(
        home.ok(&["dict", "list"]),
        "\
TERM        FROM      SOUNDS LIKE            LANG
Go          hidden                           en
Hyprland    replaced  hyper land, hyperland  any
Kubernetes  file                             any
t3code      added     t3 code                any   pin
Wayland     added                            en    no fuzzy

Never rules:
    1  `kubernetes` never replaces \"cube nettie\"
"
    );
    assert_eq!(
        home.ok(&["dict", "test", "um, push t3 code to hyper land"]),
        "\
\"Push t3code to Hyprland \"
  dictionary exact (`t3code`): \"t3 code\" → \"t3code\"
  dictionary exact (`hyprland`): \"hyper land\" → \"Hyprland\"
  rules fillers: \"um,\" → \"\"
  rules whitespace: \" \" → \"\"
  rules capitalize: \"push\" → \"Push\"
  rules trailing_space: \"\" → \" \"
"
    );

    assert_eq!(
        home.ok(&["dict", "reset", "go"]),
        "`Go` is dictionary.toml's version again\n\
         warning: term `Go` is a common en word: in en sessions it never changes the case of `go`, and doesn't protect it\n"
    );
    assert_eq!(home.ok(&["dict", "rm", "T3CODE"]), "removed `T3CODE`\n");
    assert_eq!(home.ok(&["dict", "never", "--remove", "1"]), "removed never rule 1\n");
    assert_eq!(home.fails(&["dict", "never", "--remove", "1"]), "psst: no never rule 1\n");
    assert!(home.ok(&["dict", "add", "Nix", "--sounds-like", "next"]).ends_with(
        "warning: term `Nix`: sounds_like `next` is a common en word: in en sessions every `next` becomes `Nix`\n"
    ));

    let view: DictionaryView = serde_json::from_str(&home.ok(&["dict", "list", "--json"])).expect("JSON");
    let ids: Vec<&str> = view.terms.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, ["go", "hyprland", "kubernetes", "nix", "wayland"]);
    assert!(view.warnings.iter().any(|w| w.starts_with("term `Go` is a common en word")));
}

#[tokio::test]
async fn psst_dict_reaches_another_machine_with_the_paired_token() {
    let server = dictionary_server(true);
    let endpoint = server.tcp_endpoint().unwrap();
    let home = Home::new(endpoint.clone());
    assert_eq!(
        home.fails(&["dict", "list"]),
        format!("psst: this device isn't paired with {endpoint}; run `psst pair {endpoint} <code>`\n")
    );

    let code = admin_json(&server, &["pair", "--name", "thinkpad"])["code"].as_str().unwrap().to_owned();
    home.ok(&["pair", &endpoint.to_string(), &code]);
    assert_eq!(home.ok(&["dict", "add", "t3code"]), "`t3code` added\n");
    assert!(home.ok(&["dict", "list"]).contains("\nt3code      added"));
}

#[tokio::test]
async fn psst_dict_says_when_the_server_isnt_there() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("missing.sock");
    let home = Home::new(Endpoint::Unix(socket.clone()));
    let err = home.fails(&["dict", "list"]);
    assert!(err.starts_with(&format!("psst: cannot connect to {}", socket.display())), "{err}");
    assert!(err.trim_end().ends_with(&format!("is psst-server running at {}?", Path::new(&socket).display())), "{err}");
}
