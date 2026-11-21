//! Configuration: a TOML file (optional), then `MWMBL_SOCIAL_<FIELD>` environment overrides.

use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;
use social_index::KeyScheme;
use tokenize::KeyConfig;

use crate::decide::REFUSED_ATTEMPTS;

/// The prefix of every environment override.
pub const ENV_PREFIX: &str = "MWMBL_SOCIAL_";
/// Names the config file path when `--config` is not given.
pub const ENV_CONFIG_PATH: &str = "MWMBL_SOCIAL_CONFIG";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config file {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
    #[error("environment variable {var} has an invalid value {value:?}: {reason}")]
    Env {
        var: String,
        value: String,
        reason: String,
    },
    #[error("invalid config: {field} {reason}")]
    Invalid { field: &'static str, reason: String },
}

/// The whole configuration. Every field has a default except `base_url`, which only `serve`
/// and in-process `ingest` need.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The public `http(s)` URL of this instance; the actor and the user agent derive from it.
    pub base_url: Option<String>,
    pub listen: String,
    pub data_dir: PathBuf,
    pub actor_username: String,
    pub name: String,
    pub contact_email: Option<String>,
    pub privacy_policy_url: Option<String>,
    /// The language tag (BCP 47) of the privacy policy at `privacy_policy_url`.
    pub privacy_policy_language: String,
    /// The fediverse account (`@user@host`) of the operator, advertised in `provider_info`.
    pub fediverse_account: Option<String>,
    /// Where a server admin signs in to manage their account with us, advertised as
    /// `signInUrl` in `provider_info`.
    pub sign_in_url: Option<String>,
    /// Where `GET /` redirects, e.g. the operator's own page about this instance. Unset: `/` is
    /// a 404.
    pub root_redirect: Option<String>,
    pub posts_pages: usize,
    pub accounts_pages: usize,
    pub page_size: usize,
    pub key_first_tokens: usize,
    pub key_bigrams: usize,
    pub key_special: usize,
    pub queue_cap: usize,
    pub fetch_concurrency: usize,
    /// One full pass over each index takes this long.
    pub sweep_period_secs: u64,
    /// A stored record not checked for this long is revalidated by the sweep.
    pub revalidate_after_secs: u64,
    /// Revalidation fetches to one host start at most once per this many milliseconds; the
    /// rest are rescheduled. 0 turns the budget off. Announcements and backfill are not
    /// budgeted.
    pub revalidate_interval_per_host_ms: u64,
    /// A fetch that fails transiently this many times is given up on. Must be at least
    /// `REFUSED_ATTEMPTS`, so repeated 401/403 answers count as refused and remove the item.
    pub max_transient_attempts: i64,
    /// How many objects each FASP backfill request asks a server for, per category.
    pub backfill_max_count: u32,
    /// The threshold of the `trends` subscription made with `data_sharing`: a server announces
    /// a post as trending once it has this many shares, likes or replies within this many
    /// minutes. The defaults are the spec's.
    pub trends_timeframe_minutes: u32,
    pub trends_shares: u32,
    pub trends_likes: u32,
    pub trends_replies: u32,
    /// The most post URIs the in-memory trend state holds (PLAN.md Stage 4).
    pub trends_max_uris: usize,
    /// Serve Prometheus metrics at `/metrics` on the main listener. They are public there, so
    /// firewall the path or proxy it to an internal-only listener.
    pub metrics: bool,
    /// Actors to follow (`!community@host`, `@user@host`, `user@host` or an actor URI). `serve`
    /// follows any that aren't followed yet, at start-up and every 6 hours. Removing one does
    /// not unfollow it; `unfollow` does. The environment override is a comma-separated list.
    pub follow: Vec<String>,
    /// How many outbox pages are read when a follow is accepted. 0 turns outbox backfill off.
    pub outbox_backfill_pages: usize,
    /// Bearer tokens accepted by `POST /api/v1/crawl/submissions`. None: the endpoint answers
    /// 404. The environment override is a comma-separated list.
    pub crawl_tokens: Tokens,
    /// Bearer tokens accepted by the admin API (`/api/v1/admin/...`), as for `crawl_tokens`.
    /// None: the admin API answers 404.
    pub admin_tokens: Tokens,
    /// Allow `http://` URIs. For tests only.
    pub allow_http: bool,
    /// Allow outbound requests to loopback, private, link-local and other non-public addresses
    /// (see `apub::net`). Off, they are refused, so nobody can make us reach services behind
    /// our firewall. For tests and the Docker dev setups only.
    pub allow_private_addresses: bool,
}

