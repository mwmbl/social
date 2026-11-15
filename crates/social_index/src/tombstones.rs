//! The in-memory mirror of the `tombstones` and `domain_blocks` tables: deletes and opt-outs
//! the index may still hold copies of. Search results and sweep records are checked against
//! it, and the sweep removes what it matches.

use std::collections::HashSet;

use crate::keys::Keyed;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tombstones {
    objects: HashSet<String>,
    authors: HashSet<String>,
    domains: HashSet<String>,
}

/// The lowercased host of an `http(s)://` URI, without port or userinfo.
#[must_use]
pub fn host_of(uri: &str) -> Option<String> {
    let (_, rest) = uri.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next()?;
    let host = match host_port.strip_prefix('[') {
        // An IPv6 literal keeps its colons.
        Some(v6) => v6.split(']').next()?,
        None => host_port.split(':').next()?,
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

impl Tombstones {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// One object (post or account) by URI.
    pub fn add_object(&mut self, uri: &str) {
        self.objects.insert(uri.to_string());
    }

    /// Every post by an author. Their account record is not affected.
    pub fn add_author(&mut self, actor_uri: &str) {
        self.authors.insert(actor_uri.to_string());
    }

    /// Everything from a domain and its subdomains.
    pub fn add_domain(&mut self, domain: &str) {
        self.domains
            .insert(domain.trim_end_matches('.').to_ascii_lowercase());
    }

    pub fn remove_object(&mut self, uri: &str) {
        self.objects.remove(uri);
    }

    pub fn remove_author(&mut self, actor_uri: &str) {
        self.authors.remove(actor_uri);
    }

    pub fn remove_domain(&mut self, domain: &str) {
        self.domains
            .remove(&domain.trim_end_matches('.').to_ascii_lowercase());
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.objects.len() + self.authors.len() + self.domains.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether a host, or any domain it is under, is blocked.
    #[must_use]
    pub fn domain_blocked(&self, host: &str) -> bool {
        let mut rest = host;
        loop {
            if self.domains.contains(rest) {
                return true;
            }
            match rest.split_once('.') {
                Some((_, parent)) if !parent.is_empty() => rest = parent,
                _ => return false,
            }
        }
    }

    fn uri_domain_blocked(&self, uri: &str) -> bool {
        // A URI we can't take a host from can't be checked against domain blocks, and
        // consent says when in doubt don't show it.
        host_of(uri).is_none_or(|host| self.domain_blocked(&host))
    }

    /// Whether this item must not be shown and should be removed from the index.
    #[must_use]
    pub fn blocks<T: Keyed>(&self, item: &T) -> bool {
        let author = item.author_uri();
        self.objects.contains(item.uri())
            || author.is_some_and(|a| self.authors.contains(a))
            || (!self.domains.is_empty()
                && (self.uri_domain_blocked(item.uri())
                    || author.is_some_and(|a| self.uri_domain_blocked(a))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::tests::{account, post};

    #[test]
    fn hosts() {
        assert_eq!(
            host_of("https://Mastodon.Social/users/a").as_deref(),
            Some("mastodon.social")
        );
        assert_eq!(
            host_of("https://u:p@h.example:8443/x?y#z").as_deref(),
            Some("h.example")
        );
        assert_eq!(host_of("https://[::1]:80/x").as_deref(), Some("::1"));
        assert_eq!(host_of("http://h.example").as_deref(), Some("h.example"));
        assert_eq!(host_of("not a uri"), None);
        assert_eq!(host_of("https:///path"), None);
    }

    #[test]
    fn objects_authors_and_domains() {
        let p = post("https://h.example/s/1", "x");
        let mut t = Tombstones::new();
        assert!(!t.blocks(&p));
        t.add_object("https://h.example/s/1");
        assert!(t.blocks(&p));
        t.remove_object("https://h.example/s/1");
        assert!(!t.blocks(&p));

        t.add_author("https://h.example/users/a");
        assert!(t.blocks(&p), "posts by a tombstoned author");
        let a = account("https://h.example/users/a", "a@h.example", "A");
        assert!(!t.blocks(&a), "but not the author's own account");
        t.add_domain("h.example");
        assert!(t.blocks(&a), "a domain block does cover accounts");
        t.remove_domain("h.example");
        t.remove_author("https://h.example/users/a");

        t.add_domain("Example.");
        assert!(t.blocks(&p), "subdomains of a blocked domain");
        assert!(t.domain_blocked("h.example"));
        assert!(!t.domain_blocked("example.org"));
        t.remove_domain("example");
        assert!(t.is_empty());
    }

    #[test]
    fn a_post_on_a_clean_host_by_an_author_on_a_blocked_one_is_blocked() {
        let mut p = post("https://clean.example/s/1", "x");
        p.author_uri = "https://bad.example/users/a".into();
        let mut t = Tombstones::new();
        t.add_domain("bad.example");
        assert!(t.blocks(&p));
    }

    #[test]
    fn with_domain_blocks_an_unparseable_uri_is_blocked() {
        let p = post("urn:x", "x");
        let mut t = Tombstones::new();
        assert!(!t.blocks(&p));
        t.add_domain("other.example");
        assert!(t.blocks(&p));
    }
}
