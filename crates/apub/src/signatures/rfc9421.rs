//! RFC 9421 HTTP Message Signatures, narrowed to what Mwmbl Social needs.
//!
//! Covered components are `@method`, `@target-uri`, `@authority`, `@path`, `@query` and `@status`
//! (all without parameters) and single-instance, lowercase header fields. Several instances of a
//! header are combined with `", "`. Signature parameters are `created`, `expires`, `keyid`,
//! `alg`, `nonce` and `tag`, written in that order.
//!
//! For FASP, requests cover `@method`, `@target-uri` and `content-digest`, responses cover
//! `@status` and `content-digest`, and the parameters are `created` and `keyid`.

use std::fmt::Write as _;

use super::keys::{SigningKey, VerifyingKey};
use super::sf::{self, BareItem, Value};
use super::{Headers, SignatureError, check_skew, required_header, uri};

/// Label used when none is given.
pub const DEFAULT_LABEL: &str = "sig1";

/// What is being signed: a request or a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message<'a> {
    Request {
        /// The HTTP method, as sent (`POST`).
        method: &'a str,
        /// The absolute target URI.
        target_uri: &'a str,
    },
    Response {
        status: u16,
    },
}

/// The parameters of a signature, written in this order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignatureParams {
    pub created: Option<i64>,
    pub expires: Option<i64>,
    pub keyid: Option<String>,
    pub alg: Option<String>,
    pub nonce: Option<String>,
    pub tag: Option<String>,
}

/// A signature that passed verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedSignature {
    pub label: String,
    pub keyid: String,
    pub created: i64,
    pub expires: Option<i64>,
    pub alg: Option<String>,
    pub nonce: Option<String>,
    pub tag: Option<String>,
    /// The covered components, in the signed order.
    pub components: Vec<String>,
}

/// What a verifier insists on.
#[derive(Debug, Clone, Copy)]
pub struct VerifyPolicy<'a> {
    /// The signature label to check.
    pub label: &'a str,
    /// Components that must be covered, e.g. `["@method", "@target-uri", "content-digest"]`.
    pub required_components: &'a [&'a str],
    /// The current time, Unix seconds.
    pub now: i64,
    /// How far `created` may be from `now`, either way.
    pub max_skew_secs: i64,
}

impl<'a> VerifyPolicy<'a> {
    /// A policy for the default label.
    #[must_use]
    pub fn new(required_components: &'a [&'a str], now: i64, max_skew_secs: i64) -> Self {
        Self {
            label: DEFAULT_LABEL,
            required_components,
            now,
            max_skew_secs,
        }
    }
}

fn unsupported(component: &str, reason: &str) -> SignatureError {
    SignatureError::UnsupportedComponent {
        component: component.to_owned(),
        reason: reason.to_owned(),
    }
}

fn check_component_name(component: &str) -> Result<(), SignatureError> {
    if component.starts_with('@') {
        return match component {
            "@method" | "@target-uri" | "@authority" | "@path" | "@query" | "@status"
            | "@signature-params" => Ok(()),
            _ => Err(unsupported(
                component,
                "not one of the supported components",
            )),
        };
    }
    let valid_token = !component.is_empty()
        && component
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if valid_token {
        Ok(())
    } else {
        Err(unsupported(
            component,
            "header names must be lowercase letters, digits and `-`",
        ))
    }
}

