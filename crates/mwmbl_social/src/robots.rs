//! robots.txt, as RFC 9309 reads it, for crawl submissions (PLAN.md Stage 3 item 3). Pure.
//!
//! [`parse`] keeps only what applies to us:
//! - Groups are matched on our product token ([`product_token`], `mwmbl-social`),
//!   case-insensitively, and only on the token: `User-agent: mwmbl-social/2` matches, a token
//!   that merely contains ours does not. As in Mwmbl's crawler, matching words inside a longer
//!   user agent string is what made `User-agent: github` match it.
//! - Every group naming us is merged; with none, every `*` group is merged; with neither,
//!   nothing is disallowed.
//! - `Allow`/`Disallow` with `*` (any run) and a final `$` (end of URL). The longest matching
//!   pattern wins and `Allow` wins a tie. An empty `Disallow` disallows nothing.
//! - `Crawl-delay` (not in RFC 9309) in whole seconds, a fraction rounded up as Mwmbl does, the
//!   largest of the merged groups', and capped at [`MAX_CRAWL_DELAY_SECS`] when read.
//!
//! What applies to us is small, so it is what gets stored: [`Robots::to_text`] writes it back as
//! a single `User-agent: *` group, which [`parse`] reads to the same rules.

use std::fmt::Write as _;

/// The longest `Crawl-delay` honoured. A larger one would leave a host's crawl submissions
/// waiting for hours each.
pub const MAX_CRAWL_DELAY_SECS: u64 = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    allow: bool,
    /// Normalised as [`normalise`] does to paths.
    pattern: String,
}

/// The robots.txt rules that apply to us on one host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Robots {
    rules: Vec<Rule>,
    crawl_delay_secs: Option<u64>,
}

#[derive(Debug, Default)]
struct Group {
    agents: Vec<String>,
    rules: Vec<Rule>,
    crawl_delay_secs: Option<u64>,
}

/// The robots.txt product token of a `User-Agent` string: the product name before the first
/// `/` or space, lowercased. `mwmbl-social/0.1.0 (+https://social.example)` gives
/// `mwmbl-social`.
#[must_use]
pub fn product_token(user_agent: &str) -> String {
    agent_name(user_agent)
}

/// A `User-agent` line's value reduced to its product token, lowercased.
fn agent_name(value: &str) -> String {
    value
        .trim()
        .split(|c: char| c == '/' || c.is_whitespace())
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// `Crawl-delay` in whole seconds, a fraction rounded up. Not a non-negative number: `None`.
fn parse_delay(value: &str) -> Option<u64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let digits = |s: &str| s.chars().all(|c| c.is_ascii_digit());
    if (whole.is_empty() && fraction.is_empty()) || !digits(whole) || !digits(fraction) {
        return None;
    }
    let secs = if whole.is_empty() {
        0
    } else {
        // Only digits are left, so the one failure is overflow.
        whole.parse::<u64>().unwrap_or(u64::MAX)
    };
    Some(secs.saturating_add(u64::from(fraction.chars().any(|c| c != '0'))))
}

