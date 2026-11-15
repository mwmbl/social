//! Just enough URI handling to derive signature components from an absolute `http(s)` URI.

use super::SignatureError;

pub(crate) struct UriParts {
    /// Lowercase host, plus the port unless it is the scheme's default.
    pub authority: String,
    /// The path, `/` when empty.
    pub path: String,
    /// The query without its leading `?`, `None` when the URI has no `?`.
    pub query: Option<String>,
}

pub(crate) fn parse(uri: &str) -> Result<UriParts, SignatureError> {
    let bad = |reason: &str| SignatureError::InvalidTargetUri {
        uri: uri.to_owned(),
        reason: reason.to_owned(),
    };
    if uri
        .chars()
        .any(|c| c.is_ascii_control() || c == ' ' || !c.is_ascii())
    {
        return Err(bad("contains whitespace, control or non-ASCII characters"));
    }
    let (scheme, rest) = uri.split_once("://").ok_or_else(|| bad("no scheme"))?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(bad("scheme is not http or https"));
    }
    let rest = rest.split_once('#').map_or(rest, |(before, _)| before);
    let (authority, path_and_query) = match rest.find(['/', '?']) {
        Some(i) => (
            rest.get(..i).unwrap_or_default(),
            rest.get(i..).unwrap_or_default(),
        ),
        None => (rest, ""),
    };
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if host_port.is_empty() {
        return Err(bad("empty host"));
    }
    let mut authority = host_port.to_ascii_lowercase();
    let default_port = if scheme == "http" { ":80" } else { ":443" };
    if authority.ends_with(default_port) {
        authority.truncate(authority.len() - default_port.len());
    }
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q.to_owned())),
        None => (path_and_query, None),
    };
    let path = if path.is_empty() { "/" } else { path };
    Ok(UriParts {
        authority,
        path: path.to_owned(),
        query,
    })
}

/// The `Host` header value for `target_uri`: lowercase host, with the port unless it is the
/// scheme's default.
pub fn authority(target_uri: &str) -> Result<String, SignatureError> {
    parse(target_uri).map(|p| p.authority)
}
