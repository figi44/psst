//! The history page: the dictations the server keeps, newest first, each shown as psst pasted it
//! with the dictionary's changes marked. A form in each dictation fixes a word in it: it adds the
//! right spelling to the dictionary (or teaches an existing word another way Whisper hears it), or
//! keeps a word the dictionary changed as Whisper heard it. Every change goes through the same
//! functions as `/api/dictionary`, and each one can be undone from the page.

use std::{collections::HashMap, sync::Arc};

use axum::{
    Form,
    extract::{
        Path, Query, State,
        rejection::{FormRejection, QueryRejection},
    },
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use jiff::{Timestamp, tz::TimeZone};
use minijinja::{Value, context, value::Serde};
use psst_dict::{fuzzy_key, is_common_word, term_id};
use psst_proto::{
    Final, HistoryId, Stage,
    api::{DictionaryTerm, DictionaryView, HistoryRow, Term, TermLayer, encode_segment},
};
use serde::{Deserialize, Serialize};

use super::{Viewer, chrome, message, render};
use crate::{
    api::ApiErr,
    dictionary::{self, Change},
    state::AppState,
};

type AppRef = Arc<AppState>;

/// Dictations shown before "Show older dictations"; with it, everything the server keeps.
const SHOWN: u32 = 20;
const ALL: u32 = 1000;

#[derive(Debug, Default, Deserialize)]
pub struct PageQuery {
    /// Any value shows every kept dictation.
    all: Option<String>,
    /// A fix that was just saved, to confirm in its dictation (see [`Done`]).
    fixed: Option<HistoryId>,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    heard: String,
    #[serde(default)]
    written: String,
    #[serde(default)]
    term: String,
    #[serde(default)]
    removed: String,
    #[serde(default)]
    created: String,
}

/// A fix as entered: the wrong words as Whisper heard them (or as the page shows them), how psst
/// should write them, and which button was pressed.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FixForm {
    #[serde(default)]
    heard: String,
    #[serde(default)]
    written: String,
    /// `add` (the default), `keep`, `move` or `anyway`.
    #[serde(default)]
    action: String,
    /// Set when the page showed every kept dictation, so the answer does too.
    #[serde(default)]
    all: String,
}

/// What a saved fix did, carried by the redirect so the page can confirm it and undo it.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Done {
    /// `add` (a new word), `extend` (another way Whisper hears a word), `move` (a heard spelling
    /// taken from another word), `keep` (a never rule) or `already`.
    #[serde(default)]
    kind: String,
    #[serde(default)]
    heard: String,
    #[serde(default)]
    written: String,
    /// The word a `move` took the spelling from, or the word a `keep` stopped.
    #[serde(default)]
    term: String,
    /// For `keep`: the heard spelling was one of the word's listed spellings, and was removed.
    #[serde(default)]
    removed: String,
    /// The fix created the word it wrote.
    #[serde(default)]
    created: String,
    /// The page showed every kept dictation, and keeps doing so.
    #[serde(default)]
    all: String,
}

/// One piece of a dictation as the page shows it.
#[derive(Debug, Clone, Serialize)]
struct Piece {
    /// `w` a word, `g` the spaces and punctuation between words, `fx` a word the dictionary
    /// changed, `fixed` words just fixed from this page.
    kind: &'static str,
    text: String,
    /// What Whisper heard: the same as `text`, except for `fx`.
    heard: String,
    /// For `fx`: the word that changed it, and how it matched (`exact`, `fuzzy` or `case`).
    #[serde(skip_serializing_if = "Option::is_none")]
    term: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    how: Option<&'static str>,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '\'' | '\u{2019}' | '+' | '#' | '-')
}

/// `text` as words and the gaps between them.
fn split(text: &str, out: &mut Vec<Piece>) {
    let mut push = |run: &str| {
        let word = run.chars().any(char::is_alphanumeric);
        out.push(Piece {
            kind: if word { "w" } else { "g" },
            text: run.into(),
            heard: run.into(),
            term: None,
            how: None,
        });
    };
    let mut start = 0;
    let mut current: Option<bool> = None;
    for (i, c) in text.char_indices() {
        let word = is_word_char(c);
        match current {
            Some(kind) if kind != word => {
                push(&text[start..i]);
                start = i;
                current = Some(word);
            }
            None => current = Some(word),
            _ => {}
        }
    }
    if current.is_some() {
        push(&text[start..]);
    }
}

