//! Activity and object JSON: `parse_post`, `parse_actor`, `find_public_key`, the inbox's
//! activity dispatch (`activities::handle`, the same parse `inbox_app` does with
//! `serde_json::from_slice`) for several kinds of sender, and `html_to_text` on every string.
#![no_main]
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

use apub::inbox::find_public_key;
use apub::objects::{html_to_text, parse_actor, parse_post, parse_rfc3339};
use libfuzzer_sys::fuzz_target;
use mwmbl_social::activities::{Sender, activity_actor, handle, self_delete_with_gone_key};
use serde_json::Value;

const OUR_ACTOR: &str = "https://social.example/actor";

fn strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|v| strings(v, out)),
        Value::Object(map) => map.values().for_each(|v| strings(v, out)),
        _ => {}
    }
}

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = html_to_text(text);
        let _ = parse_rfc3339(text);
    }
    let Ok(value) = serde_json::from_slice::<Value>(data) else {
        return;
    };
    let _ = parse_post(&value);
    let _ = parse_actor(&value);
    let mut all = Vec::new();
    strings(&value, &mut all);
    for s in &all {
        let _ = html_to_text(s);
        let _ = find_public_key(&value, s);
        let _ = self_delete_with_gone_key(&value, s);
    }
    let actor = activity_actor(&value).unwrap_or("https://remote.example/users/a");
    for (is_group, our_follow, follows_us) in [
        (false, None, false),
        (false, Some("https://social.example/follows/1"), true),
        (true, Some("https://social.example/follows/2"), false),
    ] {
        let sender = Sender {
            actor,
            is_group,
            our_follow,
            follows_us,
        };
        let _ = handle(OUR_ACTOR, &sender, &value);
    }
});
