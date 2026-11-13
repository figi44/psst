//! Every `psst:` example in `docs/protocol.md` for this crate parses into the real type and
//! serialises back to the same JSON, so a misspelt or stale field in the docs fails here.

use std::collections::BTreeSet;

use psst_proto::{
    api::{
        ApiError, Device, DictionaryExport, DictionaryTest, DictionaryView, Health, HistoryRow, ModelStatus, NeverRule,
        NewNeverRule, PairRedeem, PairRedeemed, Pairing, PairingRequest, Stats, Term, TermChange,
    },
    ws::{ClientMsg, ServerMsg},
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

const DOC: &str = include_str!("../../../docs/protocol.md");

/// The JSON values in every ```` ```json <tag> ```` block, with the block's tag.
fn examples(tag_prefix: &str) -> Vec<(String, Value)> {
    let mut found = Vec::new();
    let mut lines = DOC.lines();
    while let Some(line) = lines.next() {
        let Some(tag) = line.trim().strip_prefix("```json ").filter(|t| t.starts_with(tag_prefix)) else {
            continue;
        };
        let body = lines.by_ref().take_while(|l| l.trim() != "```").collect::<Vec<_>>().join("\n");
        for value in serde_json::Deserializer::from_str(&body).into_iter::<Value>() {
            found.push((tag.to_owned(), value.unwrap_or_else(|err| panic!("{tag}: invalid JSON: {err}"))));
        }
    }
    found
}

#[track_caller]
fn round_trip<T: Serialize + DeserializeOwned>(tag: &str, example: &Value) {
    let parsed: T = serde_json::from_value(example.clone()).unwrap_or_else(|err| panic!("{tag}: {err}\n{example}"));
    assert_eq!(&serde_json::to_value(parsed).unwrap(), example, "{tag}: the example has fields the type doesn't");
}

fn types(examples: &[(String, Value)]) -> BTreeSet<String> {
    examples.iter().map(|(_, v)| v["type"].as_str().unwrap().to_owned()).collect()
}

#[test]
fn client_examples_cover_every_message() {
    let examples = examples("psst:client");
    for (tag, example) in &examples {
        round_trip::<ClientMsg>(tag, example);
    }
    assert_eq!(types(&examples), BTreeSet::from(["cancel", "hello", "report", "start", "stop"].map(String::from)));
}

#[test]
fn server_examples_cover_every_message() {
    let examples = examples("psst:server");
    for (tag, example) in &examples {
        round_trip::<ServerMsg>(tag, example);
    }
    assert_eq!(
        types(&examples),
        BTreeSet::from(["cancelled", "error", "final", "progress", "ready", "welcome"].map(String::from))
    );
}

#[test]
fn api_examples_parse_as_their_types() {
    let examples = examples("psst:api:");
    let mut seen = BTreeSet::new();
    for (tag, example) in &examples {
        let name = tag.strip_prefix("psst:api:").unwrap();
        match name {
            "Health" => round_trip::<Health>(tag, example),
            "ApiError" => round_trip::<ApiError>(tag, example),
            "PairRedeem" => round_trip::<PairRedeem>(tag, example),
            "PairRedeemed" => round_trip::<PairRedeemed>(tag, example),
            "ModelStatus" => round_trip::<ModelStatus>(tag, example),
            "PairingRequest" => round_trip::<PairingRequest>(tag, example),
            "Pairing" => round_trip::<Pairing>(tag, example),
            "Device" => round_trip::<Device>(tag, example),
            "Stats" => round_trip::<Stats>(tag, example),
            "HistoryRow" => round_trip::<HistoryRow>(tag, example),
            "Term" => round_trip::<Term>(tag, example),
            "DictionaryView" => round_trip::<DictionaryView>(tag, example),
            "TermChange" => round_trip::<TermChange>(tag, example),
            "NewNeverRule" => round_trip::<NewNeverRule>(tag, example),
            "NeverRule" => round_trip::<NeverRule>(tag, example),
            "DictionaryExport" => round_trip::<DictionaryExport>(tag, example),
            "DictionaryTest" => round_trip::<DictionaryTest>(tag, example),
            other => panic!("unknown API type in docs: {other}"),
        }
        seen.insert(name);
    }
    assert_eq!(seen.len(), 17, "every API body has an example: {seen:?}");
}
