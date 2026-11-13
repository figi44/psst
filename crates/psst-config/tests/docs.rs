//! `docs/config.md` documents every key with its default. These tests keep it in step with the
//! types: each `psst:defaults:<file>` block must equal the built-in defaults, every key in it must
//! have a row in the page's tables, and each `psst:example:<file>` block must be a valid file.

use std::fmt::Debug;

use psst_config::{ClientConfig, ConfigFile, DictionaryFile, ProfilesFile, ServerConfig};

const DOC: &str = include_str!("../../../docs/config.md");

/// Code blocks whose info string is ```` ```toml <tag> ````.
fn blocks(tag: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut lines = DOC.lines();
    while let Some(line) = lines.next() {
        if line.trim() == format!("```toml {tag}") {
            found.push(lines.by_ref().take_while(|l| l.trim() != "```").collect::<Vec<_>>().join("\n"));
        }
    }
    found
}

fn leaf_keys(table: &toml::Table, prefix: &str, out: &mut Vec<String>) {
    for (key, value) in table {
        let path = format!("{prefix}{key}");
        match value {
            toml::Value::Table(inner) if !inner.is_empty() => leaf_keys(inner, &format!("{path}."), out),
            _ => out.push(path),
        }
    }
}

#[track_caller]
fn check_defaults<T: ConfigFile + PartialEq + Debug>() -> toml::Table {
    let tag = format!("psst:defaults:{}", T::FILE_NAME);
    let [block] = &blocks(&tag)[..] else { panic!("docs/config.md needs exactly one `{tag}` block") };
    assert_eq!(T::from_toml_str(block).unwrap(), T::default(), "{tag} differs from the built-in defaults");
    let documented: toml::Table = toml::from_str(block).unwrap();
    let built_in = toml::Table::try_from(T::default()).unwrap();
    assert_eq!(documented, built_in, "{tag} must list every key that has a default");
    documented
}

#[test]
fn defaults_blocks_match_the_built_in_defaults() {
    check_defaults::<ServerConfig>();
    check_defaults::<ProfilesFile>();
    check_defaults::<DictionaryFile>();
    check_defaults::<ClientConfig>();
}

#[test]
fn every_server_and_client_key_has_a_table_row() {
    let mut keys = Vec::new();
    leaf_keys(&check_defaults::<ServerConfig>(), "", &mut keys);
    leaf_keys(&check_defaults::<ClientConfig>(), "", &mut keys);
    keys.push("model.revision".into());
    for key in keys.iter().filter(|k| *k != "apps") {
        assert!(DOC.contains(&format!("| `{key}` |")), "docs/config.md has no table row for `{key}`");
    }
}

#[test]
fn examples_are_valid_files() {
    let check = |file: &str, parse: fn(&str) -> Result<(), String>| {
        let tag = format!("psst:example:{file}");
        let examples = blocks(&tag);
        assert!(!examples.is_empty(), "docs/config.md has no `{tag}` block");
        for example in examples {
            parse(&example).unwrap_or_else(|err| panic!("{tag}: {err}\n{example}"));
        }
    };
    check("profiles.toml", |s| ProfilesFile::from_toml_str(s).map(drop).map_err(|e| e.to_string()));
    check("dictionary.toml", |s| DictionaryFile::from_toml_str(s).map(drop).map_err(|e| e.to_string()));
    check("client.toml", |s| ClientConfig::from_toml_str(s).map(drop).map_err(|e| e.to_string()));
}