fn changed(edit: &psst_proto::Edit, terms: &HashMap<String, String>) -> Piece {
    let how = match edit.rule.as_str() {
        "fuzzy" => "fuzzy",
        _ if edit.before.to_lowercase() == edit.after.to_lowercase() => "case",
        _ => "exact",
    };
    let term = edit.term.as_ref().and_then(|id| terms.get(id)).cloned().unwrap_or_else(|| edit.after.clone());
    Piece { kind: "fx", text: edit.after.clone(), heard: edit.before.clone(), term: Some(term), how: Some(how) }
}

/// The final text as pieces, with the dictionary's edits marked. The edits are laid over
/// `raw_text`; if they don't line up with the final text, each dictionary replacement is found in
/// the final text instead, which can't go wrong worse than marking nothing.
fn pieces(fin: &Final, terms: &HashMap<String, String>) -> Vec<Piece> {
    let mut edits: Vec<&psst_proto::Edit> = fin.edits.iter().collect();
    edits.sort_by_key(|edit| (edit.raw_span.start, edit.raw_span.end));
    let mut out = Vec::new();
    let mut shown = String::new();
    let mut pos = 0;
    let raw = &fin.raw_text;
    let laid_out = edits.iter().all(|edit| {
        let span = &edit.raw_span;
        if span.start < pos
            || span.end > raw.len()
            || !raw.is_char_boundary(span.start)
            || !raw.is_char_boundary(span.end)
        {
            return false;
        }
        split(&raw[pos..span.start], &mut out);
        shown.push_str(&raw[pos..span.start]);
        match edit.stage {
            Stage::Dictionary => out.push(changed(edit, terms)),
            _ => split(&edit.after, &mut out),
        }
        shown.push_str(&edit.after);
        pos = span.end;
        true
    });
    if laid_out {
        split(&raw[pos..], &mut out);
        shown.push_str(&raw[pos..]);
        if shown == fin.text {
            return out;
        }
    }
    out.clear();
    let mut cursor = 0;
    for edit in edits.iter().filter(|edit| edit.stage == Stage::Dictionary && !edit.after.is_empty()) {
        if let Some(at) = fin.text[cursor..].find(&edit.after) {
            split(&fin.text[cursor..cursor + at], &mut out);
            out.push(changed(edit, terms));
            cursor += at + edit.after.len();
        }
    }
    split(&fin.text[cursor..], &mut out);
    out
}

