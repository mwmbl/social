//! The command line: `serve`, `ingest`, `search`, `forget` and `stats`.
//!
//! One process at a time may write the indexes, enforced by the lock on `writer.lock`
//! ([`WriterLock`]). `ingest` and `forget` write in-process when the lock is free and otherwise
//! only record their request in SQLite for the running server to pick up. `search` and `stats`
//! never need the lock.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use apub::actor::server_actor_key_id;
use apub::fetch::{Fetcher, FetcherConfig, user_agent};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use social_index::{SocialAccount, SocialPost, SocialReader, Tombstones};
use store::{EnqueueOutcome, FetchKind, FetchReason, NewFetch, Store, TombstoneKind};
use tinyindex::TinyIndex;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing_subscriber::EnvFilter;
use url::Url;

use crate::config::{Config, ENV_CONFIG_PATH};
use crate::decide::Action;
use crate::http::{AppState, router};
use crate::keys::load_or_generate;
use crate::now_secs;
use crate::search;
use crate::sweep::{SweepConfig, run_index, step_interval};
use crate::worker::{
    Worker, WorkerConfig, adhoc_item, persist_hosts, run_host_persistence, seed_hosts,
};
use crate::writer::{
    ForgetTarget, IndexName, Writer, WriterHandle, WriterLock, load_tombstones, run_reload,
};

/// How often the writer re-reads tombstones another process may have written.
const RELOAD_EVERY: Duration = Duration::from_secs(30);
/// How often the fetcher's host state is saved.
const PERSIST_HOSTS_EVERY: Duration = Duration::from_secs(60);
/// How long the fetch worker waits when the queue has nothing due.
const WORKER_TICK: Duration = Duration::from_secs(1);

#[derive(Debug, Parser)]
#[command(name = "mwmbl-social", version, about = "A fediverse search engine")]
pub struct Cli {
    /// The TOML config file. A missing file means all defaults.
    #[arg(long, global = true, env = ENV_CONFIG_PATH, value_name = "FILE")]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the server, the fetch worker and the sweep.
    Serve,
    /// Fetch one object or actor now and index it if it is consented.
    Ingest {
        uri: String,
        /// What the URI is; without it the fetched document decides.
        #[arg(long, value_enum)]
        kind: Option<KindArg>,
    },
    /// Search the indexes, printing one JSON object per line.
    Search {
        #[command(subcommand)]
        what: SearchCommand,
    },
    /// Remove a URI, an account (`user@host`) or a whole domain.
    Forget {
        /// A URI (contains `://`), an account (`user@host`) or a domain.
        target: String,
    },
    /// Print queue, tombstone, sweep and index statistics as JSON.
    Stats,
}

