use std::{fs, time::Duration};

use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use psst_client::api::{ApiClient, Method};
use psst_config::{ConfigFile, DictionaryFile};
use psst_proto::{
    SessionId,
    api::{DictionaryView, HistoryRow, TermLayer, routes},
};
use psst_testkit::TestServer;

use super::{
    harness::*,
    pages::{logged_in, refused_with, see_other},
};

const FILE: &str = r#"
[[terms]]
text = "Hyprland"
sounds_like = ["hyper land"]
"#;

fn history_server(texts: &[&str]) -> TestServer {
    server(script(texts.iter().map(|text| result(text, 0)).collect()), |cfg| {
        cfg.tcp = true;
        cfg.dictionary = DictionaryFile::from_toml_str(FILE).unwrap();
    })
}

fn api(server: &TestServer) -> ApiClient {
    ApiClient::new(server.endpoint(), None)
}

async fn dictate(server: &TestServer) -> String {
    let mut client = Client::connect(server).await;
    client.upload(SessionId::new_v4(), SECOND / 4).await;
    client.final_msg().await.r#final.text
}

async fn newest(server: &TestServer) -> HistoryRow {
    let rows: Vec<HistoryRow> = api(server).call(Method::Get, routes::HISTORY, None::<&()>).await.unwrap();
    rows.into_iter().next().expect("a history row")
}

async fn view(server: &TestServer) -> DictionaryView {
    api(server).call(Method::Get, routes::DICTIONARY, None::<&()>).await.unwrap()
}

async fn word(server: &TestServer, id: &str) -> Option<(TermLayer, Vec<String>)> {
    view(server).await.terms.into_iter().find(|t| t.id == id).map(|t| (t.layer, t.term.unwrap().sounds_like))
}

#[tokio::test]
async fn history_marks_the_dictionarys_changes_and_adds_a_word_from_a_dictation() {
    let server = history_server(&["push the fix to hyper land and run next test"]);
    let text = dictate(&server).await;
    assert!(text.contains("Hyprland"), "{text}");
    let id = newest(&server).await.id;
    let browser = logged_in(&server).await;

    let page = browser.get("/history").await;
    assert_eq!(page.status, 200);
    assert!(
        page.body.contains("<span class=\"w fx\" data-heard=\"hyper land\" data-term=\"Hyprland\""),
        "{}",
        page.body
    );
    assert!(
        page.body.contains("<span class=\"w\">next</span><span class=\"gap\"> </span><span class=\"w\">test</span>"),
        "{}",
        page.body
    );
    assert!(page.body.contains(&format!("action=\"/history/{id}/fix\"")), "{}", page.body);

    let fixed = browser.post(&format!("/history/{id}/fix"), &[("heard", "next test"), ("written", "nextest")]).await;
    let to = format!("/history?fixed={id}&kind=add&heard=next%20test&written=nextest&created=1#d-{id}");
    see_other(&fixed, &to);
    assert_eq!(word(&server, "nextest").await, Some((TermLayer::Added, vec!["next test".to_owned()])));
    let confirmed = browser.get(to.split('#').next().unwrap()).await;
    assert!(
        confirmed.body.contains("psst writes nextest for “next test” from your next dictation."),
        "{}",
        confirmed.body
    );
    assert!(confirmed.body.contains("<span class=\"fixed\" title=\"Fixed here\">nextest</span>"), "{}", confirmed.body);
    let undo = [("kind", "add"), ("heard", "next test"), ("written", "nextest"), ("created", "1")];
    see_other(&browser.post(&format!("/history/{id}/undo"), &undo).await, &format!("/history#d-{id}"));
    assert_eq!(word(&server, "nextest").await, None);

    let common = browser.post(&format!("/history/{id}/fix"), &[("heard", "next"), ("written", "nextest")]).await;
    refused_with(&common, "is a common word");
    assert!(common.body.contains("value=\"anyway\""), "{}", common.body);
    assert_eq!(word(&server, "nextest").await, None);
    let anyway = [("heard", "next"), ("written", "nextest"), ("action", "anyway")];
    assert_eq!(browser.post(&format!("/history/{id}/fix"), &anyway).await.status, 303);
    assert_eq!(word(&server, "nextest").await, Some((TermLayer::Added, vec!["next".to_owned()])));

    refused_with(
        &browser.post(&format!("/history/{id}/fix"), &[("heard", "run"), ("written", "")]).await,
        "Type how psst should write it",
    );
    assert_eq!(browser.post("/history/9999/fix", &[("heard", "a"), ("written", "b")]).await.status, 404);
}

