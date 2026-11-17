//! The command line: `serve`, `ingest`, `search`, `forget`, `stats`, the registration
//! commands `register`, `registrations` and `unregister`, and the follow commands `follow`,
//! `unfollow` and `following`.
//!
//! One process at a time may write the indexes, enforced by the lock on `writer.lock`
//! ([`WriterLock`]). `ingest` and `forget` write in-process when the lock is free and otherwise
//! only record their request in SQLite for the running server to pick up. `search` and `stats`
//! never need the lock. The registration commands only write SQLite (and `register` makes
//! outbound HTTP), so they need no lock either. Nor do the follow commands: they write the
//! `following` table and deliver with their own fetcher and our actor's key, and a running
//! server's inbox reads `following` from SQLite on every delivery, so it sees the follow.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use apub::actor::server_actor_key_id;
use apub::fetch::{DeliveryOutcome, Fetcher, FetcherConfig, user_agent};
use clap::{Parser, Subcommand, ValueEnum};
use fasp::registration::RegistrationOutcome;
use serde_json::{Value, json};
use social_index::{SocialAccount, SocialPost, SocialReader, Tombstones};
use store::{
    EnqueueOutcome, FetchKind, FetchReason, FollowState, Following, NewFetch, Store, TombstoneKind,
};
use tinyindex::TinyIndex;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing_subscriber::EnvFilter;
use url::Url;

use crate::admin_app::AdminApp;
use crate::config::{Config, ENV_CONFIG_PATH};
use crate::crawl_app::CrawlApp;
use crate::decide::Action;
use crate::delivery::{self, DeliveryConfig};
use crate::fasp_app::{FaspApp, MAINTAIN_EVERY};
use crate::follows::{
    FollowTarget, activity_id, fetch_followable, follow_activity, parse_follow_target, resolve,
    self_link, undo_follow_activity,
};
use crate::http::{AppState, router};
use crate::inbox_app::{FOLLOW_MAINTENANCE_EVERY, FollowSettings, InboxApp};
use crate::keys::load_or_generate;
use crate::metrics::{self, MetricsState};
use crate::now_secs;
use crate::registrations;
use crate::search;
use crate::sweep::{SweepConfig, run_index, step_interval};
use crate::worker::{
    FetchCounters, Worker, WorkerConfig, adhoc_item, persist_hosts, run_host_persistence,
    seed_hosts,
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
    /// Print queue, tombstone, registration, sweep and index statistics as JSON.
    Stats,
    /// Register with a fediverse server so it can use us as a FASP.
    Register {
        /// The server's own URL, e.g. `https://mastodon.example`.
        server_url: String,
        /// The server's FASP base URL, if discovery through nodeinfo does not find it.
        #[arg(long, value_name = "URL")]
        fasp_base_url: Option<String>,
    },
    /// List registrations (never their keys) as JSON.
    Registrations,
    /// Revoke a registration and forget its subscriptions and backfills.
    Unregister {
        /// The registration id shown by `registrations`.
        id: i64,
    },
    /// Follow a community or account, so its new posts reach our inbox.
    Follow {
        /// An actor URI, or `!community@host`, `@user@host` or `user@host`.
        target: String,
    },
    /// Stop following a community or account. What is indexed stays; its authors' own
    /// consent decides that.
    Unfollow {
        /// An actor URI, or `!community@host`, `@user@host` or `user@host`.
        target: String,
    },
    /// List the actors we follow, or asked to, as JSON.
    Following,
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
        Command::Register {
            server_url,
            fasp_base_url,
        } => {
            let outcome = register(&config, &server_url, fasp_base_url.as_deref()).await?;
            println!(
                "{}",
                registration_report(&outcome, config.require_base_url()?)
            );
            Ok(())
        }
        Command::Registrations => print_json(&registrations(&config)?),
        Command::Unregister { id } => {
            let summary = unregister(&config, id)?;
            print_json(&summary)
        }
        Command::Follow { target } => print_json(&follow(&config, &target).await?),
        Command::Unfollow { target } => print_json(&unfollow(&config, &target).await?),
        Command::Following => print_json(&following(&config)?),
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
        revalidate_interval_per_host: Duration::from_millis(config.revalidate_interval_per_host_ms),
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
) -> anyhow::Result<Arc<FetchCounters>> {
    let worker = Arc::new(Worker::new(
        Arc::clone(fetcher),
        handle.clone(),
        open_store(config)?,
        worker_config(config),
    ));
    let counters = worker.counters();
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
    Ok(counters)
}

