//! Acceptance tests for the tokeniser, written before the implementation.
//!
//! Changing what any of these produce changes index keys, so it means bumping
//! `TOKENIZER_VERSION` too.

use tokenize::{KeyConfig, TOKENIZER_VERSION, field_keys, query_keys, special_keys, tokenize};

fn toks(text: &str) -> Vec<String> {
    tokenize(text)
}

#[test]
fn version_is_set() {
    assert_eq!(TOKENIZER_VERSION, 1);
}

#[test]
fn english_lowercases_and_drops_punctuation() {
    assert_eq!(
        toks("Hello, World! It's 2026."),
        ["hello", "world", "it's", "2026"]
    );
    // A curly apostrophe is read as a straight one.
    assert_eq!(toks("Don’t panic"), ["don't", "panic"]);
    assert_eq!(toks("  \n\t "), Vec::<String>::new());
    assert_eq!(toks("emoji 😀 only"), ["emoji", "only"]);
}

#[test]
fn german_is_nfkc_normalised() {
    assert_eq!(toks("Grüße aus München"), ["grüße", "aus", "münchen"]);
    // Decomposed u + combining diaeresis gives the same token as the composed form.
    assert_eq!(toks("Mu\u{0308}nchen"), ["münchen"]);
    // Compatibility forms fold: the "ﬁ" ligature and full-width letters.
    assert_eq!(toks("ﬁsch ＡＢＣ"), ["fisch", "abc"]);
}

#[test]
fn japanese_runs_become_character_bigrams() {
    assert_eq!(toks("東京"), ["東京"]);
    assert_eq!(toks("東京タワー"), ["東京", "京タ", "タワ", "ワー"]);
    // A single CJK character is a token on its own.
    assert_eq!(toks("猫"), ["猫"]);
    // Latin words around a run stay words, and the run is bigrammed by itself.
    assert_eq!(toks("Rust 言語 rocks"), ["rust", "言語", "rocks"]);
    assert_eq!(toks("今日は"), ["今日", "日は"]);
}

#[test]
fn korean_and_chinese_runs_too() {
    assert_eq!(toks("안녕하세요"), ["안녕", "녕하", "하세", "세요"]);
    assert_eq!(toks("搜索引擎"), ["搜索", "索引", "引擎"]);
}

#[test]
fn hashtags_and_mentions_are_words_in_the_sequence() {
    // The tag's word stays in the token sequence, so bigrams still form across it.
    assert_eq!(toks("I love #Rust today"), ["i", "love", "rust", "today"]);
    assert_eq!(
        toks("hi @alice@example.social"),
        ["hi", "alice", "example.social"]
    );
}

#[test]
fn special_keys_are_hashtags_and_handles() {
    assert_eq!(
        special_keys("I love #Rust and #rust, #FediDev! cc @Alice@Example.Social and @bob"),
        ["#rust", "#fedidev", "@alice@example.social", "@bob"]
    );
    // Not a hashtag: inside a word or a URL fragment.
    assert!(special_keys("C# and https://example.com/page#section").is_empty());
    // Not a mention: an email address.
    assert!(special_keys("mail alice@example.com").is_empty());
    // Unicode hashtags.
    assert_eq!(special_keys("#日本語 #Straße"), ["#日本語", "#straße"]);
}

#[test]
fn a_truncated_text_drops_its_cut_last_word() {
    assert_eq!(toks("the quick brown fo…"), ["the", "quick", "brown"]);
    assert_eq!(
        toks("the quick brown fox"),
        ["the", "quick", "brown", "fox"]
    );
}

#[test]
fn field_keys_take_first_tokens_and_bigrams_without_stopwords() {
    let config = KeyConfig {
        first_tokens: 3,
        bigrams: 2,
        special: 10,
    };
    let keys = field_keys("The quick brown fox jumps over", &config);
    // Unigrams: first 3 tokens, stopwords dropped ("the").
    // Bigrams: first 2 adjacent pairs of the full sequence.
    // Order: unigrams, then bigrams, then special keys, each first-seen, no duplicates.
    assert_eq!(keys, ["quick", "brown", "the quick", "quick brown"]);
}

#[test]
fn field_keys_skip_bigrams_of_two_stopwords() {
    let config = KeyConfig {
        first_tokens: 10,
        bigrams: 10,
        special: 10,
    };
    let keys = field_keys("it is the end", &config);
    assert_eq!(keys, ["end", "the end"]);
}

#[test]
fn field_keys_include_every_hashtag_up_to_the_limit() {
    let config = KeyConfig {
        first_tokens: 2,
        bigrams: 1,
        special: 2,
    };
    let keys = field_keys("red green blue pink #alpha #beta #gamma", &config);
    assert_eq!(keys, ["red", "green", "red green", "#alpha", "#beta"]);
}

#[test]
fn field_keys_dedupe() {
    let config = KeyConfig::default();
    let keys = field_keys("rust rust rust", &config);
    assert_eq!(keys, ["rust", "rust rust"]);
}

#[test]
fn default_config_is_mwmbls() {
    let c = KeyConfig::default();
    assert_eq!((c.first_tokens, c.bigrams, c.special), (10, 10, 10));
}

#[test]
fn query_keys_are_every_non_stopword_token_and_every_bigram() {
    assert_eq!(
        query_keys("The Rust programming language"),
        [
            "rust",
            "programming",
            "language",
            "the rust",
            "rust programming",
            "programming language"
        ]
    );
    // Hashtag and handle queries look up their special keys too.
    assert_eq!(query_keys("#rust"), ["rust", "#rust"]);
    assert_eq!(
        query_keys("@alice@example.social"),
        [
            "alice",
            "example.social",
            "alice example.social",
            "@alice@example.social"
        ]
    );
    assert_eq!(
        query_keys("東京タワー"),
        [
            "東京",
            "京タ",
            "タワ",
            "ワー",
            "東京 京タ",
            "京タ タワ",
            "タワ ワー"
        ]
    );
    assert_eq!(query_keys("the"), ["the"]);
}

#[test]
fn a_query_for_only_stopwords_still_searches_them() {
    // "the who" is a band. If dropping stopwords leaves nothing, keep the tokens.
    assert_eq!(query_keys("The Who"), ["the", "who", "the who"]);
}
