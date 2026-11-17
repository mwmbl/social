//! Keeping outbound requests off private networks (SSRF).
//!
//! Every host this project contacts is chosen by someone else: a server being registered, an
//! announced URI, an actor's inbox, a crawl submission. A name that resolves to loopback, a
//! private network or a cloud metadata address must never be contacted, or anyone who can make
//! us fetch a URI could reach services behind our firewall.
//!
//! Two pieces do this, both from here so there is one policy:
//!
//! - [`client_builder`] starts every `reqwest` client. It installs [`PublicResolver`], which
//!   resolves names with the system resolver and drops every address that [`is_public`]
//!   refuses, failing when none remain. The check is on the addresses the connection is then
//!   made to, so DNS rebinding between a check and the connect is impossible. It also refuses
//!   redirects to IP literals that aren't public.
//! - [`check_url`] refuses URLs whose host is an IP literal that isn't public, because `reqwest`
//!   connects to a literal without asking the resolver. Callers run it where a URL is accepted.
//!
//! Both are off when `allow_private` is set: tests and the Docker dev setups talk to loopback
//! and private Docker networks.
//!
//! A proxy configured through the environment (`HTTPS_PROXY` and friends) resolves names
//! itself, so with one the filter applies to the proxy's address only.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use thiserror::Error;
use url::{Host, Url};

/// Redirects a client from [`client_builder`] follows, as `reqwest`'s default policy does.
const MAX_REDIRECTS: usize = 10;

/// An address we will not connect to, or a name we could not resolve.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AddressError {
    /// The URL's host is an IP literal that isn't public.
    #[error("`{host}` is not a public address, so it is not contacted")]
    NotPublic { host: String },
    /// Every address the name resolves to is non-public (or it resolves to none).
    #[error("`{host}` resolves only to non-public addresses, so it is not contacted")]
    ResolvesToNonPublic { host: String },
    /// The system resolver failed.
    #[error("failed to resolve `{host}`: {reason}")]
    Lookup { host: String, reason: String },
    /// A redirect chain was too long.
    #[error("too many redirects, the last to `{url}`")]
    TooManyRedirects { url: String },
}

/// Whether `ip` is a globally reachable unicast address: one we may connect to.
///
/// Refused: unspecified, loopback, private (RFC 1918), CGNAT, link-local (including the
/// 169.254.169.254 metadata address), IETF protocol assignments, documentation, benchmarking,
/// multicast, reserved and broadcast IPv4; and for IPv6 everything outside global unicast
/// (`2000::/3`), plus the documentation, IETF assignment (Teredo included) and local NAT64
/// ranges inside it. IPv6 forms that carry an IPv4 address (mapped, compatible, NAT64 and 6to4)
/// are judged by that address. When in doubt, refuse.
#[must_use]
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let refused = a == 0 // "this network", unspecified
        || a == 10 // private
        || (a == 100 && (64..128).contains(&b)) // CGNAT, 100.64/10
        || a == 127 // loopback
        || (a == 169 && b == 254) // link-local, cloud metadata
        || (a == 172 && (16..32).contains(&b)) // private, 172.16/12
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 192 && b == 0 && c == 2) // documentation
        || (a == 192 && b == 88 && c == 99) // 6to4 relay anycast
        || (a == 192 && b == 168) // private
        || (a == 198 && (b == 18 || b == 19)) // benchmarking, 198.18/15
        || (a == 198 && b == 51 && c == 100) // documentation
        || (a == 203 && b == 0 && c == 113) // documentation
        || a >= 224; // multicast, reserved, broadcast
    !refused
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    let [s0, s1, s2, s3, s4, s5, _, _] = segments;
    let octets = ip.octets();
    let [.., w, x, y, z] = octets;
    let embedded = Ipv4Addr::new(w, x, y, z);
    // ::ffff:a.b.c.d (mapped) and ::a.b.c.d (compatible, deprecated). `::` and `::1` are
    // compatible-shaped too, and are refused as 0.0.0.0 and 0.0.0.1.
    if s0 == 0 && s1 == 0 && s2 == 0 && s3 == 0 && s4 == 0 && (s5 == 0xffff || s5 == 0) {
        return is_public_v4(embedded);
    }
    // 64:ff9b::/96, NAT64 well-known prefix.
    if s0 == 0x64 && s1 == 0xff9b && s2 == 0 && s3 == 0 && s4 == 0 && s5 == 0 {
        return is_public_v4(embedded);
    }
    // 2002::/16, 6to4: the IPv4 address is in the second and third segments.
    if s0 == 0x2002 {
        let [hi, lo] = s1.to_be_bytes();
        let [hi2, lo2] = s2.to_be_bytes();
        return is_public_v4(Ipv4Addr::new(hi, lo, hi2, lo2));
    }
    // Everything else outside 2000::/3 (link-local, unique-local, multicast, discard, local
    // NAT64 64:ff9b:1::/48, ...) is not global unicast.
    if s0 & 0xe000 != 0x2000 {
        return false;
    }
    // 2001::/23 is IETF protocol assignments (Teredo included), 2001:db8::/32 and 3fff::/20
    // are documentation.
    let refused = (s0 == 0x2001 && (s1 < 0x0200 || s1 == 0x0db8)) || (s0 == 0x3fff && s1 < 0x1000);
    !refused
}