fn component_value<H: Headers + ?Sized>(
    message: &Message<'_>,
    headers: &H,
    component: &str,
) -> Result<String, SignatureError> {
    check_component_name(component)?;
    let request_only = || unsupported(component, "only valid for requests");
    match (component, message) {
        ("@signature-params", _) => Err(unsupported(component, "cannot be covered by itself")),
        ("@status", Message::Response { status }) => Ok(status.to_string()),
        ("@status", Message::Request { .. }) => {
            Err(unsupported(component, "only valid for responses"))
        }
        ("@method", Message::Request { method, .. }) => Ok((*method).to_owned()),
        ("@target-uri", Message::Request { target_uri, .. }) => Ok((*target_uri).to_owned()),
        ("@authority", Message::Request { target_uri, .. }) => {
            uri::parse(target_uri).map(|u| u.authority)
        }
        ("@path", Message::Request { target_uri, .. }) => uri::parse(target_uri).map(|u| u.path),
        ("@query", Message::Request { target_uri, .. }) => {
            uri::parse(target_uri).map(|u| format!("?{}", u.query.unwrap_or_default()))
        }
        (c, Message::Response { .. }) if c.starts_with('@') => Err(request_only()),
        (name, _) => required_header(headers, name),
    }
}

fn invalid(what: &'static str, reason: String) -> SignatureError {
    SignatureError::InvalidValue { what, reason }
}

/// The inner list and parameters, serialised: `("@method" "content-digest");created=1;keyid="k"`.
pub fn signature_params_value(
    components: &[&str],
    params: &SignatureParams,
) -> Result<String, SignatureError> {
    let mut out = String::from("(");
    for (i, component) in components.iter().enumerate() {
        check_component_name(component)?;
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&sf::serialize_string(component).map_err(|r| invalid("component", r))?);
    }
    out.push(')');
    if let Some(created) = params.created {
        let _ = write!(out, ";created={created}");
    }
    if let Some(expires) = params.expires {
        let _ = write!(out, ";expires={expires}");
    }
    for (name, value) in [
        ("keyid", &params.keyid),
        ("alg", &params.alg),
        ("nonce", &params.nonce),
        ("tag", &params.tag),
    ] {
        if let Some(value) = value {
            let quoted =
                sf::serialize_string(value).map_err(|r| invalid("signature parameter", r))?;
            let _ = write!(out, ";{name}={quoted}");
        }
    }
    Ok(out)
}

fn base_for<H: Headers + ?Sized>(
    message: &Message<'_>,
    headers: &H,
    components: &[&str],
    params_value: &str,
) -> Result<String, SignatureError> {
    let mut seen: Vec<&str> = Vec::with_capacity(components.len());
    let mut base = String::new();
    for component in components {
        if seen.contains(component) {
            return Err(SignatureError::DuplicateComponent {
                component: (*component).to_owned(),
            });
        }
        seen.push(component);
        let value = component_value(message, headers, component)?;
        let name = sf::serialize_string(component).map_err(|r| invalid("component", r))?;
        let _ = writeln!(base, "{name}: {value}");
    }
    let _ = write!(base, "\"@signature-params\": {params_value}");
    Ok(base)
}

/// The signature base (RFC 9421 2.5) for `components` and `params`.
pub fn signature_base<H: Headers + ?Sized>(
    message: &Message<'_>,
    headers: &H,
    components: &[&str],
    params: &SignatureParams,
) -> Result<String, SignatureError> {
    base_for(
        message,
        headers,
        components,
        &signature_params_value(components, params)?,
    )
}

/// Signs `message` and returns the `Signature-Input` and `Signature` header values, both for
/// signature `label`.
pub fn sign<H: Headers + ?Sized>(
    message: &Message<'_>,
    headers: &H,
    label: &str,
    components: &[&str],
    params: &SignatureParams,
    key: &SigningKey,
) -> Result<(String, String), SignatureError> {
    if !sf::is_valid_key(label) {
        return Err(SignatureError::InvalidLabel {
            label: label.to_owned(),
        });
    }
    let params_value = signature_params_value(components, params)?;
    let base = base_for(message, headers, components, &params_value)?;
    let signature = key.sign(base.as_bytes())?;
    Ok((
        format!("{label}={params_value}"),
        format!("{label}={}", sf::serialize_bytes(&signature)),
    ))
}