/// Spaces collapsed, ends trimmed, lowercase: how two phrases are compared.
fn norm(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// The pieces `phrase` covers, compared as Whisper heard them, then as shown.
fn find(pieces: &[Piece], phrase: &str) -> Option<(usize, usize)> {
    let want = norm(phrase);
    if want.is_empty() {
        return None;
    }
    for shown in [false, true] {
        for a in (0..pieces.len()).filter(|&a| pieces[a].kind != "g") {
            let mut text = String::new();
            for (b, piece) in pieces.iter().enumerate().skip(a) {
                text.push_str(if shown { &piece.text } else { &piece.heard });
                let got = norm(&text);
                if got == want && piece.kind != "g" {
                    return Some((a, b));
                }
                if got.len() > want.len() + 1 {
                    break;
                }
            }
        }
    }
    None
}

/// Words that a fix just saved, shown in place.
fn mark_fixed(pieces: &mut Vec<Piece>, done: &Done) {
    let Some((a, b)) = find(pieces, &done.heard) else { return };
    let text = if done.kind == "keep" { done.heard.clone() } else { done.written.clone() };
    let piece = Piece { kind: "fixed", text, heard: done.heard.clone(), term: None, how: None };
    pieces.splice(a..=b, [piece]);
}

/// "2 min ago", or the local time of day.
fn ago(at: Timestamp, now: Timestamp, tz: &TimeZone) -> String {
    let minutes = (now.as_second() - at.as_second()) / 60;
    match minutes {
        ..1 => "just now".into(),
        1..60 => format!("{minutes} min ago"),
        _ => at.to_zoned(tz.clone()).strftime("%H:%M").to_string(),
    }
}

/// "Today", "Yesterday", or the date.
fn day(at: Timestamp, now: Timestamp, tz: &TimeZone) -> String {
    let (date, today) = (at.to_zoned(tz.clone()).date(), now.to_zoned(tz.clone()).date());
    if date == today {
        "Today".into()
    } else if today.yesterday().ok() == Some(date) {
        "Yesterday".into()
    } else {
        at.to_zoned(tz.clone()).strftime("%A %-d %B").to_string()
    }
}

/// The form's state after a refused fix: the values entered, why, and what the page offers.
#[derive(Debug, Clone, Default, Serialize)]
struct Refusal {
    row: HistoryId,
    heard: String,
    written: String,
    error: String,
    /// `warn` (a choice to make) or `error`.
    level: &'static str,
    /// A button that goes ahead anyway: its `action` and label.
    offer: Option<(String, String)>,
}

fn page(state: &AppState, viewer: &Viewer, query: &PageQuery, refusal: Option<Refusal>) -> Response {
    let all = query.all.is_some();
    let rows = match state.db.history(if all { ALL } else { SHOWN + 1 }) {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!("cannot read history: {err:#}");
            return message(StatusCode::INTERNAL_SERVER_ERROR, "History is unavailable", "See the server's log.", None);
        }
    };
    let more = !all && rows.len() > SHOWN as usize;
    let devices: HashMap<String, String> =
        state.db.devices().unwrap_or_default().into_iter().map(|device| (device.id, device.name)).collect();
    let dictionary = state.live.dictionary();
    let terms: HashMap<String, String> = dictionary.terms().iter().map(|t| (t.id.clone(), t.text.clone())).collect();
    let (now, tz) = (Timestamp::now(), TimeZone::system());
    let done = query.fixed.map(|row| {
        let done = Done {
            kind: query.kind.clone(),
            heard: query.heard.clone(),
            written: query.written.clone(),
            term: query.term.clone(),
            removed: query.removed.clone(),
            created: query.created.clone(),
            all: if all { "1".into() } else { String::new() },
        };
        (row, done)
    });
    let active = refusal.as_ref().map(|r| r.row).or(done.as_ref().map(|(row, _)| *row));
    let newest = rows.first().map(|row| row.id);
    let mut days: Vec<(String, Vec<Value>)> = Vec::new();
    for row in rows.iter().take(if all { ALL as usize } else { SHOWN as usize }) {
        let Some(fin) = &row.r#final else { continue };
        let mut shown = pieces(fin, &terms);
        let confirm = done.as_ref().filter(|(id, _)| *id == row.id).map(|(_, done)| done);
        if let Some(done) = confirm.filter(|done| done.kind != "already") {
            mark_fixed(&mut shown, done);
        }
        let label = day(row.created_at, now, &tz);
        let device =
            devices.get(&row.device).cloned().unwrap_or_else(|| row.device.trim_start_matches("local:").to_owned());
        let entry = context! {
            id => row.id,
            ago => ago(row.created_at, now, &tz),
            when => row.created_at.to_string(),
            device,
            profile => (row.profile != "default").then(|| row.profile.clone()),
            language => fin.language.clone(),
            latest => Some(row.id) == newest,
            boxed => Some(row.id) == active,
            empty => fin.text.trim().is_empty(),
            pieces => Value::from(Serde(shown)),
            refusal => refusal.as_ref().filter(|r| r.row == row.id).map(|r| Value::from(Serde(r.clone()))),
            done => confirm.map(|done| context! {
                text => confirmation(done),
                undo => done.kind != "already",
                fields => Value::from(Serde(done.clone())),
            }),
        };
        match days.last_mut() {
            Some((last, entries)) if *last == label => entries.push(entry),
            _ => days.push((label, vec![entry])),
        }
    }
    let known: Vec<String> = dictionary.terms().iter().map(|t| t.text.clone()).collect();
    let status = if refusal.is_some() { StatusCode::UNPROCESSABLE_ENTITY } else { StatusCode::OK };
    let ctx = context! {
        days => days.into_iter().map(|(label, entries)| context! { label, entries }).collect::<Vec<_>>(),
        more,
        all,
        known,
        kept_days => state.server.history_days,
        ..chrome(state, viewer, "history")
    };
    render(status, "history.html", ctx)
}

