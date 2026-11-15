//! Parsing fetched JSON. Fixtures are shared with `tests/consent/` (read-only).

use apub::objects::{ParseError, html_to_text, parse_actor, parse_post, parse_rfc3339};
use serde_json::{Value, json};

fn load(name: &str) -> Value {
    let path = format!("{}/tests/consent/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

#[test]
fn mastodon_note() {
    let p = parse_post(&load("mastodon_note")).unwrap();
    assert_eq!(p.uri, "https://mastodon.example/users/alice/statuses/1001");
    assert_eq!(p.author_uri, "https://mastodon.example/users/alice");
    assert_eq!(p.content_text, "Planting garlic today #gardening");
    assert_eq!(p.summary_text, None);
    assert_eq!(p.name, None);
    assert_eq!(p.published, Some(1_793_534_400)); // 2026-11-01T12:00:00Z
    assert_eq!(p.updated, None);
    assert_eq!(p.language.as_deref(), Some("en"));
    assert_eq!(p.hashtags, vec!["gardening"]);
    assert_eq!(p.in_reply_to, None);
    // `replies` is embedded but has no totalItems.
    assert_eq!((p.likes, p.shares, p.replies), (None, None, None));
    assert!(!p.sensitive);
}

#[test]
fn gotosocial_note() {
    let p = parse_post(&load("gotosocial_note")).unwrap();
    assert_eq!(p.content_text, "The night train to Vienna is lovely.");
    assert_eq!(
        p.summary_text, None,
        "an empty summary is no content warning"
    );
    assert_eq!(p.language.as_deref(), Some("en"));
    assert!(p.hashtags.is_empty());
}

#[test]
fn misskey_note() {
    let p = parse_post(&load("misskey_note")).unwrap();
    assert_eq!(p.content_text, "My cat is asleep on the keyboard");
    assert_eq!(p.published, Some(1_793_700_000)); // 2026-11-03T10:00:00.000Z
    assert_eq!(p.language, None);
}

#[test]
fn pleroma_note_with_fractional_seconds() {
    let p = parse_post(&load("pleroma_note")).unwrap();
    assert_eq!(p.content_text, "Third coffee of the morning");
    assert_eq!(p.author_uri, "https://pleroma.example/users/dave");
    assert_eq!(p.published, parse_rfc3339("2026-11-04T07:00:00Z"));
}

#[test]
fn rich_post() {
    let v = json!({
        "id": "https://a.example/n/1",
        "type": ["Note", "Extra"],
        "attributedTo": [{"id": "https://a.example/u/x", "type": "Person"}],
        "summary": "<p>CW: spiders &amp; webs</p>",
        "name": "  A title ",
        "content": null,
        "contentMap": {"de": "<p>Hallo<br>Welt</p>", "en": "<p>Hello</p>"},
        "published": "2026-11-01T12:00:00+01:00",
        "updated": "2026-11-02T12:00:00.123Z",
        "inReplyTo": {"id": "https://b.example/n/9"},
        "sensitive": true,
        "likes": {"type": "Collection", "totalItems": 7},
        "shares": {"type": "Collection", "totalItems": 3},
        "replies": {"type": "Collection", "totalItems": 2},
        "tag": [
            {"type": "Hashtag", "name": "#Rust"},
            {"type": "Hashtag", "name": "Rust"},
            {"type": "Mention", "name": "@bob@b.example"},
            {"type": "Emoji", "name": ":blob:"},
            {"type": "Hashtag", "name": "#"}
        ]
    });
    let p = parse_post(&v).unwrap();
    assert_eq!(p.author_uri, "https://a.example/u/x");
    assert_eq!(p.summary_text.as_deref(), Some("CW: spiders & webs"));
    assert_eq!(p.name.as_deref(), Some("A title"));
    // serde_json's maps are sorted, so the "first" contentMap entry is the alphabetical one.
    assert_eq!(p.language.as_deref(), Some("de"));
    assert_eq!(p.content_text, "Hallo\nWelt");
    assert_eq!(p.published, Some(1_793_534_400 - 3600));
    assert_eq!(p.updated, Some(1_793_620_800));
    assert_eq!(p.in_reply_to.as_deref(), Some("https://b.example/n/9"));
    assert!(p.sensitive);
    assert_eq!((p.likes, p.shares, p.replies), (Some(7), Some(3), Some(2)));
    assert_eq!(p.hashtags, vec!["Rust"]);
}

#[test]
fn single_hashtag_object_and_missing_content() {
    let v = json!({
        "id": "https://a.example/n/2", "type": "Article", "attributedTo": "https://a.example/u/x",
        "name": "Just a title", "tag": {"type": "Hashtag", "name": "#solo"}
    });
    let p = parse_post(&v).unwrap();
    assert_eq!(p.content_text, "");
    assert_eq!(p.hashtags, vec!["solo"]);
}

#[test]
fn post_errors() {
    assert_eq!(parse_post(&json!("x")), Err(ParseError::NotAnObject));
    assert_eq!(
        parse_post(&json!({"type": "Note"})),
        Err(ParseError::MissingId)
    );
    assert!(matches!(
        parse_post(&json!({"id": "https://a.example/1", "type": "Person"})),
        Err(ParseError::UnsupportedType { kind, .. }) if kind == "Person"
    ));
    assert!(matches!(
        parse_post(&json!({"id": "https://a.example/1", "type": "Note"})),
        Err(ParseError::MissingField {
            field: "attributedTo",
            ..
        })
    ));
}

#[test]
fn mastodon_actor() {
    let a = parse_actor(&load("mastodon_actor")).unwrap();
    assert_eq!(a.uri, "https://mastodon.example/users/alice");
    assert_eq!(a.preferred_username, "alice");
    assert_eq!(a.name.as_deref(), Some("Alice"));
    assert_eq!(a.summary_text.as_deref(), Some("Rust and gardens."));
    assert_eq!(a.host, "mastodon.example");
    assert_eq!(a.acct, "alice@mastodon.example");
    assert_eq!(a.kind, "Person");
    assert_eq!(a.published, parse_rfc3339("2022-11-01T00:00:00Z"));
    assert!(a.discoverable && a.indexable);
    assert_eq!((a.followers, a.following, a.statuses), (None, None, None));
    let key = a.public_key.unwrap();
    assert_eq!(key.id, "https://mastodon.example/users/alice#main-key");
    assert_eq!(key.owner, a.uri);
    assert!(key.pem.starts_with("-----BEGIN PUBLIC KEY-----"));
    assert_eq!(a.inbox, "https://mastodon.example/users/alice/inbox");
    assert_eq!(
        a.shared_inbox.as_deref(),
        Some("https://mastodon.example/inbox")
    );
}

#[test]
fn other_servers_actors() {
    let g = parse_actor(&load("gotosocial_actor")).unwrap();
    assert_eq!(g.acct, "bob@gts.example");
    assert_eq!(
        g.shared_inbox.as_deref(),
        Some("https://gts.example/sharedInbox")
    );
    let k = parse_actor(&load("misskey_actor")).unwrap();
    assert_eq!(k.acct, "carol@misskey.example");
    assert_eq!(
        k.shared_inbox.as_deref(),
        Some("https://misskey.example/inbox")
    );
    let p = parse_actor(&load("pleroma_actor")).unwrap();
    assert!(!p.preferred_username.is_empty());
}

#[test]
fn actor_with_counts_and_flags_defaults() {
    let v = json!({
        "id": "https://x.example:8443/actor", "type": "Group", "preferredUsername": "club",
        "inbox": "https://x.example:8443/inbox",
        "discoverable": null,
        "followers": {"type": "OrderedCollection", "totalItems": 10},
        "following": {"type": "OrderedCollection", "totalItems": 2},
        "outbox": {"type": "OrderedCollection", "totalItems": 99},
        "publicKey": {"id": "k"}
    });
    let a = parse_actor(&v).unwrap();
    assert_eq!(a.host, "x.example:8443");
    assert_eq!(a.acct, "club@x.example:8443");
    assert_eq!(a.kind, "Group");
    assert!(!a.discoverable && !a.indexable);
    assert_eq!(
        (a.followers, a.following, a.statuses),
        (Some(10), Some(2), Some(99))
    );
    assert_eq!(a.public_key, None);
    assert_eq!(a.shared_inbox, None);
}

#[test]
fn actor_errors() {
    let base = json!({"id": "https://a.example/u", "type": "Person", "preferredUsername": "u", "inbox": "https://a.example/i"});
    assert!(parse_actor(&base).is_ok());
    let mut no_inbox = base.clone();
    no_inbox.as_object_mut().unwrap().remove("inbox");
    assert!(matches!(
        parse_actor(&no_inbox),
        Err(ParseError::MissingField { field: "inbox", .. })
    ));
    let mut no_name = base.clone();
    no_name.as_object_mut().unwrap().remove("preferredUsername");
    assert!(matches!(
        parse_actor(&no_name),
        Err(ParseError::MissingField {
            field: "preferredUsername",
            ..
        })
    ));
    assert!(matches!(
        parse_actor(&json!({"id": "https://a.example/u", "type": "Note"})),
        Err(ParseError::UnsupportedType { .. })
    ));
    let mut bad = base;
    bad["id"] = json!("not a uri");
    assert!(matches!(parse_actor(&bad), Err(ParseError::BadUri { .. })));
}

#[test]
fn rfc3339() {
    let cases: [(&str, Option<i64>); 16] = [
        ("1970-01-01T00:00:00Z", Some(0)),
        ("1970-01-01T00:00:01Z", Some(1)),
        ("2000-02-29T00:00:00Z", Some(951_782_400)),
        ("2026-11-01T12:00:00Z", Some(1_793_534_400)),
        ("2026-11-01T12:00:00.999999Z", Some(1_793_534_400)),
        ("2026-11-01T12:00:00z", Some(1_793_534_400)),
        ("2026-11-01t12:00:00Z", Some(1_793_534_400)),
        ("2026-11-01T12:00:00+02:00", Some(1_793_534_400 - 7200)),
        ("2026-11-01T12:00:00-05:30", Some(1_793_534_400 + 19_800)),
        ("2026-11-01T12:00:00", Some(1_793_534_400)),
        ("1969-12-31T23:59:59Z", Some(-1)),
        ("2026-02-29T00:00:00Z", None),
        ("2026-13-01T00:00:00Z", None),
        ("2026-11-01T24:00:00Z", None),
        ("2026-11-01T12:00:00.Z", None),
        ("yesterday", None),
    ];
    for (text, expected) in cases {
        assert_eq!(parse_rfc3339(text), expected, "{text}");
    }
    assert_eq!(parse_rfc3339("2026-11-01T12:00:00Z "), Some(1_793_534_400));
    assert_eq!(
        parse_rfc3339("2026-11-01T12:00:00+0200"),
        Some(1_793_534_400 - 7200)
    );
    assert_eq!(parse_rfc3339(""), None);
}

#[test]
fn html_basics() {
    let cases = [
        ("", ""),
        ("plain", "plain"),
        ("<p>one</p><p>two</p>", "one\ntwo"),
        ("a<br>b<br/>c<br />d", "a\nb\nc\nd"),
        ("<p>a</p><p></p><p></p><p>b</p>", "a\n\nb"),
        ("<ul><li>x</li><li>y</li></ul>", "x\ny"),
        (
            "<h1>T</h1><p>body</p><blockquote>q</blockquote>",
            "T\nbody\nq",
        ),
        ("  lots   of \n\t space  ", "lots of space"),
        ("<div>a</div><div>b</div>", "a\nb"),
        ("a <b>bold</b> <i>word</i>", "a bold word"),
        ("1 < 2 and 3 > 2", "1 < 2 and 3 > 2"),
        ("<3 you", "<3 you"),
        ("x<script>alert('<p>')</script>y", "xy"),
        ("x<STYLE type=\"text/css\">p{}</style>y", "xy"),
        ("a<!-- hidden <p> -->b", "ab"),
        ("<!DOCTYPE html>t", "t"),
        (
            "<a href=\"https://x.example/?a=1&b=2\" title=\"a > b\">link</a>",
            "link",
        ),
        ("unterminated <a href=\"x", "unterminated"),
    ];
    for (html, text) in cases {
        assert_eq!(html_to_text(html), text, "{html:?}");
    }
}

#[test]
fn html_entities() {
    let cases = [
        ("&amp;&lt;&gt;&quot;&apos;", "&<>\"'"),
        ("a&nbsp;&nbsp;b", "a b"),
        (
            "&hellip;&mdash;&ndash;&lsquo;&rsquo;&ldquo;&rdquo;",
            "\u{2026}\u{2014}\u{2013}\u{2018}\u{2019}\u{201c}\u{201d}",
        ),
        ("&copy;&reg;&trade;", "\u{a9}\u{ae}\u{2122}"),
        ("&#65;&#x42;&#X43;", "ABC"),
        ("&#128512;", "\u{1f600}"),
        ("&#0;&#xD800;&#99999999;", "\u{fffd}\u{fffd}\u{fffd}"),
        (
            "AT&T and &unknown; and & alone",
            "AT&T and &unknown; and & alone",
        ),
        ("&amp", "&amp"),
        ("&amp;lt;", "&lt;"),
        ("&#;&#x;", "&#;&#x;"),
    ];
    for (html, text) in cases {
        assert_eq!(html_to_text(html), text, "{html:?}");
    }
}

#[test]
fn mastodon_links_and_mentions() {
    let html = "<p>Read <a href=\"https://example.com/a/very/long/path\" rel=\"nofollow noopener\" target=\"_blank\"><span class=\"invisible\">https://</span><span class=\"ellipsis\">example.com/a/very/lo</span><span class=\"invisible\">ng/path</span></a> cc <span class=\"h-card\"><a href=\"https://b.example/@bob\" class=\"u-url mention\">@<span>bob</span></a></span> <a href=\"https://m.example/tags/rust\" class=\"mention hashtag\" rel=\"tag\">#<span>rust</span></a></p>";
    assert_eq!(
        html_to_text(html),
        "Read example.com/a/very/lo\u{2026} cc @bob #rust"
    );
    // Nested spans inside an invisible one are dropped too, and the count recovers afterwards.
    assert_eq!(
        html_to_text(
            "a<span class='invisible'>x<span>y</span><span class=\"ellipsis\">z</span></span>b<span class=\"ellipsis\">c</span>"
        ),
        "abc\u{2026}"
    );
    assert_eq!(
        html_to_text("<span class=\"visible invisible2\">v</span>"),
        "v"
    );
    assert_eq!(
        html_to_text("<span class=invisible>gone</span>kept"),
        "kept"
    );
}

#[test]
fn html_never_panics() {
    // Deterministic pseudo-random inputs, biased towards markup characters.
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let pieces = [
        "<",
        ">",
        "</",
        "/>",
        "&",
        ";",
        "#",
        "x",
        "X",
        "=",
        "\"",
        "'",
        " ",
        "\n",
        "span",
        "script",
        "style",
        "class",
        "invisible",
        "ellipsis",
        "br",
        "p",
        "<!--",
        "-->",
        "<!",
        "<?",
        "&#",
        "&#x",
        "&amp;",
        "é",
        "日本",
        "\u{1f600}",
        "\0",
        "\u{a0}",
        "<span class=\"invisible\">",
        "</span>",
        "<script>",
        "</script",
        "</p>",
        "a",
        "1",
    ];
    for _ in 0..4000 {
        let len = usize::try_from(next() % 40).unwrap();
        let mut s = String::new();
        for _ in 0..len {
            if next() % 5 == 0 {
                let c = char::from_u32(u32::try_from(next() % 0x11_0000).unwrap()).unwrap_or('?');
                s.push(c);
            } else {
                s.push_str(pieces[usize::try_from(next()).unwrap() % pieces.len()]);
            }
        }
        let out = html_to_text(&s);
        assert!(!out.starts_with(char::is_whitespace) && !out.ends_with(char::is_whitespace));
        assert!(!out.contains("  "));
    }
    // Pathological nesting and length.
    let _ = html_to_text(&"<span class=\"invisible\">".repeat(50_000));
    let _ = html_to_text(&"</span>".repeat(50_000));
    let _ = html_to_text(&"<".repeat(50_000));
    let _ = html_to_text(&"&".repeat(50_000));
    let _ = html_to_text(&format!("<a {}", "x=\"".repeat(10_000)));
}
