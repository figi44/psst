use std::{fmt::Write as _, process::ExitCode};

use clap::{Args, Subcommand};
use psst_client::api::{ApiCallError, ApiClient, Method};
use psst_proto::{
    Edit, Final, Stage,
    api::{
        ApiErrorCode, DictionaryTerm, DictionaryTest, DictionaryView, NeverRule, NewNeverRule, Term, TermChange,
        TermLayer,
        routes::{self, with_id},
    },
};
use serde::Serialize;

use crate::server_api;

#[derive(Debug, Args)]
pub struct DictArgs {
    /// Print the server's answers as JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: DictCommand,
}

#[derive(Debug, Subcommand)]
enum DictCommand {
    /// List every term with where its version comes from, the never rules, and warnings.
    List,
    /// Add a term; with --replace, replace the term with its identity if there is one.
    Add {
        /// The exact spelling to produce.
        text: String,
        /// Another spelling the engine produces for it; repeat for several.
        #[arg(long = "sounds-like", value_name = "SPELLING")]
        sounds_like: Vec<String>,
        /// Only for sessions in this language (for example `en` or `cs`), instead of all.
        #[arg(long, value_name = "LANG")]
        lang: Option<String>,
        /// Turn off fuzzy matching for it.
        #[arg(long)]
        no_fuzzy: bool,
        /// Always offer it to the engine first.
        #[arg(long)]
        pin: bool,
        /// Free text for people.
        #[arg(long)]
        notes: Option<String>,
        /// Replace the term with this identity (from dictionary.toml or added) instead of failing.
        #[arg(long)]
        replace: bool,
    },
    /// Hide a term that dictionary.toml has, or remove an added one.
    Rm { term: String },
    /// Drop the database's entry for a term, bringing back dictionary.toml's version (if any).
    Reset { term: String },
    /// Never let TERM replace SPAN, exactly or fuzzily; or remove the rule with --remove ID.
    Never {
        #[arg(required_unless_present = "remove")]
        term: Option<String>,
        #[arg(required_unless_present = "remove")]
        span: Option<String>,
        /// The id of a rule to remove, as `psst dict list` shows it.
        #[arg(long, value_name = "ID", conflicts_with_all = ["term", "span"])]
        remove: Option<u64>,
    },
    /// Show what the server makes of TEXT, as if it had been dictated.
    Test {
        text: String,
        /// The session language; the default profile's when absent.
        #[arg(long, value_name = "LANG")]
        lang: Option<String>,
    },
}

pub fn run(args: DictArgs) -> anyhow::Result<ExitCode> {
    let api = server_api::connect()?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime
        .block_on(execute(&api, args.json, args.command))
        .map_err(|err| server_api::describe(err, api.endpoint()))?;
    Ok(ExitCode::SUCCESS)
}

async fn execute(api: &ApiClient, json: bool, command: DictCommand) -> Result<(), ApiCallError> {
    match command {
        DictCommand::List => {
            let view: DictionaryView = api.call(Method::Get, routes::DICTIONARY, None::<&()>).await?;
            print(json, &view, list);
        }
        DictCommand::Add { text, sounds_like, lang, no_fuzzy, pin, notes, replace } => {
            let term = Term {
                text,
                sounds_like,
                languages: lang.unwrap_or_else(|| "any".into()),
                fuzzy: !no_fuzzy,
                pin,
                notes,
            };
            let put = with_id(routes::DICTIONARY_TERM, &term.text);
            let change: TermChange = match replace {
                false => api.call(Method::Post, routes::DICTIONARY_TERMS, Some(&term)).await?,
                true => match api.call(Method::Put, &put, Some(&term)).await {
                    Err(ApiCallError::Api { error, .. }) if error.code == ApiErrorCode::NotFound => {
                        api.call(Method::Post, routes::DICTIONARY_TERMS, Some(&term)).await?
                    }
                    other => other?,
                },
            };
            print(json, &change, |change| changed(change, &term.text));
        }
        DictCommand::Rm { term } => {
            let change: TermChange =
                api.call(Method::Delete, &with_id(routes::DICTIONARY_TERM, &term), None::<&()>).await?;
            print(json, &change, |change| changed(change, &term));
        }
        DictCommand::Reset { term } => {
            let path = with_id(routes::DICTIONARY_TERM_RESET, &term);
            let change: TermChange = api.call(Method::Post, &path, None::<&()>).await?;
            print(json, &change, |change| changed(change, &term));
        }
        DictCommand::Never { remove: Some(id), .. } => {
            api.call_empty(Method::Delete, &with_id(routes::DICTIONARY_NEVER_RULE, &id.to_string()), None::<&()>)
                .await?;
            if !json {
                println!("removed never rule {id}");
            }
        }
        DictCommand::Never { term, span, remove: None } => {
            let rule = NewNeverRule {
                term: term.expect("clap requires a term without --remove"),
                span: span.expect("clap requires a span without --remove"),
            };
            let rule: NeverRule = api.call(Method::Post, routes::DICTIONARY_NEVER, Some(&rule)).await?;
            print(json, &rule, |rule| format!("never rule {}: {}\n", rule.id, describe_never(rule)));
        }
        DictCommand::Test { text, lang } => {
            let test = DictionaryTest { text, language: lang };
            let fin: Final = api.call(Method::Post, routes::DICTIONARY_TEST, Some(&test)).await?;
            print(json, &fin, tested);
        }
    }
    Ok(())
}

