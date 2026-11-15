#![deny(
    clippy::exit,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::panic,
    clippy::string_slice,
    clippy::unimplemented,
    clippy::unreachable,
    clippy::todo,
    clippy::pedantic
)]
// As in mwmbl_core: every public fn here returns Result, and the error types say what failed.
#![allow(clippy::missing_errors_doc)]
//! Tokeniser and index-key selection for Mwmbl Social.
//!
//! Free of fediverse knowledge beyond hashtags and handles, so it can be offered upstream.

mod special;
mod stopwords;

use std::collections::HashSet;

use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

pub use special::special_keys;
pub use stopwords::is_stopword;

/// Version of the tokeniser. Any change to the output of the functions in this crate changes
/// index keys, and must bump this version.
pub const TOKENIZER_VERSION: u32 = 1;

/// How many keys of each kind to take from one field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyConfig {
    /// Number of leading tokens used as unigram keys.
    pub first_tokens: usize,
    /// Number of leading adjacent token pairs used as bigram keys.
    pub bigrams: usize,
    /// Number of hashtag/mention keys.
    pub special: usize,
}

impl Default for KeyConfig {
    /// Mwmbl's `NUM_FIRST_TOKENS` and `NUM_BIGRAMS`.
    fn default() -> Self {
        Self {
            first_tokens: 10,
            bigrams: 10,
            special: 10,
        }
    }
}

fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{3005}' | '\u{3007}'
        | '\u{3040}'..='\u{309F}'   // Hiragana
        | '\u{30A0}'..='\u{30FF}'   // Katakana, incl. U+30FC
        | '\u{31F0}'..='\u{31FF}'   // Katakana extensions
        | '\u{3400}'..='\u{4DBF}'   // Han ext. A
        | '\u{4E00}'..='\u{9FFF}'   // Han
        | '\u{F900}'..='\u{FAFF}'   // Han compatibility
        | '\u{20000}'..='\u{323AF}' // Han ext. B onwards
        | '\u{1100}'..='\u{11FF}'   // Hangul jamo
        | '\u{3130}'..='\u{318F}'   // Hangul compatibility jamo
        | '\u{A960}'..='\u{A97F}'
        | '\u{AC00}'..='\u{D7AF}'   // Hangul syllables
        | '\u{D7B0}'..='\u{D7FF}'
    )
}

fn is_cjk_word(word: &str) -> bool {
    !word.is_empty() && word.chars().all(is_cjk_char)
}

fn flush_run(run: &mut Vec<char>, out: &mut Vec<String>) {
    if run.len() == 1 {
        out.push(run.iter().collect());
    } else {
        out.extend(run.windows(2).map(|pair| pair.iter().collect::<String>()));
    }
    run.clear();
}

/// Split `text` into index tokens: NFKC-normalised, lowercased words, with CJK runs replaced by
/// overlapping character bigrams. If the text ends with `…`, the last (cut) word is dropped.
#[must_use]
pub fn tokenize(text: &str) -> Vec<String> {
    let normalised: String = text
        .replace('\u{2019}', "'")
        .nfkc()
        .collect::<String>()
        .to_lowercase();
    let mut tokens: Vec<String> = Vec::new();
    let mut run: Vec<char> = Vec::new();
    for word in normalised.unicode_words() {
        if is_cjk_word(word) {
            run.extend(word.chars());
        } else {
            flush_run(&mut run, &mut tokens);
            tokens.push(word.to_string());
        }
    }
    flush_run(&mut run, &mut tokens);
    if text.trim_end().ends_with('…') {
        tokens.pop();
    }
    tokens
}

fn bigrams(tokens: &[String]) -> impl Iterator<Item = (&String, &String)> {
    tokens
        .windows(2)
        .filter_map(|w| Some((w.first()?, w.get(1)?)))
}

fn dedupe(keys: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    keys.into_iter()
        .filter(|k| seen.insert(k.clone()))
        .collect()
}

/// The index keys for one field: unigrams, then bigrams, then hashtag/mention keys,
/// deduplicated with the first occurrence winning.
#[must_use]
pub fn field_keys(text: &str, config: &KeyConfig) -> Vec<String> {
    let tokens = tokenize(text);
    let mut keys: Vec<String> = tokens
        .iter()
        .take(config.first_tokens)
        .filter(|t| !is_stopword(t))
        .cloned()
        .collect();
    keys.extend(
        bigrams(&tokens)
            .take(config.bigrams)
            .filter(|(a, b)| !(is_stopword(a) && is_stopword(b)))
            .map(|(a, b)| format!("{a} {b}")),
    );
    keys.extend(special_keys(text).into_iter().take(config.special));
    dedupe(keys)
}

/// The keys to look up for a query: every non-stopword token, every bigram except those of two
/// stopwords, and all hashtag/mention keys. A query of only stopwords keeps all its tokens and
/// bigrams, so "The Who" still searches.
#[must_use]
pub fn query_keys(query: &str) -> Vec<String> {
    let tokens = tokenize(query);
    let mut keys: Vec<String> = tokens.iter().filter(|t| !is_stopword(t)).cloned().collect();
    let only_stopwords = keys.is_empty();
    if only_stopwords {
        keys.clone_from(&tokens);
    }
    keys.extend(
        bigrams(&tokens)
            .filter(|(a, b)| only_stopwords || !(is_stopword(a) && is_stopword(b)))
            .map(|(a, b)| format!("{a} {b}")),
    );
    keys.extend(special_keys(query));
    dedupe(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_punctuation_only() {
        assert_eq!(tokenize(""), Vec::<String>::new());
        assert_eq!(tokenize("!?,.;:-- ()"), Vec::<String>::new());
        assert_eq!(tokenize("…"), Vec::<String>::new());
        assert_eq!(field_keys("", &KeyConfig::default()), Vec::<String>::new());
        assert_eq!(query_keys(""), Vec::<String>::new());
        assert_eq!(query_keys("#"), Vec::<String>::new());
    }

    #[test]
    fn mixed_scripts() {
        assert_eq!(
            tokenize("Rust日本語rocks"),
            ["rust", "日本", "本語", "rocks"]
        );
        assert_eq!(tokenize("東京 Tokyo 大阪"), ["東京", "tokyo", "大阪"]);
        assert_eq!(tokenize("猫 dog 犬"), ["猫", "dog", "犬"]);
    }

    #[test]
    fn hashtag_at_start_and_end() {
        assert_eq!(
            field_keys("#rust", &KeyConfig::default()),
            ["rust", "#rust"]
        );
        assert_eq!(tokenize("go #rust"), ["go", "rust"]);
    }

    #[test]
    fn lone_symbols() {
        assert_eq!(tokenize("@"), Vec::<String>::new());
        assert_eq!(tokenize("#"), Vec::<String>::new());
    }

    #[test]
    fn ellipsis_only_counts_at_the_end() {
        assert_eq!(tokenize("wait… what"), ["wait", "what"]);
        assert_eq!(tokenize("cut off wor…  \n"), ["cut", "off"]);
    }

    #[test]
    fn very_long_input() {
        let text = "word ".repeat(100_000);
        assert_eq!(tokenize(&text).len(), 100_000);
        let keys = field_keys(&text, &KeyConfig::default());
        assert_eq!(keys, ["word", "word word"]);
        let cjk = "日".repeat(50_000);
        assert_eq!(tokenize(&cjk).len(), 49_999);
    }
}