/// A path or pattern in one canonical form, so the two compare octet by octet: characters
/// outside printable ASCII are percent-encoded as UTF-8, and existing escapes are uppercased.
fn normalise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut hex_left = 0_u8;
    for c in text.chars() {
        if c.is_ascii_graphic() {
            if hex_left > 0 && c.is_ascii_hexdigit() {
                out.push(c.to_ascii_uppercase());
                hex_left -= 1;
            } else {
                hex_left = if c == '%' { 2 } else { 0 };
                out.push(c);
            }
        } else {
            hex_left = 0;
            let mut buf = [0_u8; 4];
            for byte in c.encode_utf8(&mut buf).bytes() {
                // Writing to a String cannot fail.
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// Whether `pattern` matches `path` from its start: `*` matches any run of octets, a final `$`
/// requires the path to end there, and otherwise a match of a prefix is enough.
fn matches(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(stem) => (stem, true),
        None => (pattern, false),
    };
    let (p, s) = (pattern.as_bytes(), path.as_bytes());
    let (mut pi, mut si) = (0_usize, 0_usize);
    // Where the last `*` was, and how much of the path it has taken so far.
    let mut star: Option<(usize, usize)> = None;
    loop {
        match p.get(pi) {
            Some(b'*') => {
                star = Some((pi, si));
                pi += 1;
                continue;
            }
            Some(c) if s.get(si) == Some(c) => {
                pi += 1;
                si += 1;
                continue;
            }
            None if !anchored || si == s.len() => return true,
            Some(_) | None => {}
        }
        // A mismatch: let the last `*` take one more octet, if there is one to take.
        match star {
            Some((star_at, taken)) if taken < s.len() => {
                star = Some((star_at, taken + 1));
                pi = star_at + 1;
                si = taken + 1;
            }
            _ => return false,
        }
    }
}

/// The rules in `text` that apply to the product token `token`.
#[must_use]
pub fn parse(text: &str, token: &str) -> Robots {
    let token = token.to_ascii_lowercase();
    let mut groups: Vec<Group> = Vec::new();
    // Whether the last field was a `User-agent`, so the next one joins the same group.
    let mut reading_agents = false;
    for line in text.trim_start_matches('\u{feff}').split(['\n', '\r']) {
        let line = line.split('#').next().unwrap_or_default();
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            "user-agent" => {
                if !reading_agents {
                    groups.push(Group::default());
                    reading_agents = true;
                }
                if let Some(group) = groups.last_mut() {
                    group.agents.push(agent_name(value));
                }
            }
            "allow" | "disallow" => {
                reading_agents = false;
                // Rules before any `User-agent` belong to no group.
                if let Some(group) = groups.last_mut()
                    && !value.is_empty()
                {
                    let pattern = if value.starts_with('/') || value.starts_with('*') {
                        normalise(value)
                    } else {
                        normalise(&format!("/{value}"))
                    };
                    group.rules.push(Rule {
                        allow: key == "allow",
                        pattern,
                    });
                }
            }
            "crawl-delay" => {
                reading_agents = false;
                if let Some(group) = groups.last_mut()
                    && let Some(secs) = parse_delay(value)
                {
                    group.crawl_delay_secs = group.crawl_delay_secs.max(Some(secs));
                }
            }
            // Sitemap and anything unknown: not part of a group, and not the end of one.
            _ => {}
        }
    }
    let ours: Vec<&Group> = groups
        .iter()
        .filter(|g| g.agents.contains(&token))
        .collect();
    let chosen = if ours.is_empty() {
        groups
            .iter()
            .filter(|g| g.agents.iter().any(|a| a == "*"))
            .collect()
    } else {
        ours
    };
    Robots {
        rules: chosen
            .iter()
            .flat_map(|g| g.rules.iter().cloned())
            .collect(),
        crawl_delay_secs: chosen.iter().filter_map(|g| g.crawl_delay_secs).max(),
    }
}

impl Robots {
    /// Whether we may fetch `path_and_query` (the URL's path, plus `?` and its query if any).
    #[must_use]
    pub fn allows(&self, path_and_query: &str) -> bool {
        let path = normalise(path_and_query);
        self.rules
            .iter()
            .filter(|rule| matches(&rule.pattern, &path))
            .max_by_key(|rule| (rule.pattern.len(), rule.allow))
            .is_none_or(|rule| rule.allow)
    }

    /// Whether [`Robots::allows`] a URL: its path and query.
    #[must_use]
    pub fn allows_url(&self, url: &url::Url) -> bool {
        match url.query() {
            Some(query) => self.allows(&format!("{}?{query}", url.path())),
            None => self.allows(url.path()),
        }
    }

    /// The `Crawl-delay` asked of us, at most [`MAX_CRAWL_DELAY_SECS`].
    #[must_use]
    pub fn crawl_delay_secs(&self) -> Option<u64> {
        self.crawl_delay_secs.map(|s| s.min(MAX_CRAWL_DELAY_SECS))
    }

    /// The rules as a robots.txt of one `User-agent: *` group; empty when there are none.
    #[must_use]
    pub fn to_text(&self) -> String {
        if self.rules.is_empty() && self.crawl_delay_secs.is_none() {
            return String::new();
        }
        let mut out = String::from("User-agent: *\n");
        for rule in &self.rules {
            let field = if rule.allow { "Allow" } else { "Disallow" };
            let _ = writeln!(out, "{field}: {}", rule.pattern);
        }
        if let Some(secs) = self.crawl_delay_secs {
            let _ = writeln!(out, "Crawl-delay: {secs}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const US: &str = "mwmbl-social";

    /// lemmy-ui's robots.txt, served the same on every instance: reconstructed from lemmy-ui's
    /// robots handler (a template string, hence the indentation) and PLAN.md's description.
    const LEMMY: &str = "User-Agent: *
        Disallow: /login
        Disallow: /login_reset
        Disallow: /settings
        Disallow: /create_community
        Disallow: /create_post
        Disallow: /create_private_message
        Disallow: /inbox
        Disallow: /setup
        Disallow: /admin
        Disallow: /password_change
        Disallow: /search
        Disallow: /modlog
        Disallow: /api
        Crawl-delay: 60";

    #[test]
    fn lemmy_allows_posts_comments_and_communities_only() {
        let robots = parse(LEMMY, US);
        let cases = [
            ("/post/1", true),
            ("/post/12345?scrollToComments=true", true),
            ("/comment/7", true),
            ("/c/rust", true),
            ("/c/rust@lemmy.example", true),
            ("/u/alice", true),
            ("/", true),
            ("/login", false),
            ("/login_reset", false),
            ("/settings", false),
            ("/search?q=rust", false),
            ("/search/q/rust", false),
            ("/modlog", false),
            ("/api/v3/post/list", false),
            ("/inbox", false),
            ("/admin", false),
        ];
        for (path, allowed) in cases {
            assert_eq!(robots.allows(path), allowed, "{path}");
        }
        assert_eq!(robots.crawl_delay_secs(), Some(60));
    }

    #[test]
    fn rules() {
        // (robots.txt, path, allowed)
        let cases: &[(&str, &str, bool)] = &[
            // No robots.txt rules, or none for us.
            ("", "/a", true),
            ("User-agent: other\nDisallow: /", "/a", true),
            ("Disallow: /\n", "/a", true),
            // Prefix match.
            ("User-agent: *\nDisallow: /a", "/a", false),
            ("User-agent: *\nDisallow: /a", "/abc", false),
            ("User-agent: *\nDisallow: /a", "/b", true),
            ("User-agent: *\nDisallow: /a/", "/a", true),
            // An empty Disallow disallows nothing.
            ("User-agent: *\nDisallow:", "/a", true),
            // Longest match wins, whichever comes first.
            ("User-agent: *\nDisallow: /\nAllow: /post/", "/post/1", true),
            ("User-agent: *\nAllow: /post/\nDisallow: /", "/c/x", false),
            (
                "User-agent: *\nAllow: /p\nDisallow: /post",
                "/post/1",
                false,
            ),
            // Allow wins a tie.
            (
                "User-agent: *\nDisallow: /page\nAllow: /page",
                "/page",
                true,
            ),
            (
                "User-agent: *\nAllow: /page\nDisallow: /page",
                "/page",
                true,
            ),
            // Wildcards.
            ("User-agent: *\nDisallow: /*.php", "/index.php", false),
            ("User-agent: *\nDisallow: /*.php", "/a/b.php?x=1", false),
            ("User-agent: *\nDisallow: /*.php$", "/a/b.php?x=1", true),
            ("User-agent: *\nDisallow: /*.php$", "/a/b.php", false),
            ("User-agent: *\nDisallow: /a*b*c", "/aXbYc/z", false),
            ("User-agent: *\nDisallow: /a*b*c", "/aXbY", true),
            ("User-agent: *\nDisallow: /fish$", "/fish", false),
            ("User-agent: *\nDisallow: /fish$", "/fish/", true),
            ("User-agent: *\nDisallow: *private", "/a/private", false),
            ("User-agent: *\nDisallow: /$", "/", false),
            ("User-agent: *\nDisallow: /$", "/a", true),
            // The query is part of what is matched.
            (
                "User-agent: *\nDisallow: /*?sort=",
                "/c/rust?sort=new",
                false,
            ),
            ("User-agent: *\nDisallow: /*?sort=", "/c/rust", true),
            // A rule without a leading slash gets one.
            ("User-agent: *\nDisallow: private", "/private", false),
            // Comments, case, whitespace, CR line ends and a byte order mark.
            (
                "\u{feff}# hi\r\nUSER-AGENT : *  # all\r\n  disallow :/x # no\r\n",
                "/x",
                false,
            ),
            ("User-agent: *\rDisallow: /x\r", "/x", false),
            // Percent-encoding compares as octets: non-ASCII in a rule, lowercase hex.
            ("User-agent: *\nDisallow: /ä", "/%C3%A4", false),
            ("User-agent: *\nDisallow: /%c3%a4", "/%C3%A4", false),
            ("User-agent: *\nDisallow: /%C3%A4", "/ä", false),
        ];
        for (text, path, allowed) in cases {
            assert_eq!(parse(text, US).allows(path), *allowed, "{text:?} {path}");
        }
    }

    #[test]
    fn groups() {
        // (robots.txt, path, allowed)
        let cases: &[(&str, &str, bool)] = &[
            // Our own group replaces `*`.
            (
                "User-agent: *\nDisallow: /\n\nUser-agent: mwmbl-social\nAllow: /",
                "/a",
                true,
            ),
            (
                "User-agent: *\nAllow: /\n\nUser-agent: mwmbl-social\nDisallow: /a",
                "/a",
                false,
            ),
            // Matched case-insensitively and on the product token, a version included.
            ("User-agent: MWMBL-Social\nDisallow: /a", "/a", false),
            ("User-agent: mwmbl-social/2.0\nDisallow: /a", "/a", false),
            // A token that only contains ours, or that ours contains, is someone else.
            ("User-agent: mwmbl\nDisallow: /a", "/a", true),
            ("User-agent: mwmbl-social-bot\nDisallow: /a", "/a", true),
            ("User-agent: social\nDisallow: /a", "/a", true),
            // Several user agents share one group.
            (
                "User-agent: googlebot\nUser-agent: mwmbl-social\nDisallow: /a",
                "/a",
                false,
            ),
            // Every group naming us is merged.
            (
                "User-agent: mwmbl-social\nDisallow: /a\n\nUser-agent: x\nDisallow: /b\n\n\
                 User-agent: mwmbl-social\nDisallow: /c",
                "/c",
                false,
            ),
            // Every `*` group is merged too: the first one allowing all does not hide the rest.
            (
                "User-agent: *\nAllow: /\n\nUser-agent: *\nDisallow: /private",
                "/private/x",
                false,
            ),
            // A User-agent after rules starts a new group.
            (
                "User-agent: mwmbl-social\nDisallow: /a\nUser-agent: other\nDisallow: /b",
                "/b",
                true,
            ),
            // Sitemap lines do not end a group's user agents.
            (
                "User-agent: other\nSitemap: https://x.example/s.xml\nUser-agent: mwmbl-social\n\
                 Disallow: /a",
                "/a",
                false,
            ),
        ];
        for (text, path, allowed) in cases {
            assert_eq!(parse(text, US).allows(path), *allowed, "{text:?} {path}");
        }
    }

    #[test]
    fn crawl_delay() {
        let delay = |text: &str| parse(text, US).crawl_delay_secs();
        assert_eq!(delay("User-agent: *\nCrawl-delay: 10"), Some(10));
        assert_eq!(delay("User-agent: *\nCrawl-delay: 0.5"), Some(1));
        assert_eq!(delay("User-agent: *\nCrawl-delay: 2.0"), Some(2));
        assert_eq!(delay("User-agent: *\nCrawl-delay: .25"), Some(1));
        assert_eq!(delay("User-agent: *\nCrawl-delay: 86400"), Some(300));
        assert_eq!(
            delay("User-agent: *\nCrawl-delay: 99999999999999999999999"),
            Some(300)
        );
        for bad in ["-1", "soon", "", ".", "1e3", "1.2.3"] {
            assert_eq!(delay(&format!("User-agent: *\nCrawl-delay: {bad}")), None);
        }
        // Another agent's delay, or one outside any group, is not ours.
        assert_eq!(
            delay("Crawl-delay: 5\nUser-agent: other\nCrawl-delay: 9"),
            None
        );
        // Our group's, else the largest of the `*` groups'.
        assert_eq!(
            delay("User-agent: *\nCrawl-delay: 9\nUser-agent: mwmbl-social\nCrawl-delay: 3"),
            Some(3),
            "a User-agent after a Crawl-delay starts our own group"
        );
        assert_eq!(
            delay("User-agent: *\nUser-agent: mwmbl-social\nCrawl-delay: 3"),
            Some(3),
            "one group, two agents"
        );
        assert_eq!(
            delay(
                "User-agent: *\nCrawl-delay: 9\n\nUser-agent: mwmbl-social\nAllow: /\nCrawl-delay: 3"
            ),
            Some(3)
        );
        assert_eq!(
            delay("User-agent: *\nCrawl-delay: 2\n\nUser-agent: *\nCrawl-delay: 7"),
            Some(7)
        );
    }

    #[test]
    fn the_stored_form_reads_back_to_the_same_rules() {
        for text in [
            LEMMY,
            "",
            "User-agent: mwmbl-social\nAllow: /a$\nDisallow: /*.php\nCrawl-delay: 4",
            "User-agent: *\nDisallow: /ä\n",
            "User-agent: other\nDisallow: /",
        ] {
            let robots = parse(text, US);
            let stored = robots.to_text();
            assert_eq!(parse(&stored, US), robots, "{text:?}");
            assert_eq!(parse(&stored, "anyone-else"), robots, "stored for us alone");
        }
        assert_eq!(parse("User-agent: other\nDisallow: /", US).to_text(), "");
        assert_eq!(
            parse("User-agent: *\nDisallow: /a\nCrawl-delay: 2", US).to_text(),
            "User-agent: *\nDisallow: /a\nCrawl-delay: 2\n"
        );
    }

    #[test]
    fn urls_are_matched_on_path_and_query() -> Result<(), url::ParseError> {
        let robots = parse("User-agent: *\nDisallow: /*?sort=\nDisallow: /x$", US);
        let url = |s: &str| url::Url::parse(s);
        assert!(!robots.allows_url(&url("https://l.example/c/a?sort=new")?));
        assert!(robots.allows_url(&url("https://l.example/c/a")?));
        assert!(!robots.allows_url(&url("https://l.example/x#frag")?));
        assert!(robots.allows_url(&url("https://l.example/x?a=1")?));
        Ok(())
    }

    #[test]
    fn product_tokens() {
        assert_eq!(
            product_token("mwmbl-social/0.1.0 (+https://social.example)"),
            "mwmbl-social"
        );
        assert_eq!(product_token("Mwmbl-Social"), "mwmbl-social");
        assert_eq!(product_token("mwmbl-social-test"), "mwmbl-social-test");
        assert_eq!(product_token(""), "");
    }
}
