//! Fetched `ActivityPub` JSON as neutral structs. No index types: the binary maps these to
//! index records.
//!
//! Parsing is structural only. Whether we may index something is `apub::consent`'s job, and
//! callers run it on the raw JSON first.

use serde_json::{Map, Value};
use thiserror::Error;
use url::Url;

const POST_TYPES: [&str; 4] = ["Note", "Article", "Page", "Question"];
const ACTOR_TYPES: [&str; 5] = ["Person", "Service", "Group", "Application", "Organization"];

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    #[error("expected a JSON object")]
    NotAnObject,
    #[error("object has no string `id`")]
    MissingId,
    #[error("`{uri}` has unsupported type `{kind}`")]
    UnsupportedType { uri: String, kind: String },
    #[error("`{uri}` has no `{field}`")]
    MissingField { uri: String, field: &'static str },
    #[error("`{uri}` is not a usable URI: {reason}")]
    BadUri { uri: String, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPost {
    pub uri: String,
    pub author_uri: String,
    /// The content as plain text. Empty if the object has none.
    pub content_text: String,
    /// The content warning, as plain text.
    pub summary_text: Option<String>,
    /// The title of an Article or Page.
    pub name: Option<String>,
    /// Unix seconds.
    pub published: Option<i64>,
    pub updated: Option<i64>,
    /// BCP 47, from the first key of `contentMap`, else Lemmy's `language` object.
    pub language: Option<String>,
    /// Without the `#`.
    pub hashtags: Vec<String>,
    pub in_reply_to: Option<String>,
    pub likes: Option<u64>,
    pub shares: Option<u64>,
    pub replies: Option<u64>,
    pub sensitive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPublicKey {
    pub id: String,
    pub owner: String,
    pub pem: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedActor {
    pub uri: String,
    /// At most [`MAX_USERNAME_CHARS`] characters.
    pub preferred_username: String,
    pub name: Option<String>,
    pub summary_text: Option<String>,
    /// Host of `uri`, with the port if it has one.
    pub host: String,
    /// `preferred_username@host`.
    pub acct: String,
    /// The actor type, e.g. `Person`.
    pub kind: String,
    pub published: Option<i64>,
    pub discoverable: bool,
    pub indexable: bool,
    pub followers: Option<u64>,
    pub following: Option<u64>,
    pub statuses: Option<u64>,
    pub public_key: Option<ParsedPublicKey>,
    pub inbox: String,
    pub shared_inbox: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Helpers over JSON

fn as_object(value: &Value) -> Result<&Map<String, Value>, ParseError> {
    value.as_object().ok_or(ParseError::NotAnObject)
}

fn string_field<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(Value::as_str)
}

/// The first of the object's types that is in `allowed`, else its first type (for the error).
fn pick_type(obj: &Map<String, Value>, allowed: &[&str]) -> String {
    let types: Vec<&str> = match obj.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    types
        .iter()
        .find(|t| allowed.contains(t))
        .or_else(|| types.first())
        .map_or_else(String::new, |t| (*t).to_owned())
}

/// A link that is a bare string, an object with an `id` (or `href`), or a list of those (first).
fn link(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Object(o) => string_field(o, "id")
            .or_else(|| string_field(o, "href"))
            .map(str::to_owned),
        Value::Array(items) => items.iter().find_map(link),
        _ => None,
    }
}

/// `totalItems` of an embedded collection. A bare URI (not embedded) gives `None`.
fn total_items(value: Option<&Value>) -> Option<u64> {
    value?.as_object()?.get("totalItems")?.as_u64()
}

/// Plain text field (`name`): trimmed, empty is `None`. Not HTML, so not stripped.
fn plain(obj: &Map<String, Value>, key: &str) -> Option<String> {
    let text = string_field(obj, key)?.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// HTML field (`summary`): converted to text, empty is `None`.
fn html_field(obj: &Map<String, Value>, key: &str) -> Option<String> {
    let text = html_to_text(string_field(obj, key)?);
    (!text.is_empty()).then_some(text)
}

fn timestamp(obj: &Map<String, Value>, key: &str) -> Option<i64> {
    parse_rfc3339(string_field(obj, key)?)
}

// ---------------------------------------------------------------------------------------------
// Posts

/// Parses a `Note`, `Article`, `Page` or `Question`.
pub fn parse_post(value: &Value) -> Result<ParsedPost, ParseError> {
    let obj = as_object(value)?;
    let uri = string_field(obj, "id")
        .filter(|id| !id.is_empty())
        .ok_or(ParseError::MissingId)?
        .to_owned();
    let kind = pick_type(obj, &POST_TYPES);
    if !POST_TYPES.contains(&kind.as_str()) {
        return Err(ParseError::UnsupportedType { uri, kind });
    }
    let author_uri =
        obj.get("attributedTo")
            .and_then(link)
            .ok_or_else(|| ParseError::MissingField {
                uri: uri.clone(),
                field: "attributedTo",
            })?;

    let content_map = obj.get("contentMap").and_then(Value::as_object);
    let first_mapped = content_map.and_then(|m| m.iter().next());
    let language = first_mapped
        .map(|(lang, _)| lang.trim().to_owned())
        .filter(|lang| !lang.is_empty())
        .or_else(|| obj.get("language").and_then(language_tag));
    let content_html = string_field(obj, "content").or_else(|| first_mapped?.1.as_str());
    let content_text = content_html.map(html_to_text).unwrap_or_default();

    Ok(ParsedPost {
        author_uri,
        content_text,
        summary_text: html_field(obj, "summary"),
        name: plain(obj, "name"),
        published: timestamp(obj, "published"),
        updated: timestamp(obj, "updated"),
        language,
        hashtags: hashtags(obj.get("tag")),
        in_reply_to: obj.get("inReplyTo").and_then(link),
        likes: total_items(obj.get("likes")),
        shares: total_items(obj.get("shares")),
        replies: total_items(obj.get("replies")),
        sensitive: obj
            .get("sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        uri,
    })
}

/// Lemmy's `language`: `{"identifier": "en", "name": "English"}`, or a list of those (first),
/// or a bare tag. `und` (undetermined, Lemmy's default) is no language.
fn language_tag(value: &Value) -> Option<String> {
    let tag = match value {
        Value::String(tag) => tag.as_str(),
        Value::Object(o) => string_field(o, "identifier")?,
        Value::Array(items) => return items.iter().find_map(language_tag),
        _ => return None,
    }
    .trim();
    (!tag.is_empty() && !tag.eq_ignore_ascii_case("und")).then(|| tag.to_owned())
}

/// Names of `Hashtag` items in `tag` (a list or a single object), without the `#`.
fn hashtags(tag: Option<&Value>) -> Vec<String> {
    let items: Vec<&Value> = match tag {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(single @ Value::Object(_)) => vec![single],
        _ => Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        if pick_type(obj, &["Hashtag"]) != "Hashtag" {
            continue;
        }
        let Some(name) = string_field(obj, "name") else {
            continue;
        };
        let name = name.trim().trim_start_matches('#');
        if !name.is_empty() && !out.iter().any(|seen| seen == name) {
            out.push(name.to_owned());
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Actors

/// Most characters of `preferredUsername` [`parse_actor`] keeps; the rest are cut, on a char
/// boundary. Mastodon allows 30, Lemmy and Misskey 20, `GoToSocial` 64, so 64 cuts no real
/// username. Without a cap, a hostile actor's long hyphenated username becomes thousands of
/// index keys (audit E2). Cutting here means the stored `acct` already holds the cut name, so
/// account keys stay a pure function of the stored record.
pub const MAX_USERNAME_CHARS: usize = 64;

/// Parses a `Person`, `Service`, `Group`, `Application` or `Organization`.
pub fn parse_actor(value: &Value) -> Result<ParsedActor, ParseError> {
    let obj = as_object(value)?;
    let uri = string_field(obj, "id")
        .filter(|id| !id.is_empty())
        .ok_or(ParseError::MissingId)?
        .to_owned();
    let kind = pick_type(obj, &ACTOR_TYPES);
    if !ACTOR_TYPES.contains(&kind.as_str()) {
        return Err(ParseError::UnsupportedType { uri, kind });
    }
    let missing = |field| ParseError::MissingField {
        uri: uri.clone(),
        field,
    };
    let preferred_username: String = plain(obj, "preferredUsername")
        .ok_or_else(|| missing("preferredUsername"))?
        .chars()
        .take(MAX_USERNAME_CHARS)
        .collect();
    let inbox = obj
        .get("inbox")
        .and_then(link)
        .ok_or_else(|| missing("inbox"))?;
    let parsed = Url::parse(&uri).map_err(|e| ParseError::BadUri {
        uri: uri.clone(),
        reason: e.to_string(),
    })?;
    let host = match (parsed.host_str(), parsed.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_owned(),
        (None, _) => {
            return Err(ParseError::BadUri {
                uri: uri.clone(),
                reason: "no host".to_owned(),
            });
        }
    };
    let flag = |key: &str| obj.get(key).and_then(Value::as_bool).unwrap_or(false);
    let endpoints = obj.get("endpoints").and_then(Value::as_object);
    let shared_inbox = endpoints
        .and_then(|e| e.get("sharedInbox"))
        .or_else(|| obj.get("sharedInbox"))
        .and_then(link);

    Ok(ParsedActor {
        acct: format!("{preferred_username}@{host}"),
        name: plain(obj, "name"),
        summary_text: html_field(obj, "summary"),
        published: timestamp(obj, "published"),
        discoverable: flag("discoverable"),
        indexable: flag("indexable"),
        followers: total_items(obj.get("followers")),
        following: total_items(obj.get("following")),
        statuses: total_items(obj.get("outbox")),
        public_key: obj.get("publicKey").and_then(public_key),
        shared_inbox,
        preferred_username,
        host,
        kind,
        inbox,
        uri,
    })
}

fn public_key(value: &Value) -> Option<ParsedPublicKey> {
    let obj = value.as_object()?;
    Some(ParsedPublicKey {
        id: string_field(obj, "id")?.to_owned(),
        owner: obj.get("owner").and_then(link)?,
        pem: string_field(obj, "publicKeyPem")?.to_owned(),
    })
}

// ---------------------------------------------------------------------------------------------
// Timestamps

/// Parses an RFC 3339 timestamp (`2026-11-01T12:00:00Z`, `...+01:00`, with fractional seconds)
/// into Unix seconds. Fractions are dropped. A missing offset is taken as UTC, since some
/// servers omit it. Anything malformed is `None`.
#[must_use]
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    parse_rfc3339_inner(text.trim().as_bytes())
}

fn parse_rfc3339_inner(bytes: &[u8]) -> Option<i64> {
    struct Cursor<'a> {
        bytes: &'a [u8],
        pos: usize,
    }
    impl Cursor<'_> {
        fn number(&mut self, digits: usize) -> Option<i64> {
            let slice = self.bytes.get(self.pos..self.pos.checked_add(digits)?)?;
            if !slice.iter().all(u8::is_ascii_digit) {
                return None;
            }
            self.pos += digits;
            Some(
                slice
                    .iter()
                    .fold(0, |acc, d| acc * 10 + i64::from(d - b'0')),
            )
        }
        fn eat(&mut self, accepted: &[u8]) -> Option<()> {
            accepted.contains(self.bytes.get(self.pos)?).then(|| {
                self.pos += 1;
            })
        }
        fn peek(&self) -> Option<u8> {
            self.bytes.get(self.pos).copied()
        }
    }

    let mut c = Cursor { bytes, pos: 0 };
    let year = c.number(4)?;
    c.eat(b"-")?;
    let month = c.number(2)?;
    c.eat(b"-")?;
    let day = c.number(2)?;
    c.eat(b"Tt ")?;
    let hour = c.number(2)?;
    c.eat(b":")?;
    let minute = c.number(2)?;
    c.eat(b":")?;
    let second = c.number(2)?;
    if c.peek() == Some(b'.') || c.peek() == Some(b',') {
        c.pos += 1;
        let start = c.pos;
        while c.peek().is_some_and(|b| b.is_ascii_digit()) {
            c.pos += 1;
        }
        if c.pos == start {
            return None;
        }
    }
    let offset_secs = match c.peek() {
        None | Some(b'Z' | b'z') => {
            c.pos += usize::from(c.peek().is_some());
            0
        }
        Some(sign @ (b'+' | b'-')) => {
            c.pos += 1;
            let oh = c.number(2)?;
            if c.peek() == Some(b':') {
                c.pos += 1;
            }
            let om = c.number(2)?;
            if oh > 23 || om > 59 {
                return None;
            }
            let secs = oh * 3600 + om * 60;
            if sign == b'-' { -secs } else { secs }
        }
        Some(_) => return None,
    };
    if c.pos != bytes.len() {
        return None;
    }
    let leap = [1, 3, 5, 7, 8, 10, 12];
    let days_in_month = match month {
        m if leap.contains(&m) => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => return None,
    };
    // Second 60 is a leap second; it rolls into the next minute.
    if day < 1 || day > days_in_month || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some(
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
            - offset_secs,
    )
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ---------------------------------------------------------------------------------------------
// HTML to text

#[derive(Clone, Copy, PartialEq, Eq)]
enum Span {
    Invisible,
    Ellipsis,
    Other,
}

struct Tag {
    name: String,
    closing: bool,
    self_closing: bool,
    /// Value of the `class` attribute.
    class: String,
    /// Index just past the closing `>`.
    end: usize,
}

/// Parses the tag starting at `chars[start]` (`<`). `None` if it is not a tag (a bare `<`).
fn parse_tag(chars: &[char], start: usize) -> Option<Tag> {
    let mut i = start + 1;
    let closing = chars.get(i) == Some(&'/');
    if closing {
        i += 1;
    }
    let name_start = i;
    while chars.get(i).is_some_and(char::is_ascii_alphanumeric) {
        i += 1;
    }
    if i == name_start || !chars.get(name_start)?.is_ascii_alphabetic() {
        return None;
    }
    let name: String = chars
        .get(name_start..i)?
        .iter()
        .map(char::to_ascii_lowercase)
        .collect();
    // Attributes, up to the `>` that is not inside quotes. An unterminated tag runs to the end.
    let mut class = String::new();
    let mut quote: Option<char> = None;
    let mut attr = String::new();
    let mut value = String::new();
    let mut in_value = false;
    let mut last_non_space = ' ';
    while let Some(&c) = chars.get(i) {
        i += 1;
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                value.push(c);
            }
            continue;
        }
        if c == '>' {
            break;
        }
        match c {
            '"' | '\'' if in_value => quote = Some(c),
            '=' if !in_value => in_value = true,
            c if c.is_whitespace() => {
                if in_value && !value.is_empty() || (!in_value && !attr.is_empty()) {
                    if attr.eq_ignore_ascii_case("class") {
                        class = std::mem::take(&mut value);
                    }
                    attr.clear();
                    value.clear();
                    in_value = false;
                }
            }
            c if in_value => value.push(c),
            c => attr.push(c),
        }
        if !c.is_whitespace() {
            last_non_space = c;
        }
    }
    if attr.eq_ignore_ascii_case("class") && !value.is_empty() {
        class = value;
    }
    Some(Tag {
        name,
        closing,
        self_closing: last_non_space == '/',
        class,
        end: i,
    })
}

/// Index just past the end of the comment or declaration starting at `chars[start]` (`<!` or `<?`).
fn skip_markup(chars: &[char], start: usize) -> usize {
    let is_comment = chars.get(start + 2) == Some(&'-') && chars.get(start + 3) == Some(&'-');
    let mut i = start + 2;
    while i < chars.len() {
        if is_comment {
            if chars.get(i) == Some(&'-')
                && chars.get(i + 1) == Some(&'-')
                && chars.get(i + 2) == Some(&'>')
            {
                return i + 3;
            }
        } else if chars.get(i) == Some(&'>') {
            return i + 1;
        }
        i += 1;
    }
    chars.len()
}

/// Index just past `</name ...>`, searching from `from`; the end of input if there is none.
fn skip_raw_element(chars: &[char], from: usize, name: &str) -> usize {
    let needle: Vec<char> = format!("</{name}").chars().collect();
    let mut i = from;
    while i < chars.len() {
        let matches = needle.iter().enumerate().all(|(k, n)| {
            chars
                .get(i + k)
                .is_some_and(|c| c.to_ascii_lowercase() == *n)
        });
        if matches {
            let mut j = i + needle.len();
            while j < chars.len() && chars.get(j) != Some(&'>') {
                j += 1;
            }
            return j + 1;
        }
        i += 1;
    }
    chars.len()
}

fn named_entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "hellip" => '\u{2026}',
        "mdash" => '\u{2014}',
        "ndash" => '\u{2013}',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201c}',
        "rdquo" => '\u{201d}',
        "copy" => '\u{a9}',
        "reg" => '\u{ae}',
        "trade" => '\u{2122}',
        _ => return None,
    })
}