/// Refuses `url` when its host is an IP literal that isn't public (unless `allow_private`).
/// A name is left to [`PublicResolver`]; a URL with no host passes, for the caller to reject.
pub fn check_url(url: &Url, allow_private: bool) -> Result<(), AddressError> {
    if allow_private {
        return Ok(());
    }
    let ip = match url.host() {
        Some(Host::Ipv4(ip)) => IpAddr::V4(ip),
        Some(Host::Ipv6(ip)) => IpAddr::V6(ip),
        Some(Host::Domain(_)) | None => return Ok(()),
    };
    if is_public(ip) {
        Ok(())
    } else {
        Err(AddressError::NotPublic {
            host: url.host_str().unwrap_or_default().to_owned(),
        })
    }
}

/// The first [`AddressError`] in `err`'s source chain: why a request that `reqwest` reports as
/// a connect failure was really refused.
#[must_use]
pub fn address_error<'a>(err: &'a (dyn std::error::Error + 'static)) -> Option<&'a AddressError> {
    let mut next = Some(err);
    while let Some(err) = next {
        if let Some(found) = err.downcast_ref::<AddressError>() {
            return Some(found);
        }
        next = err.source();
    }
    None
}

/// A `reqwest` resolver that uses the system resolver and keeps only public addresses.
#[derive(Debug, Clone, Copy, Default)]
pub struct PublicResolver;

/// The public addresses among `addrs`, or why there are none.
fn public_addresses(
    host: &str,
    addrs: impl Iterator<Item = SocketAddr>,
) -> Result<Vec<SocketAddr>, AddressError> {
    let public: Vec<SocketAddr> = addrs.filter(|addr| is_public(addr.ip())).collect();
    if public.is_empty() {
        tracing::debug!(
            host,
            "refusing a host that resolves only to non-public addresses"
        );
        return Err(AddressError::ResolvesToNonPublic {
            host: host.to_owned(),
        });
    }
    Ok(public)
}

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            // The port is replaced by the connector.
            let addrs = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| AddressError::Lookup {
                    host: host.clone(),
                    reason: e.to_string(),
                })?;
            let public = public_addresses(&host, addrs)?;
            let addrs: Addrs = Box::new(public.into_iter());
            Ok(addrs)
        })
    }
}

