//! A strict subset of RFC 8941 Structured Field Values: dictionaries whose members are items or
//! inner lists, with integers, strings, tokens, byte sequences and booleans. Decimals and dates
//! are rejected. Every failure is a returned reason, never a panic.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_PAD_INDIFFERENT};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BareItem {
    Integer(i64),
    String(String),
    Token(String),
    Bytes(Vec<u8>),
    Boolean(bool),
}

pub(crate) type Params = Vec<(String, BareItem)>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    pub value: BareItem,
    pub params: Params,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InnerList {
    pub items: Vec<Item>,
    pub params: Params,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    Item(Item),
    InnerList(InnerList),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Member {
    pub key: String,
    pub value: Value,
    /// The member's value exactly as written (everything after `key=`), without surrounding
    /// whitespace. RFC 9421 signs the `Signature-Input` value as received.
    pub raw: String,
}

/// The last member of a dictionary with this key (later members override earlier ones).
pub(crate) fn find<'a>(members: &'a [Member], key: &str) -> Option<&'a Member> {
    members.iter().rev().find(|m| m.key == key)
}

/// The value of parameter `key`, the last if repeated.
pub(crate) fn param<'a>(params: &'a Params, key: &str) -> Option<&'a BareItem> {
    params.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
}

struct Cursor<'a> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

type Res<T> = Result<T, String>;

impl<'a> Cursor<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            bytes: text.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn eat(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn skip_while(&mut self, pred: impl Fn(u8) -> bool) {
        while self.peek().is_some_and(&pred) {
            self.pos += 1;
        }
    }

    fn skip_sp(&mut self) {
        self.skip_while(|b| b == b' ');
    }

    fn skip_ows(&mut self) {
        self.skip_while(|b| b == b' ' || b == b'\t');
    }

    fn describe(&self) -> String {
        match self.peek() {
            Some(b) if b.is_ascii_graphic() || b == b' ' => {
                format!("unexpected `{}` at offset {}", char::from(b), self.pos)
            }
            Some(b) => format!("unexpected byte 0x{b:02x} at offset {}", self.pos),
            None => format!("unexpected end of input at offset {}", self.pos),
        }
    }

    fn slice(&self, start: usize, end: usize) -> Res<&'a str> {
        self.text
            .get(start..end)
            .ok_or_else(|| format!("invalid text at offset {start}"))
    }
}

/// Parses a dictionary whose members are items or inner lists.
pub(crate) fn parse_dictionary(input: &str) -> Res<Vec<Member>> {
    let mut c = Cursor::new(input);
    let mut members = Vec::new();
    c.skip_sp();
    if c.peek().is_none() {
        return Ok(members);
    }
    loop {
        let key = parse_key(&mut c)?;
        let start = c.pos;
        let value = if c.eat(b'=') {
            let value_start = c.pos;
            let v = if c.peek() == Some(b'(') {
                Value::InnerList(parse_inner_list(&mut c)?)
            } else {
                Value::Item(parse_item(&mut c)?)
            };
            let raw = c.slice(value_start, c.pos)?.to_owned();
            (v, raw)
        } else {
            let params = parse_params(&mut c)?;
            let raw = c.slice(start, c.pos)?.to_owned();
            (
                Value::Item(Item {
                    value: BareItem::Boolean(true),
                    params,
                }),
                raw,
            )
        };
        members.push(Member {
            key,
            value: value.0,
            raw: value.1,
        });
        c.skip_ows();
        if c.peek().is_none() {
            return Ok(members);
        }
        if !c.eat(b',') {
            return Err(c.describe());
        }
        c.skip_ows();
        if c.peek().is_none() {
            return Err("trailing comma".to_owned());
        }
    }
}

fn parse_key(c: &mut Cursor<'_>) -> Res<String> {
    let start = c.pos;
    match c.peek() {
        Some(b) if b.is_ascii_lowercase() || b == b'*' => c.pos += 1,
        _ => return Err(format!("expected a key: {}", c.describe())),
    }
    c.skip_while(|b| {
        b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.' | b'*')
    });
    Ok(c.slice(start, c.pos)?.to_owned())
}

fn parse_params(c: &mut Cursor<'_>) -> Res<Params> {
    let mut params = Vec::new();
    while c.eat(b';') {
        c.skip_sp();
        let key = parse_key(c)?;
        let value = if c.eat(b'=') {
            parse_bare_item(c)?
        } else {
            BareItem::Boolean(true)
        };
        params.push((key, value));
    }
    Ok(params)
}

fn parse_item(c: &mut Cursor<'_>) -> Res<Item> {
    let value = parse_bare_item(c)?;
    let params = parse_params(c)?;
    Ok(Item { value, params })
}