/// Decodes the entity whose `&` is at `chars[start]`. Returns the character and the index just
/// past the `;`. `None` leaves the `&` as literal text.
fn decode_entity(chars: &[char], start: usize) -> Option<(char, usize)> {
    let mut end = start + 1;
    // The longest entity we handle is `&#x10FFFF;`; anything longer is not one.
    while end < chars.len() && end - start <= 10 && chars.get(end) != Some(&';') {
        end += 1;
    }
    if chars.get(end) != Some(&';') {
        return None;
    }
    let body: String = chars.get(start + 1..end)?.iter().collect();
    let decoded = if let Some(num) = body.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse::<u32>().ok()?,
        };
        // NUL, surrogates and out-of-range values become the replacement character.
        char::from_u32(code)
            .filter(|c| *c != '\0')
            .unwrap_or('\u{fffd}')
    } else {
        named_entity(&body)?
    };
    Some((decoded, end + 1))
}

/// Converts HTML (as in `content` and `summary`) to plain text.
///
/// Tags are dropped. `<br>` and the ends of block elements become newlines. Mastodon's
/// `<span class="invisible">` (hidden URL prefixes and suffixes) is dropped, and the text of an
/// `ellipsis` span is followed by `…`. `<script>` and `<style>` contents are dropped, entities
/// decoded and whitespace collapsed. Never panics, whatever the input.
#[must_use]
pub fn html_to_text(html: &str) -> String {
    let chars: Vec<char> = html.chars().collect();
    let mut out = String::with_capacity(html.len());
    let mut spans: Vec<Span> = Vec::new();
    let mut invisible = 0_usize;
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        match c {
            '<' if matches!(chars.get(i + 1), Some('!' | '?')) => {
                i = skip_markup(&chars, i);
            }
            '<' => {
                let Some(tag) = parse_tag(&chars, i) else {
                    if invisible == 0 {
                        out.push('<');
                    }
                    i += 1;
                    continue;
                };
                i = tag.end;
                handle_tag(&tag, &chars, &mut i, &mut out, &mut spans, &mut invisible);
            }
            '&' => {
                let (decoded, next) = decode_entity(&chars, i).unwrap_or(('&', i + 1));
                if invisible == 0 {
                    out.push(source_whitespace(decoded));
                }
                i = next;
            }
            _ => {
                if invisible == 0 {
                    out.push(source_whitespace(c));
                }
                i += 1;
            }
        }
    }
    normalise_whitespace(&out)
}