async fn serve(config: Config) -> anyhow::Result<()> {
    config.require_base_url()?;
    let lock = acquire_lock(&config)?;
    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("failed to listen on {}", config.listen))?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });
    serve_with_lock(config, lock, listener, shutdown_rx).await
}

fn acquire_lock(config: &Config) -> anyhow::Result<WriterLock> {
    WriterLock::try_acquire(config)?.with_context(|| {
        format!(
            "another mwmbl-social process already holds the writer lock {}; \
             only one server may write the indexes",
            config.lock_path().display()
        )
    })
}

/// Runs the whole server on `listener` until `shutdown` is signalled: what `serve` does, minus
/// the signal handling. Tests use it to run the real thing in-process.
pub async fn serve_on(
    config: Config,
    listener: tokio::net::TcpListener,
    shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    config.require_base_url()?;
    let lock = acquire_lock(&config)?;
    serve_with_lock(config, lock, listener, shutdown).await
}

async fn serve_with_lock(
    config: Config,
    lock: WriterLock,
    listener: tokio::net::TcpListener,
    shutdown_rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let base_url = config.require_base_url()?.to_string();
    let key = load_or_generate(&config.keys_dir())?;
    let public_key_pem = key
        .verifying_key()
        .to_spki_pem()
        .context("failed to encode the actor's public key")?;
    let writer = Writer::start_with_lock(&config, lock)?;
    let handle = writer.handle();
    let fetcher = Arc::new(build_fetcher(&config, &base_url, key)?);
    let seeded = seed_hosts(&fetcher, &open_store(&config)?, now_secs())?;
    tracing::info!(hosts = seeded, "restored host state");

    let mut tasks: JoinSet<()> = JoinSet::new();
    let counters = spawn_background(&mut tasks, &config, &handle, &fetcher, &shutdown_rx)?;
    let (posts, accounts) = handle.readers();

    let fasp = FaspApp::new(
        open_store(&config)?,
        http_client(&base_url)?,
        &config,
        &base_url,
        (posts.clone(), accounts.clone()),
        handle.tombstones(),
    );
    tasks.spawn(
        fasp.clone()
            .run_maintenance(MAINTAIN_EVERY, shutdown_rx.clone()),
    );
    let (deliveries, delivering) = delivery::start(
        Arc::clone(&fetcher),
        DeliveryConfig::default(),
        shutdown_rx.clone(),
    );
    tasks.spawn(delivering);
    let inbox = InboxApp::new(
        &base_url,
        Arc::clone(&fetcher),
        open_store(&config)?,
        handle.clone(),
        deliveries,
        config.queue_cap,
        follow_settings(&config)?,
    );
    tasks.spawn(
        inbox
            .clone()
            .run_follow_maintenance(FOLLOW_MAINTENANCE_EVERY, shutdown_rx.clone()),
    );
    let mut app = router(AppState {
        base_url: base_url.clone(),
        actor_username: config.actor_username.clone(),
        name: config.name.clone(),
        public_key_pem,
        posts: posts.clone(),
        accounts: accounts.clone(),
        tombstones: handle.tombstones(),
        root_redirect: config.root_redirect.clone(),
    })
    .merge(fasp.routes())
    .merge(admin_routes(&config, &fasp))
    .merge(inbox.routes())
    .merge(crawl_routes(&config, &handle)?);
    if config.metrics {
        app = app.merge(metrics::router(Arc::new(MetricsState {
            store: std::sync::Mutex::new(open_store(&config)?),
            counters,
            posts,
            accounts,
        })));
        tracing::warn!("serving /metrics on the main listener: firewall or proxy that path");
    }
    tracing::info!(listen = %config.listen, base_url, "serving");
    let server = {
        let shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(signalled(shutdown))
                .await
        })
    };

    signalled(shutdown_rx).await;
    tracing::info!("shutting down");
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

