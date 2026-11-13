//! Every engine example in `docs/protocol.md` parses into the real message and serialises back to
//! the same JSON, and every message type has an example.

use std::collections::BTreeSet;

use psst_engine_proto::{FromEngine, Message, ToEngine, decode, encode};
use serde_json::Value;

const DOC: &str = include_str!("../../../../docs/protocol.md");

fn examples(tag: &str) -> Vec<Value> {
    let mut found = Vec::new();
    let mut lines = DOC.lines();
    while let Some(line) = lines.next() {
        if line.trim() != format!("```json {tag}") {
            continue;
        }
        let body = lines.by_ref().take_while(|l| l.trim() != "```").collect::<Vec<_>>().join("\n");
        for value in serde_json::Deserializer::from_str(&body).into_iter::<Value>() {
            found.push(value.unwrap_or_else(|err| panic!("{tag}: invalid JSON: {err}")));
        }
    }
    found
}

#[track_caller]
fn check<M: Message>(tag: &str) {
    let mut seen = BTreeSet::new();
    for example in examples(tag) {
        let msg: M = decode(example.to_string().as_bytes()).unwrap_or_else(|err| panic!("{tag}: {err}\n{example}"));
        let back: Value = serde_json::from_slice(&encode(&msg)).unwrap();
        assert_eq!(back, example, "{tag}: the example has fields the type doesn't");
        seen.insert(msg.type_name());
    }
    assert_eq!(seen, M::TYPES.iter().copied().collect(), "{tag}: every message type needs an example");
}

#[test]
fn engine_to_server_examples() {
    check::<FromEngine>("psst:from-engine");
}

#[test]
fn server_to_engine_examples() {
    check::<ToEngine>("psst:to-engine");
}
