//! The web UI's pages (plan 6.8) at the HTTP level, as a logged-in browser uses them: every
//! dictionary action through its forms, escaping, and what a page needs to be shown.

use psst_client::api::{ApiClient, Method};
use psst_config::{ConfigFile, DictionaryFile, ProfilesFile};
use psst_engine_proto::fake::Script;
use psst_proto::api::{DictionaryView, ProfileLayer, ProfilesView, TermLayer, routes};
use psst_testkit::TestServer;
use serde_json::json;

use crate::{access::pair, browser::*, harness::*};

const FILE: &str = r#"
[[terms]]
text = "Hyprland"
sounds_like = ["hyper land"]

[[terms]]
text = "Go"
languages = "en"

[[terms]]
text = "Kubernetes"
"#;

fn web_server() -> TestServer {
    server(Script::default(), |cfg| {
        cfg.tcp = true;
        cfg.dictionary = DictionaryFile::from_toml_str(FILE).unwrap();
    })
}

async fn logged_in(server: &TestServer) -> Browser {
    let mut browser = Browser::new(server.tcp_addr().unwrap());
    let link = login_link(server.endpoint(), None).await;
    assert_eq!(browser.log_in(&link).await.status, 303);
    browser
}

async fn view(server: &TestServer) -> DictionaryView {
    ApiClient::new(server.endpoint(), None).call(Method::Get, routes::DICTIONARY, None::<&()>).await.unwrap()
}

/// The layer of the term with this identity, if either layer knows it.
async fn layer(server: &TestServer, id: &str) -> Option<TermLayer> {
    view(server).await.terms.into_iter().find(|t| t.id == id).map(|t| t.layer)
}

#[track_caller]
fn see_other(response: &Response, to: &str) {
    assert_eq!((response.status, response.header("location")), (303, Some(to)), "{}", response.body);
}

#[track_caller]
fn refused_with(response: &Response, reason: &str) {
    assert_eq!(response.status, 422, "{}", response.body);
    assert!(response.body.contains(reason), "the page says why ({reason}): {}", response.body);
}

#[tokio::test]
async fn every_dictionary_action_works_through_the_pages() {
    let server = web_server();
    let browser = logged_in(&server).await;

    let page = browser.get("/dictionary").await;
    assert_eq!(page.status, 200);
    for text in ["Hyprland", "sounds like hyper land", "Go", "en only", "Kubernetes", "large-v3-turbo", "Log out"] {
        assert!(page.body.contains(text), "{text} on the page");
    }
    let found = browser.get("/dictionary?q=HYPER").await;
    let listed = |text: &str| found.body.contains(&format!("<span class=\"term-text\">{text}</span>"));
    assert!(listed("Hyprland") && !listed("Kubernetes"), "{}", found.body);
    assert!(found.body.contains("1 of 3 terms match"), "{}", found.body);

    // Add, and come back to the same search.
    let fields = [
        ("text", "t3code"),
        ("sounds_like", "t3 code, tee three code,"),
        ("languages", ""),
        ("fuzzy", "on"),
        ("pin", "on"),
        ("notes", "the editor"),
        ("q", "hyper"),
    ];
    see_other(&browser.post("/dictionary/terms", &fields).await, "/dictionary?q=hyper");
    let added = view(&server).await.terms.into_iter().find(|t| t.id == "t3code").expect("t3code was added");
    let term = added.term.unwrap();
    assert_eq!((added.layer, term.sounds_like.len(), term.languages.as_str()), (TermLayer::Added, 2, "any"));
    assert!(term.fuzzy && term.pin && term.notes.as_deref() == Some("the editor"));
    let refused = browser.post("/dictionary/terms", &[("text", "Hypr"), ("sounds_like", "hyper land")]).await;
    refused_with(&refused, "claim the spelling");
    assert!(refused.body.contains("value=\"Hypr\""), "the entered values are kept");

    // Edit: the form, a save, and a refused rename.
    let edit = browser.get("/dictionary?edit=HYPRLAND").await;
    assert!(edit.body.contains("action=\"/dictionary/terms/hyprland\""), "{}", edit.body);
    let save = [("text", "Hyprland"), ("sounds_like", "hyper land, hyperland"), ("languages", "any")];
    see_other(&browser.post("/dictionary/terms/hyprland", &save).await, "/dictionary");
    let hyprland = view(&server).await.terms.into_iter().find(|t| t.id == "hyprland").unwrap();
    assert_eq!(hyprland.layer, TermLayer::Replaced);
    assert!(!hyprland.term.unwrap().fuzzy, "an unticked box turns fuzzy matching off");
    assert!(browser.get("/dictionary").await.body.contains("Replaces dictionary.toml's Hyprland"));
    refused_with(
        &browser.post("/dictionary/terms/hyprland", &[("text", "Wayland")]).await,
        "renaming is a hide plus an add",
    );

    // Hide, reset and remove.
    see_other(&browser.post("/dictionary/terms/go/hide", &[]).await, "/dictionary");
    assert_eq!(layer(&server, "go").await, Some(TermLayer::Hidden));
    refused_with(&browser.post("/dictionary/terms/go/hide", &[]).await, "already hidden");
    for id in ["go", "hyprland"] {
        see_other(&browser.post(&format!("/dictionary/terms/{id}/reset"), &[]).await, "/dictionary");
        assert_eq!(layer(&server, id).await, Some(TermLayer::File));
    }
    see_other(&browser.post("/dictionary/terms/t3code/hide", &[("q", "t3")]).await, "/dictionary?q=t3");
    assert_eq!(layer(&server, "t3code").await, None);

    // Never rules.
    see_other(
        &browser.post("/dictionary/never", &[("term", "Kubernetes"), ("span", "cube nettie")]).await,
        "/dictionary",
    );
    let rule = view(&server).await.never.pop().expect("the never rule");
    assert_eq!((rule.term.as_str(), rule.span_key.as_str()), ("kubernetes", "cubenettie"));
    assert!(browser.get("/dictionary").await.body.contains("never replaces “cube nettie”"));
    refused_with(&browser.post("/dictionary/never", &[("term", "Nope"), ("span", "x")]).await, "no term `Nope`");
    see_other(&browser.post(&format!("/dictionary/never/{}/remove", rule.id), &[]).await, "/dictionary");
    assert!(view(&server).await.never.is_empty());

    // Export.
    browser.post("/dictionary/terms", &[("text", "psst"), ("fuzzy", "on")]).await;
    let export = browser.get("/dictionary.toml").await;
    assert_eq!(export.status, 200);
    assert_eq!(export.header("content-disposition"), Some("attachment; filename=\"dictionary.toml\""));
    assert!(export.body.starts_with("# The psst dictionary's database layer"), "{}", export.body);
    assert!(export.body.contains("[[terms]]\ntext = \"psst\""), "{}", export.body);
}