/// `POST /api/v1/crawl/submissions`, which answers 404 unless `crawl_tokens` are configured.
fn crawl_routes(config: &Config, handle: &WriterHandle) -> anyhow::Result<axum::Router> {
    if !config.crawl_tokens.0.is_empty() {
        tracing::info!(
            tokens = config.crawl_tokens.0.len(),
            "accepting crawl submissions"
        );
    }
    Ok(CrawlApp::new(open_store(config)?, handle.tombstones(), config).routes())
}

/// The admin API, which answers 404 unless `admin_tokens` are configured. It shares the FASP
/// state's store connection and HTTP client.
fn admin_routes(config: &Config, fasp: &FaspApp) -> axum::Router {
    if !config.admin_tokens.0.is_empty() {
        tracing::info!(
            tokens = config.admin_tokens.0.len(),
            "serving the admin API"
        );
    }
    AdminApp::new(fasp.clone(), config).routes()
}

fn follow_settings(config: &Config) -> anyhow::Result<FollowSettings> {
    let targets = config
        .follow
        .iter()
        .map(|t| parse_follow_target(t))
        .collect::<Result<_, _>>()?;
    Ok(FollowSettings {
        targets,
        allow_http: config.allow_http,
        outbox_backfill_pages: config.outbox_backfill_pages,
    })
}

/// The HTTP client for our own calls to servers' FASP APIs and registration.
fn http_client(base_url: &str) -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(user_agent(base_url))
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build the HTTP client")
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
        let worker = Worker::new(
            Arc::clone(&fetcher),
            writer.handle(),
            open_store(config)?,
            worker_config(config),
        );
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
// register, registrations, unregister

/// Registers with the server at `server_url` and returns what the admin needs to see. Writes
/// only SQLite and makes outbound HTTP, so it works with or without a running server.
pub async fn register(
    config: &Config,
    server_url: &str,
    fasp_base_url: Option<&str>,
) -> anyhow::Result<RegistrationOutcome> {
    let store = std::sync::Mutex::new(open_store(config)?);
    let http = http_client(config.require_base_url()?)?;
    registrations::register_server(&http, &store, config, server_url, fasp_base_url, now_secs())
        .await
        .with_context(|| format!("failed to register with {server_url}"))
}

/// What `register` prints: the fingerprint to compare, the link to approve at, and what to do
/// next.
#[must_use]
pub fn registration_report(outcome: &RegistrationOutcome, our_base_url: &str) -> String {
    format!(
        "Registered with {server} (registration {id}, pending approval).\n\n\
         Key fingerprint: {fingerprint}\n\
         Approval link:   {link}\n\n\
         Next: open the approval link, compare the fingerprint shown there with the one above, \
         and approve. Then enable the data_sharing capability for this provider on the server. \
         Our server must be running and reachable from it at {base}.",
        server = outcome.fasp_base_url,
        id = outcome.registration_id,
        fingerprint = outcome.fingerprint,
        link = outcome.completion_uri,
        base = our_base_url,
    )
}

fn registrations(config: &Config) -> anyhow::Result<Value> {
    Ok(registrations::list_registrations(&open_store(config)?)?)
}

/// Revokes a registration: see [`registrations::revoke_registration`].
pub fn unregister(config: &Config, id: i64) -> anyhow::Result<Value> {
    registrations::revoke_registration(&open_store(config)?, id, now_secs())?
        .with_context(|| format!("there is no registration with id {id}"))
}

// ---------------------------------------------------------------------------------------------
// follow, unfollow, following

/// What the follow commands need: our actor, a fetcher with our actor's key, and SQLite.
struct FollowContext {
    our_actor: String,
    fetcher: Fetcher,
    store: Store,
}

