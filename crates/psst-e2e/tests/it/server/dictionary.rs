//! The dictionary's database layer through `/api/dictionary` (plan 5.2 Phase 2, 7.1): adding,
//! replacing, hiding and resetting terms, never rules, the checks on every change, export, the
//! test route, and what sessions see.

use std::{fs, time::Duration};

use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use psst_client::api::{ApiCallError, ApiClient, Method};
use psst_config::{ConfigFile, DictionaryFile};
use psst_engine_proto::fake::Script;
use psst_proto::{
    Final, SessionId,
    api::{
        ApiErrorCode, DictionaryExport, DictionaryView, NeverRule, TermChange, TermLayer,
        routes::{self, with_id},
    },
    ws::Mode,
};
use psst_testkit::TestServer;
use serde_json::{Value, json};

use super::harness::*;

const FILE: &str = r#"
[[terms]]
text = "Hyprland"
sounds_like = ["hyper land"]

[[terms]]
text = "Kubernetes"
sounds_like = ["kube"]

[[terms]]
text = "Go"
languages = "en"
"#;

/// A server whose `dictionary.toml` is [`FILE`] and whose engine transcribes `texts` in order.
fn dictionary_server(texts: &[&str]) -> TestServer {
    server(script(texts.iter().map(|text| result(text, 0)).collect()), |cfg| {
        cfg.dictionary = DictionaryFile::from_toml_str(FILE).unwrap();
    })
}

fn api(server: &TestServer) -> ApiClient {
    ApiClient::new(server.endpoint(), None)
}

async fn view(api: &ApiClient) -> DictionaryView {
    api.call(Method::Get, routes::DICTIONARY, None::<&()>).await.expect("GET /api/dictionary")
}

/// Every identity with its layer and whether sessions use it.
async fn layers(api: &ApiClient) -> Vec<(String, TermLayer, bool)> {
    view(api).await.terms.into_iter().map(|t| (t.id, t.layer, t.active)).collect()
}

async fn add(api: &ApiClient, term: Value) -> Result<TermChange, ApiCallError> {
    api.call(Method::Post, routes::DICTIONARY_TERMS, Some(&term)).await
}

async fn replace(api: &ApiClient, id: &str, term: Value) -> Result<TermChange, ApiCallError> {
    api.call(Method::Put, &with_id(routes::DICTIONARY_TERM, id), Some(&term)).await
}

async fn remove(api: &ApiClient, id: &str) -> Result<TermChange, ApiCallError> {
    api.call(Method::Delete, &with_id(routes::DICTIONARY_TERM, id), None::<&()>).await
}

async fn reset(api: &ApiClient, id: &str) -> Result<TermChange, ApiCallError> {
    api.call(Method::Post, &with_id(routes::DICTIONARY_TERM_RESET, id), None::<&()>).await
}

async fn never(api: &ApiClient, term: &str, span: &str) -> Result<NeverRule, ApiCallError> {
    api.call(Method::Post, routes::DICTIONARY_NEVER, Some(&json!({"term": term, "span": span}))).await
}

async fn test(api: &ApiClient, body: Value) -> Result<Final, ApiCallError> {
    api.call(Method::Post, routes::DICTIONARY_TEST, Some(&body)).await
}

/// The status, code and message of a refused call.
#[track_caller]
fn refused<T: std::fmt::Debug>(result: Result<T, ApiCallError>) -> (u16, ApiErrorCode, String) {
    match result {
        Err(ApiCallError::Api { status, error }) => (status, error.code, error.message),
        other => panic!("expected an API error, got {other:?}"),
    }
}

/// Dictates once; the final text and the bias terms the engine was sent.
async fn dictate(server: &TestServer) -> (String, Value) {
    let mut client = Client::connect(server).await;
    client.upload(SessionId::new_v4(), SECOND / 4).await;
    let text = client.final_msg().await.r#final.text;
    (text, transcribes(server).pop().expect("a transcribe request").header["bias"]["terms"].clone())
}

fn hangup(server: &TestServer) {
    kill(Pid::from_raw(server.pid() as i32), Signal::SIGHUP).expect("send SIGHUP");
}