#[derive(Debug, Subcommand)]
pub enum SearchCommand {
    Posts {
        q: String,
        #[arg(long, default_value_t = search::DEFAULT_LIMIT)]
        limit: usize,
    },
    Accounts {
        q: String,
        #[arg(long, default_value_t = search::DEFAULT_LIMIT)]
        limit: usize,
        #[arg(long)]
        language: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KindArg {
    Object,
    Actor,
}

impl From<KindArg> for FetchKind {
    fn from(kind: KindArg) -> Self {
        match kind {
            KindArg::Object => Self::Object,
            KindArg::Actor => Self::Actor,
        }
    }
}

/// Parse arguments, set up logging and run the command.
pub fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_logging();
    let config = Config::load(cli.config.as_deref())?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;
    runtime.block_on(run(cli.command, config))
}

/// `RUST_LOG`, defaulting to `info`, to stderr (stdout is for command output).
fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // Fails only if a subscriber is already set, which is fine.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

pub async fn run(command: Command, config: Config) -> anyhow::Result<()> {
    match command {
        Command::Serve => serve(config).await,
        Command::Ingest { uri, kind } => ingest(&config, &uri, kind.map(Into::into)).await,
        Command::Search { what } => search_command(&config, what),
        Command::Forget { target } => forget(&config, &target).await,
        Command::Stats => print_json(&stats(&config)?),
    }
}

fn print_json(value: &Value) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn build_fetcher(
    config: &Config,
    base_url: &str,
    key: apub::signatures::keys::RsaSigningKey,
) -> anyhow::Result<Fetcher> {
    let mut fetcher = FetcherConfig::new(server_actor_key_id(base_url), key, user_agent(base_url));
    fetcher.allow_http = config.allow_http;
    Fetcher::new(fetcher).context("failed to build the ActivityPub fetch client")
}

fn worker_config(config: &Config) -> WorkerConfig {
    WorkerConfig {
        fetch_concurrency: config.fetch_concurrency,
        max_transient_attempts: config.max_transient_attempts,
        tick: WORKER_TICK,
    }
}

/// Resolves when `shutdown` has been signalled.
async fn signalled(mut shutdown: watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            break;
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

// ---------------------------------------------------------------------------------------------
// serve

/// The long-running tasks beside the HTTP server: the fetch worker, a sweep per index, host
/// state persistence and the tombstone reload. Each gets its own SQLite connection.
fn spawn_background(
    tasks: &mut JoinSet<()>,
    config: &Config,
    handle: &WriterHandle,
    fetcher: &Arc<Fetcher>,
    shutdown: &watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let worker = Arc::new(Worker::new(
        Arc::clone(fetcher),
        handle.clone(),
        worker_config(config),
    ));
    tasks.spawn(worker.run(open_store(config)?, shutdown.clone()));

    let (posts, accounts) = handle.readers();
    let revalidate_after_secs = i64::try_from(config.revalidate_after_secs).unwrap_or(i64::MAX);
    for (index, pages) in [
        (IndexName::Posts, posts.index().num_pages()),
        (IndexName::Accounts, accounts.index().num_pages()),
    ] {
        let sweep = SweepConfig {
            step_interval: step_interval(config.sweep_period_secs, pages),
            revalidate_after_secs,
            queue_cap: config.queue_cap,
        };
        tasks.spawn(run_index(
            index,
            pages,
            open_store(config)?,
            handle.clone(),
            sweep,
            shutdown.clone(),
        ));
    }
    tasks.spawn(run_host_persistence(
        Arc::clone(fetcher),
        open_store(config)?,
        PERSIST_HOSTS_EVERY,
        shutdown.clone(),
    ));
    tasks.spawn(run_reload(handle.clone(), RELOAD_EVERY, shutdown.clone()));
    Ok(())
}

async fn serve(config: Config) -> anyhow::Result<()> {
    let base_url = config.require_base_url()?.to_string();
    let lock = WriterLock::try_acquire(&config)?.with_context(|| {
        format!(
            "another mwmbl-social process already holds the writer lock {}; \
             only one server may write the indexes",
            config.lock_path().display()
        )
    })?;
    let key = load_or_generate(&config.keys_dir())?;
    let public_key_pem = key
        .verifying_key()
        .to_spki_pem()
        .context("failed to encode the actor's public key")?;
    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("failed to listen on {}", config.listen))?;
    let writer = Writer::start_with_lock(&config, lock)?;
    let handle = writer.handle();
    let fetcher = Arc::new(build_fetcher(&config, &base_url, key)?);
    let seeded = seed_hosts(&fetcher, &open_store(&config)?, now_secs())?;
    tracing::info!(hosts = seeded, "restored host state");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut tasks: JoinSet<()> = JoinSet::new();
    spawn_background(&mut tasks, &config, &handle, &fetcher, &shutdown_rx)?;
    let (posts, accounts) = handle.readers();

    let app = router(AppState {
        base_url: base_url.clone(),
        actor_username: config.actor_username.clone(),
        name: config.name.clone(),
        public_key_pem,
        posts,
        accounts,
        tombstones: handle.tombstones(),
    });
    tracing::info!(listen = %config.listen, base_url, "serving");
    let server = {
        let shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(signalled(shutdown))
                .await
        })
    };

    shutdown_signal().await;
    tracing::info!("shutting down");
    let _ = shutdown_tx.send(true);
    match server.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!(error = %e, "the HTTP server failed"),
        Err(e) => tracing::error!(error = %e, "the HTTP server task failed"),
    }
    while let Some(done) = tasks.join_next().await {
        if let Err(e) = done {
            tracing::error!(error = %e, "a background task failed");
        }
    }
    if let Err(e) = persist_hosts(&fetcher, &open_store(&config)?, now_secs()) {
        tracing::error!(error = %e, "failed to persist host state");
    }
    drop(handle);
    writer.finish();
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// ingest