impl FollowContext {
    fn open(config: &Config) -> anyhow::Result<Self> {
        let base_url = config.require_base_url()?.to_string();
        let key = load_or_generate(&config.keys_dir())?;
        let fetcher = build_fetcher(config, &base_url, key)?;
        let store = open_store(config)?;
        seed_hosts(&fetcher, &store, now_secs())?;
        Ok(Self {
            our_actor: apub::actor::server_actor_url(&base_url),
            fetcher,
            store,
        })
    }

    async fn resolve(&self, config: &Config, text: &str) -> anyhow::Result<String> {
        let target = parse_follow_target(text)?;
        let uri = resolve(&self.fetcher, config.allow_http, &target).await?;
        if let FollowTarget::Handle { .. } = target {
            tracing::info!(handle = %target, actor = uri, "resolved by WebFinger");
        }
        Ok(uri)
    }

    /// Delivers `activity` to `inbox` once, now.
    async fn deliver(&self, inbox: &str, activity: &Value) -> anyhow::Result<DeliveryOutcome> {
        let body = serde_json::to_vec(activity)?;
        Ok(self.fetcher.deliver(inbox, &body).await)
    }

    fn finish(&self) {
        if let Err(e) = persist_hosts(&self.fetcher, &self.store, now_secs()) {
            tracing::warn!(error = %e, "failed to persist host state");
        }
    }
}

/// Whether a `serve` process holds the writer lock (and so receives our inbox's `Accept`s).
fn server_running(config: &Config) -> anyhow::Result<bool> {
    Ok(WriterLock::try_acquire(config)?.is_none())
}

/// Follows `target`: resolves it, fetches the actor (any actor type: a community or an
/// account), records a pending `following` row and delivers our `Follow`. The `Accept` arrives
/// at a running server's inbox, which then backfills from the actor's outbox.
pub async fn follow(config: &Config, target: &str) -> anyhow::Result<Value> {
    let cx = FollowContext::open(config)?;
    let uri = cx.resolve(config, target).await?;
    let tombstones = load_tombstones(&cx.store)?;
    let blocked =
        social_index::tombstones::host_of(&uri).is_none_or(|host| tombstones.domain_blocked(&host));
    if blocked || cx.store.is_forgotten(&uri)? {
        bail!("{uri} is on a blocked domain or was forgotten; not following it");
    }
    let actor = fetch_followable(&cx.fetcher, &uri).await?;
    if let Some(row) = cx.store.following(&actor.uri)? {
        if row.state == FollowState::Pending {
            return resend_follow(&cx, &row).await;
        }
        return Ok(json!({
            "actor": row.actor_uri, "state": row.state.as_str(), "note": "already followed",
        }));
    }
    let id = activity_id(
        &cx.our_actor,
        "follows",
        crate::now_millis(),
        u64::from(std::process::id()),
    );
    cx.store
        .add_following(&actor.uri, &actor.inbox, &id, now_secs())?;
    let outcome = cx
        .deliver(
            &actor.inbox,
            &follow_activity(&cx.our_actor, &id, &actor.uri),
        )
        .await?;
    cx.finish();
    let delivered = match &outcome {
        DeliveryOutcome::Delivered { .. } => true,
        outcome if outcome.is_transient() => false,
        outcome => {
            cx.store.remove_following(&actor.uri)?;
            bail!("{} refused our Follow: {outcome:?}", actor.inbox);
        }
    };
    tracing::info!(
        actor = actor.uri,
        group = actor.info.is_group,
        delivered,
        "following"
    );
    let mut note = if delivered {
        "pending until the actor accepts; a running server then backfills from its outbox"
            .to_owned()
    } else {
        format!("delivery failed for now ({outcome:?}); a running server re-sends it within a day")
    };
    if !server_running(config)? {
        note.push_str("; no server is running, so start `serve` to receive the Accept");
    }
    Ok(json!({
        "actor": actor.uri,
        "type": if actor.info.is_group { "community" } else { "account" },
        "state": "pending",
        "delivered": delivered,
        "note": note,
    }))
}