/// Signs a request under label `sig1`, with parameters `created` and `keyid`.
///
/// Returns the `Signature-Input` and `Signature` header values.
pub fn sign_request<H: Headers + ?Sized>(
    method: &str,
    target_uri: &str,
    headers: &H,
    components: &[&str],
    keyid: &str,
    created: i64,
    key: &SigningKey,
) -> Result<(String, String), SignatureError> {
    let params = SignatureParams {
        created: Some(created),
        keyid: Some(keyid.to_owned()),
        ..SignatureParams::default()
    };
    sign(
        &Message::Request { method, target_uri },
        headers,
        DEFAULT_LABEL,
        components,
        &params,
        key,
    )
}

/// Signs a response under label `sig1`, with parameters `created` and `keyid`.
pub fn sign_response<H: Headers + ?Sized>(
    status: u16,
    headers: &H,
    components: &[&str],
    keyid: &str,
    created: i64,
    key: &SigningKey,
) -> Result<(String, String), SignatureError> {
    let params = SignatureParams {
        created: Some(created),
        keyid: Some(keyid.to_owned()),
        ..SignatureParams::default()
    };
    sign(
        &Message::Response { status },
        headers,
        DEFAULT_LABEL,
        components,
        &params,
        key,
    )
}

/// One parsed `Signature-Input` member.
struct ParsedInput {
    components: Vec<String>,
    params: SignatureParams,
    /// The member value as received, which is what `@signature-params` reproduces.
    raw: String,
}

fn malformed_input(reason: impl Into<String>) -> SignatureError {
    SignatureError::MalformedSignatureInput {
        reason: reason.into(),
    }
}

