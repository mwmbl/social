//! The single writer: one thread owns both index files and applies every change to them.
//!
//! Everything else (the fetch worker, the sweep, the CLI) talks to it through a
//! [`WriterHandle`]. The thread also owns the only SQLite connection that writes tombstones,
//! and keeps the in-memory [`Tombstones`] that search reads in step with the table.
//!
//! The consent guarantees live in [`Core::apply`]: what a decision from [`crate::decide`] means
//! for the index. Across processes the writer is the holder of an exclusive lock on
//! `writer.lock`, so a second `serve` (or an `ingest` that would write the index) cannot run
//! beside it.

use std::collections::HashSet;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock, RwLockWriteGuard};
use std::thread::JoinHandle;

use social_index::tombstones::host_of;
use social_index::{
    DeleteOutcome, SocialAccount, SocialIndex, SocialIndexError, SocialPost, SocialReader,
    SweepOutcome, SweepSeen, Tombstones, sweep_page,
};
use store::{Store, StoreError, TombstoneKind};
use tinyindex::IndexError;
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;
use crate::decide::Action;
use crate::now_secs;

/// Commands queued to the writer before senders wait.
const CHANNEL_CAPACITY: usize = 256;
/// Capacity of each index's [`SweepSeen`].
const SWEEP_SEEN_CAPACITY: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexName {
    Posts,
    Accounts,
}

impl IndexName {
    /// The name the sweep state is stored under.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Posts => "posts",
            Self::Accounts => "accounts",
        }
    }
}

/// What `forget` removes. An account handle is resolved to an actor URI before it gets here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgetTarget {
    /// An object or actor URI: removed from both indexes and tombstoned both ways.
    Uri(String),
    /// A domain and its subdomains: blocked.
    Domain(String),
}

#[derive(Debug, thiserror::Error)]
pub enum WriterError {
    #[error("{operation}: {source}")]
    Index {
        operation: String,
        source: IndexError,
    },
    #[error("{operation}: {source}")]
    Store {
        operation: String,
        source: StoreError,
    },
    #[error("the index writer has stopped")]
    Stopped,
}

fn index_err(operation: impl Into<String>) -> impl FnOnce(IndexError) -> WriterError {
    let operation = operation.into();
    move |source| WriterError::Index { operation, source }
}