/// The shortest bearer token accepted (`crawl_tokens`, `admin_tokens`).
pub const MIN_TOKEN_LEN: usize = 16;

/// Bearer tokens, which are secrets: `Debug` shows how many there are, never what they are.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Tokens(pub Vec<String>);

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Tokens({} redacted)", self.0.len())
    }
}

impl Tokens {
    /// Each token must be at least [`MIN_TOKEN_LEN`] characters, without whitespace.
    fn validate(&self, field: &'static str) -> Result<(), ConfigError> {
        if self
            .0
            .iter()
            .any(|t| t.chars().count() < MIN_TOKEN_LEN || t.chars().any(char::is_whitespace))
        {
            return Err(ConfigError::Invalid {
                field,
                reason: format!(
                    "each must be at least {MIN_TOKEN_LEN} characters, without whitespace"
                ),
            });
        }
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_url: None,
            listen: "127.0.0.1:8080".to_string(),
            data_dir: PathBuf::from("./data"),
            actor_username: "mwmbl-social".to_string(),
            name: "Mwmbl Social".to_string(),
            contact_email: None,
            privacy_policy_url: None,
            privacy_policy_language: "en".to_string(),
            fediverse_account: None,
            sign_in_url: None,
            root_redirect: None,
            posts_pages: 65_536,
            accounts_pages: 16_384,
            page_size: 4096,
            key_first_tokens: 10,
            key_bigrams: 10,
            key_special: 10,
            queue_cap: 100_000,
            fetch_concurrency: 8,
            sweep_period_secs: 6 * 86_400,
            revalidate_after_secs: 6 * 86_400,
            revalidate_interval_per_host_ms: 1000,
            max_transient_attempts: 10,
            backfill_max_count: 100,
            trends_timeframe_minutes: 15,
            trends_shares: 3,
            trends_likes: 3,
            trends_replies: 3,
            trends_max_uris: 5000,
            metrics: false,
            follow: Vec::new(),
            outbox_backfill_pages: 5,
            crawl_tokens: Tokens::default(),
            admin_tokens: Tokens::default(),
            allow_http: false,
            allow_private_addresses: false,
        }
    }
}

fn parse_env<T>(var: &str, value: &str) -> Result<T, ConfigError>
where
    T: FromStr,
    T::Err: Display,
{
    value.trim().parse().map_err(|e: T::Err| ConfigError::Env {
        var: var.to_string(),
        value: value.to_string(),
        reason: e.to_string(),
    })
}

/// `value` must be an `http(s)` URL with a host.
fn check_http_url(field: &'static str, value: &str) -> Result<(), ConfigError> {
    let url = url::Url::parse(value).map_err(|e| ConfigError::Invalid {
        field,
        reason: format!("{value:?} is not a URL: {e}"),
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ConfigError::Invalid {
            field,
            reason: format!("{value:?} must be an http(s) URL with a host"),
        });
    }
    Ok(())
}