/// What a saved fix does from now on.
fn confirmation(done: &Done) -> String {
    let (heard, written, term) = (&done.heard, &done.written, &done.term);
    match done.kind.as_str() {
        "keep" => format!("{term} won't replace \u{201c}{heard}\u{201d} from your next dictation."),
        "move" => format!("\u{201c}{heard}\u{201d} now becomes {written} instead of {term}, from your next dictation."),
        "already" => {
            format!("Already fixed: psst writes {written} for \u{201c}{heard}\u{201d} from your next dictation.")
        }
        _ if norm(heard) == norm(written) => format!("psst writes {written} from your next dictation."),
        _ => format!("psst writes {written} for \u{201c}{heard}\u{201d} from your next dictation."),
    }
}

/// `GET /history`.
pub async fn show(
    viewer: Viewer,
    State(state): State<AppRef>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let Query(query) = query.unwrap_or_default();
    page(&state, &viewer, &query, None)
}

/// The page as it was before a form was sent: every kept dictation when `all` is set.
fn same_view(all: &str) -> PageQuery {
    PageQuery { all: (!all.is_empty()).then(|| all.to_owned()), ..PageQuery::default() }
}

fn back(id: HistoryId, done: &Done) -> Response {
    let mut to = format!("/history?fixed={id}&kind={}", encode_segment(&done.kind));
    for (key, value) in [
        ("heard", &done.heard),
        ("written", &done.written),
        ("term", &done.term),
        ("removed", &done.removed),
        ("created", &done.created),
        ("all", &done.all),
    ] {
        if !value.is_empty() {
            to.push_str(&format!("&{key}={}", encode_segment(value)));
        }
    }
    Redirect::to(&format!("{to}#d-{id}")).into_response()
}

fn entry<'a>(view: &'a DictionaryView, id: &str) -> Option<&'a DictionaryTerm> {
    view.terms.iter().find(|t| t.id == id)
}

/// `term` as `entry`'s new version, or back to the file's version when it's the same.
fn saving(entry: &DictionaryTerm, term: Term) -> Change {
    if entry.layer == TermLayer::Replaced && entry.file.as_ref() == Some(&term) {
        Change::Reset(entry.id.clone())
    } else {
        Change::Replace(entry.id.clone(), term.into())
    }
}

fn save(state: &AppState, entry: &DictionaryTerm, term: Term) -> Result<(), ApiErr> {
    dictionary::change_term(&state.live, &state.db, saving(entry, term)).map(drop)
}

fn spelling(heard: &str) -> String {
    norm(heard)
}

fn without(term: &Term, heard: &str) -> Term {
    let heard = spelling(heard);
    Term { sounds_like: term.sounds_like.iter().filter(|s| norm(s) != heard).cloned().collect(), ..term.clone() }
}

fn with(term: &Term, heard: &str) -> Term {
    let mut term = term.clone();
    if norm(&term.text) != spelling(heard) && !term.sounds_like.iter().any(|s| norm(s) == spelling(heard)) {
        term.sounds_like.push(spelling(heard));
    }
    term
}

/// The active word whose own spelling or a listed one is `heard`, other than `except`.
fn claimant<'a>(view: &'a DictionaryView, heard: &str, except: &str) -> Option<&'a DictionaryTerm> {
    let heard = spelling(heard);
    view.terms.iter().filter(|t| t.active && t.id != except).find(|t| {
        t.term
            .as_ref()
            .is_some_and(|term| norm(&term.text) == heard || term.sounds_like.iter().any(|s| norm(s) == heard))
    })
}