/// Delivers a pending follow again, under a new id (Lemmy 0.19 rejects an activity id it has
/// seen). Lemmy 0.19 doesn't deliver an `Accept` to an instance it has just met: its delivery
/// worker for a new instance starts after the newest activity, so the first follow from a new
/// server is accepted but never answered (seen in dev/lemmy). Following again is how an admin
/// re-asks without waiting a day.
async fn resend_follow(cx: &FollowContext, row: &Following) -> anyhow::Result<Value> {
    let id = activity_id(
        &cx.our_actor,
        "follows",
        crate::now_millis(),
        u64::from(std::process::id()),
    );
    let activity = follow_activity(&cx.our_actor, &id, &row.actor_uri);
    let outcome = cx.deliver(&row.inbox, &activity).await?;
    cx.finish();
    let delivered = matches!(outcome, DeliveryOutcome::Delivered { .. });
    if delivered {
        cx.store.following_resent(&row.actor_uri, &id, now_secs())?;
    }
    tracing::info!(actor = row.actor_uri, delivered, "re-sent a pending follow");
    Ok(json!({
        "actor": row.actor_uri,
        "state": row.state.as_str(),
        "delivered": delivered,
        "note": if delivered {
            "still pending, so our Follow was sent again".to_owned()
        } else {
            format!("still pending; sending it again failed ({outcome:?})")
        },
    }))
}

/// Unfollows `target`: delivers `Undo{Follow}` and drops the row. Nothing indexed is removed:
/// a community's posts are consented by their authors and their host, not by our follow, and
/// an account's own flags (or its opt-in) decide for it.
pub async fn unfollow(config: &Config, target: &str) -> anyhow::Result<Value> {
    let cx = FollowContext::open(config)?;
    let uri = cx.resolve(config, target).await?;
    let row = cx
        .store
        .following(&uri)?
        .with_context(|| format!("we don't follow {uri}"))?;
    let undo = undo_follow_activity(&cx.our_actor, &row.follow_id, &row.actor_uri);
    let outcome = cx.deliver(&row.inbox, &undo).await?;
    cx.finish();
    let delivered = matches!(outcome, DeliveryOutcome::Delivered { .. });
    if !delivered {
        tracing::warn!(
            actor = row.actor_uri,
            inbox = row.inbox,
            ?outcome,
            "our Undo was not delivered; unfollowing here anyway"
        );
    }
    cx.store.remove_following(&row.actor_uri)?;
    let mut report = json!({ "actor": row.actor_uri, "unfollowed": true, "delivered": delivered });
    if cx.store.is_follow_optin(&row.actor_uri)?
        && let Some(object) = report.as_object_mut()
    {
        object.insert(
            "note".to_owned(),
            json!(
                "this actor follows us (an opt-in): its posts stay indexable, but no \
                   longer reach our inbox"
            ),
        );
    }
    Ok(report)
}

