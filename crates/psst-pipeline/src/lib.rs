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
    pub dictionary: bool,
}

impl Default for PipelineOptions {
    fn default() -> Self {
        Self { dictionary: true }
    }
}

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
