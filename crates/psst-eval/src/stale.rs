use psst_config::{ModelConfig, Profile};
use psst_dict::Dictionary;
use psst_engine_proto::Bias;
use psst_proto::EngineTrace;
use serde_json::Value;

use crate::cases::EvalSet;

/// The faster-whisper engine's `BUDGET` (`bias.py`).
pub const PROMPT_BUDGET_TOKENS: usize = 223;

#[derive(Debug, Clone, PartialEq)]
pub struct EngineRequest {
    pub language: String,
    pub bias: Bias,
    pub options: Value,
}

pub fn engine_request(profile: &Profile, dictionary: &Dictionary, model: &ModelConfig, bias: bool) -> EngineRequest {
    let bias = if bias {
        Bias { style: profile.prompt_style.clone(), terms: dictionary.bias_terms(&profile.language) }
    } else {
        Bias::default()
    };
    EngineRequest { language: profile.language.clone(), bias, options: model.options.to_json() }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleTrace {
    pub case: String,
    pub trace: &'static str,
    pub reason: String,
}

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

/// The engine may add options. An option deleted from `ModelOptions` goes unseen, since the
/// engine may still report the value it used.
fn check_options(requested: &Value, recorded: &Value) -> Result<(), String> {
    let (Value::Object(requested), Value::Object(recorded)) = (requested, recorded) else {
        return Err(format!("options {recorded} aren't an object"));
    };
    for (key, want) in requested {
        match recorded.get(key) {
            Some(got) if got == want => {}
            Some(got) => return Err(format!("option `{key}` is {got} in the trace, {want} now")),
            None => return Err(format!("option `{key}` (now {want}) is missing from the trace")),
        }
    }
    Ok(())
}

/// Must render a bias as the faster-whisper engine does, before its token budget.
pub fn render_prompt(style: &str, terms: &[String]) -> String {
    let terms = (!terms.is_empty()).then(|| format!("{}.", terms.join(", ")));
    [(!style.is_empty()).then(|| style.to_owned()), terms].into_iter().flatten().collect::<Vec<_>>().join(" ")
}

/// Every token is at least one byte, and the engine may add a leading space and a separator.
fn surely_fits(prompt: &str) -> bool {
    prompt.len() + 2 <= PROMPT_BUDGET_TOKENS
}

/// The engine drops terms only to fit the budget, so a trace that dropped a term that surely fits
/// was recorded with a different term list. A change to terms past a cut that really is over
/// the budget goes unseen; that needs the tokenizer.
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