/// The `following` table as JSON.
pub fn following(config: &Config) -> anyhow::Result<Value> {
    let store = open_store(config)?;
    let mut out = Vec::new();
    for row in store.list_following()? {
        out.push(json!({
            "actor": row.actor_uri,
            "inbox": row.inbox,
            "state": row.state.as_str(),
            "sent_at": row.created_at,
            "follows_us": store.is_follow_optin(&row.actor_uri)?,
        }));
    }
    Ok(Value::Array(out))
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
    let (registrations_by_status, registration_capabilities) =
        metrics::registration_counts(&store)?;
    let registrations_total: usize = registrations_by_status.values().sum();
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
        "follow_optins": store.follow_optins()?.len(),
        "following": store
            .count_following()?
            .into_iter()
            .map(|(state, n)| (state.as_str().to_owned(), n))
            .collect::<std::collections::BTreeMap<_, _>>(),
        "registrations": {
            "total": registrations_total,
            "by_status": registrations_by_status,
            "capabilities": registration_capabilities,
        },
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
    use store::RegistrationStatus;

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
    fn registration_commands_parse() {
        let cli = Cli::try_parse_from([
            "mwmbl-social",
            "register",
            "https://m.example",
            "--fasp-base-url",
            "https://m.example/api/fasp",
        ])
        .ok();
        assert!(matches!(
            cli.map(|c| c.command),
            Some(Command::Register { server_url, fasp_base_url: Some(base) })
                if server_url == "https://m.example" && base == "https://m.example/api/fasp"
        ));
        let cli = Cli::try_parse_from(["mwmbl-social", "register", "https://m.example"]).ok();
        assert!(matches!(
            cli.map(|c| c.command),
            Some(Command::Register {
                fasp_base_url: None,
                ..
            })
        ));
        let cli = Cli::try_parse_from(["mwmbl-social", "unregister", "3"]).ok();
        assert!(matches!(
            cli.map(|c| c.command),
            Some(Command::Unregister { id: 3 })
        ));
        assert!(matches!(
            Cli::try_parse_from(["mwmbl-social", "registrations"])
                .ok()
                .map(|c| c.command),
            Some(Command::Registrations)
        ));
        assert!(Cli::try_parse_from(["mwmbl-social", "unregister", "x"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["mwmbl-social", "follow", "!rust@l.example"])
                .ok()
                .map(|c| c.command),
            Some(Command::Follow { target }) if target == "!rust@l.example"
        ));
        assert!(matches!(
            Cli::try_parse_from(["mwmbl-social", "unfollow", "https://l.example/c/rust"])
                .ok()
                .map(|c| c.command),
            Some(Command::Unfollow { .. })
        ));
        assert!(matches!(
            Cli::try_parse_from(["mwmbl-social", "following"])
                .ok()
                .map(|c| c.command),
            Some(Command::Following)
        ));
    }

    #[test]
    fn the_registration_report_tells_the_admin_what_to_do() -> anyhow::Result<()> {
        let report = registration_report(
            &RegistrationOutcome {
                registration_id: 4,
                fingerprint: "FINGERPRINT=".into(),
                completion_uri: "https://m.example/admin/fasps/4".into(),
                fasp_base_url: Url::parse("https://m.example/api/fasp")?,
            },
            "https://social.example",
        );
        for needle in [
            "FINGERPRINT=",
            "https://m.example/admin/fasps/4",
            "registration 4",
            "compare the fingerprint",
            "approve",
            "enable the data_sharing capability",
            "https://social.example",
        ] {
            assert!(report.contains(needle), "{needle:?} missing from\n{report}");
        }
        Ok(())
    }

    #[test]
    fn unregister_revokes_and_clears_without_touching_the_server() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = Config {
            data_dir: dir.path().to_path_buf(),
            ..Config::default()
        };
        let store = open_store(&config)?;
        let registration = store.create_registration(store::NewRegistration {
            base_url: "https://m.example/api/fasp".into(),
            server_id: "sid".into(),
            our_private_key: vec![7; 32],
            created_at: 1,
        })?;
        let id = registration.id;
        store.set_capability(id, "data_sharing", "0.1", true, 1)?;
        store.add_subscription(
            id,
            "11",
            store::Category::Content,
            store::SubscriptionType::Lifecycle,
            1,
        )?;
        store.add_backfill(id, "12", store::Category::Content)?;

        let summary = unregister(&config, id)?;
        assert_eq!(summary["subscriptions_deleted"], 1);
        assert_eq!(summary["backfills_deleted"], 1);
        let after = store.registration(id)?.map(|r| r.status);
        assert_eq!(after, Some(RegistrationStatus::Revoked));
        assert_eq!(store.list_subscriptions(id)?.len(), 0);
        assert_eq!(store.list_backfills(id)?.len(), 0);
        assert_eq!(store.enabled_capabilities(id)?.len(), 0);

        let listed = registrations(&config)?;
        assert_eq!(listed[0]["status"], "revoked");
        assert!(!listed.to_string().contains("private"));
        let err = unregister(&config, id + 100).err().map(|e| e.to_string());
        assert!(err.is_some_and(|e| e.contains("no registration")));
        Ok(())
    }

    #[test]
    fn uris_are_checked() {
        assert!(check_uri("https://h.example/x").is_ok());
        assert!(check_uri("http://127.0.0.1:1/x").is_ok());
        assert!(check_uri("ftp://h.example/x").is_err());
        assert!(check_uri("nonsense").is_err());
    }
}