#[tokio::test]
async fn a_dotted_word_is_kept_as_heard_or_its_spelling_moved_to_another_word() {
    let server = history_server(&["push the fix to hyper land"]);
    dictate(&server).await;
    let id = newest(&server).await.id;
    let browser = logged_in(&server).await;

    let keep = browser
        .post(&format!("/history/{id}/fix"), &[("heard", "Hyprland"), ("written", ""), ("action", "keep")])
        .await;
    see_other(&keep, &format!("/history?fixed={id}&kind=keep&heard=hyper%20land&term=Hyprland&removed=1#d-{id}"));
    let rules = view(&server).await.never;
    assert_eq!(
        rules.iter().map(|r| (r.term.as_str(), r.span_key.as_str())).collect::<Vec<_>>(),
        [("hyprland", "hyperland")]
    );
    assert_eq!(word(&server, "hyprland").await, Some((TermLayer::Replaced, vec![])));
    let undo = [("kind", "keep"), ("heard", "hyper land"), ("term", "Hyprland"), ("removed", "1")];
    see_other(&browser.post(&format!("/history/{id}/undo"), &undo).await, &format!("/history#d-{id}"));
    assert!(view(&server).await.never.is_empty());
    assert_eq!(word(&server, "hyprland").await, Some((TermLayer::File, vec!["hyper land".to_owned()])));

    let clash = browser.post(&format!("/history/{id}/fix"), &[("heard", "Hyprland"), ("written", "Hyprlang")]).await;
    refused_with(&clash, "Hyprland already catches “hyper land”");
    assert!(clash.body.contains("value=\"move\""), "{}", clash.body);
    let moved = [("heard", "Hyprland"), ("written", "Hyprlang"), ("action", "move")];
    let response = browser.post(&format!("/history/{id}/fix"), &moved).await;
    see_other(
        &response,
        &format!("/history?fixed={id}&kind=move&heard=hyper%20land&written=Hyprlang&term=Hyprland&created=1#d-{id}"),
    );
    assert_eq!(word(&server, "hyprland").await, Some((TermLayer::Replaced, vec![])));
    assert_eq!(word(&server, "hyprlang").await, Some((TermLayer::Added, vec!["hyper land".to_owned()])));
    let undo =
        [("kind", "move"), ("heard", "hyper land"), ("written", "Hyprlang"), ("term", "Hyprland"), ("created", "1")];
    see_other(&browser.post(&format!("/history/{id}/undo"), &undo).await, &format!("/history#d-{id}"));
    assert_eq!(word(&server, "hyprland").await, Some((TermLayer::File, vec!["hyper land".to_owned()])));
    assert_eq!(word(&server, "hyprlang").await, None);

    // A move the new word can't take leaves the spelling where it was.
    let other = browser.post("/dictionary/terms", &[("text", "Hyprlock"), ("sounds_like", "Hyprlang")]).await;
    assert_eq!(other.status, 303, "{}", other.body);
    let refused = browser.post(&format!("/history/{id}/fix"), &moved).await;
    refused_with(&refused, "both claim the spelling");
    assert_eq!(word(&server, "hyprland").await, Some((TermLayer::File, vec!["hyper land".to_owned()])));
}

#[tokio::test]
async fn a_refused_move_changes_nothing_even_when_a_left_out_word_wants_the_spelling() {
    let server = history_server(&["push the fix to hyper land"]);
    dictate(&server).await;
    let id = newest(&server).await.id;
    let browser = logged_in(&server).await;
    // Hyperion is saved while the file leaves "hyper land" free, then left out once it doesn't.
    let file = server.config_dir().join("dictionary.toml");
    let reload = |text: &str, count| {
        fs::write(&file, text).unwrap();
        kill(Pid::from_raw(server.pid() as i32), Signal::SIGHUP).expect("send SIGHUP");
        server.wait_for_log("dictionary.toml: reloaded", count, Duration::from_secs(10));
    };
    reload("[[terms]]\ntext = \"Hyprland\"\n", 1);
    let hyperion = browser.post("/dictionary/terms", &[("text", "Hyperion"), ("sounds_like", "hyper land")]).await;
    assert_eq!(hyperion.status, 303, "{}", hyperion.body);
    reload(FILE, 2);
    let left_out = || async { view(&server).await.terms.into_iter().find(|t| t.id == "hyperion").unwrap().active };
    assert!(!left_out().await);

    // Taking the spelling from Hyprland would hand it to Hyperion, which Hyprlang clashes with.
    let moved = [("heard", "Hyprland"), ("written", "Hyprlang"), ("action", "move")];
    refused_with(&browser.post(&format!("/history/{id}/fix"), &moved).await, "both claim the spelling");
    assert_eq!(word(&server, "hyprland").await, Some((TermLayer::File, vec!["hyper land".to_owned()])));
    assert_eq!(word(&server, "hyprlang").await, None);
    assert!(!left_out().await);
}

#[tokio::test]
async fn a_fix_to_an_older_dictation_answers_on_the_page_that_shows_it() {
    let mut texts = vec!["run next test"];
    texts.extend(["all good"; 20]);
    let server = history_server(&texts);
    dictate(&server).await;
    let id = newest(&server).await.id;
    for _ in 1..texts.len() {
        dictate(&server).await;
    }
    let browser = logged_in(&server).await;
    let (fix, anchor) = (format!("/history/{id}/fix"), format!("id=\"d-{id}\""));
    assert!(!browser.get("/history").await.body.contains(&anchor));
    let older = browser.get("/history?all=1").await;
    assert!(older.body.contains(&anchor) && older.body.contains("name=\"all\" value=\"1\""), "{}", older.body);

    let common = browser.post(&fix, &[("heard", "next"), ("written", "nextest"), ("all", "1")]).await;
    refused_with(&common, "is a common word");
    assert!(common.body.contains("value=\"anyway\""), "{}", common.body);
    let to = format!("/history?fixed={id}&kind=add&heard=next%20test&written=nextest&created=1&all=1#d-{id}");
    see_other(&browser.post(&fix, &[("heard", "next test"), ("written", "nextest"), ("all", "1")]).await, &to);
    let confirmed = browser.get(to.split('#').next().unwrap()).await;
    assert!(confirmed.body.contains("psst writes nextest for “next test”"), "{}", confirmed.body);
    let undo = [("kind", "add"), ("heard", "next test"), ("written", "nextest"), ("created", "1"), ("all", "1")];
    see_other(&browser.post(&format!("/history/{id}/undo"), &undo).await, &format!("/history?all=1#d-{id}"));
}