/// `POST /history/{id}/fix`.
pub async fn fix(
    viewer: Viewer,
    State(state): State<AppRef>,
    Path(id): Path<HistoryId>,
    form: Result<Form<FixForm>, FormRejection>,
) -> Response {
    let form = form.map(|Form(form)| form).unwrap_or_default();
    let row = match state.db.history_row(id) {
        Ok(Some(row)) if row.r#final.is_some() => row,
        Ok(_) => return message(StatusCode::NOT_FOUND, "Not found", "That dictation isn't kept any more.", None),
        Err(err) => {
            tracing::error!("cannot read history row {id}: {err:#}");
            return message(StatusCode::INTERNAL_SERVER_ERROR, "Not saved", "See the server's log.", None);
        }
    };
    match apply(&state, &row, &form) {
        Ok(done) => back(id, &Done { all: form.all, ..done }),
        Err(refusal) => page(&state, &viewer, &same_view(&form.all), Some(*refusal)),
    }
}

fn apply(state: &AppState, row: &HistoryRow, form: &FixForm) -> Result<Done, Box<Refusal>> {
    let fin = row.r#final.as_ref().expect("a finished row");
    let terms: HashMap<String, String> =
        state.live.dictionary().terms().iter().map(|t| (t.id.clone(), t.text.clone())).collect();
    let shown = pieces(fin, &terms);
    let refuse = |error: String, level: &'static str, offer: Option<(String, String)>| {
        Box::new(Refusal {
            row: row.id,
            heard: form.heard.trim().to_owned(),
            written: form.written.trim().to_owned(),
            error,
            level,
            offer,
        })
    };
    // Typed words may be the ones the page shows; a fix needs what Whisper heard.
    let (heard, edit) = match find(&shown, &form.heard) {
        Some((a, b)) if a == b && shown[a].kind == "fx" => (shown[a].heard.clone(), Some(shown[a].clone())),
        Some((a, b)) => (shown[a..=b].iter().map(|p| p.heard.as_str()).collect::<String>(), None),
        None => (form.heard.trim().to_owned(), None),
    };
    let heard = heard.split_whitespace().collect::<Vec<_>>().join(" ");
    let written = form.written.split_whitespace().collect::<Vec<_>>().join(" ");
    if heard.is_empty() {
        return Err(refuse("Select the words psst got wrong, or type them.".into(), "error", None));
    }
    let view = state.live.layered().view(&state.live.dictionary());

    if form.action == "keep" {
        let Some(edit) = edit else {
            return Err(refuse("Only a word your dictionary changed can be kept as heard.".into(), "error", None));
        };
        let word = edit.term.clone().unwrap_or_default();
        let id = term_id(&word);
        dictionary::add_never(&state.live, &state.db, &word, &heard)
            .map_err(|err| refuse(err.message().into(), "error", None))?;
        let mut removed = String::new();
        if let Some(entry) = entry(&view, &id).filter(|e| e.layer != TermLayer::Hidden)
            && let Some(term) = &entry.term
            && term.sounds_like.iter().any(|s| norm(s) == spelling(&heard))
        {
            save(state, entry, without(term, &heard)).map_err(|err| refuse(err.message().into(), "error", None))?;
            removed = "1".into();
        }
        return Ok(Done { kind: "keep".into(), heard, term: word, removed, ..Done::default() });
    }

    if written.is_empty() || norm(&written) == norm(&heard) && written == heard {
        return Err(refuse("Type how psst should write it.".into(), "error", None));
    }
    let id = term_id(&written);
    let existing = entry(&view, &id).filter(|e| e.term.is_some());
    if let Some(entry) = existing.filter(|e| e.active && e.layer != TermLayer::Hidden) {
        let term = entry.term.as_ref().expect("filtered");
        if norm(&term.text) == spelling(&heard) || term.sounds_like.iter().any(|s| norm(s) == spelling(&heard)) {
            return Ok(Done { kind: "already".into(), heard, written: term.text.clone(), ..Done::default() });
        }
    }
    let common = !heard.contains(' ') && is_common_word(&fin.language, &heard);
    if common && form.action != "anyway" && claimant(&view, &heard, &id).is_none() {
        return Err(refuse(
            format!(
                "\u{201c}{heard}\u{201d} is a common word, so psst would change it every time you say it. Include the words around it."
            ),
            "warn",
            Some(("anyway".into(), format!("Always write {written} for \u{201c}{heard}\u{201d}"))),
        ));
    }

    let mut done = Done { heard: heard.clone(), written: written.clone(), ..Done::default() };
    // A move takes the spelling from the word that has it, in the same change that gives it on.
    let mut changes = Vec::new();
    if form.action == "move"
        && let Some(giver) = claimant(&view, &heard, &id)
        && let Some(term) = &giver.term
    {
        changes.push(saving(giver, without(term, &heard)));
        done.kind = "move".into();
        done.term = term.text.clone();
    }
    changes.push(match entry(&view, &id).and_then(|e| e.term.clone().map(|term| (e, term))) {
        Some((entry, term)) if entry.layer != TermLayer::Hidden => {
            done.written = term.text.clone();
            saving(entry, with(&term, &heard))
        }
        Some((_, term)) => {
            done.written = term.text.clone();
            Change::Add(with(&term, &heard).into())
        }
        None => {
            let sounds_like = if norm(&written) == spelling(&heard) { vec![] } else { vec![spelling(&heard)] };
            done.created = "1".into();
            if done.kind.is_empty() {
                done.kind = "add".into();
            }
            Change::Add(
                Term {
                    text: written.clone(),
                    sounds_like,
                    languages: "any".into(),
                    fuzzy: true,
                    pin: false,
                    notes: None,
                }
                .into(),
            )
        }
    });
    if let Err(err) = dictionary::change_terms(&state.live, &state.db, changes) {
        if done.kind != "move"
            && let Some(giver) = claimant(&view, &heard, &id).and_then(|g| g.term.as_ref())
        {
            return Err(refuse(
                format!(
                    "{} already catches \u{201c}{heard}\u{201d}. Moving it means psst writes {written} instead.",
                    giver.text
                ),
                "warn",
                Some(("move".into(), format!("Move \u{201c}{heard}\u{201d} to {written}"))),
            ));
        }
        return Err(refuse(err.message().into(), "error", None));
    }
    if done.kind.is_empty() {
        done.kind = "extend".into();
    }
    Ok(done)
}

