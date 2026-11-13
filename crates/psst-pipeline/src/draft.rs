use std::ops::Range;

use psst_proto::{Edit, EngineResult, Final, Word};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Token {
    /// Empty once removed: `finish` doesn't skip removed tokens.
    pub text: String,
    pub space: String,
    /// Grows when a dictionary match merges tokens.
    pub raw: Range<usize>,
    pub raw_space: Range<usize>,
    pub prob: Option<f32>,
    pub start_ms: Option<u32>,
    pub end_ms: Option<u32>,
    pub removed: bool,
    pub protected: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct SegmentTokens {
    /// Into `EngineResult::segments`, which also has the segments without text.
    pub index: usize,
    pub tokens: Range<usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct Draft {
    pub raw: String,
    pub segments: Vec<SegmentTokens>,
    pub tokens: Vec<Token>,
    pub tail: String,
    pub edits: Vec<Edit>,
}

impl Draft {
    pub fn new(result: &EngineResult) -> Self {
        let mut raw = String::new();
        let mut segments = Vec::new();
        let mut tokens: Vec<Token> = Vec::new();
        for (index, segment) in result.segments.iter().enumerate() {
            let text = segment.text.trim();
            if text.is_empty() {
                continue;
            }
            let mut space_start = raw.len();
            if !raw.is_empty() {
                raw.push(' ');
            }
            let base = raw.len();
            raw.push_str(text);
            let first = tokens.len();
            for range in non_whitespace(text) {
                let range = base + range.start..base + range.end;
                tokens.push(Token {
                    text: raw[range.clone()].to_owned(),
                    space: raw[space_start..range.start].to_owned(),
                    raw_space: space_start..range.start,
                    raw: range.clone(),
                    prob: None,
                    start_ms: None,
                    end_ms: None,
                    removed: false,
                    protected: false,
                });
                space_start = range.end;
            }
            if let Some(words) = &segment.words {
                annotate(&mut tokens[first..], words);
            }
            segments.push(SegmentTokens { index, tokens: first..tokens.len() });
        }
        Self { raw, segments, tokens, tail: String::new(), edits: Vec::new() }
    }

    pub fn overlapping(&self, range: &Range<usize>) -> Range<usize> {
        let start = self.tokens.partition_point(|t| t.raw.end <= range.start);
        let end = start + self.tokens[start..].partition_point(|t| t.raw.start < range.end);
        start..end
    }

    pub fn has_text(&self) -> bool {
        self.tokens.iter().any(|t| !t.removed)
    }

    pub fn finish(self, language: String) -> Final {
        let mut text = String::with_capacity(self.raw.len() + self.tail.len());
        for token in &self.tokens {
            text.push_str(&token.space);
            text.push_str(&token.text);
        }
        text.push_str(&self.tail);
        Final { text, raw_text: self.raw, edits: self.edits, language }
    }
}

fn non_whitespace(text: &str) -> impl Iterator<Item = Range<usize>> + '_ {
    let mut start = None;
    text.char_indices().chain(std::iter::once((text.len(), ' '))).filter_map(move |(i, c)| {
        match (c.is_whitespace(), start) {
            (true, Some(s)) => {
                start = None;
                Some(s..i)
            }
            (false, None) => {
                start = Some(i);
                None
            }
            _ => None,
        }
    })
}

/// Whisper's words don't follow whitespace ("3" + ".14" is one token), so the word texts are
/// walked against the tokens. Words that don't spell out exactly the segment's text annotate
/// nothing; the text is never rebuilt from words.
fn annotate(tokens: &mut [Token], words: &[Word]) {
    let mut owner: Vec<usize> = Vec::new();
    let mut chars: Vec<char> = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        for c in token.text.chars() {
            owner.push(i);
            chars.push(c);
        }
    }
    let mut at = 0;
    let mut spans: Vec<(Range<usize>, &Word)> = Vec::with_capacity(words.len());
    for word in words {
        let start = at;
        for c in word.text.chars().filter(|c| !c.is_whitespace()) {
            if chars.get(at) != Some(&c) {
                return;
            }
            at += 1;
        }
        if at > start {
            spans.push((start..at, word));
        }
    }
    if at != chars.len() {
        return;
    }
    for (span, word) in spans {
        for token in &mut tokens[owner[span.start]..=owner[span.end - 1]] {
            token.prob = Some(token.prob.map_or(word.prob, |p| p.min(word.prob)));
            token.start_ms = Some(token.start_ms.map_or(word.start_ms, |t| t.min(word.start_ms)));
            token.end_ms = Some(token.end_ms.map_or(word.end_ms, |t| t.max(word.end_ms)));
        }
    }
}

#[cfg(test)]
mod tests {
    use psst_proto::{EngineResult, Segment, Word};

    use super::Draft;

    fn word(text: &str, start_ms: u32, end_ms: u32, prob: f32) -> Word {
        Word { text: text.into(), start_ms, end_ms, prob }
    }

    fn draft(text: &str, words: Vec<Word>) -> Draft {
        let segment = Segment { text: text.into(), start_ms: 0, end_ms: 0, no_speech_prob: None, words: Some(words) };
        Draft::new(&EngineResult { language: "en".into(), speech_ms: None, segments: vec![segment] })
    }

    type Annotation = (String, Option<f32>, Option<u32>, Option<u32>);

    fn annotations(draft: &Draft) -> Vec<Annotation> {
        draft.tokens.iter().map(|t| (t.text.clone(), t.prob, t.start_ms, t.end_ms)).collect()
    }

    fn annotated(text: &str, prob: f32, start: u32, end: u32) -> Annotation {
        (text.into(), Some(prob), Some(start), Some(end))
    }

    // The final text doesn't depend on these annotations, so they're checked at the token seam.
    #[test]
    fn a_number_split_into_two_words_annotates_one_token_with_the_weaker_word() {
        let draft = draft(
            " Pi is 3.14.",
            vec![
                word(" Pi", 0, 200, 0.9),
                word(" is", 200, 300, 0.95),
                word(" 3", 300, 500, 0.8),
                word(".14.", 500, 900, 0.4),
            ],
        );
        assert_eq!(
            annotations(&draft),
            [annotated("Pi", 0.9, 0, 200), annotated("is", 0.95, 200, 300), annotated("3.14.", 0.4, 300, 900)]
        );
    }

    #[test]
    fn a_word_spanning_two_tokens_annotates_both() {
        let draft = draft(" New York", vec![word(" New York", 0, 800, 0.7)]);
        assert_eq!(annotations(&draft), [annotated("New", 0.7, 0, 800), annotated("York", 0.7, 0, 800)]);
    }

    #[test]
    fn words_that_dont_spell_the_text_leave_the_segment_unannotated() {
        for words in [
            vec![word(" Hello", 0, 100, 0.9), word(" word", 100, 200, 0.9)],
            vec![word(" Hello", 0, 100, 0.9)],
            vec![word(" Hello", 0, 100, 0.9), word(" world", 100, 200, 0.9), word(" again", 200, 300, 0.9)],
        ] {
            let draft = draft(" Hello world", words);
            assert_eq!(annotations(&draft), [("Hello".into(), None, None, None), ("world".into(), None, None, None)]);
        }
    }
}