/// Parses the `Signature-Input` header value and returns the member for `label`.
fn parse_input(value: &str, label: &str) -> Result<ParsedInput, SignatureError> {
    let members = sf::parse_dictionary(value).map_err(malformed_input)?;
    let member = sf::find(&members, label).ok_or_else(|| SignatureError::LabelNotFound {
        label: label.to_owned(),
    })?;
    let Value::InnerList(list) = &member.value else {
        return Err(malformed_input(format!("`{label}` is not an inner list")));
    };
    let mut components = Vec::with_capacity(list.items.len());
    for item in &list.items {
        let BareItem::String(name) = &item.value else {
            return Err(malformed_input("covered component is not a string"));
        };
        if !item.params.is_empty() {
            return Err(SignatureError::UnsupportedComponent {
                component: name.clone(),
                reason: "component parameters are not supported".to_owned(),
            });
        }
        components.push(name.clone());
    }
    let string_param = |key: &str| -> Result<Option<String>, SignatureError> {
        match sf::param(&list.params, key) {
            None => Ok(None),
            Some(BareItem::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(malformed_input(format!("`{key}` must be a string"))),
        }
    };
    let int_param = |key: &str| -> Result<Option<i64>, SignatureError> {
        match sf::param(&list.params, key) {
            None => Ok(None),
            Some(BareItem::Integer(n)) => Ok(Some(*n)),
            Some(_) => Err(malformed_input(format!("`{key}` must be an integer"))),
        }
    };
    let params = SignatureParams {
        created: int_param("created")?,
        expires: int_param("expires")?,
        keyid: string_param("keyid")?,
        alg: string_param("alg")?,
        nonce: string_param("nonce")?,
        tag: string_param("tag")?,
    };
    Ok(ParsedInput {
        components,
        params,
        raw: member.raw.clone(),
    })
}

/// The labels, covered components and parameters in a `Signature-Input` header value.
pub fn parse_signature_input(
    value: &str,
) -> Result<Vec<(String, Vec<String>, SignatureParams)>, SignatureError> {
    let members = sf::parse_dictionary(value).map_err(malformed_input)?;
    let mut out = Vec::with_capacity(members.len());
    for member in &members {
        let parsed = parse_input(value, &member.key)?;
        out.push((member.key.clone(), parsed.components, parsed.params));
    }
    Ok(out)
}

/// The raw signature bytes for `label` in a `Signature` header value.
pub fn parse_signature(value: &str, label: &str) -> Result<Vec<u8>, SignatureError> {
    let members = sf::parse_dictionary(value)
        .map_err(|reason| SignatureError::MalformedSignature { reason })?;
    let member = sf::find(&members, label).ok_or_else(|| SignatureError::SignatureNotFound {
        label: label.to_owned(),
    })?;
    match &member.value {
        Value::Item(item) => match &item.value {
            BareItem::Bytes(bytes) => Ok(bytes.clone()),
            _ => Err(SignatureError::MalformedSignature {
                reason: format!("`{label}` is not a byte sequence"),
            }),
        },
        Value::InnerList(_) => Err(SignatureError::MalformedSignature {
            reason: format!("`{label}` is a list"),
        }),
    }
}

/// Verifies the signature `policy.label` on `message`.
///
/// Checks, in order: the header syntax; that every required component is covered; that `created`
/// is present and within the allowed skew; that `expires`, if present, has not passed; that the
/// `keyid` resolves to a key; that any `alg` matches the key; and the signature itself.
///
/// Body integrity is not checked here: when `content-digest` is covered, also call
/// [`super::verify_content_digest`] on the received body.
pub fn verify<H: Headers + ?Sized>(
    message: &Message<'_>,
    headers: &H,
    policy: &VerifyPolicy<'_>,
    key_lookup: impl Fn(&str) -> Option<VerifyingKey>,
) -> Result<VerifiedSignature, SignatureError> {
    let input_value = required_header(headers, "signature-input")?;
    let signature_value = required_header(headers, "signature")?;
    let input = parse_input(&input_value, policy.label)?;
    let signature = parse_signature(&signature_value, policy.label)?;

    for required in policy.required_components {
        if !input.components.iter().any(|c| c == required) {
            return Err(SignatureError::ComponentNotCovered {
                component: (*required).to_owned(),
            });
        }
    }
    let created = input.params.created.ok_or(SignatureError::MissingCreated)?;
    check_skew("created", created, policy.now, policy.max_skew_secs)?;
    if let Some(expires) = input.params.expires
        && policy.now > expires
    {
        return Err(SignatureError::Expired {
            seconds: policy.now.saturating_sub(expires),
        });
    }
    let keyid = input
        .params
        .keyid
        .clone()
        .ok_or(SignatureError::MissingKeyId)?;
    let key = key_lookup(&keyid).ok_or_else(|| SignatureError::UnknownKey {
        keyid: keyid.clone(),
    })?;
    if let Some(declared) = &input.params.alg
        && declared != key.algorithm()
    {
        return Err(SignatureError::AlgorithmMismatch {
            keyid,
            declared: declared.clone(),
            expected: key.algorithm(),
        });
    }

    let components: Vec<&str> = input.components.iter().map(String::as_str).collect();
    let base = base_for(message, headers, &components, &input.raw)?;
    if !key.verify(base.as_bytes(), &signature) {
        return Err(SignatureError::BadSignature { keyid });
    }
    Ok(VerifiedSignature {
        label: policy.label.to_owned(),
        keyid,
        created,
        expires: input.params.expires,
        alg: input.params.alg,
        nonce: input.params.nonce,
        tag: input.params.tag,
        components: input.components,
    })
}

/// Verifies a request signature. See [`verify`].
pub fn verify_request<H: Headers + ?Sized>(
    method: &str,
    target_uri: &str,
    headers: &H,
    policy: &VerifyPolicy<'_>,
    key_lookup: impl Fn(&str) -> Option<VerifyingKey>,
) -> Result<VerifiedSignature, SignatureError> {
    verify(
        &Message::Request { method, target_uri },
        headers,
        policy,
        key_lookup,
    )
}

/// Verifies a response signature. See [`verify`].
pub fn verify_response<H: Headers + ?Sized>(
    status: u16,
    headers: &H,
    policy: &VerifyPolicy<'_>,
    key_lookup: impl Fn(&str) -> Option<VerifyingKey>,
) -> Result<VerifiedSignature, SignatureError> {
    verify(&Message::Response { status }, headers, policy, key_lookup)
}
