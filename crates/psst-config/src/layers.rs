//! Edits made outside a config file, layered over it: a JSON object shaped like the file, holding
//! only the keys the edits set. `null` removes the file's key, so an optional key goes back to
//! unset and a table entry (an app rule) disappears.
//!
//! Each key is applied on its own and checked with the file's rules; one that breaks them is left
//! out, so a stale or bad edit costs only that key, never the whole config.

use std::fmt;

use serde_json::{Map, Value};

use crate::{ClientConfig, ConfigFile, ServerConfig};

pub trait Layerable: ConfigFile + Clone + PartialEq {
    /// Top-level keys, or `table.key`, that only the file sets.
    const LOCKED: &'static [&'static str];
}

impl Layerable for ServerConfig {
    const LOCKED: &'static [&'static str] = &[
        "listen",
        "socket",
        "local_users",
        "auto_pair",
        "tailscale_socket",
        "eval_mode",
        "model.engine",
        "model.command",
    ];
}

impl Layerable for ClientConfig {
    const LOCKED: &'static [&'static str] = &["server", "insert"];
}

/// A path of keys; the segments of a Wayland app id keep their dots.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyPath(pub Vec<String>);

impl KeyPath {
    /// Splits at every dot, so only for keys whose segments have none.
    pub fn parse(key: &str) -> KeyPath {
        KeyPath(key.split('.').map(String::from).collect())
    }

    pub fn of(segments: &[&str]) -> KeyPath {
        KeyPath(segments.iter().map(|s| s.to_string()).collect())
    }

    pub fn starts_with(&self, prefix: &KeyPath) -> bool {
        self.0.starts_with(&prefix.0)
    }

    fn locked<T: Layerable>(&self) -> bool {
        T::LOCKED.iter().any(|locked| self.starts_with(&KeyPath::parse(locked)))
    }
}

/// As TOML writes a dotted key: `apps."org.gnome.Ptyxis".paste`.
impl fmt::Display for KeyPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, segment) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            let bare =
                !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
            if bare { f.write_str(segment)? } else { write!(f, "{segment:?}")? }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub key: KeyPath,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Layered<T> {
    pub config: T,
    /// The edits left out, in the order they were tried.
    pub problems: Vec<Problem>,
}

impl<T> Layered<T> {
    pub fn problem(&self, key: &KeyPath) -> Option<&str> {
        self.problems.iter().find(|p| &p.key == key).map(|p| p.message.as_str())
    }
}

pub fn to_json<T: serde::Serialize>(config: &T) -> Value {
    serde_json::to_value(config).expect("config types always serialise")
}

/// `base` with each of the edits that keeps it valid, in key order.
pub fn layer<T: Layerable>(base: &T, edits: &Value) -> Layered<T> {
    layer_keys(base, edits, true)
}

/// A file's keys over `base`, each that keeps it valid. Unlike an edit, a file sets locked keys.
pub fn read<T: Layerable>(base: &T, file: &Value) -> Layered<T> {
    layer_keys(base, file, false)
}

fn layer_keys<T: Layerable>(base: &T, edits: &Value, locks: bool) -> Layered<T> {
    let mut config = base.clone();
    let mut merged = to_json(base);
    let mut problems = Vec::new();
    for (key, value) in leaves(edits) {
        if locks && key.locked::<T>() {
            problems.push(Problem { key, message: format!("only {} sets it", T::FILE_NAME) });
            continue;
        }
        let mut candidate = merged.clone();
        apply(&mut candidate, &key, &value);
        let parsed = serde_json::from_value::<T>(candidate.clone()).map_err(|err| err.to_string());
        match parsed.and_then(|parsed| parsed.check().map(|()| parsed)) {
            Ok(parsed) => {
                config = parsed;
                merged = candidate;
            }
            Err(message) => problems.push(Problem { key, message }),
        }
    }
    Layered { config, problems }
}

/// What a config sets that its type's default doesn't, as a file of only those keys.
pub fn sparse<T: Layerable>(config: &T) -> Value {
    diff(&to_json(config), &to_json(&T::default())).unwrap_or_else(|| Value::Object(Map::new()))
}

fn diff(value: &Value, default: &Value) -> Option<Value> {
    match (value, default) {
        (Value::Object(value), Value::Object(default)) => {
            let changed: Map<String, Value> = value
                .iter()
                .filter_map(|(key, v)| match default.get(key) {
                    Some(d) => diff(v, d).map(|v| (key.clone(), v)),
                    None => Some((key.clone(), v.clone())),
                })
                .collect();
            (!changed.is_empty()).then_some(Value::Object(changed))
        }
        _ if value == default => None,
        _ => Some(value.clone()),
    }
}

/// Every key the edits set, in order. A leaf is anything but a table: a value, or `null`.
pub fn leaves(edits: &Value) -> Vec<(KeyPath, Value)> {
    fn walk(value: &Value, path: &mut Vec<String>, out: &mut Vec<(KeyPath, Value)>) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    path.push(key.clone());
                    walk(value, path, out);
                    path.pop();
                }
            }
            leaf if !path.is_empty() => out.push((KeyPath(path.clone()), leaf.clone())),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(edits, &mut Vec::new(), &mut out);
    out
}

pub fn get<'a>(value: &'a Value, key: &KeyPath) -> Option<&'a Value> {
    key.0.iter().try_fold(value, |value, segment| value.get(segment))
}

/// A key of the edits, by itself: `None` when they don't set it.
pub fn edit<'a>(edits: &'a Value, key: &KeyPath) -> Option<&'a Value> {
    get(edits, key).filter(|value| !value.is_object())
}

/// Sets one key of the edits, replacing any edits of the keys under it.
pub fn set_edit(edits: &mut Value, key: &KeyPath, value: Value) {
    let Some((last, parents)) = key.0.split_last() else { return };
    let mut table = edits;
    for segment in parents {
        if !table.is_object() {
            *table = Value::Object(Map::new());
        }
        let map = table.as_object_mut().expect("made a table above");
        table = map.entry(segment.clone()).or_insert_with(|| Value::Object(Map::new()));
    }
    if !table.is_object() {
        *table = Value::Object(Map::new());
    }
    table.as_object_mut().expect("made a table above").insert(last.clone(), value);
}

/// Removes a key, and every key under it, from the edits, and the tables it leaves empty.
pub fn remove_edit(edits: &mut Value, key: &KeyPath) -> bool {
    fn remove(table: &mut Value, segments: &[String]) -> bool {
        let Some(map) = table.as_object_mut() else { return false };
        match segments {
            [] => false,
            [last] => map.remove(last).is_some(),
            [first, rest @ ..] => {
                let removed = map.get_mut(first).is_some_and(|child| remove(child, rest));
                if map.get(first).is_some_and(|child| child.as_object().is_some_and(Map::is_empty)) {
                    map.remove(first);
                }
                removed
            }
        }
    }
    remove(edits, &key.0)
}

/// The edits over a file's keys, as one file: what `export` writes.
pub fn merge(file: &Value, edits: &Value) -> Value {
    let mut merged = file.clone();
    for (key, value) in leaves(edits) {
        apply(&mut merged, &key, &value);
    }
    merged
}

fn apply(target: &mut Value, key: &KeyPath, value: &Value) {
    if value.is_null() {
        let Some((last, parents)) = key.0.split_last() else { return };
        if let Some(Value::Object(parent)) = parents.iter().try_fold(&mut *target, |t, s| t.get_mut(s)) {
            parent.remove(last);
        }
    } else {
        set_edit(target, key, value.clone());
    }
}