impl Config {
    /// Load the configuration: `path` (a missing file means all defaults) then overrides from
    /// the process environment, then validate.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        Self::load_with(path, |var| std::env::var(var).ok())
    }

    /// [`Config::load`] with the environment given as a lookup, so tests need not touch the
    /// process environment.
    pub fn load_with(
        path: Option<&Path>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let text = match path {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(text) => Some((path.display().to_string(), text)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(source) => {
                    return Err(ConfigError::Read {
                        path: path.to_path_buf(),
                        source,
                    });
                }
            },
            None => None,
        };
        let config = Self::from_sources(text.as_ref().map(|(p, t)| (p.as_str(), t.as_str())), env)?;
        config.validate()?;
        Ok(config)
    }

    /// Parse `file` (its name, for errors, and its text) and apply overrides from `env`.
    /// Does not validate.
    pub fn from_sources(
        file: Option<(&str, &str)>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let mut config = match file {
            Some((name, text)) => toml::from_str(text).map_err(|source| ConfigError::Parse {
                path: name.to_string(),
                source,
            })?,
            None => Self::default(),
        };
        config.apply_env(&env)?;
        Ok(config)
    }

    fn apply_env(&mut self, env: &impl Fn(&str) -> Option<String>) -> Result<(), ConfigError> {
        // Each scalar field, by its (lowercase) name. Strings are taken verbatim; the rest are
        // parsed with `FromStr`. An empty value for an optional field clears it.
        macro_rules! set {
            ($($field:ident),+ $(,)?) => {$(
                let var = format!("{ENV_PREFIX}{}", stringify!($field).to_ascii_uppercase());
                if let Some(value) = env(&var) {
                    self.$field = parse_env(&var, &value)?;
                }
            )+};
        }
        macro_rules! set_optional {
            ($($field:ident),+ $(,)?) => {$(
                let var = format!("{ENV_PREFIX}{}", stringify!($field).to_ascii_uppercase());
                if let Some(value) = env(&var) {
                    self.$field = (!value.is_empty()).then_some(value);
                }
            )+};
        }
        set_optional!(
            base_url,
            contact_email,
            privacy_policy_url,
            fediverse_account,
            sign_in_url,
            root_redirect,
        );
        set!(
            listen,
            data_dir,
            actor_username,
            name,
            privacy_policy_language,
            posts_pages,
            accounts_pages,
            page_size,
            key_first_tokens,
            key_bigrams,
            key_special,
            queue_cap,
            fetch_concurrency,
            sweep_period_secs,
            revalidate_after_secs,
            revalidate_interval_per_host_ms,
            max_transient_attempts,
            backfill_max_count,
            trends_timeframe_minutes,
            trends_shares,
            trends_likes,
            trends_replies,
            trends_max_uris,
            outbox_backfill_pages,
            metrics,
            allow_http,
            allow_private_addresses,
        );
        let list = |name: &str| {
            env(&format!("{ENV_PREFIX}{name}")).map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
        };
        if let Some(follow) = list("FOLLOW") {
            self.follow = follow;
        }
        if let Some(tokens) = list("CRAWL_TOKENS") {
            self.crawl_tokens = Tokens(tokens);
        }
        if let Some(tokens) = list("ADMIN_TOKENS") {
            self.admin_tokens = Tokens(tokens);
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |field: &'static str, reason: &str| ConfigError::Invalid {
            field,
            reason: reason.to_string(),
        };
        if self.posts_pages == 0 {
            return Err(invalid("posts_pages", "must be greater than 0"));
        }
        if self.accounts_pages == 0 {
            return Err(invalid("accounts_pages", "must be greater than 0"));
        }
        if self.page_size < 1024 {
            return Err(invalid("page_size", "must be at least 1024"));
        }
        if self.queue_cap == 0 {
            return Err(invalid("queue_cap", "must be greater than 0"));
        }
        if self.fetch_concurrency == 0 {
            return Err(invalid("fetch_concurrency", "must be greater than 0"));
        }
        if self.sweep_period_secs == 0 {
            return Err(invalid("sweep_period_secs", "must be greater than 0"));
        }
        // Below this a 401 or 403 is given up on as transient before it can count as refused,
        // so a post whose host stops serving it would never be removed.
        if self.max_transient_attempts < REFUSED_ATTEMPTS {
            return Err(invalid(
                "max_transient_attempts",
                &format!(
                    "is {}, but must be at least REFUSED_ATTEMPTS ({REFUSED_ATTEMPTS})",
                    self.max_transient_attempts
                ),
            ));
        }
        if self.backfill_max_count == 0 {
            return Err(invalid("backfill_max_count", "must be greater than 0"));
        }
        for (field, value) in [
            ("trends_timeframe_minutes", self.trends_timeframe_minutes),
            ("trends_shares", self.trends_shares),
            ("trends_likes", self.trends_likes),
            ("trends_replies", self.trends_replies),
        ] {
            if value == 0 {
                return Err(invalid(field, "must be greater than 0"));
            }
        }
        if self.trends_max_uris == 0 {
            return Err(invalid("trends_max_uris", "must be greater than 0"));
        }
        for target in &self.follow {
            crate::follows::parse_follow_target(target).map_err(|e| ConfigError::Invalid {
                field: "follow",
                reason: e.to_string(),
            })?;
        }
        self.crawl_tokens.validate("crawl_tokens")?;
        self.admin_tokens.validate("admin_tokens")?;
        if self.privacy_policy_language.trim().is_empty() {
            return Err(invalid("privacy_policy_language", "must not be empty"));
        }
        for (field, value) in [
            ("base_url", &self.base_url),
            ("sign_in_url", &self.sign_in_url),
            ("root_redirect", &self.root_redirect),
        ] {
            if let Some(value) = value {
                check_http_url(field, value)?;
            }
        }
        Ok(())
    }

    /// The base URL, for the commands that cannot run without it.
    pub fn require_base_url(&self) -> Result<&str, ConfigError> {
        self.base_url
            .as_deref()
            .ok_or_else(|| ConfigError::Invalid {
                field: "base_url",
                reason: format!("is required (set it in the config file or {ENV_PREFIX}BASE_URL)"),
            })
    }

    #[must_use]
    pub fn key_scheme(&self) -> KeyScheme {
        KeyScheme {
            config: KeyConfig {
                first_tokens: self.key_first_tokens,
                bigrams: self.key_bigrams,
                special: self.key_special,
            },
        }
    }

    /// The `threshold` sent with the `trends` subscription.
    #[must_use]
    pub fn trends_threshold(&self) -> fasp::data_sharing::TrendsThreshold {
        fasp::data_sharing::TrendsThreshold {
            timeframe_minutes: self.trends_timeframe_minutes,
            shares: self.trends_shares,
            likes: self.trends_likes,
            replies: self.trends_replies,
        }
    }

    #[must_use]
    pub fn store_path(&self) -> PathBuf {
        self.data_dir.join("mwmbl-social.sqlite")
    }

    #[must_use]
    pub fn posts_path(&self) -> PathBuf {
        self.data_dir.join("posts.tinyindex")
    }

    #[must_use]
    pub fn accounts_path(&self) -> PathBuf {
        self.data_dir.join("accounts.tinyindex")
    }

    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.data_dir.join("writer.lock")
    }

    #[must_use]
    pub fn keys_dir(&self) -> PathBuf {
        self.data_dir.join("keys")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |var| map.get(var).cloned()
    }

    #[test]
    fn defaults() -> TestResult {
        let config = Config::from_sources(None, env_of(&[]))?;
        assert_eq!(config, Config::default());
        assert_eq!(config.listen, "127.0.0.1:8080");
        assert_eq!(config.posts_pages, 65_536);
        assert_eq!(config.accounts_pages, 16_384);
        assert_eq!(config.page_size, 4096);
        assert_eq!(config.sweep_period_secs, 518_400);
        assert_eq!(config.revalidate_after_secs, 518_400);
        assert_eq!(config.max_transient_attempts, 10);
        assert_eq!(config.revalidate_interval_per_host_ms, 1000);
        assert!(!config.allow_http);
        assert!(!config.allow_private_addresses);
        assert_eq!(config.privacy_policy_language, "en");
        assert_eq!(config.fediverse_account, None);
        assert_eq!(config.sign_in_url, None);
        assert_eq!(config.root_redirect, None);
        assert_eq!(config.backfill_max_count, 100);
        assert_eq!(
            config.trends_threshold(),
            fasp::data_sharing::TrendsThreshold::default()
        );
        assert_eq!(config.trends_max_uris, 5000);
        assert!(!config.metrics);
        assert_eq!(config.follow, Vec::<String>::new());
        assert_eq!(config.outbox_backfill_pages, 5);
        assert_eq!(config.crawl_tokens, Tokens::default());
        assert_eq!(config.admin_tokens, Tokens::default());
        assert_eq!(config.base_url, None);
        config.validate()?;
        assert!(config.require_base_url().is_err());
        assert_eq!(config.key_scheme(), KeyScheme::default());
        Ok(())
    }

    #[test]
    fn parses_toml() -> TestResult {
        let text = r#"
            base_url = "https://social.example"
            listen = "0.0.0.0:9000"
            data_dir = "/var/lib/mwmbl"
            posts_pages = 128
            contact_email = "admin@social.example"
            allow_http = true
            allow_private_addresses = true
            key_bigrams = 3
            privacy_policy_url = "https://social.example/privacy"
            privacy_policy_language = "de"
            fediverse_account = "@admin@social.example"
            sign_in_url = "https://social.example/sign-in"
            root_redirect = "https://mwmbl.example/social"
            backfill_max_count = 25
            trends_timeframe_minutes = 60
            trends_shares = 5
            trends_max_uris = 10
            metrics = true
            follow = ["!rust@lemmy.example", "@alice@m.example"]
            outbox_backfill_pages = 2
            crawl_tokens = ["0123456789abcdef"]
            admin_tokens = ["admin-0123456789"]
        "#;
        let config = Config::from_sources(Some(("c.toml", text)), env_of(&[]))?;
        assert_eq!(config.base_url.as_deref(), Some("https://social.example"));
        assert_eq!(config.listen, "0.0.0.0:9000");
        assert_eq!(config.data_dir, PathBuf::from("/var/lib/mwmbl"));
        assert_eq!(config.posts_pages, 128);
        assert_eq!(config.accounts_pages, 16_384);
        assert_eq!(
            config.contact_email.as_deref(),
            Some("admin@social.example")
        );
        assert!(config.allow_http);
        assert!(config.allow_private_addresses);
        assert_eq!(config.key_scheme().config.bigrams, 3);
        assert_eq!(config.privacy_policy_language, "de");
        assert_eq!(
            config.fediverse_account.as_deref(),
            Some("@admin@social.example")
        );
        assert_eq!(
            config.sign_in_url.as_deref(),
            Some("https://social.example/sign-in")
        );
        assert_eq!(
            config.root_redirect.as_deref(),
            Some("https://mwmbl.example/social")
        );
        assert_eq!(config.backfill_max_count, 25);
        assert_eq!(config.trends_threshold().timeframe_minutes, 60);
        assert_eq!(config.trends_threshold().shares, 5);
        assert_eq!(config.trends_threshold().likes, 3);
        assert_eq!(config.trends_max_uris, 10);
        assert!(config.metrics);
        assert_eq!(config.follow, ["!rust@lemmy.example", "@alice@m.example"]);
        assert_eq!(config.outbox_backfill_pages, 2);
        assert_eq!(config.crawl_tokens.0, ["0123456789abcdef"]);
        assert_eq!(format!("{:?}", config.crawl_tokens), "Tokens(1 redacted)");
        assert_eq!(config.admin_tokens.0, ["admin-0123456789"]);
        assert!(!format!("{config:?}").contains("0123456789abcdef"));
        assert!(!format!("{config:?}").contains("admin-0123456789"));
        config.validate()?;
        assert_eq!(
            config.store_path(),
            PathBuf::from("/var/lib/mwmbl/mwmbl-social.sqlite")
        );
        Ok(())
    }

    #[test]
    fn unknown_fields_and_bad_toml_name_the_file() {
        let err = Config::from_sources(Some(("c.toml", "bogus = 1")), env_of(&[]));
        assert!(matches!(&err, Err(ConfigError::Parse { path, .. }) if path == "c.toml"));
        let err = Config::from_sources(Some(("c.toml", "posts_pages = \"many\"")), env_of(&[]));
        assert!(err.is_err_and(|e| e.to_string().contains("c.toml")));
    }

    #[test]
    fn env_overrides_file_and_defaults() -> TestResult {
        let text = "posts_pages = 128\nlisten = \"1.2.3.4:1\"";
        let config = Config::from_sources(
            Some(("c.toml", text)),
            env_of(&[
                ("MWMBL_SOCIAL_POSTS_PAGES", "256"),
                ("MWMBL_SOCIAL_BASE_URL", "https://x.example"),
                ("MWMBL_SOCIAL_ALLOW_HTTP", "true"),
                ("MWMBL_SOCIAL_ALLOW_PRIVATE_ADDRESSES", "true"),
                ("MWMBL_SOCIAL_DATA_DIR", "/tmp/d"),
                ("MWMBL_SOCIAL_SWEEP_PERIOD_SECS", "60"),
                ("MWMBL_SOCIAL_CONTACT_EMAIL", "a@b.example"),
                ("MWMBL_SOCIAL_NAME", "Mine"),
                ("MWMBL_SOCIAL_KEY_SPECIAL", "2"),
                ("MWMBL_SOCIAL_PRIVACY_POLICY_LANGUAGE", "fr"),
                ("MWMBL_SOCIAL_FEDIVERSE_ACCOUNT", "@me@x.example"),
                ("MWMBL_SOCIAL_SIGN_IN_URL", "https://x.example/in"),
                ("MWMBL_SOCIAL_ROOT_REDIRECT", "https://x.example/about"),
                ("MWMBL_SOCIAL_BACKFILL_MAX_COUNT", "7"),
                ("MWMBL_SOCIAL_METRICS", "true"),
                ("MWMBL_SOCIAL_REVALIDATE_INTERVAL_PER_HOST_MS", "250"),
                (
                    "MWMBL_SOCIAL_FOLLOW",
                    " !a@l.example, ,https://m.example/users/b ",
                ),
                ("MWMBL_SOCIAL_OUTBOX_BACKFILL_PAGES", "0"),
                (
                    "MWMBL_SOCIAL_CRAWL_TOKENS",
                    "aaaaaaaaaaaaaaaa, bbbbbbbbbbbbbbbb,",
                ),
                ("MWMBL_SOCIAL_ADMIN_TOKENS", " cccccccccccccccc "),
            ]),
        )?;
        assert_eq!(
            config.crawl_tokens.0,
            ["aaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbb"]
        );
        assert_eq!(config.admin_tokens.0, ["cccccccccccccccc"]);
        assert_eq!(config.follow, ["!a@l.example", "https://m.example/users/b"]);
        assert_eq!(config.outbox_backfill_pages, 0);
        assert_eq!(config.revalidate_interval_per_host_ms, 250);
        assert_eq!(config.privacy_policy_language, "fr");
        assert_eq!(config.fediverse_account.as_deref(), Some("@me@x.example"));
        assert_eq!(config.sign_in_url.as_deref(), Some("https://x.example/in"));
        assert_eq!(
            config.root_redirect.as_deref(),
            Some("https://x.example/about")
        );
        assert_eq!(config.backfill_max_count, 7);
        assert!(config.metrics);
        assert_eq!(config.posts_pages, 256);
        assert_eq!(config.listen, "1.2.3.4:1");
        assert_eq!(config.base_url.as_deref(), Some("https://x.example"));
        assert!(config.allow_http);
        assert!(config.allow_private_addresses);
        assert_eq!(config.data_dir, PathBuf::from("/tmp/d"));
        assert_eq!(config.sweep_period_secs, 60);
        assert_eq!(config.contact_email.as_deref(), Some("a@b.example"));
        assert_eq!(config.name, "Mine");
        assert_eq!(config.key_special, 2);
        // An empty optional clears it.
        let config = Config::from_sources(
            Some(("c.toml", "contact_email = \"x@y.example\"")),
            env_of(&[("MWMBL_SOCIAL_CONTACT_EMAIL", "")]),
        )?;
        assert_eq!(config.contact_email, None);
        Ok(())
    }

    #[test]
    fn a_bad_env_value_names_the_variable() {
        let err = Config::from_sources(None, env_of(&[("MWMBL_SOCIAL_PAGE_SIZE", "big")]));
        assert!(
            matches!(&err, Err(ConfigError::Env { var, value, .. }) if var == "MWMBL_SOCIAL_PAGE_SIZE" && value == "big")
        );
        let err = Config::from_sources(None, env_of(&[("MWMBL_SOCIAL_ALLOW_HTTP", "maybe")]));
        assert!(err.is_err());
    }

    #[test]
    fn validation() {
        let bad = |f: &dyn Fn(&mut Config)| {
            let mut c = Config::default();
            f(&mut c);
            c.validate().err().map(|e| e.to_string())
        };
        assert!(bad(&|c| c.posts_pages = 0).is_some_and(|e| e.contains("posts_pages")));
        assert!(bad(&|c| c.accounts_pages = 0).is_some_and(|e| e.contains("accounts_pages")));
        assert!(bad(&|c| c.page_size = 1023).is_some_and(|e| e.contains("page_size")));
        assert!(bad(&|c| c.page_size = 1024).is_none());
        assert!(bad(&|c| c.fetch_concurrency = 0).is_some());
        assert!(bad(&|c| c.queue_cap = 0).is_some());
        assert!(bad(&|c| c.sweep_period_secs = 0).is_some());
        assert!(bad(&|c| c.max_transient_attempts = 0).is_some());
        // D8: below REFUSED_ATTEMPTS a 401/403 never removes anything. The error names both.
        assert!(
            bad(&|c| c.max_transient_attempts = REFUSED_ATTEMPTS - 1)
                .is_some_and(|e| e.contains("max_transient_attempts")
                    && e.contains(&format!("is {}", REFUSED_ATTEMPTS - 1))
                    && e.contains(&format!("REFUSED_ATTEMPTS ({REFUSED_ATTEMPTS})")))
        );
        assert!(bad(&|c| c.max_transient_attempts = REFUSED_ATTEMPTS).is_none());
        assert!(
            bad(&|c| c.backfill_max_count = 0).is_some_and(|e| e.contains("backfill_max_count"))
        );
        assert!(bad(&|c| c.privacy_policy_language = " ".into()).is_some());
        let tokens = |t: &[&str]| Tokens(t.iter().map(|t| (*t).to_owned()).collect());
        assert!(
            bad(&|c| c.crawl_tokens = tokens(&["short"]))
                .is_some_and(|e| e.contains("crawl_tokens"))
        );
        assert!(bad(&|c| c.crawl_tokens = tokens(&["0123456789 abcdef"])).is_some());
        assert!(bad(&|c| c.crawl_tokens = tokens(&["0123456789abcdef"])).is_none());
        assert!(
            bad(&|c| c.admin_tokens = tokens(&["0123456789abcdef", "short"]))
                .is_some_and(|e| e.contains("admin_tokens"))
        );
        assert!(bad(&|c| c.admin_tokens = tokens(&["0123456789abcdef"])).is_none());
        assert!(bad(&|c| c.follow = vec!["rust".into()]).is_some_and(|e| e.contains("follow")));
        assert!(bad(&|c| c.follow = vec!["!rust@l.example".into()]).is_none());
        assert!(
            bad(&|c| c.base_url = Some("not a url".into())).is_some_and(|e| e.contains("base_url"))
        );
        assert!(bad(&|c| c.base_url = Some("ftp://x.example".into())).is_some());
        assert!(bad(&|c| c.base_url = Some("mailto:a@b.example".into())).is_some());
        assert!(bad(&|c| c.base_url = Some("http://localhost:8080".into())).is_none());
        assert!(bad(&|c| c.base_url = Some("https://social.example/".into())).is_none());
        assert!(
            bad(&|c| c.sign_in_url = Some("javascript:alert(1)".into()))
                .is_some_and(|e| e.contains("sign_in_url"))
        );
        assert!(bad(&|c| c.sign_in_url = Some("https://mwmbl.example/in".into())).is_none());
        assert!(
            bad(&|c| c.root_redirect = Some("/relative".into()))
                .is_some_and(|e| e.contains("root_redirect"))
        );
        assert!(bad(&|c| c.root_redirect = Some("https://mwmbl.example/".into())).is_none());
    }

    #[test]
    fn a_missing_file_is_all_defaults_and_a_present_one_is_read() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("c.toml");
        assert_eq!(
            Config::load_with(Some(&path), env_of(&[]))?,
            Config::default()
        );
        assert_eq!(Config::load_with(None, env_of(&[]))?, Config::default());
        std::fs::write(&path, "posts_pages = 7")?;
        assert_eq!(Config::load_with(Some(&path), env_of(&[]))?.posts_pages, 7);
        std::fs::write(&path, "posts_pages = 0")?;
        assert!(Config::load_with(Some(&path), env_of(&[])).is_err());
        Ok(())
    }
}