#[tokio::test]
async fn the_dictionary_shows_each_term_with_the_layer_its_version_comes_from() {
    let server = dictionary_server(&[]);
    let api = api(&server);
    let initial = view(&api).await;
    assert_eq!(
        layers(&api).await,
        [
            ("go".into(), TermLayer::File, true),
            ("hyprland".into(), TermLayer::File, true),
            ("kubernetes".into(), TermLayer::File, true)
        ]
    );
    assert!(initial.warnings.iter().any(|w| w.starts_with("term `Go` is a common en word")), "{:?}", initial.warnings);

    let added = add(&api, json!({"text": "t3code", "sounds_like": ["t3 code"]})).await.unwrap();
    let added = added.term.expect("the added term");
    assert_eq!((added.id.as_str(), added.layer, added.active), ("t3code", TermLayer::Added, true));
    let replaced = replace(&api, "HYPRLAND", json!({"text": "Hyprland", "sounds_like": ["hyper land", "hyperland"]}))
        .await
        .unwrap()
        .term
        .expect("the replaced term");
    assert_eq!((replaced.layer, replaced.term.unwrap().sounds_like.len()), (TermLayer::Replaced, 2));
    assert_eq!(replaced.file.expect("the file's version").sounds_like, ["hyper land"]);
    let hidden = remove(&api, "go").await.unwrap().term.expect("the hidden term");
    assert_eq!((hidden.layer, hidden.active, hidden.term.unwrap().text.as_str()), (TermLayer::Hidden, false, "Go"));

    assert_eq!(
        layers(&api).await,
        [
            ("go".into(), TermLayer::Hidden, false),
            ("hyprland".into(), TermLayer::Replaced, true),
            ("kubernetes".into(), TermLayer::File, true),
            ("t3code".into(), TermLayer::Added, true),
        ]
    );
    assert!(view(&api).await.warnings.is_empty(), "hiding `Go` takes its warning with it");
}

#[tokio::test]
async fn new_dictations_use_a_change_at_once() {
    let server = dictionary_server(&["push t3 code to hyper land", "push t3 code to hyper land"]);
    let api = api(&server);
    add(&api, json!({"text": "t3code", "sounds_like": ["t3 code"], "pin": true})).await.unwrap();
    let (text, bias) = dictate(&server).await;
    assert_eq!(text, "Push t3code to Hyprland ");
    assert_eq!(bias, json!(["t3code", "Go", "Hyprland", "Kubernetes"]));

    remove(&api, "Hyprland").await.unwrap();
    let (text, bias) = dictate(&server).await;
    assert_eq!(text, "Push t3code to hyper land ");
    assert_eq!(bias, json!(["t3code", "Go", "Kubernetes"]));
    assert_eq!(requests_of(&server, "load").len(), 1, "a change doesn't touch the engine");
}

