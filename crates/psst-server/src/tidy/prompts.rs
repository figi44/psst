use std::sync::OnceLock;

use serde::Deserialize;

/// A tidy-up style: its prompt, and the settings it was tuned with on `examples.jsonl`.
#[derive(Debug)]
pub struct Variant {
    pub id: &'static str,
    pub name: &'static str,
    pub about: &'static str,
    text: &'static str,
    pub model: &'static str,
    pub effort: &'static str,
    pub tier: &'static str,
}

impl Variant {
    pub fn text(&self) -> &'static str {
        self.text.trim()
    }
}

/// GPT-6.1 Terra without reasoning, on Fast: it passed every example with both styles, at about
/// 1 s, where Luna, GPT-6 Sol and Terra with reasoning were no better and slower on long texts.
pub const VARIANTS: &[Variant] = &[
    Variant {
        id: "light",
        name: "Light",
        about: "fixes punctuation, fillers and spoken corrections, and keeps your words",
        text: include_str!("../../tidy-prompts/light.txt"),
        model: "gpt-6.1-terra",
        effort: "none",
        tier: "priority",
    },
    Variant {
        id: "polished",
        name: "Polished",
        about: "also tightens the wording and formats lists, paragraphs and messages",
        text: include_str!("../../tidy-prompts/polished.txt"),
        model: "gpt-6.1-terra",
        effort: "none",
        tier: "priority",
    },
];

pub const DEFAULT: &str = "polished";

pub fn find(id: &str) -> Option<&'static Variant> {
    VARIANTS.iter().find(|v| v.id == id)
}

pub fn default() -> &'static Variant {
    find(DEFAULT).expect("the default style exists")
}

pub fn with_vocabulary(prompt: &str, words: &[String]) -> String {
    if words.is_empty() {
        return prompt.to_owned();
    }
    format!(
        "{prompt}\n\nSpell these words exactly as written, keeping their case even at the start of a sentence, and never swap them for similar-sounding words: {}.",
        words.join(", ")
    )
}

#[derive(Debug, Deserialize)]
pub struct Example {
    pub id: String,
    pub text: String,
}

pub fn examples() -> &'static [Example] {
    static EXAMPLES: OnceLock<Vec<Example>> = OnceLock::new();
    EXAMPLES.get_or_init(|| {
        include_str!("../../tidy-prompts/examples.jsonl")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("the bundled examples parse"))
            .collect()
    })
}
