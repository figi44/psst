//! The stale-trace check (plan 7.3): recompute each case's engine request from the current code
//! and `evals/*.toml`, and compare it with what the recorded traces say was used.
//!
//! A trace records the model, the effective options and the prompt the engine built, not the
//! request itself, so the check compares against those:
//!
//! - **Model:** `trace.model` is `[model] model`, and `trace.revision` the pinned revision if
//!   there is one.
//! - **Language:** the engine transcribed in the profile's language.
//! - **Options:** every requested option is in `trace.options` with the requested value. The
//!   engine may add options it resolved, and may resolve `path = "auto"` to the path it took.
//! - **Bias:** with `eval.bias = false` the prompt is empty. Otherwise it's the style and the
//!   first `k` terms, rendered as in docs/protocol.md (`style + " " + terms.join(", ") + "."`),
//!   or a tail of the style alone when the style is over the budget. Since the engine drops terms
//!   only to fit 223 tokens, and a token is at least one byte, dropping a term whose prompt would
//!   have been under 223 bytes is stale too: the trace was recorded with a different term list.
//!
//! The prompt check can't see a change to the terms past the ones the engine kept, when the
//! prompt with the next term really is over the budget; that needs the tokenizer. Nor can the
//! options check see an option removed from the config (only `batch_size` can be unset): the
//! engine may report the default it used under the same key.

use psst_config::{ModelConfig, Profile};
use psst_dict::Dictionary;
use psst_engine_proto::Bias;
use psst_proto::EngineTrace;
use serde_json::Value;

use crate::cases::EvalSet;

/// The prompt budget in tokens, shared by style and terms (plan 5.3).
pub const PROMPT_BUDGET_TOKENS: usize = 223;

/// The parts of a `transcribe` request that shape the engine's output.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineRequest {
    pub language: String,
    pub bias: Bias,
    pub options: Value,
}

/// The request the server sends for a dictation with `profile` (plan 6.1 step 2): the profile's
/// language, its style and the dictionary's bias terms, or no bias at all for
/// `eval.bias = false`, and `[model.options]`.
pub fn engine_request(profile: &Profile, dictionary: &Dictionary, model: &ModelConfig, bias: bool) -> EngineRequest {
    let bias = if bias {
        Bias { style: profile.prompt_style.clone(), terms: dictionary.bias_terms(&profile.language) }
    } else {
        Bias::default()
    };
    EngineRequest { language: profile.language.clone(), bias, options: model.options.to_json() }
}

/// A recorded trace that no longer matches its recomputed request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleTrace {
    pub case: String,
    /// `engine_output` or `engine_output_raw`.
    pub trace: &'static str,
    pub reason: String,
}

/// Every stale trace in the set.
pub fn stale_traces(set: &EvalSet) -> anyhow::Result<Vec<StaleTrace>> {
    let model = &set.files.server.model;
    let mut stale = Vec::new();
    for case in &set.cases {
        let Some((output, raw)) = case.traces()? else { continue };
        let profile = set.profile_for(&case.lang)?;
        for (name, trace, bias) in [("engine_output", output, true), ("engine_output_raw", raw, false)] {
            let request = engine_request(&profile, &set.dictionary, model, bias);
            if let Err(reason) = check_trace(&request, model, trace) {
                stale.push(StaleTrace { case: case.id.clone(), trace: name, reason });
            }
        }
    }
    Ok(stale)
}

/// Whether `trace` could have come from `request`; the reason if not.
pub fn check_trace(request: &EngineRequest, model: &ModelConfig, trace: &EngineTrace) -> Result<(), String> {
    if trace.model != model.model {
        return Err(format!("recorded with model `{}`, configured `{}`", trace.model, model.model));
    }
    if let Some(revision) = &model.revision
        && &trace.revision != revision
    {
        return Err(format!("recorded with revision `{}`, pinned `{revision}`", trace.revision));
    }
    if trace.result.language != request.language {
        return Err(format!("transcribed in `{}`, the profile says `{}`", trace.result.language, request.language));
    }
    check_options(&request.options, &trace.options)?;
    check_prompt(&request.bias, &trace.prompt)
}

fn check_options(requested: &Value, recorded: &Value) -> Result<(), String> {
    let (Value::Object(requested), Value::Object(recorded)) = (requested, recorded) else {
        return Err(format!("options {recorded} aren't an object"));
    };
    for (key, want) in requested {
        match recorded.get(key) {
            Some(got) if got == want => {}
            Some(got) if key == "path" && want == "auto" && matches!(got.as_str(), Some("sequential" | "batched")) => {}
            Some(got) => return Err(format!("option `{key}` is {got} in the trace, {want} now")),
            None => return Err(format!("option `{key}` (now {want}) is missing from the trace")),
        }
    }
    Ok(())
}

/// The prompt the faster-whisper engine builds from a bias (docs/protocol.md), before its
/// token budget.
pub fn render_prompt(style: &str, terms: &[String]) -> String {
    let terms = (!terms.is_empty()).then(|| format!("{}.", terms.join(", ")));
    [(!style.is_empty()).then(|| style.to_owned()), terms].into_iter().flatten().collect::<Vec<_>>().join(" ")
}

/// Whether a prompt fits the budget whatever the tokenizer does: every token is at least one
/// byte, and the engine may add a leading space and a separator.
fn surely_fits(prompt: &str) -> bool {
    prompt.len() + 2 <= PROMPT_BUDGET_TOKENS
}

fn check_prompt(bias: &Bias, prompt: &str) -> Result<(), String> {
    if bias.style.is_empty() && bias.terms.is_empty() {
        return match prompt {
            "" => Ok(()),
            _ => Err(format!("recorded with prompt {prompt:?}, now none")),
        };
    }
    let terms = &bias.terms;
    if let Some(used) = (0..=terms.len()).find(|&k| render_prompt(&bias.style, &terms[..k]) == prompt) {
        if used < terms.len() && surely_fits(&render_prompt(&bias.style, &terms[..=used])) {
            return Err(format!(
                "the prompt has {used} of {} terms, but the next one (`{}`) fits the budget",
                terms.len(),
                terms[used]
            ));
        }
        return Ok(());
    }
    let style_cut = !prompt.is_empty() && prompt.len() < bias.style.len() && bias.style.ends_with(prompt);
    if style_cut && !surely_fits(&bias.style) {
        return Ok(());
    }
    Err(format!(
        "recorded with prompt {prompt:?}, which the current style and terms don't give (they start {:?})",
        truncate(&render_prompt(&bias.style, terms), 120)
    ))
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_owned(),
    }
}