/// `POST /history/{id}/undo`: takes back one saved fix.
pub async fn undo(
    viewer: Viewer,
    State(state): State<AppRef>,
    Path(id): Path<HistoryId>,
    form: Result<Form<Done>, FormRejection>,
) -> Response {
    let done = form.map(|Form(done)| done).unwrap_or_default();
    match take_back(&state, &done) {
        Ok(()) => {
            let all = if done.all.is_empty() { "" } else { "?all=1" };
            Redirect::to(&format!("/history{all}#d-{id}")).into_response()
        }
        Err(err) => {
            let refusal = Refusal {
                row: id,
                error: format!("Couldn't undo: {}", err.message()),
                level: "error",
                ..Refusal::default()
            };
            page(&state, &viewer, &same_view(&done.all), Some(refusal))
        }
    }
}

fn take_back(state: &AppState, done: &Done) -> Result<(), ApiErr> {
    let view = state.live.layered().view(&state.live.dictionary());
    let heard = &done.heard;
    match done.kind.as_str() {
        "keep" => {
            let id = term_id(&done.term);
            let key = fuzzy_key(heard);
            if let Some(rule) = view.never.iter().find(|rule| rule.term == id && rule.span_key == key) {
                dictionary::remove_never(&state.live, &state.db, rule.id)?;
            }
            if !done.removed.is_empty()
                && let Some(entry) = entry(&view, &id)
                && let Some(term) = &entry.term
            {
                save(state, entry, with(term, heard))?;
            }
        }
        "add" | "extend" | "move" => {
            // A move goes back in one change, like it was made.
            let mut changes = Vec::new();
            let id = term_id(&done.written);
            if let Some(entry) = entry(&view, &id)
                && let Some(term) = &entry.term
            {
                let rest = without(term, heard);
                changes.push(
                    if !done.created.is_empty() && entry.layer == TermLayer::Added && rest.sounds_like.is_empty() {
                        Change::Remove(id)
                    } else {
                        saving(entry, rest)
                    },
                );
            }
            if done.kind == "move"
                && let Some(entry) = entry(&view, &term_id(&done.term))
                && let Some(term) = &entry.term
            {
                changes.push(saving(entry, with(term, heard)));
            }
            dictionary::change_terms(&state.live, &state.db, changes)?;
        }
        _ => {}
    }
    Ok(())
}