/// A short, content-free description of what was decided, for `ingest`'s output.
#[must_use]
pub fn action_summary(action: &Action) -> Value {
    match action {
        Action::IndexPost { record, replaces } => json!({
            "action": "index_post", "uri": record.uri, "replaces_stored_record": replaces.is_some(),
        }),
        Action::RemovePost {
            uri,
            record,
            tombstone,
        } => json!({
            "action": "remove_post", "uri": uri, "had_stored_record": record.is_some(),
            "tombstone": tombstone,
        }),
        Action::IndexAccount { record, replaces } => json!({
            "action": "index_account", "uri": record.uri, "replaces_stored_record": replaces.is_some(),
        }),
        Action::RemoveAccount {
            uri,
            record,
            tombstone,
        } => json!({
            "action": "remove_account", "uri": uri, "had_stored_record": record.is_some(),
            "tombstone": tombstone,
        }),
        Action::AddAuthorTombstone { actor_uri } => {
            json!({ "action": "add_author_tombstone", "actor_uri": actor_uri })
        }
        Action::ClearAuthorTombstone { actor_uri } => {
            json!({ "action": "clear_author_tombstone", "actor_uri": actor_uri })
        }
        Action::Retry { reason } => json!({ "action": "retry", "reason": reason }),
    }
}

