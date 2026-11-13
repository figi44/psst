//! The text pipeline (plan 6.4): guard → dictionary → rules. Pure; the server and every eval
//! call the same [`run`].
//!
//! 1. **`raw_text`** is the segment texts, each trimmed, joined with single spaces. It's built
//!    before anything is removed.
//! 2. **Tokens** split `raw_text` on whitespace and keep the spacing. Engine words are mapped onto
//!    them by character offsets; a segment whose words don't spell out its text gets no word
//!    annotations, and the text is never rebuilt from words.
//! 3. **The guard** drops everything when the engine's VAD found no speech, and a segment that is a
//!    phrase Whisper invents on silence when its speech evidence is weak.
//! 4. **The dictionary** (with [`PipelineOptions::dictionary`]) replaces exact spellings, then,
//!    in English, spans close to a `fuzzy` term, and protects every token it accepts. The
//!    tokens' `prob` sets how close a fuzzy match has to be.
//! 5. **Rules** (plan 8) run in the profile's order and leave protected tokens alone.
//! 6. **Assembly:** [`Final::text`] is the tokens with their spacing.
//!
//! The session's language, which picks the dictionary's terms, the common words and the guard's
//! phrases, is the language the engine reports, or the profile's if it reports none. Fillers come
//! from the profile.
//!
//! Every change is an [`Edit`](psst_proto::Edit) with its UTF-8 span in `raw_text`, in pipeline
//! order. A dictionary edit covers exactly the characters it replaced; a rules edit covers the raw
//! span of every token it touches (the spaces it removed, for `whitespace`).

mod dictionary;
mod draft;
mod guard;
mod rules;

use psst_config::Profile;
use psst_dict::Dictionary;
use psst_proto::{EngineTrace, Final};

use crate::draft::Draft;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineOptions {
    /// Apply the dictionary. `false` for eval raw runs.
    pub dictionary: bool,
}

impl Default for PipelineOptions {
    fn default() -> Self {
        Self { dictionary: true }
    }
}

/// Turns an engine's trace into the final text for a profile.
pub fn run(trace: &EngineTrace, profile: &Profile, dict: &Dictionary, opts: &PipelineOptions) -> Final {
    let result = &trace.result;
    let language = match result.language.as_str() {
        "" => profile.language.as_str(),
        language => language,
    };
    let mut draft = Draft::new(result);
    guard::apply(&mut draft, result, language);
    if opts.dictionary {
        dictionary::apply(&mut draft, dict, language);
    }
    rules::apply(&mut draft, &profile.rules, &profile.fillers);
    draft.finish(result.language.clone())
}