/// The start of every HTTP client this project builds. Unless `allow_private`, names are
/// resolved by [`PublicResolver`] and redirects to non-public IP literals are refused (other
/// redirects are followed, up to 10, as by default). Callers still run [`check_url`] on the
/// URLs they request.
pub fn client_builder(allow_private: bool) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder();
    if allow_private {
        return builder;
    }
    builder
        .dns_resolver(PublicResolver)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                let error = AddressError::TooManyRedirects {
                    url: attempt.url().to_string(),
                };
                return attempt.error(error);
            }
            match check_url(attempt.url(), false) {
                Ok(()) => attempt.follow(),
                Err(e) => attempt.error(e),
            }
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_addresses() {
        let cases: &[(&str, bool)] = &[
            // IPv4 public
            ("1.1.1.1", true),
            ("8.8.8.8", true),
            ("93.184.216.34", true),
            ("100.63.255.255", true),
            ("100.128.0.0", true),
            ("172.15.255.255", true),
            ("172.32.0.0", true),
            ("192.0.1.1", true),
            ("198.17.255.255", true),
            ("198.20.0.0", true),
            ("223.255.255.255", true),
            // IPv4 refused
            ("0.0.0.0", false),
            ("0.1.2.3", false),
            ("10.0.0.1", false),
            ("10.255.255.255", false),
            ("100.64.0.1", false),
            ("100.127.255.255", false),
            ("127.0.0.1", false),
            ("127.255.255.254", false),
            ("169.254.169.254", false),
            ("169.254.0.1", false),
            ("172.16.0.1", false),
            ("172.31.255.255", false),
            ("192.0.0.8", false),
            ("192.0.2.1", false),
            ("192.88.99.1", false),
            ("192.168.1.1", false),
            ("198.18.0.1", false),
            ("198.19.255.255", false),
            ("198.51.100.1", false),
            ("203.0.113.1", false),
            ("224.0.0.1", false),
            ("239.255.255.255", false),
            ("240.0.0.1", false),
            ("255.255.255.255", false),
            // IPv6 public
            ("2606:4700:4700::1111", true),
            ("2a00:1450:4001:80b::200e", true),
            ("2001:4860:4860::8888", true),
            ("3ffe::1", true),
            // IPv6 refused
            ("::", false),
            ("::1", false),
            ("fe80::1", false),
            ("febf::1", false),
            ("fec0::1", false),
            ("fc00::1", false),
            ("fd12:3456:789a::1", false),
            ("ff02::1", false),
            ("ff0e::1", false),
            ("100::1", false),
            ("64:ff9b:1::1", false),
            ("2001::1", false),
            ("2001:0:4136:e378::1", false),
            ("2001:1ff::1", false),
            ("2001:db8::1", false),
            ("3fff::1", false),
            ("3fff:fff::1", false),
            ("4000::1", false),
            ("1::1", false),
            // IPv4 inside IPv6: judged by the IPv4 address
            ("::ffff:127.0.0.1", false),
            ("::ffff:10.0.0.1", false),
            ("::ffff:169.254.169.254", false),
            ("::ffff:192.168.0.1", false),
            ("::ffff:8.8.8.8", true),
            ("::127.0.0.1", false),
            ("::10.1.2.3", false),
            ("::8.8.8.8", true),
            ("64:ff9b::127.0.0.1", false),
            ("64:ff9b::a9fe:a9fe", false),
            ("64:ff9b::808:808", true),
            ("2002:7f00:1::", false),
            ("2002:a9fe:a9fe::1", false),
            ("2002:c0a8:101::1", false),
            ("2002:808:808::1", true),
        ];
        for (text, public) in cases {
            let Ok(ip) = text.parse::<IpAddr>() else {
                panic!("{text} is not an address");
            };
            assert_eq!(is_public(ip), *public, "{text}");
        }
    }

    #[test]
    fn checks_ip_literal_hosts() {
        let cases: &[(&str, bool, bool)] = &[
            ("https://example.com/x", false, true),
            ("https://localhost/x", false, true), // a name: the resolver decides
            ("https://8.8.8.8/x", false, true),
            ("https://[2606:4700:4700::1111]/x", false, true),
            ("https://127.0.0.1/x", false, false),
            ("https://127.0.0.1/x", true, true),
            ("http://169.254.169.254/latest/meta-data", false, false),
            ("https://[::1]:8080/x", false, false),
            ("https://[::ffff:7f00:1]/x", false, false),
            ("https://[fd00::1]/x", false, false),
            // Forms the URL parser normalises to 127.0.0.1.
            ("https://2130706433/x", false, false),
            ("https://0x7f.1/x", false, false),
            ("https://127.1/x", false, false),
            ("https://127.0.0.1./x", false, false),
        ];
        for (text, allow_private, ok) in cases {
            let Ok(url) = Url::parse(text) else {
                panic!("{text} is not a URL");
            };
            assert_eq!(check_url(&url, *allow_private).is_ok(), *ok, "{text}");
        }
    }

    #[test]
    fn literal_refusal_names_the_host() {
        let Ok(url) = Url::parse("https://10.1.2.3:8443/inbox") else {
            panic!("bad test URL");
        };
        let err = check_url(&url, false).err().map(|e| e.to_string());
        assert_eq!(
            err.as_deref(),
            Some("`10.1.2.3` is not a public address, so it is not contacted")
        );
    }

    #[test]
    fn keeps_only_public_resolved_addresses() {
        let addr = |text: &str| -> SocketAddr {
            let Ok(addr) = text.parse() else {
                panic!("{text} is not a socket address");
            };
            addr
        };
        let mixed = [addr("127.0.0.1:0"), addr("8.8.8.8:0"), addr("[::1]:0")];
        assert_eq!(
            public_addresses("mixed.example", mixed.into_iter()),
            Ok(vec![addr("8.8.8.8:0")])
        );
        let private = [addr("10.0.0.1:0"), addr("[fd00::1]:0")];
        assert_eq!(
            public_addresses("rebind.example", private.into_iter()),
            Err(AddressError::ResolvesToNonPublic {
                host: "rebind.example".to_owned()
            })
        );
        assert!(public_addresses("none.example", std::iter::empty()).is_err());
    }

    #[tokio::test]
    async fn resolver_refuses_localhost() {
        let Ok(name) = "localhost".parse::<Name>() else {
            panic!("bad name");
        };
        let result = PublicResolver.resolve(name).await;
        let Err(err) = result else {
            panic!("localhost resolved to a public address");
        };
        assert_eq!(
            address_error(err.as_ref()),
            Some(&AddressError::ResolvesToNonPublic {
                host: "localhost".to_owned()
            })
        );
    }
}
