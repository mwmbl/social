//! Versioned schema. `PRAGMA user_version` records how many entries of [`MIGRATIONS`] have run.
//! Append new migrations; never edit one that has shipped.
//!
//! Every table is bounded by registrations, hosts, a fixed time window, or explicit user and
//! admin actions. None has a row per
//! document or account, and none holds term lists (CLAUDE.md principle 1).

pub(crate) const MIGRATIONS: &[&str] = &[
    "
CREATE TABLE registrations (
    id               INTEGER PRIMARY KEY,
    base_url         TEXT NOT NULL UNIQUE,
    server_id        TEXT NOT NULL UNIQUE,
    fasp_id          TEXT,
    our_private_key  BLOB NOT NULL,
    their_public_key BLOB,
    status           TEXT NOT NULL CHECK (status IN ('pending','active','revoked')),
    created_at       INTEGER NOT NULL
);
CREATE INDEX registrations_fasp_id ON registrations(fasp_id);

CREATE TABLE capabilities (
    registration_id INTEGER NOT NULL REFERENCES registrations(id) ON DELETE CASCADE,
    capability      TEXT NOT NULL,
    version         TEXT NOT NULL,
    enabled         INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    PRIMARY KEY (registration_id, capability, version)
);

CREATE TABLE subscriptions (
    id               INTEGER PRIMARY KEY,
    registration_id  INTEGER NOT NULL REFERENCES registrations(id) ON DELETE CASCADE,
    remote_id        TEXT NOT NULL,
    category         TEXT NOT NULL CHECK (category IN ('content','account')),
    subscription_type TEXT NOT NULL CHECK (subscription_type IN ('lifecycle','trends')),
    created_at       INTEGER NOT NULL,
    UNIQUE (registration_id, remote_id)
);

CREATE TABLE backfills (
    id              INTEGER PRIMARY KEY,
    registration_id INTEGER NOT NULL REFERENCES registrations(id) ON DELETE CASCADE,
    remote_id       TEXT NOT NULL,
    category        TEXT NOT NULL CHECK (category IN ('content','account')),
    state           TEXT NOT NULL CHECK (state IN ('active','done','failed')),
    more_available  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX backfills_state ON backfills(state);

CREATE TABLE fetch_queue (
    uri         TEXT PRIMARY KEY,
    kind        TEXT NOT NULL CHECK (kind IN ('object','actor','unknown')),
    reason      TEXT NOT NULL CHECK (reason IN ('announce','backfill','revalidate')),
    priority    INTEGER NOT NULL CHECK (priority BETWEEN 0 AND 2),
    record      TEXT,
    not_before  INTEGER NOT NULL,
    attempts    INTEGER NOT NULL DEFAULT 0,
    last_error  TEXT,
    enqueued_at INTEGER NOT NULL
);
CREATE INDEX fetch_queue_due ON fetch_queue(priority, not_before, enqueued_at);

CREATE TABLE hosts (
    host             TEXT PRIMARY KEY,
    signature_scheme TEXT CHECK (signature_scheme IN ('rfc9421','cavage')),
    backoff_until    INTEGER,
    blocked          INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE domain_blocks (
    domain     TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL
);

CREATE TABLE tombstones (
    uri        TEXT NOT NULL,
    kind       TEXT NOT NULL CHECK (kind IN ('object','author')),
    created_at INTEGER NOT NULL,
    PRIMARY KEY (uri, kind)
);
CREATE INDEX tombstones_created_at ON tombstones(created_at);

CREATE TABLE sweep_state (
    index_name               TEXT PRIMARY KEY,
    current_page             INTEGER NOT NULL,
    current_sweep_started_at INTEGER NOT NULL,
    last_sweep_started_at    INTEGER,
    last_sweep_finished_at   INTEGER
);
",
    "
-- Admin `forget <uri>`: permanent, unlike tombstones, which sweeps clear. Bounded by admin
-- actions, not by the index.
CREATE TABLE forgotten (
    uri        TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL
);
",
    "
-- Subscriptions we stopped using while a capability was disabled, so we couldn't call the
-- server to delete them. They are deleted on the server when the capability is enabled again.
-- Bounded by registrations.
CREATE TABLE orphaned_subscriptions (
    registration_id INTEGER NOT NULL REFERENCES registrations(id) ON DELETE CASCADE,
    remote_id       TEXT NOT NULL,
    PRIMARY KEY (registration_id, remote_id)
);
",
    "
-- Nodeinfo `software.name` per host (lowercased), for the Lemmy consent rule and crawl
-- submissions, and when it was last looked up. NULL software with a check time: the lookup
-- failed. Bounded by hosts.
ALTER TABLE hosts ADD COLUMN software TEXT;
ALTER TABLE hosts ADD COLUMN software_checked_at INTEGER;

-- Authors who opted in to indexing by following our server actor (PLAN.md Stage 3). A record of
-- a user action: bounded by those actions, like `forgotten` is by admin actions. A row goes
-- when the author unfollows.
CREATE TABLE follow_optins (
    actor_uri   TEXT PRIMARY KEY,
    followed_at INTEGER NOT NULL
);
",
    "
-- Actors our server actor follows: authors who followed us (the follow-back, PLAN.md Stage 3)
-- and, later, communities an admin chose. Our Follow activity's id is kept so we can Undo it
-- and match the Accept. Bounded by follow opt-ins and admin actions, never by the index.
CREATE TABLE following (
    actor_uri  TEXT PRIMARY KEY,
    inbox      TEXT NOT NULL,
    follow_id  TEXT NOT NULL,
    state      TEXT NOT NULL CHECK (state IN ('pending','accepted')),
    created_at INTEGER NOT NULL
);
",
];