#[tokio::test]
async fn a_session_keeps_the_dictionary_it_started_with() {
    let server = server(script(vec![result("push t3 code", 0), result("push t3 code", 0)]), |_| {});
    let api = api(&server);
    let mut client = Client::connect(&server).await;
    let id = SessionId::new_v4();
    client.start(id, Mode::Live, false).await;
    client.ready().await;
    client.audio(SECOND / 4).await;

    add(&api, json!({"text": "t3code", "sounds_like": ["t3 code"]})).await.unwrap();
    client.stop(id, SECOND as u64 / 4).await;
    assert_eq!(client.final_msg().await.r#final.text, "Push t3 code ", "the session started before the change");
    assert_eq!(transcribes(&server)[0].header["bias"]["terms"], json!([]));

    let (text, bias) = dictate(&server).await;
    assert_eq!((text.as_str(), bias), ("Push t3code ", json!(["t3code"])));
}

#[tokio::test]
async fn a_change_that_fails_the_checks_is_refused_and_changes_nothing() {
    let server = dictionary_server(&[]);
    let api = api(&server);
    let before = view(&api).await;
    let cases = [
        (add(&api, json!({"text": " "})).await, 400, "terms.text must not be empty"),
        (add(&api, json!({"text": "Nix", "sounds_like": [""]})).await, 400, "sounds_like entries must not be empty"),
        (add(&api, json!({"text": "Nix", "languages": ""})).await, 400, "languages must be"),
        (add(&api, json!({"sounds_like": ["nix"]})).await, 400, "text"),
        (
            add(&api, json!({"text": "Hypr", "sounds_like": ["HYPER LAND"]})).await,
            400,
            "terms `Hyprland` and `Hypr` both claim the spelling `HYPER LAND` in every language",
        ),
        (add(&api, json!({"text": "kubernetes"})).await, 400, "`kubernetes` is already in the dictionary"),
        (
            replace(&api, "hyprland", json!({"text": "Wayland"})).await,
            400,
            "`Wayland` has the identity `wayland`, not `hyprland`; renaming is a hide plus an add",
        ),
        (
            replace(&api, "kubernetes", json!({"text": "Kubernetes", "sounds_like": ["hyper land"]})).await,
            400,
            "both claim the spelling `hyper land`",
        ),
        (replace(&api, "wayland", json!({"text": "Wayland"})).await, 404, "no term `wayland`"),
        (remove(&api, "wayland").await, 404, "no term `wayland`"),
        (reset(&api, "hyprland").await, 404, "`hyprland` has no database entry to reset"),
    ];
    for (result, status, message) in cases {
        let (got_status, code, got_message) = refused(result);
        let want_code = if status == 400 { ApiErrorCode::BadRequest } else { ApiErrorCode::NotFound };
        assert_eq!((got_status, code), (status, want_code), "{got_message}");
        assert!(got_message.contains(message), "{got_message:?} should contain {message:?}");
    }
    let body = api.call::<TermChange>(Method::Post, routes::DICTIONARY_TERMS, Some("not json")).await;
    assert_eq!(refused(body).0, 400);
    assert_eq!(view(&api).await, before);
}

#[tokio::test]
async fn a_change_that_would_leave_out_an_entry_in_use_is_refused() {
    let server = dictionary_server(&[]);
    let api = api(&server);
    // Free `kube` from the file's Kubernetes, then give it to a new term.
    replace(&api, "kubernetes", json!({"text": "Kubernetes", "sounds_like": ["k8s"]})).await.unwrap();
    add(&api, json!({"text": "Kube"})).await.unwrap();
    let (status, _, message) = refused(reset(&api, "kubernetes").await);
    assert_eq!(status, 400);
    assert_eq!(message, "terms `Kubernetes` and `Kube` both claim the spelling `Kube` in every language");
    assert!(layers(&api).await.iter().all(|(_, _, active)| *active));
}

#[tokio::test]
async fn a_change_returns_the_warnings_it_adds() {
    let server = dictionary_server(&[]);
    let api = api(&server);
    let change = add(&api, json!({"text": "Nix", "sounds_like": ["next"]})).await.unwrap();
    assert_eq!(
        change.warnings,
        ["term `Nix`: sounds_like `next` is a common en word: in en sessions every `next` becomes `Nix`"]
    );
    let change = add(&api, json!({"text": "t3code"})).await.unwrap();
    assert!(change.warnings.is_empty(), "the file's `Go` and the earlier `Nix` aren't this change's: {change:?}");
}

#[tokio::test]
async fn reset_and_remove_undo_database_entries() {
    let server = dictionary_server(&[]);
    let api = api(&server);
    let initial = view(&api).await;
    replace(&api, "hyprland", json!({"text": "HyprLand"})).await.unwrap();
    let back = reset(&api, "hyprland").await.unwrap().term.expect("the file's term");
    assert_eq!((back.layer, back.term.unwrap().text.as_str()), (TermLayer::File, "Hyprland"));

    remove(&api, "go").await.unwrap();
    let (status, _, message) = refused(remove(&api, "go").await);
    assert_eq!((status, message.as_str()), (404, "`go` is already hidden; reset it to bring it back"));
    assert_eq!(reset(&api, "go").await.unwrap().term.unwrap().layer, TermLayer::File);

    add(&api, json!({"text": "t3code"})).await.unwrap();
    assert_eq!(remove(&api, "T3CODE").await.unwrap().term, None, "an added term is removed, not hidden");
    add(&api, json!({"text": "t3code"})).await.unwrap();
    assert_eq!(reset(&api, "t3code").await.unwrap().term, None);
    assert_eq!(view(&api).await, initial);

    // A hidden file term comes back in the version added for it.
    remove(&api, "go").await.unwrap();
    let back = add(&api, json!({"text": "Go", "languages": "cs"})).await.unwrap().term.unwrap();
    assert_eq!((back.layer, back.term.unwrap().languages.as_str()), (TermLayer::Replaced, "cs"));
}

#[tokio::test]
async fn never_rules_are_kept_but_inactive_while_their_term_is_hidden() {
    let server = dictionary_server(&[]);
    let api = api(&server);
    let rule = never(&api, "KUBERNETES", "Cube Nettie").await.unwrap();
    assert_eq!(
        (rule.term.as_str(), rule.span.as_str(), rule.span_key.as_str(), rule.active),
        ("kubernetes", "Cube Nettie", "cubenettie", true)
    );
    assert_eq!(never(&api, "Kubernetes", "cube-nettie").await.unwrap().id, rule.id, "the same rule again");
    assert_eq!(refused(never(&api, "Wayland", "way land").await).0, 404);
    assert_eq!(refused(never(&api, "Kubernetes", "...").await).0, 400);

    // Renaming is a hide plus an add; the rule stays with the old identity.
    remove(&api, "kubernetes").await.unwrap();
    add(&api, json!({"text": "K8s", "sounds_like": ["kube"]})).await.unwrap();
    let rules = view(&api).await.never;
    assert_eq!(rules.iter().map(|r| (r.id, r.active)).collect::<Vec<_>>(), [(rule.id, false)]);
    reset(&api, "k8s").await.unwrap();
    reset(&api, "kubernetes").await.unwrap();
    assert!(view(&api).await.never[0].active);

    let path = with_id(routes::DICTIONARY_NEVER_RULE, &rule.id.to_string());
    api.call_empty(Method::Delete, &path, None::<&()>).await.expect("the rule is deleted");
    assert_eq!(refused(api.call_empty(Method::Delete, &path, None::<&()>).await).0, 404);
    assert_eq!(refused(api.call_empty(Method::Delete, "/api/dictionary/never/abc", None::<&()>).await).0, 400);
    assert!(view(&api).await.never.is_empty());
}

#[tokio::test]
async fn the_export_is_the_database_layer_in_dictionary_toml_format() {
    let server = dictionary_server(&[]);
    let api = api(&server);
    add(&api, json!({"text": "t3code", "sounds_like": ["t3 code"], "pin": true})).await.unwrap();
    replace(&api, "hyprland", json!({"text": "Hyprland", "sounds_like": ["hyper land", "hyperland"]})).await.unwrap();
    remove(&api, "go").await.unwrap();
    never(&api, "Kubernetes", "cube nettie").await.unwrap();

    let export: DictionaryExport = api.call(Method::Get, routes::DICTIONARY_EXPORT, None::<&()>).await.unwrap();
    let file = DictionaryFile::from_toml_str(&export.toml).expect("the export is a valid dictionary.toml");
    let terms: Vec<(&str, usize, bool)> =
        file.terms.iter().map(|t| (t.text.as_str(), t.sounds_like.len(), t.pin)).collect();
    assert_eq!(terms, [("Hyprland", 2, false), ("t3code", 1, true)]);
    for line in [
        "# Replaces dictionary.toml's \"Hyprland\".",
        "# - \"Go\"",
        "# - \"kubernetes\" never replaces \"cube nettie\"",
    ] {
        assert!(export.toml.lines().any(|l| l == line), "{line:?} in:\n{}", export.toml);
    }
    assert!(!export.toml.contains("fuzzy"), "keys at their default are left out:\n{}", export.toml);

    let printed = admin(&server, &["dict", "export"]);
    assert!(printed.status.success(), "{}", String::from_utf8_lossy(&printed.stderr));
    assert_eq!(String::from_utf8(printed.stdout).unwrap(), export.toml, "`psst-server dict export` prints it");
}

#[tokio::test]
async fn test_runs_the_pipeline_on_text_with_the_current_dictionary() {
    let server = dictionary_server(&[]);
    let api = api(&server);
    let fin = test(&api, json!({"text": "um, push the fix to hyper land", "language": "en"})).await.unwrap();
    assert_eq!(fin.text, "Push the fix to Hyprland ");
    assert_eq!(fin.raw_text, "um, push the fix to hyper land");
    let dictionary: Vec<_> = fin.edits.iter().filter(|e| e.rule == "exact").collect();
    assert_eq!(dictionary.len(), 1);
    assert_eq!((dictionary[0].before.as_str(), dictionary[0].term.as_deref()), ("hyper land", Some("hyprland")));

    add(&api, json!({"text": "Dvořák", "languages": "cs", "sounds_like": ["dvorak"]})).await.unwrap();
    assert_eq!(test(&api, json!({"text": "dvorak"})).await.unwrap().text, "Dvorak ", "the default profile is English");
    let czech = test(&api, json!({"text": "ehm dvorak", "language": "cs"})).await.unwrap();
    assert_eq!((czech.text.as_str(), czech.language.as_str()), ("Dvořák ", "cs"));
    assert_eq!(refused(test(&api, json!({"text": "x", "language": ""})).await).0, 400);
    assert!(admin_json(&server, &["history"]).as_array().unwrap().is_empty(), "nothing is recorded");
}

#[tokio::test]
async fn a_never_rule_from_the_database_stops_the_correction() {
    let server = dictionary_server(&[]);
    let api = api(&server);
    let text = json!({"text": "push the fix to hyper land", "language": "en"});
    assert_eq!(test(&api, text.clone()).await.unwrap().text, "Push the fix to Hyprland ");
    never(&api, "Hyprland", "hyper land").await.unwrap();
    let fin = test(&api, text).await.unwrap();
    assert_eq!(fin.text, "Push the fix to hyper land ");
    assert!(fin.edits.iter().all(|e| e.rule != "exact" && e.rule != "fuzzy"), "{:?}", fin.edits);
}

#[tokio::test]
async fn the_database_layer_survives_restarts_and_follows_dictionary_toml() {
    let mut server = dictionary_server(&[]);
    add(&api(&server), json!({"text": "t3code", "sounds_like": ["tee three code"]})).await.unwrap();
    remove(&api(&server), "go").await.unwrap();
    never(&api(&server), "t3code", "t three").await.unwrap();
    let before = view(&api(&server)).await;
    server.restart();
    assert_eq!(view(&api(&server)).await, before);

    // The file gains a term that claims t3code's alias, and t3code itself.
    let file = format!("{FILE}\n[[terms]]\ntext = \"Teeth Reecode\"\nsounds_like = [\"tee three code\"]\n");
    fs::write(server.config_dir().join("dictionary.toml"), file).unwrap();
    hangup(&server);
    server.wait_for_log("dictionary.toml: reloaded", 1, Duration::from_secs(10));
    let t3code = view(&api(&server)).await.terms.into_iter().find(|t| t.id == "t3code").unwrap();
    assert_eq!((t3code.layer, t3code.active), (TermLayer::Added, false));
    assert_eq!(
        t3code.problem.as_deref(),
        Some("terms `Teeth Reecode` and `t3code` both claim the spelling `tee three code` in every language")
    );
    assert!(!view(&api(&server)).await.never[0].active);
    let fin = test(&api(&server), json!({"text": "tee three code"})).await.unwrap();
    assert_eq!(fin.text, "Teeth Reecode ");
    assert_eq!(reset(&api(&server), "t3code").await.unwrap().term, None, "a left-out entry can still be reset");

    let file = format!("{FILE}\n[[terms]]\ntext = \"T3code\"\n");
    fs::write(server.config_dir().join("dictionary.toml"), file).unwrap();
    add(&api(&server), json!({"text": "t3code"})).await.unwrap();
    hangup(&server);
    server.wait_for_log("dictionary.toml: reloaded", 2, Duration::from_secs(10));
    let t3code = view(&api(&server)).await.terms.into_iter().find(|t| t.id == "t3code").unwrap();
    assert_eq!((t3code.layer, t3code.active), (TermLayer::Replaced, true), "the file now has its identity");
}

#[tokio::test]
async fn paired_devices_reach_the_dictionary_with_their_token() {
    let server = server(Script::default(), |cfg| cfg.tcp = true);
    let paired = super::access::pair(&server).await;
    let endpoint = server.tcp_endpoint().unwrap();
    let api = ApiClient::new(endpoint.clone(), Some(paired.token));
    add(&api, json!({"text": "t3code"})).await.unwrap();
    assert_eq!(view(&api).await.terms.len(), 1);
    let (status, code, _) = refused(view_result(&ApiClient::new(endpoint, None)).await);
    assert_eq!((status, code), (401, ApiErrorCode::Unauthorized));
}

async fn view_result(api: &ApiClient) -> Result<DictionaryView, ApiCallError> {
    api.call(Method::Get, routes::DICTIONARY, None::<&()>).await
}

#[tokio::test]
async fn any_spelling_of_an_identity_reaches_its_term_through_the_route() {
    let server = dictionary_server(&[]);
    let api = api(&server);
    for text in ["C/C++", "C#", "Dvořák", "t3 code?"] {
        add(&api, json!({"text": text})).await.unwrap();
    }
    let decomposed = "DVOR\u{30c}A\u{301}K";
    assert_eq!(
        replace(&api, decomposed, json!({"text": "Dvořák", "pin": true})).await.unwrap().term.unwrap().id,
        "dvořák"
    );
    for spelling in ["c/c++", "c#", decomposed, "T3 CODE?"] {
        assert_eq!(remove(&api, spelling).await.unwrap().term, None, "{spelling}");
    }
    assert_eq!(layers(&api).await.len(), 3, "only the file's terms are left");
}
