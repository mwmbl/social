//! Turning parsed `ActivityPub` objects into index records.
//!
//! Only call these on objects that passed consent. Everything a record holds is what the
//! index stores and therefore what its keys are derived from (PLAN.md, Index documents).

use apub::objects::{ParsedActor, ParsedPost};
use social_index::{SocialAccount, SocialPost};

/// Longest extract or summary we store, in characters.
pub const MAX_EXTRACT_CHARS: usize = 300;
/// Longest title we store, in characters.
pub const MAX_TITLE_CHARS: usize = 200;
/// Longest `acct` we store, in characters: a username of up to
/// [`apub::objects::MAX_USERNAME_CHARS`], the `@`, and a host of up to 253 chars (the DNS
/// limit) plus a `:port` of up to 6. `parse_actor` already keeps `acct` within this; the cut
/// here bounds the stored record however the actor was built (audit E2).
pub const MAX_ACCT_CHARS: usize = apub::objects::MAX_USERNAME_CHARS + 1 + 253 + 6;

/// The epoch that recency factors are measured from (2025-01-01T00:00:00Z).
const RECENCY_EPOCH: i64 = 1_735_689_600;
/// A post this much newer than another, with equal quality and engagement, scores twice
/// as high, so newer posts push older ones off full pages.
const RECENCY_DOUBLING_SECS: f64 = 30.0 * 86_400.0;

/// Collapse whitespace to single spaces and cut to `max` characters. A cut text ends at a
/// word boundary followed by "…", which the tokeniser reads as "last word was cut" and drops,
/// so we lose a whole word rather than index half of one.
#[must_use]
pub fn truncate_text(text: &str, max: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        return collapsed;
    }
    let cut: String = collapsed.chars().take(max.saturating_sub(1)).collect();
    let at_word = match cut.rfind(' ') {
        Some(space) if space > 0 => cut.get(..space).unwrap_or(&cut).to_string(),
        _ => cut,
    };
    format!("{at_word}…")
}

/// Content warning, then content, then any hashtags the text doesn't already show.
fn extract_of(post: &ParsedPost) -> String {
    let mut text = String::new();
    if let Some(cw) = post
        .summary_text
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        text.push_str(cw);
        text.push(' ');
    }
    text.push_str(&post.content_text);
    let lower = text.to_lowercase();
    for tag in &post.hashtags {
        let tag = format!("#{}", tag.trim_start_matches('#'));
        if !lower.contains(&tag.to_lowercase()) {
            text.push(' ');
            text.push_str(&tag);
        }
    }
    truncate_text(&text, MAX_EXTRACT_CHARS)
}

#[allow(clippy::cast_precision_loss)] // Counts and seconds; f64 precision is ample for a score.
fn as_f64(n: i64) -> f64 {
    n as f64
}

fn count(n: Option<u64>) -> f64 {
    as_f64(i64::try_from(n.unwrap_or(0)).unwrap_or(i64::MAX))
}

/// Text quality × engagement × recency, computed at write time.
#[must_use]
pub fn post_score(post: &ParsedPost, extract: &str, published: i64) -> f64 {
    let chars = as_f64(i64::try_from(extract.chars().count()).unwrap_or(0));
    let mut quality = (chars / 80.0).clamp(0.2, 1.0);
    if post.in_reply_to.is_some() {
        quality *= 0.7;
    }
    let engagement =
        1.0 + (count(post.likes) + 2.0 * count(post.shares) + count(post.replies)).ln_1p();
    let doublings = (as_f64(published - RECENCY_EPOCH) / RECENCY_DOUBLING_SECS).clamp(-40.0, 40.0);
    quality * engagement * doublings.exp2()
}

/// The record for a consented post. `now` becomes `checked`, and stands in for a missing
/// `published`.
#[must_use]
pub fn post_record(post: &ParsedPost, now: i64) -> SocialPost {
    let extract = extract_of(post);
    let published = post.published.unwrap_or(now);
    SocialPost {
        uri: post.uri.clone(),
        author_uri: post.author_uri.clone(),
        score: post_score(post, &extract, published),
        extract,
        term: String::new(),
        published,
        checked: now,
        title: post
            .name
            .as_deref()
            .map(|n| truncate_text(n, MAX_TITLE_CHARS))
            .filter(|n| !n.is_empty()),
        language: post.language.clone(),
    }
}

/// From followers and activity.
#[must_use]
pub fn account_score(actor: &ParsedActor) -> f64 {
    1.0 + count(actor.followers).ln_1p() + 0.5 * count(actor.statuses).ln_1p()
}