fn print<T: Serialize>(json: bool, value: &T, human: impl FnOnce(&T) -> String) {
    if json {
        println!("{}", serde_json::to_string_pretty(value).expect("API types serialise"));
    } else {
        print!("{}", human(value));
    }
}

fn name(term: &DictionaryTerm) -> &str {
    term.term.as_ref().map_or(&term.id, |t| t.text.as_str())
}

fn layer_name(layer: TermLayer) -> &'static str {
    match layer {
        TermLayer::File => "file",
        TermLayer::Added => "added",
        TermLayer::Replaced => "replaced",
        TermLayer::Hidden => "hidden",
    }
}

fn options(term: &Term) -> String {
    let mut options = Vec::new();
    if term.pin {
        options.push("pin".to_owned());
    }
    if !term.fuzzy {
        options.push("no fuzzy".to_owned());
    }
    if let Some(notes) = &term.notes {
        options.push(format!("notes: {notes}"));
    }
    options.join(", ")
}

fn describe_never(rule: &NeverRule) -> String {
    let inactive = if rule.active { "" } else { " (inactive: the term is hidden or gone)" };
    format!("`{}` never replaces {:?}{inactive}", rule.term, rule.span)
}

fn list(view: &DictionaryView) -> String {
    let rows: Vec<[String; 5]> = view
        .terms
        .iter()
        .map(|entry| {
            let term = entry.term.as_ref();
            let mut extra = term.map(options).unwrap_or_default();
            if let Some(problem) = &entry.problem {
                if !extra.is_empty() {
                    extra.push_str(", ");
                }
                extra.push_str(&format!("left out: {problem}"));
            }
            [
                name(entry).to_owned(),
                layer_name(entry.layer).to_owned(),
                term.map(|t| t.sounds_like.join(", ")).unwrap_or_default(),
                term.map(|t| t.languages.clone()).unwrap_or_default(),
                extra,
            ]
        })
        .collect();
    let mut out = String::new();
    if rows.is_empty() {
        out.push_str("The dictionary is empty.\n");
    } else {
        let header = ["TERM", "FROM", "SOUNDS LIKE", "LANG", ""].map(String::from);
        let mut widths = [0; 4];
        for row in std::iter::once(&header).chain(&rows) {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.chars().count());
            }
        }
        for row in std::iter::once(&header).chain(&rows) {
            let mut line = String::new();
            for (width, cell) in widths.iter().zip(row) {
                let _ = write!(line, "{cell:<width$}  ");
            }
            line.push_str(&row[4]);
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }
    if !view.never.is_empty() {
        out.push_str("\nNever rules:\n");
        for rule in &view.never {
            let _ = writeln!(out, "  {:>3}  {}", rule.id, describe_never(rule));
        }
    }
    if !view.warnings.is_empty() {
        out.push_str("\nWarnings:\n");
        for warning in &view.warnings {
            let _ = writeln!(out, "  {warning}");
        }
    }
    out
}

fn changed(change: &TermChange, asked: &str) -> String {
    let mut out = match &change.term {
        None => format!("removed `{asked}`\n"),
        Some(term) => {
            let what = match (term.layer, term.active) {
                (TermLayer::Added, true) => "added",
                (TermLayer::Replaced, true) => "replaces dictionary.toml's version",
                (TermLayer::File, _) => "is dictionary.toml's version again",
                (TermLayer::Hidden, _) => "is hidden; `psst dict reset` brings it back",
                (TermLayer::Added | TermLayer::Replaced, false) => "is saved, but left out",
            };
            format!("`{}` {what}\n", name(term))
        }
    };
    for warning in &change.warnings {
        let _ = writeln!(out, "warning: {warning}");
    }
    out
}

/// Quoted so spacing shows.
fn tested(fin: &Final) -> String {
    let mut out = format!("{:?}\n", fin.text);
    for edit in &fin.edits {
        let _ = writeln!(out, "  {}", describe_edit(edit));
    }
    out
}

fn describe_edit(edit: &Edit) -> String {
    let stage = match edit.stage {
        Stage::Guard => "guard",
        Stage::Dictionary => "dictionary",
        Stage::Rules => "rules",
    };
    let term = edit.term.as_ref().map(|t| format!(" (`{t}`)")).unwrap_or_default();
    format!("{stage} {}{term}: {:?} → {:?}", edit.rule, edit.before, edit.after)
}