/// Line breaks in HTML source are just whitespace; only tags make real ones.
fn source_whitespace(c: char) -> char {
    if matches!(c, '\n' | '\r' | '\t') {
        ' '
    } else {
        c
    }
}

fn handle_tag(
    tag: &Tag,
    chars: &[char],
    i: &mut usize,
    out: &mut String,
    spans: &mut Vec<Span>,
    invisible: &mut usize,
) {
    match (tag.name.as_str(), tag.closing) {
        ("script" | "style", false) if !tag.self_closing => {
            *i = skip_raw_element(chars, *i, &tag.name);
        }
        ("span", false) if !tag.self_closing => {
            let classes: Vec<&str> = tag.class.split_whitespace().collect();
            let kind = if classes.contains(&"invisible") {
                *invisible += 1;
                Span::Invisible
            } else if classes.contains(&"ellipsis") {
                Span::Ellipsis
            } else {
                Span::Other
            };
            spans.push(kind);
        }
        ("span", true) => match spans.pop() {
            Some(Span::Invisible) => *invisible = invisible.saturating_sub(1),
            Some(Span::Ellipsis) if *invisible == 0 => out.push('\u{2026}'),
            _ => {}
        },
        ("br", _) if *invisible == 0 => out.push('\n'),
        ("p" | "div" | "li" | "blockquote" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6", true)
            if *invisible == 0 =>
        {
            out.push('\n');
        }
        _ => {}
    }
}

/// Collapses runs of spaces, trims each line, allows at most one blank line, trims the result.
fn normalise_whitespace(text: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let mut collapsed = String::with_capacity(line.len());
        let mut pending_space = false;
        for c in line.chars() {
            if c.is_whitespace() {
                pending_space = !collapsed.is_empty();
            } else {
                if pending_space {
                    collapsed.push(' ');
                    pending_space = false;
                }
                collapsed.push(c);
            }
        }
        lines.push(collapsed);
    }
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in &lines {
        if line.is_empty() {
            blank_run += 1;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if blank_run > 0 {
                out.push('\n');
            }
        }
        blank_run = 0;
        out.push_str(line);
    }
    out
}