/// The record for a consented account. Observed languages carry over from `previous`.
#[must_use]
pub fn account_record(
    actor: &ParsedActor,
    previous: Option<&SocialAccount>,
    now: i64,
) -> SocialAccount {
    SocialAccount {
        uri: actor.uri.clone(),
        acct: actor.acct.chars().take(MAX_ACCT_CHARS).collect(),
        display_name: truncate_text(actor.name.as_deref().unwrap_or(""), MAX_TITLE_CHARS),
        summary: truncate_text(
            actor.summary_text.as_deref().unwrap_or(""),
            MAX_EXTRACT_CHARS,
        ),
        score: account_score(actor),
        term: String::new(),
        checked: now,
        languages: previous.and_then(|p| p.languages.clone()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn parsed_post(uri: &str, author: &str, text: &str) -> ParsedPost {
        ParsedPost {
            uri: uri.into(),
            author_uri: author.into(),
            content_text: text.into(),
            summary_text: None,
            name: None,
            published: Some(RECENCY_EPOCH),
            updated: None,
            language: Some("en".into()),
            hashtags: vec![],
            in_reply_to: None,
            likes: None,
            shares: None,
            replies: None,
            sensitive: false,
        }
    }

    #[test]
    fn truncation_cuts_at_a_word_and_marks_it() {
        assert_eq!(truncate_text("  a  b\n c ", 10), "a b c");
        let long = "word ".repeat(100);
        let t = truncate_text(&long, 30);
        assert!(t.chars().count() <= 30, "{t}");
        assert!(t.ends_with("word…"), "{t}");
        // One long word: cut mid-word, still marked.
        let t = truncate_text(&"x".repeat(50), 10);
        assert_eq!(t, format!("{}…", "x".repeat(9)));
        // Multibyte text is cut on characters, not bytes.
        let t = truncate_text(&"日本語 ".repeat(50), 20);
        assert!(t.chars().count() <= 20);
    }

    #[test]
    fn extract_has_content_warning_and_missing_hashtags() {
        let mut p = parsed_post(
            "https://h.example/s/1",
            "https://h.example/u/a",
            "Garlic day #gardening",
        );
        p.summary_text = Some("food".into());
        p.hashtags = vec!["gardening".into(), "Allotment".into()];
        let r = post_record(&p, RECENCY_EPOCH + 10);
        assert_eq!(r.extract, "food Garlic day #gardening #Allotment");
        assert_eq!(r.checked, RECENCY_EPOCH + 10);
        assert_eq!(r.published, RECENCY_EPOCH);
        assert_eq!(r.language.as_deref(), Some("en"));
    }

    #[test]
    fn scores_prefer_newer_engaged_substantial_posts() {
        let base = parsed_post("u", "a", &"words ".repeat(20));
        let score = |p: &ParsedPost| post_record(p, RECENCY_EPOCH).score;
        let mut newer = base.clone();
        newer.published = Some(RECENCY_EPOCH + 30 * 86_400);
        assert!((score(&newer) / score(&base) - 2.0).abs() < 1e-9);
        let mut liked = base.clone();
        liked.likes = Some(50);
        assert!(score(&liked) > score(&base));
        let mut reply = base.clone();
        reply.in_reply_to = Some("x".into());
        assert!(score(&reply) < score(&base));
        let short = parsed_post("u", "a", "hi");
        assert!(score(&short) < score(&base));
        let mut ancient = base.clone();
        ancient.published = Some(0);
        assert!(score(&ancient).is_finite() && score(&ancient) > 0.0);
    }

    fn actor(acct: &str) -> ParsedActor {
        ParsedActor {
            uri: "https://h.example/users/a".into(),
            preferred_username: "a".into(),
            name: Some("Alice".into()),
            summary_text: Some("Bio".into()),
            host: "h.example".into(),
            acct: acct.into(),
            kind: "Person".into(),
            published: None,
            discoverable: true,
            indexable: true,
            followers: Some(100),
            following: None,
            statuses: Some(10),
            public_key: None,
            inbox: "https://h.example/users/a/inbox".into(),
            shared_inbox: None,
        }
    }

    #[test]
    fn account_records_cut_acct_to_max_chars() {
        let at = "a".repeat(MAX_ACCT_CHARS);
        assert_eq!(account_record(&actor(&at), None, 5).acct, at);
        let over = "a".repeat(MAX_ACCT_CHARS + 1);
        assert_eq!(account_record(&actor(&over), None, 5).acct, at);
        // Counted in chars, and a multi-byte char at the boundary is never split.
        let wide = format!("{}ü€", "a".repeat(MAX_ACCT_CHARS - 1));
        let cut = account_record(&actor(&wide), None, 5).acct;
        assert_eq!(cut, format!("{}ü", "a".repeat(MAX_ACCT_CHARS - 1)));
    }

    #[test]
    fn account_records_keep_observed_languages() {
        let actor = ParsedActor {
            uri: "https://h.example/users/a".into(),
            preferred_username: "a".into(),
            name: Some("Alice".into()),
            summary_text: Some("Bio".into()),
            host: "h.example".into(),
            acct: "a@h.example".into(),
            kind: "Person".into(),
            published: None,
            discoverable: true,
            indexable: true,
            followers: Some(100),
            following: None,
            statuses: Some(10),
            public_key: None,
            inbox: "https://h.example/users/a/inbox".into(),
            shared_inbox: None,
        };
        let first = account_record(&actor, None, 5);
        assert_eq!(
            (
                first.acct.as_str(),
                first.display_name.as_str(),
                first.summary.as_str()
            ),
            ("a@h.example", "Alice", "Bio")
        );
        assert!(first.score > 1.0);
        let mut prev = first.clone();
        prev.languages = Some(vec!["en".into()]);
        assert_eq!(
            account_record(&actor, Some(&prev), 6).languages,
            Some(vec!["en".into()])
        );
    }
}