fn store_err(operation: impl Into<String>) -> impl FnOnce(StoreError) -> WriterError {
    let operation = operation.into();
    move |source| WriterError::Store { operation, source }
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("failed to {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("another process holds the writer lock {path}; only one may write the indexes")]
    Locked { path: PathBuf },
    #[error(transparent)]
    Index(#[from] SocialIndexError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The exclusive lock on `{data_dir}/writer.lock`. Held while the writer thread runs.
#[derive(Debug)]
pub struct WriterLock {
    _file: File,
    path: PathBuf,
}

impl WriterLock {
    /// Try to take the lock, creating the data directory if needed. `Ok(None)` means another
    /// process (normally a running `serve`) holds it.
    pub fn try_acquire(config: &Config) -> Result<Option<Self>, OpenError> {
        std::fs::create_dir_all(&config.data_dir).map_err(|source| OpenError::Io {
            action: "create data directory",
            path: config.data_dir.clone(),
            source,
        })?;
        let path = config.lock_path();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| OpenError::Io {
                action: "open lock file",
                path: path.clone(),
                source,
            })?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file, path })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(source)) => Err(OpenError::Io {
                action: "lock",
                path,
                source,
            }),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// What the tombstone and domain-block tables hold, as the in-memory set search uses.
pub fn load_tombstones(store: &Store) -> Result<Tombstones, StoreError> {
    let mut tombstones = Tombstones::new();
    for row in store.list_tombstones()? {
        match row.kind {
            TombstoneKind::Object => tombstones.add_object(&row.uri),
            TombstoneKind::Author => tombstones.add_author(&row.uri),
        }
    }
    for block in store.list_domain_blocks()? {
        tombstones.add_domain(&block.domain);
    }
    // Forgotten URIs stay hidden after sweeps clear their tombstone rows.
    for uri in store.list_forgotten()? {
        tombstones.add_object(&uri);
        tombstones.add_author(&uri);
    }
    Ok(tombstones)
}

/// What one page of the sweep did, for whichever index it was.
#[derive(Debug, Clone, PartialEq)]
pub enum SweepStepOutcome {
    Posts(SweepOutcome<SocialPost>),
    Accounts(SweepOutcome<SocialAccount>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexSize {
    pub pages: usize,
    pub page_size: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterStats {
    pub posts: IndexSize,
    pub accounts: IndexSize,
    /// Objects, authors and domains in the in-memory tombstone set.
    pub tombstones: usize,
}

type Reply<T> = oneshot::Sender<Result<T, WriterError>>;

#[derive(Debug)]
enum WriterCommand {
    Apply {
        actions: Vec<Action>,
        reply: Reply<()>,
    },
    Forget {
        target: ForgetTarget,
        reply: Reply<()>,
    },
    SweepStep {
        index: IndexName,
        page: usize,
        stale_before: i64,
        reply: Reply<SweepStepOutcome>,
    },
    ReloadTombstones {
        reply: Reply<()>,
    },
    ClearTombstonesBefore {
        before: i64,
        reply: Reply<usize>,
    },
    Stats {
        reply: Reply<WriterStats>,
    },
}

/// Cloneable access to the writer, and to the read side of what it writes.
#[derive(Debug, Clone)]
pub struct WriterHandle {
    tx: mpsc::Sender<WriterCommand>,
    posts: SocialReader<SocialPost>,
    accounts: SocialReader<SocialAccount>,
    tombstones: Arc<RwLock<Tombstones>>,
}

impl WriterHandle {
    async fn call<R>(
        &self,
        make: impl FnOnce(Reply<R>) -> WriterCommand,
    ) -> Result<R, WriterError> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(make(reply))
            .await
            .map_err(|_| WriterError::Stopped)?;
        response.await.map_err(|_| WriterError::Stopped)?
    }

    /// Apply decided actions in order. `Retry` actions are ignored: the fetch worker handles
    /// those.
    pub async fn apply(&self, actions: Vec<Action>) -> Result<(), WriterError> {
        self.call(|reply| WriterCommand::Apply { actions, reply })
            .await
    }

    pub async fn forget(&self, target: ForgetTarget) -> Result<(), WriterError> {
        self.call(|reply| WriterCommand::Forget { target, reply })
            .await
    }

    /// Sweep one page of one index.
    pub async fn sweep_step(
        &self,
        index: IndexName,
        page: usize,
        stale_before: i64,
    ) -> Result<SweepStepOutcome, WriterError> {
        self.call(|reply| WriterCommand::SweepStep {
            index,
            page,
            stale_before,
            reply,
        })
        .await
    }

    /// Re-read the tombstone and domain-block tables into memory, to pick up rows another
    /// process (the CLI) wrote.
    pub async fn reload_tombstones(&self) -> Result<(), WriterError> {
        self.call(|reply| WriterCommand::ReloadTombstones { reply })
            .await
    }

    /// Delete tombstones created before `before` and reload the in-memory set. Returns how
    /// many rows were deleted.
    pub async fn clear_tombstones_before(&self, before: i64) -> Result<usize, WriterError> {
        self.call(|reply| WriterCommand::ClearTombstonesBefore { before, reply })
            .await
    }

    pub async fn stats(&self) -> Result<WriterStats, WriterError> {
        self.call(|reply| WriterCommand::Stats { reply }).await
    }

    /// Read handles on both indexes, for search.
    #[must_use]
    pub fn readers(&self) -> (SocialReader<SocialPost>, SocialReader<SocialAccount>) {
        (self.posts.clone(), self.accounts.clone())
    }

    /// The in-memory tombstones the writer keeps current. Search holds the read lock while it
    /// filters.
    #[must_use]
    pub fn tombstones(&self) -> Arc<RwLock<Tombstones>> {
        Arc::clone(&self.tombstones)
    }
}

/// The writer thread and a handle to it.
#[derive(Debug)]
pub struct Writer {
    handle: WriterHandle,
    thread: JoinHandle<()>,
}

impl Writer {
    /// Take the writer lock and start. Fails with [`OpenError::Locked`] if it is held.
    pub fn start(config: &Config) -> Result<Self, OpenError> {
        let lock = WriterLock::try_acquire(config)?.ok_or_else(|| OpenError::Locked {
            path: config.lock_path(),
        })?;
        Self::start_with_lock(config, lock)
    }

    /// Open (creating if needed) the indexes and the store and start the thread, which holds
    /// `lock` until it stops.
    pub fn start_with_lock(config: &Config, lock: WriterLock) -> Result<Self, OpenError> {
        let scheme = config.key_scheme();
        let posts = SocialIndex::<SocialPost>::open_or_create(
            &config.posts_path(),
            config.posts_pages,
            config.page_size,
            scheme,
        )?;
        let accounts = SocialIndex::<SocialAccount>::open_or_create(
            &config.accounts_path(),
            config.accounts_pages,
            config.page_size,
            scheme,
        )?;
        let store = Store::open(&config.store_path())?;
        let tombstones = Arc::new(RwLock::new(load_tombstones(&store)?));
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let handle = WriterHandle {
            tx,
            posts: posts.reader(),
            accounts: accounts.reader(),
            tombstones: Arc::clone(&tombstones),
        };
        let forgotten = store.list_forgotten()?.into_iter().collect();
        let mut core = Core {
            forgotten,
            posts,
            accounts,
            store,
            tombstones,
            seen_posts: SweepSeen::new(SWEEP_SEEN_CAPACITY),
            seen_accounts: SweepSeen::new(SWEEP_SEEN_CAPACITY),
        };
        let thread = std::thread::Builder::new()
            .name("index-writer".to_string())
            .spawn(move || {
                // Held until the thread ends.
                let _lock = lock;
                while let Some(command) = rx.blocking_recv() {
                    core.handle(command);
                }
                tracing::debug!("index writer stopped");
            })
            .map_err(|source| OpenError::Io {
                action: "spawn the writer thread for",
                path: config.data_dir.clone(),
                source,
            })?;
        Ok(Self { handle, thread })
    }

    #[must_use]
    pub fn handle(&self) -> WriterHandle {
        self.handle.clone()
    }

    /// Stop the writer once every other [`WriterHandle`] has been dropped, and wait for it.
    pub fn finish(self) {
        let Self { handle, thread } = self;
        drop(handle);
        if thread.join().is_err() {
            tracing::error!("the index writer thread panicked");
        }
    }
}

struct Core {
    /// URIs an admin forgot: never indexed again, whatever a fetch says.
    forgotten: HashSet<String>,
    posts: SocialIndex<SocialPost>,
    accounts: SocialIndex<SocialAccount>,
    store: Store,
    tombstones: Arc<RwLock<Tombstones>>,
    seen_posts: SweepSeen,
    seen_accounts: SweepSeen,
}

fn describe(action: &Action) -> String {
    match action {
        Action::IndexPost { record, .. } => format!("index post {}", record.uri),
        Action::RemovePost { uri, .. } => format!("remove post {uri}"),
        Action::IndexAccount { record, .. } => format!("index account {}", record.uri),
        Action::RemoveAccount { uri, .. } => format!("remove account {uri}"),
        Action::AddAuthorTombstone { actor_uri } => format!("tombstone author {actor_uri}"),
        Action::ClearAuthorTombstone { actor_uri } => {
            format!("clear tombstone of author {actor_uri}")
        }
        Action::Retry { .. } => "retry".to_string(),
    }
}

impl Core {
    fn handle(&mut self, command: WriterCommand) {
        // A dropped receiver just means the caller stopped waiting; the work is done.
        match command {
            WriterCommand::Apply { actions, reply } => {
                let _ = reply.send(self.apply(actions));
            }
            WriterCommand::Forget { target, reply } => {
                let _ = reply.send(self.forget(&target));
            }
            WriterCommand::SweepStep {
                index,
                page,
                stale_before,
                reply,
            } => {
                let result = self.sweep_step(index, page, stale_before);
                if let Err(e) = &result {
                    tracing::error!(index = index.as_str(), page, error = %e, "sweep step failed");
                }
                let _ = reply.send(result);
            }
            WriterCommand::ReloadTombstones { reply } => {
                let _ = reply.send(self.reload());
            }
            WriterCommand::ClearTombstonesBefore { before, reply } => {
                let result = self
                    .store
                    .clear_tombstones_before(before)
                    .map_err(store_err(format!("clear tombstones before {before}")))
                    .and_then(|n| self.reload().map(|()| n));
                let _ = reply.send(result);
            }
            WriterCommand::Stats { reply } => {
                let _ = reply.send(Ok(self.stats()));
            }
        }
    }

    fn tombstones_mut(&self) -> RwLockWriteGuard<'_, Tombstones> {
        self.tombstones
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn reload(&mut self) -> Result<(), WriterError> {
        let fresh = load_tombstones(&self.store).map_err(store_err("reload tombstones"))?;
        *self.tombstones_mut() = fresh;
        self.forgotten = self
            .store
            .list_forgotten()
            .map_err(store_err("reload forgotten"))?
            .into_iter()
            .collect();
        Ok(())
    }

    fn stats(&self) -> WriterStats {
        let size = |pages: usize, page_size: usize| IndexSize { pages, page_size };
        let (posts, accounts) = (self.posts.reader(), self.accounts.reader());
        WriterStats {
            posts: size(
                posts.index().num_pages(),
                posts.index().metadata().page_size,
            ),
            accounts: size(
                accounts.index().num_pages(),
                accounts.index().metadata().page_size,
            ),
            tombstones: self
                .tombstones
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .len(),
        }
    }

    /// Record an object tombstone: in the store first, so it survives a crash, then in memory.
    fn clear_object_tombstone(&self, uri: &str) -> Result<(), WriterError> {
        if self
            .store
            .remove_tombstone(uri, TombstoneKind::Object)
            .map_err(store_err(format!("clear object tombstone {uri}")))?
        {
            self.tombstones_mut().remove_object(uri);
        }
        Ok(())
    }

    fn tombstone_object(&self, uri: &str) -> Result<(), WriterError> {
        self.store
            .add_tombstone(uri, TombstoneKind::Object, now_secs())
            .map_err(store_err(format!("tombstone object {uri}")))?;
        self.tombstones_mut().add_object(uri);
        Ok(())
    }

    fn tombstone_author(&self, uri: &str) -> Result<(), WriterError> {
        self.store
            .add_tombstone(uri, TombstoneKind::Author, now_secs())
            .map_err(store_err(format!("tombstone author {uri}")))?;
        self.tombstones_mut().add_author(uri);
        Ok(())
    }

    fn apply(&mut self, actions: Vec<Action>) -> Result<(), WriterError> {
        for action in actions {
            let operation = describe(&action);
            self.apply_one(action).map_err(|e| {
                tracing::error!(operation, error = %e, "failed to apply an action");
                e
            })?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)] // One arm per action; splitting hides the consent rules.
    fn apply_one(&mut self, action: Action) -> Result<(), WriterError> {
        match action {
            Action::IndexPost { record, replaces } => {
                let operation = format!("index post {}", record.uri);
                if self.forgotten.contains(&record.uri)
                    || self.forgotten.contains(&record.author_uri)
                {
                    tracing::info!(uri = %record.uri, "not indexing a forgotten post or author");
                    return Ok(());
                }
                // A fresh fetch just passed consent, so an object tombstone from an earlier
                // non-public version must not make the sweep delete this one.
                self.clear_object_tombstone(&record.uri)?;
                match replaces {
                    Some(old) => self
                        .posts
                        .replace(&old, &record)
                        .map_err(index_err(operation)),
                    None => self
                        .posts
                        .upsert(&record)
                        .map(|_| ())
                        .map_err(index_err(operation)),
                }
            }
            Action::RemovePost {
                uri,
                record,
                tombstone,
            } => {
                let operation = format!("remove post {uri}");
                // The tombstone goes in first: if the delete fails, search already hides it.
                if tombstone {
                    self.tombstone_object(&uri)?;
                }
                if let Some(record) = &record {
                    self.posts
                        .delete_record(record)
                        .map_err(index_err(operation.clone()))?;
                }
                // Also by URI: catches a newer version stored under the same `uri:` key.
                let outcome = self.posts.delete(&uri).map_err(index_err(operation))?;
                if record.is_none() && outcome == DeleteOutcome::NotFound && !tombstone {
                    self.tombstone_object(&uri)?;
                }
                Ok(())
            }
            Action::IndexAccount {
                mut record,
                replaces,
            } => {
                let operation = format!("index account {}", record.uri);
                if self.forgotten.contains(&record.uri) {
                    tracing::info!(uri = %record.uri, "not indexing a forgotten account");
                    return Ok(());
                }
                self.clear_object_tombstone(&record.uri)?;
                if replaces.is_none()
                    && let Some(existing) = self
                        .accounts
                        .find(&record.uri)
                        .map_err(index_err(operation.clone()))?
                    && existing.languages.is_some()
                {
                    record.languages = existing.languages;
                }
                match replaces {
                    Some(old) => self
                        .accounts
                        .replace(&old, &record)
                        .map_err(index_err(operation)),
                    None => self
                        .accounts
                        .upsert(&record)
                        .map(|_| ())
                        .map_err(index_err(operation)),
                }
            }
            Action::RemoveAccount {
                uri,
                record,
                tombstone,
            } => {
                let operation = format!("remove account {uri}");
                if tombstone {
                    self.tombstone_object(&uri)?;
                }
                if let Some(record) = &record {
                    self.accounts
                        .delete_record(record)
                        .map_err(index_err(operation.clone()))?;
                }
                let outcome = self.accounts.delete(&uri).map_err(index_err(operation))?;
                if record.is_none() && outcome == DeleteOutcome::NotFound && !tombstone {
                    self.tombstone_object(&uri)?;
                }
                Ok(())
            }
            Action::AddAuthorTombstone { actor_uri } => self.tombstone_author(&actor_uri),
            Action::ClearAuthorTombstone { actor_uri } => {
                if self.forgotten.contains(&actor_uri) {
                    return Ok(());
                }
                self.store
                    .remove_tombstone(&actor_uri, TombstoneKind::Author)
                    .map_err(store_err(format!("clear author tombstone {actor_uri}")))?;
                self.tombstones_mut().remove_author(&actor_uri);
                Ok(())
            }
            Action::Retry { .. } => Ok(()),
        }
    }

    fn forget(&mut self, target: &ForgetTarget) -> Result<(), WriterError> {
        match target {
            ForgetTarget::Uri(uri) => {
                let operation = format!("forget {uri}");
                // The uri may be an object or an actor: hide it as both first, then delete.
                self.store
                    .add_forgotten(uri, now_secs())
                    .map_err(store_err(format!("forget {uri}")))?;
                self.forgotten.insert(uri.clone());
                self.tombstone_object(uri)?;
                self.tombstone_author(uri)?;
                self.posts
                    .delete(uri)
                    .map_err(index_err(operation.clone()))?;
                self.accounts.delete(uri).map_err(index_err(operation))?;
                Ok(())
            }
            ForgetTarget::Domain(domain) => {
                let domain = self
                    .store
                    .add_domain_block(domain, now_secs())
                    .map_err(store_err(format!("block domain {domain}")))?;
                self.tombstones_mut().add_domain(&domain);
                Ok(())
            }
        }
    }

    fn sweep_step(
        &mut self,
        index: IndexName,
        page: usize,
        stale_before: i64,
    ) -> Result<SweepStepOutcome, WriterError> {
        let operation = format!("sweep page {page} of the {} index", index.as_str());
        // A new pass starts at page 0: forget which URIs the last pass handled.
        if page == 0 {
            match index {
                IndexName::Posts => self.seen_posts.clear(),
                IndexName::Accounts => self.seen_accounts.clear(),
            }
        }
        let tombstones = self
            .tombstones
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        match index {
            IndexName::Posts => sweep_page(
                &mut self.posts,
                page,
                &tombstones,
                stale_before,
                &mut self.seen_posts,
            )
            .map(SweepStepOutcome::Posts),
            IndexName::Accounts => sweep_page(
                &mut self.accounts,
                page,
                &tombstones,
                stale_before,
                &mut self.seen_accounts,
            )
            .map(SweepStepOutcome::Accounts),
        }
        .map_err(index_err(operation))
    }
}

/// Whether `uri`'s host is blocked by a domain block in `tombstones`.
#[must_use]
pub fn host_blocked(tombstones: &RwLock<Tombstones>, uri: &str) -> bool {
    host_of(uri).is_some_and(|host| {
        tombstones
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .domain_blocked(&host)
    })
}

/// Re-read the tombstone and domain-block tables every `every` until shutdown, so rows the
/// CLI wrote from another process take effect in the running server.
pub async fn run_reload(
    writer: WriterHandle,
    every: std::time::Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(every);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await;
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown.changed() => {}
        }
        if *shutdown.borrow() {
            break;
        }
        if let Err(e) = writer.reload_tombstones().await {
            tracing::error!(error = %e, "failed to reload tombstones");
        }
    }
}