#[tokio::test]
async fn pages_escape_what_the_dictionary_and_the_request_hold() {
    let server = web_server();
    let browser = logged_in(&server).await;
    let api = ApiClient::new(server.endpoint(), None);
    let term = json!({"text": "<script>alert(1)</script>", "sounds_like": ["a&b"], "notes": "\"><img src=x onerror=alert(2)>"});
    let _: serde_json::Value = api.call(Method::Post, routes::DICTIONARY_TERMS, Some(&term)).await.unwrap();
    let _: serde_json::Value =
        api.call(Method::Post, routes::DICTIONARY_TERMS, Some(&json!({"text": "CI/CD"}))).await.unwrap();

    let page = browser.get("/dictionary").await;
    assert!(!page.body.contains("<script>alert") && !page.body.contains("<img"), "{}", page.body);
    assert!(page.body.contains("&lt;script&gt;alert(1)&lt;&#x2f;script&gt;"), "{}", page.body);
    assert!(page.body.contains("a&amp;b") && page.body.contains("&quot;&gt;&lt;img src=x"), "{}", page.body);
    // An identity is one path segment, `/` and all.
    assert!(page.body.contains("action=\"/dictionary/terms/ci%2Fcd/hide\""), "{}", page.body);
    see_other(&browser.post("/dictionary/terms/ci%2Fcd/hide", &[]).await, "/dictionary");
    assert_eq!(layer(&server, "ci/cd").await, None);

    let search = browser.get("/dictionary?q=%22%3E%3Cb%3Ex%3C%2Fb%3E").await;
    assert!(!search.body.contains("\"><b>x</b>"), "{}", search.body);
    assert!(search.body.contains("value=\"&quot;&gt;&lt;b&gt;x&lt;&#x2f;b&gt;\""), "{}", search.body);
}