fn parse_inner_list(c: &mut Cursor<'_>) -> Res<InnerList> {
    if !c.eat(b'(') {
        return Err(c.describe());
    }
    let mut items = Vec::new();
    loop {
        c.skip_sp();
        if c.eat(b')') {
            let params = parse_params(c)?;
            return Ok(InnerList { items, params });
        }
        if c.peek().is_none() {
            return Err("unterminated inner list".to_owned());
        }
        items.push(parse_item(c)?);
        match c.peek() {
            Some(b' ' | b')') => {}
            _ => return Err(c.describe()),
        }
    }
}

fn parse_bare_item(c: &mut Cursor<'_>) -> Res<BareItem> {
    match c.peek() {
        Some(b'"') => parse_string(c).map(BareItem::String),
        Some(b':') => parse_bytes(c).map(BareItem::Bytes),
        Some(b'?') => {
            c.pos += 1;
            match c.bump() {
                Some(b'0') => Ok(BareItem::Boolean(false)),
                Some(b'1') => Ok(BareItem::Boolean(true)),
                _ => Err(format!("invalid boolean: {}", c.describe())),
            }
        }
        Some(b) if b == b'-' || b.is_ascii_digit() => parse_integer(c).map(BareItem::Integer),
        Some(b) if b.is_ascii_alphabetic() || b == b'*' => {
            let start = c.pos;
            c.skip_while(|b| {
                b.is_ascii_alphanumeric()
                    || matches!(
                        b,
                        b':' | b'/'
                            | b'!'
                            | b'#'
                            | b'$'
                            | b'%'
                            | b'&'
                            | b'\''
                            | b'*'
                            | b'+'
                            | b'-'
                            | b'.'
                            | b'^'
                            | b'_'
                            | b'`'
                            | b'|'
                            | b'~'
                    )
            });
            Ok(BareItem::Token(c.slice(start, c.pos)?.to_owned()))
        }
        _ => Err(format!("expected a value: {}", c.describe())),
    }
}

fn parse_integer(c: &mut Cursor<'_>) -> Res<i64> {
    let start = c.pos;
    c.eat(b'-');
    let digits_start = c.pos;
    c.skip_while(|b| b.is_ascii_digit());
    let digits = c.pos - digits_start;
    if digits == 0 {
        return Err(format!("expected digits: {}", c.describe()));
    }
    if digits > 15 {
        return Err(format!("integer longer than 15 digits at offset {start}"));
    }
    if c.peek() == Some(b'.') {
        return Err(format!("decimals are not supported (offset {})", c.pos));
    }
    c.slice(start, c.pos)?
        .parse::<i64>()
        .map_err(|e| format!("invalid integer at offset {start}: {e}"))
}

fn parse_string(c: &mut Cursor<'_>) -> Res<String> {
    c.pos += 1;
    let mut out = String::new();
    loop {
        match c.bump() {
            Some(b'"') => return Ok(out),
            Some(b'\\') => match c.bump() {
                Some(b @ (b'"' | b'\\')) => out.push(char::from(b)),
                _ => return Err(format!("invalid escape in string: {}", c.describe())),
            },
            Some(b) if (0x20..=0x7e).contains(&b) => out.push(char::from(b)),
            Some(b) => {
                return Err(format!(
                    "invalid byte 0x{b:02x} in string at offset {}",
                    c.pos - 1
                ));
            }
            None => return Err("unterminated string".to_owned()),
        }
    }
}

fn parse_bytes(c: &mut Cursor<'_>) -> Res<Vec<u8>> {
    c.pos += 1;
    let start = c.pos;
    c.skip_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='));
    let encoded = c.slice(start, c.pos)?;
    if !c.eat(b':') {
        return Err(format!("unterminated byte sequence: {}", c.describe()));
    }
    STANDARD_PAD_INDIFFERENT
        .decode(encoded)
        .map_err(|e| format!("invalid base64 in byte sequence at offset {start}: {e}"))
}

/// Serialises an sf-string. Only printable ASCII can be represented.
pub(crate) fn serialize_string(value: &str) -> Res<String> {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        if !(' '..='~').contains(&ch) {
            return Err(format!("{ch:?} cannot be written in a structured string"));
        }
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    Ok(out)
}

/// Serialises a byte sequence item.
pub(crate) fn serialize_bytes(value: &[u8]) -> String {
    format!(":{}:", STANDARD.encode(value))
}

/// Whether `key` is a valid dictionary key (used for signature labels).
pub(crate) fn is_valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '*')
        && chars.all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.' | '*')
        })
}