fn check_uri(uri: &str) -> anyhow::Result<()> {
    let url = Url::parse(uri).with_context(|| format!("{uri:?} is not a URI"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("{uri:?} must be an http(s) URI with a host");
    }
    Ok(())
}

fn open_store(config: &Config) -> anyhow::Result<Store> {
    std::fs::create_dir_all(&config.data_dir).with_context(|| {
        format!(
            "failed to create data directory {}",
            config.data_dir.display()
        )
    })?;
    Store::open(&config.store_path())
        .with_context(|| format!("failed to open {}", config.store_path().display()))
}

async fn ingest(config: &Config, uri: &str, kind: Option<FetchKind>) -> anyhow::Result<()> {
    check_uri(uri)?;
    let kind = kind.unwrap_or(FetchKind::Unknown);
    let Some(lock) = WriterLock::try_acquire(config)? else {
        // A server is running and owns the indexes: queue it for the server's worker.
        let store = open_store(config)?;
        let outcome = store.enqueue(
            &NewFetch {
                uri: uri.to_string(),
                kind,
                reason: FetchReason::Announce,
                record: None,
                not_before: now_secs(),
            },
            now_secs(),
            config.queue_cap,
        )?;
        return print_json(&json!({
            "mode": "queued",
            "uri": uri,
            "queue": format!("{outcome:?}"),
            "note": "the running server holds the writer lock and will fetch it from the queue",
        }));
    };

    let base_url = config.require_base_url()?.to_string();
    let key = load_or_generate(&config.keys_dir())?;
    let fetcher = Arc::new(build_fetcher(config, &base_url, key)?);
    let writer = Writer::start_with_lock(config, lock)?;
    let store = open_store(config)?;
    seed_hosts(&fetcher, &store, now_secs())?;
    let result = {
        let worker = Worker::new(Arc::clone(&fetcher), writer.handle(), worker_config(config));
        let now = now_secs();
        let processed = worker.process(&adhoc_item(uri, kind, now), now).await;
        let mut report = json!({
            "mode": "in-process",
            "uri": uri,
            "actions": processed.actions.iter().map(action_summary).collect::<Vec<_>>(),
        });
        if let Some(retry) = &processed.retry {
            // Keep it for a running server, or the next `serve`, to try again.
            let outcome = store.enqueue(
                &NewFetch {
                    uri: uri.to_string(),
                    kind,
                    reason: FetchReason::Announce,
                    record: None,
                    not_before: retry.not_before_hint.unwrap_or(now + 60),
                },
                now,
                config.queue_cap,
            )?;
            if let Some(object) = report.as_object_mut() {
                object.insert(
                    "retry".to_string(),
                    json!({
                        "reason": retry.reason,
                        "queued": !matches!(outcome, EnqueueOutcome::Deferred),
                    }),
                );
            }
        }
        report
    };
    persist_hosts(&fetcher, &store, now_secs())?;
    writer.finish();
    print_json(&result)
}

// ---------------------------------------------------------------------------------------------
// search

/// The tombstones and domain blocks search must honour, read from SQLite.
fn load_search_tombstones(config: &Config) -> anyhow::Result<std::sync::RwLock<Tombstones>> {
    let store = Store::open(&config.store_path())
        .with_context(|| format!("failed to open {}", config.store_path().display()))?;
    Ok(std::sync::RwLock::new(load_tombstones(&store)?))
}

fn search_command(config: &Config, what: SearchCommand) -> anyhow::Result<()> {
    let scheme = config.key_scheme();
    match what {
        SearchCommand::Posts { q, limit } => {
            let reader = SocialReader::<SocialPost>::open(&config.posts_path(), &scheme)?;
            let tombstones = load_search_tombstones(config)?;
            for view in search::posts(&reader, &tombstones, &q, limit, now_secs())? {
                println!("{}", serde_json::to_string(&view)?);
            }
        }
        SearchCommand::Accounts { q, limit, language } => {
            let reader = SocialReader::<SocialAccount>::open(&config.accounts_path(), &scheme)?;
            let tombstones = load_search_tombstones(config)?;
            for view in search::accounts(&reader, &tombstones, &q, limit, language.as_deref())? {
                println!("{}", serde_json::to_string(&view)?);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// forget

/// What the `forget` argument names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Uri(String),
    Acct { user: String, host: String },
    Domain(String),
}

/// Contains `://`: a URI. Contains `@` (a leading one is stripped): an account. Else a domain.
pub fn parse_target(text: &str) -> anyhow::Result<Target> {
    let text = text.trim();
    if text.contains("://") {
        check_uri(text)?;
        return Ok(Target::Uri(text.to_string()));
    }
    if text.contains('@') {
        let acct = text.strip_prefix('@').unwrap_or(text);
        return match acct.split_once('@') {
            Some((user, host)) if !user.is_empty() && !host.is_empty() && !host.contains('@') => {
                Ok(Target::Acct {
                    user: user.to_string(),
                    host: host.to_ascii_lowercase(),
                })
            }
            _ => bail!("{text:?} is not an account of the form user@host"),
        };
    }
    let domain = text.trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() || domain.contains(|c: char| c.is_whitespace() || c == '/') {
        bail!("{text:?} is not a URI, an account or a domain");
    }
    Ok(Target::Domain(domain))
}

/// The `self` link of a `WebFinger` document that points at an `ActivityPub` actor.
#[must_use]
pub fn self_link(jrd: &Value) -> Option<String> {
    jrd.get("links")?.as_array()?.iter().find_map(|link| {
        let is_self = link.get("rel").and_then(Value::as_str) == Some("self");
        let kind = link.get("type").and_then(Value::as_str).unwrap_or_default();
        let activity = kind.contains("activity+json") || kind.contains("ld+json");
        (is_self && activity)
            .then(|| link.get("href").and_then(Value::as_str).map(str::to_string))
            .flatten()
    })
}

/// Look an account up by `WebFinger`: one request, made on an administrator's say-so.
async fn resolve_acct(config: &Config, user: &str, host: &str) -> anyhow::Result<String> {
    let scheme = if config.allow_http { "http" } else { "https" };
    let mut url = Url::parse(&format!("{scheme}://{host}/.well-known/webfinger"))
        .with_context(|| format!("{host:?} is not a valid host"))?;
    url.query_pairs_mut()
        .append_pair("resource", &format!("acct:{user}@{host}"));
    let agent = config
        .base_url
        .as_deref()
        .map_or_else(|| "mwmbl-social".to_string(), user_agent);
    let client = reqwest::Client::builder()
        .user_agent(agent)
        .timeout(Duration::from_secs(20))
        .build()
        .context("failed to build the HTTP client")?;
    let response = client
        .get(url.clone())
        .header("accept", "application/jrd+json, application/json")
        .send()
        .await
        .with_context(|| format!("WebFinger request to {host} failed"))?;
    if !response.status().is_success() {
        bail!(
            "WebFinger for {user}@{host} answered {}",
            response.status().as_u16()
        );
    }
    let body = response
        .bytes()
        .await
        .with_context(|| format!("failed to read the WebFinger answer from {host}"))?;
    if body.len() > 1024 * 1024 {
        bail!("the WebFinger answer from {host} is too large");
    }
    let jrd: Value = serde_json::from_slice(&body)
        .with_context(|| format!("the WebFinger answer from {host} is not JSON"))?;
    self_link(&jrd)
        .with_context(|| format!("WebFinger for {user}@{host} has no ActivityPub `self` link"))
}

async fn forget(config: &Config, target: &str) -> anyhow::Result<()> {
    let resolved = match parse_target(target)? {
        Target::Uri(uri) => ForgetTarget::Uri(uri),
        Target::Acct { user, host } => ForgetTarget::Uri(resolve_acct(config, &user, &host).await?),
        Target::Domain(domain) => ForgetTarget::Domain(domain),
    };
    let (kind, name) = match &resolved {
        ForgetTarget::Uri(uri) => ("uri", uri.clone()),
        ForgetTarget::Domain(domain) => ("domain", domain.clone()),
    };
    let Some(lock) = WriterLock::try_acquire(config)? else {
        // A server is running: record the request. It reloads these rows every 30 seconds and
        // removes what they cover from the index during its next sweep.
        let store = open_store(config)?;
        let now = now_secs();
        match &resolved {
            ForgetTarget::Uri(uri) => {
                store.add_forgotten(uri, now)?;
                store.add_tombstone(uri, TombstoneKind::Object, now)?;
                store.add_tombstone(uri, TombstoneKind::Author, now)?;
            }
            ForgetTarget::Domain(domain) => {
                store.add_domain_block(domain, now)?;
            }
        }
        return print_json(&json!({
            "mode": "recorded", "kind": kind, "target": name,
            "note": "the running server hides it from search within 30 seconds and removes it \
                     from the index during its next sweep",
        }));
    };
    let writer = Writer::start_with_lock(config, lock)?;
    writer.handle().forget(resolved).await?;
    writer.finish();
    print_json(&json!({ "mode": "applied", "kind": kind, "target": name }))
}

// ---------------------------------------------------------------------------------------------
// stats

fn index_size<T: tinyindex::IndexItem>(path: &std::path::Path) -> anyhow::Result<Value> {
    if !path.exists() {
        return Ok(Value::Null);
    }
    let index = TinyIndex::<T>::open(path)
        .with_context(|| format!("failed to open index {}", path.display()))?;
    Ok(json!({
        "pages": index.num_pages(),
        "page_size": index.metadata().page_size,
    }))
}

fn sweep_json(store: &Store, index: IndexName, now: i64) -> anyhow::Result<Value> {
    let s = store.sweep_state(index.as_str(), now)?;
    Ok(json!({
        "current_page": s.current_page,
        "current_sweep_started_at": s.current_sweep_started_at,
        "last_sweep_started_at": s.last_sweep_started_at,
        "last_sweep_finished_at": s.last_sweep_finished_at,
    }))
}

fn stats(config: &Config) -> anyhow::Result<Value> {
    let store = open_store(config)?;
    let now = now_secs();
    let tombstones = store.list_tombstones()?;
    let count = |kind| tombstones.iter().filter(|t| t.kind == kind).count();
    let value = json!({
        "queue": {
            "total": store.queue_len()?,
            "by_reason": store.queue_len_by_reason()?,
        },
        "tombstones": {
            "object": count(TombstoneKind::Object),
            "author": count(TombstoneKind::Author),
        },
        "domain_blocks": store.list_domain_blocks()?.len(),
        "registrations": store.list_registrations()?.len(),
        "sweep": {
            "posts": sweep_json(&store, IndexName::Posts, now)?,
            "accounts": sweep_json(&store, IndexName::Accounts, now)?,
        },
        "indexes": {
            "posts": index_size::<SocialPost>(&config.posts_path())?,
            "accounts": index_size::<SocialAccount>(&config.accounts_path())?,
        },
    });
    Ok(value)
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn the_cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn target_parsing() {
        assert_eq!(
            parse_target("https://h.example/users/a").ok(),
            Some(Target::Uri("https://h.example/users/a".into()))
        );
        assert_eq!(
            parse_target("@Alice@Mastodon.Example").ok(),
            Some(Target::Acct {
                user: "Alice".into(),
                host: "mastodon.example".into()
            })
        );
        assert_eq!(
            parse_target("alice@h.example").ok(),
            Some(Target::Acct {
                user: "alice".into(),
                host: "h.example".into()
            })
        );
        assert_eq!(
            parse_target("Bad.Example.").ok(),
            Some(Target::Domain("bad.example".into()))
        );
        // An account handle with a URL-ish part is a URI first.
        assert!(matches!(
            parse_target("https://h.example/@a@b").ok(),
            Some(Target::Uri(_))
        ));
        for bad in [
            "",
            "@",
            "a@",
            "@@h.example",
            "a@b@c",
            "h.example/path",
            "two words",
            "ftp://h.example/x",
        ] {
            assert!(parse_target(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn webfinger_self_link() {
        let jrd = json!({"subject": "acct:a@h.example", "links": [
            {"rel": "http://webfinger.net/rel/profile-page", "type": "text/html", "href": "https://h.example/@a"},
            {"rel": "self", "type": "application/activity+json", "href": "https://h.example/users/a"},
        ]});
        assert_eq!(
            self_link(&jrd).as_deref(),
            Some("https://h.example/users/a")
        );
        let ld = json!({"links": [{"rel": "self", "type": "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\"", "href": "https://h.example/u"}]});
        assert_eq!(self_link(&ld).as_deref(), Some("https://h.example/u"));
        assert_eq!(
            self_link(&json!({"links": [{"rel": "self", "type": "text/html", "href": "x"}]})),
            None
        );
        assert_eq!(self_link(&json!({})), None);
        assert_eq!(self_link(&json!({"links": "no"})), None);
    }

    #[test]
    fn action_summaries_carry_no_content() {
        let action = Action::RemovePost {
            uri: "https://h.example/s/1".into(),
            record: None,
            tombstone: true,
        };
        assert_eq!(
            action_summary(&action),
            json!({"action": "remove_post", "uri": "https://h.example/s/1",
                   "had_stored_record": false, "tombstone": true})
        );
    }

    #[test]
    fn uris_are_checked() {
        assert!(check_uri("https://h.example/x").is_ok());
        assert!(check_uri("http://127.0.0.1:1/x").is_ok());
        assert!(check_uri("ftp://h.example/x").is_err());
        assert!(check_uri("nonsense").is_err());
    }
}