#[tokio::test]
async fn pages_need_a_session_or_a_token_and_refuse_posts_from_elsewhere() {
    let server = web_server();
    let addr = server.tcp_addr().unwrap();
    let anonymous = Browser::new(addr);
    let page = anonymous.get("/dictionary").await;
    assert_eq!(page.status, 401);
    assert!(page.body.contains("psst ui"), "{}", page.body);
    assert_eq!(anonymous.get("/").await.header("location"), Some("/dictionary"));

    // A paired device's token works too, with nothing to log out of.
    let paired = pair(&server).await;
    let page =
        request(addr, "GET", "/dictionary", &[("Authorization", format!("Bearer {}", paired.token.as_str()))], None)
            .await;
    assert_eq!(page.status, 200);
    assert!(!page.body.contains("Log out"));

    let browser = logged_in(&server).await;
    let page = browser.get("/dictionary").await;
    let headers = [
        ("content-security-policy", "default-src 'self'; frame-ancestors 'none'; form-action 'self'; base-uri 'none'"),
        ("x-frame-options", "DENY"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("cache-control", "no-store"),
        ("content-type", "text/html; charset=utf-8"),
    ];
    for (name, value) in headers {
        assert_eq!(page.header(name), Some(value), "{name}");
    }
    let elsewhere = [("Origin", "http://evil.example".to_owned())];
    let refused = browser.post_with("/dictionary/terms", &elsewhere, &[("text", "evil")]).await;
    assert_eq!(refused.status, 403);
    assert!(refused.body.contains("Cross-site request refused"));
    assert_eq!(layer(&server, "evil").await, None);

    // Assets need no session; unknown paths are pages outside /api and API errors in it.
    let htmx = anonymous.get("/static/htmx.min.js").await;
    assert_eq!((htmx.status, htmx.header("content-type")), (200, Some("text/javascript; charset=utf-8")));
    assert_eq!(htmx.header("cache-control"), Some("public, max-age=31536000, immutable"));
    let missing = anonymous.get("/nowhere").await;
    assert_eq!((missing.status, missing.header("content-type")), (404, Some("text/html; charset=utf-8")));
    assert_eq!(anonymous.get("/api/nowhere").await.json()["code"], "not_found");
}

async fn profiles(server: &TestServer) -> ProfilesView {
    ApiClient::new(server.endpoint(), None).call(Method::Get, routes::PROFILES, None::<&()>).await.unwrap()
}

#[tokio::test]
async fn profiles_are_edited_added_and_reset_through_the_pages() {
    let server = server(Script::default(), |cfg| {
        cfg.tcp = true;
        cfg.profiles = ProfilesFile::from_toml_str(
            "[profiles.default]\nprompt_style = \"File style.\"\n[profiles.czech]\nlanguage = \"cs\"\n",
        )
        .unwrap();
    });
    let browser = logged_in(&server).await;
    let page = browser.get("/profiles").await;
    assert_eq!(page.status, 200);
    for text in ["<h2>czech</h2>", "<h2>default</h2>", "action=\"/profiles/czech\"", "File style."] {
        assert!(page.body.contains(text), "{text} on the page: {}", page.body);
    }

    let czech = [("language", "cs"), ("prompt_style", "Nový styl."), ("rules", "whitespace capitalize")];
    see_other(&browser.post("/profiles/czech", &czech).await, "/profiles");
    let stored = profiles(&server).await.profiles.into_iter().find(|p| p.name == "czech").unwrap();
    assert_eq!(
        (stored.layer, stored.profile.rules.join(",")),
        (ProfileLayer::Replaced, "whitespace,capitalize".into())
    );
    assert!(browser.get("/profiles").await.body.contains("Reset to profiles.toml"));
    let refused = browser.post("/profiles/czech", &[("language", "cs"), ("rules", "fillers, shout")]).await;
    refused_with(&refused, "unknown rule `shout`");
    assert!(refused.body.contains("value=\"fillers, shout\""), "the entered values are kept");
    let long = "word ".repeat(100);
    see_other(&browser.post("/profiles/czech", &[("language", "cs"), ("prompt_style", &long)]).await, "/profiles");
    assert!(browser.get("/profiles").await.body.contains("profiles.czech.prompt_style is over about 100 tokens"));

    // Add, with a name the page has to escape.
    let new = [
        ("name", "<b>notes</b>"),
        ("language", "en"),
        ("prompt_style", "</textarea><script>x()</script>"),
        ("rules", ""),
    ];
    see_other(&browser.post("/profiles", &new).await, "/profiles");
    let added = profiles(&server).await.profiles.into_iter().find(|p| p.name == "<b>notes</b>").expect("added");
    assert_eq!((added.layer, added.profile.rules.len()), (ProfileLayer::Added, 0));
    let page = browser.get("/profiles").await;
    assert!(page.body.contains("<h2>&lt;b&gt;notes&lt;&#x2f;b&gt;</h2>"), "{}", page.body);
    assert!(!page.body.contains("<script>x()"), "{}", page.body);
    assert!(page.body.contains("action=\"/profiles/%3Cb%3Enotes%3C%2Fb%3E/reset\""), "{}", page.body);
    refused_with(&browser.post("/profiles", &[("name", "default"), ("language", "en")]).await, "already");

    // The export, then reset and remove.
    let export = browser.get("/profiles.toml").await;
    assert_eq!(export.header("content-disposition"), Some("attachment; filename=\"profiles.toml\""));
    let exported = ProfilesFile::from_toml_str(&export.body).unwrap_or_else(|err| panic!("{err}\n{}", export.body));
    assert_eq!(exported.profiles.len(), 3);
    see_other(&browser.post("/profiles/czech/reset", &[]).await, "/profiles");
    see_other(&browser.post("/profiles/%3Cb%3Enotes%3C%2Fb%3E/reset", &[]).await, "/profiles");
    refused_with(&browser.post("/profiles/default/reset", &[]).await, "no database version");
    let layers: Vec<_> = profiles(&server).await.profiles.into_iter().map(|p| (p.name, p.layer)).collect();
    assert_eq!(layers, [("czech".into(), ProfileLayer::File), ("default".into(), ProfileLayer::File)]);
}
