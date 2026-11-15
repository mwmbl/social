//! Hashtag and mention scanners, written by hand over chars.

use unicode_normalization::UnicodeNormalization;

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_user_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '-')
}

fn is_host_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '.')
}

/// A `#` starts a hashtag at the start of the text or after a char that isn't part of a word
/// or a URL/entity-like context.
fn hashtag_allowed_after(before: Option<char>) -> bool {
    before.is_none_or(|b| !(b.is_alphanumeric() || matches!(b, '_' | '/' | '#' | '&' | '=')))
}

/// Length of the longest prefix of `chars` (from `start`) whose chars satisfy `pred`,
/// with trailing dots removed.
fn run_len(chars: &[char], start: usize, pred: fn(char) -> bool) -> usize {
    let mut len = chars.iter().skip(start).take_while(|c| pred(**c)).count();
    while len > 0 && chars.get(start + len - 1) == Some(&'.') {
        len -= 1;
    }
    len
}

fn slice_string(chars: &[char], start: usize, len: usize) -> String {
    chars.iter().skip(start).take(len).collect()
}

/// Hashtags and mentions in `text`, NFKC-normalised and lowercased, deduplicated in
/// first-seen order. Hashtags keep their `#`, mentions their `@` (`@user` or `@user@host`).
#[must_use]
pub fn special_keys(text: &str) -> Vec<String> {
    let normalised: String = text.nfkc().collect();
    let chars: Vec<char> = normalised.chars().collect();
    let mut keys: Vec<String> = Vec::new();
    let mut push = |key: String| {
        let key = key.to_lowercase();
        if !keys.contains(&key) {
            keys.push(key);
        }
    };
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        let before = i.checked_sub(1).and_then(|p| chars.get(p)).copied();
        if c == '#' && hashtag_allowed_after(before) {
            let len = chars
                .iter()
                .skip(i + 1)
                .take_while(|c| is_word_char(**c))
                .count();
            if len > 0 {
                push(format!("#{}", slice_string(&chars, i + 1, len)));
                i += 1 + len;
                continue;
            }
        } else if c == '@' && before.is_none_or(|b| !b.is_alphanumeric()) {
            let user_len = run_len(&chars, i + 1, is_user_char);
            if user_len > 0 {
                let mut key = format!("@{}", slice_string(&chars, i + 1, user_len));
                let mut end = i + 1 + user_len;
                if chars.get(end) == Some(&'@')
                    && chars.get(end + 1).is_some_and(|c| c.is_alphanumeric())
                {
                    let host_len = run_len(&chars, end + 1, is_host_char);
                    if host_len > 0 {
                        key.push('@');
                        key.push_str(&slice_string(&chars, end + 1, host_len));
                        end += 1 + host_len;
                    }
                }
                push(key);
                i = end;
                continue;
            }
        }
        i += 1;
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lone_symbols_are_nothing() {
        assert!(special_keys("#").is_empty());
        assert!(special_keys("@").is_empty());
        assert!(special_keys("# @ #. @.").is_empty());
        assert!(special_keys("").is_empty());
    }

    #[test]
    fn hashtag_at_start_and_end() {
        assert_eq!(special_keys("#start middle #end"), ["#start", "#end"]);
        assert_eq!(special_keys("#a#b"), ["#a"]);
    }

    #[test]
    fn mention_trailing_dot_and_host() {
        assert_eq!(special_keys("hi @bob."), ["@bob"]);
        assert_eq!(special_keys("@bob@host.example."), ["@bob@host.example"]);
        assert_eq!(special_keys("(@Bob)"), ["@bob"]);
        assert_eq!(special_keys("@a@ x"), ["@a"]);
    }

    #[test]
    fn mention_after_word_char_is_not_one() {
        assert!(special_keys("a@b.c").is_empty());
    }

    #[test]
    fn dedupes_case_insensitively() {
        assert_eq!(special_keys("#Rust #RUST @A @a"), ["#rust", "@a"]);
    }
}
