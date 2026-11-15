//! Configuration: a TOML file (optional), then `MWMBL_SOCIAL_<FIELD>` environment overrides.

use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;
use social_index::KeyScheme;
use tokenize::KeyConfig;

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
    /// A fetch that fails transiently this many times is given up on.
    pub max_transient_attempts: i64,
    /// Allow `http://` URIs. For tests only.
    pub allow_http: bool,
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
            max_transient_attempts: 10,
            allow_http: false,
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
        set_optional!(base_url, contact_email, privacy_policy_url);
        set!(
            listen,
            data_dir,
            actor_username,
            name,
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
            max_transient_attempts,
            allow_http,
        );
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
        if self.max_transient_attempts < 1 {
            return Err(invalid("max_transient_attempts", "must be at least 1"));
        }
        if let Some(base_url) = &self.base_url {
            let url = url::Url::parse(base_url).map_err(|e| ConfigError::Invalid {
                field: "base_url",
                reason: format!("{base_url:?} is not a URL: {e}"),
            })?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                return Err(ConfigError::Invalid {
                    field: "base_url",
                    reason: format!("{base_url:?} must be an http(s) URL with a host"),
                });
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
        assert!(!config.allow_http);
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
            key_bigrams = 3
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
        assert_eq!(config.key_scheme().config.bigrams, 3);
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
                ("MWMBL_SOCIAL_DATA_DIR", "/tmp/d"),
                ("MWMBL_SOCIAL_SWEEP_PERIOD_SECS", "60"),
                ("MWMBL_SOCIAL_CONTACT_EMAIL", "a@b.example"),
                ("MWMBL_SOCIAL_NAME", "Mine"),
                ("MWMBL_SOCIAL_KEY_SPECIAL", "2"),
            ]),
        )?;
        assert_eq!(config.posts_pages, 256);
        assert_eq!(config.listen, "1.2.3.4:1");
        assert_eq!(config.base_url.as_deref(), Some("https://x.example"));
        assert!(config.allow_http);
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
        assert!(
            bad(&|c| c.base_url = Some("not a url".into())).is_some_and(|e| e.contains("base_url"))
        );
        assert!(bad(&|c| c.base_url = Some("ftp://x.example".into())).is_some());
        assert!(bad(&|c| c.base_url = Some("mailto:a@b.example".into())).is_some());
        assert!(bad(&|c| c.base_url = Some("http://localhost:8080".into())).is_none());
        assert!(bad(&|c| c.base_url = Some("https://social.example/".into())).is_none());
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
