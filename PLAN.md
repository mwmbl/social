# Mwmbl Social: implementation plan

A search engine for fediverse content, written in Rust. Server owners can deploy it and connect
it to their own server as a FASP (Fediverse Auxiliary Service Provider). Mwmbl will also run a
hosted instance, which will later feed social results into Mwmbl search. The project is
independent of Mwmbl but reuses Mwmbl's index format and code, and will upstream improvements.

The project is called **Mwmbl Social**. The binary is `mwmbl-social`, and the binary crate is
`mwmbl_social`.

## Goals and non-goals

**Goals, in priority order**

1. Server-owner adoption: one binary (or Docker image) an admin can run and register with
   their Mastodon server in a few minutes, with low resource use.
2. Strict consent: index only what creators have opted into, and remove it promptly when that
   changes.
3. The same code runs our hosted, multi-server instance.

**Non-goals for now**

- **No end-user search UI.** In FASP the fediverse server is the UI. Mastodon calls us and
  shows the results. We expose a JSON search API and a CLI for testing and debugging.
- **The spec requires one small admin-facing flow: registration.** The FASP must show the admin
  a key fingerprint. Mwmbl Social has no web UI for it: self-hosters run the `register` CLI
  command, and an operator's own sign-up UI calls the token-authenticated admin registration
  API (see "Deployment and adoption"), which does the same thing. The spec defines the
  registration protocol but not who a FASP must accept; that is policy, and it lives with the
  operator. The server admin's approval step already proves their consent.
- **Mwmbl integration happens in Mwmbl,** at query time over the search API (see "Deployment
  and adoption"). Nothing about Mwmbl's search lives here.
- **Lemmy** comes after FASP.

## Spec facts that shape the design

From `mastodon/fediverse_auxiliary_service_provider_specifications`, checked 2026-11-15.

- **Specs that exist:**
  - `general/v0.1`: protocol basics, registration, provider_info.
  - `discovery/data_sharing/v0.1`, `discovery/account_search/v0.1`, `discovery/trends/v0.1`,
    `discovery/follow_recommendation/v0.1`.
  - Status/post search is only "tentatively planned", with no spec yet.
- **Registration:**
  1. The admin gives the FASP their server URL.
  2. The FASP reads `.well-known/nodeinfo` and finds `metadata.faspBaseUrl`.
  3. The FASP generates an Ed25519 keypair and a `serverId`, then sends
     `POST <faspBaseUrl>/registration` with `{name, baseUrl, serverId, publicKey}` (key base64).
  4. The server replies 201 with `{faspId, publicKey, registrationCompletionUri}`.
  5. The FASP shows the admin its key fingerprint (base64 of the key's SHA-256) and a link to
     `registrationCompletionUri`.
  6. The admin approves on the server side.
- **Capabilities:** the server calls `GET /provider_info`, which returns `name`,
  `privacyPolicy[]`, `capabilities[{id, version}]`, and optionally `signInUrl`, `contactEmail`
  and `fediverseAccount`. It then calls `POST` or `DELETE /capabilities/<id>/<version>/activation`;
  we reply 204, or 404 if unsupported. **We must not call a server API for a capability that
  isn't enabled.**
- **Request signing:** every request carries RFC 9530 `content-digest` (SHA-256). Requests are
  signed with RFC 9421, using Ed25519 with components `@method`, `@target-uri`,
  `content-digest` and params `created` and `keyid`. `keyid` is the ID exchanged at
  registration. Responses are signed with `@status` and `content-digest`. A bad signature gets
  401. Rate limits get 429 with `Retry-After`.
- **data_sharing:**
  - We subscribe with `POST /data_sharing/v0/event_subscriptions`, sending `{category:
    content|account, subscriptionType: lifecycle|trends, maxBatchSize?, threshold?}`.
  - We request backfill with `POST /data_sharing/v0/backfill_requests {category, maxCount}`,
    then `POST .../{id}/continuation`.
  - The server pushes `POST <ours>/data_sharing/v0/announcements` with `{source, category,
    objectUris, eventType | moreObjectsAvailable}` and we reply 204. **These carry URIs only.**
- **Fetching:** we fetch every object ourselves with a signed ActivityPub `GET`.
  - Try RFC 9421 first. On 401 or 403, fall back to draft-cavage-12, and cache which one each
    host accepts.
  - This needs a server actor at `/actor` with a public key, plus WebFinger for its
    `preferredUsername`.
- **Consent:**
  - Only fully public objects qualify. Check `to:` for the public collection. Unlisted and
    quiet-public objects don't qualify.
  - The creator must have opted in via FEP-5feb, and accounts must have `discoverable: true`.
  - For post full-text search we require `indexable: true` as well.
  - Two exceptions, both in Stage 3: Lemmy authors, and authors who opt in by following our
    server actor when their software has no `indexable` flag.
  - Servers send remote content too, so we deduplicate by URI.
- **Revalidation:** deletes arrive as lifecycle events, but we must also recheck every stored
  item at least weekly. A delivered change notification lets us skip the check for that object
  for one week.
- **account_search:** `GET /account_search/v0/search?term=&limit=20&language=` returns 200 with
  a JSON array of actor URIs ranked by relevance. It may page with `Link: rel="next"`. A missing
  `term` gets 422. It returns only opted-in (discoverable) accounts.

**Implication:** `data_sharing` alone gives admins nothing visible. It only feeds us. The first
visible feature is `account_search`, so it follows `data_sharing` straight away (Stage 2).
**Check early** which capabilities current Mastodon actually uses, and behind which flag. As of
my last knowledge FASP support was experimental (`EXPERIMENTAL_FEATURES=fasp`).

## What we reuse from Mwmbl

Source: `rust/mwmbl_core` in github.com/mwmbl/mwmbl. PR mwmbl/mwmbl#645 only touches the crawler, not the index.
Mwmbl is AGPL-3.0, so this project should be AGPL-3.0 too. That also suits a network service.

- **What exists in Rust:** `mwmbl_core/src/index.rs` handles page I/O. A page is a zstd frame
  of a JSON array of document tuples, zero-padded to 4096 bytes. `pack_page` binary-searches
  the largest best-first prefix that fits. There are also `read_index_page` /
  `write_index_page` (positioned I/O on a raw fd) and the tests in `tests/index.rs`. Copy these
  as they are.
- **What `indexer.rs` has:**
  - `TinyIndexMetadata` to and from bytes: the `mwmbl-tiny-search` header plus JSON, in a
    4096-byte metadata block.
  - `TinyIndex::from_file`, `create`, `validate`, and `create_index`.
  - A stubbed `Document` (`new` is `todo!()`).
  - **Bug:** `create` passes `METADATA_SIZE` as the page size. It's harmless because both are
    4096, but fix it.
- **What is still only in Python, to port:**
  - Key → page: `mmh3.hash(key, signed=False) % num_pages`, which is MurmurHash3 x86_32 with
    seed 0. Use the `murmur3` crate, or a small vendored implementation, and pin it with test
    vectors from Python so index files stay compatible.
  - The `Document` tuple layout: `[title, url, extract, score, term, state, user_ids,
    last_crawled, source]` with trailing `None`s dropped. A page is shared by many terms, and
    reads filter by `term`, at index 4. We use our own item types on the same page format;
    see Index documents.
  - Page retrieve and store, and the merge-into-page logic from Python's `TinyIndex` /
    `index_batches.py`.
  - Tokenising (`mwmbl/tokenizer.py`, `mwmbl/indexer/index.py`): lowercase, whitespace split,
    and for each field the first 10 tokens plus the first 10 bigrams, all used as index keys.
- **Properties to keep in mind:**
  - Each document is copied in full onto every term page it is indexed under, so extracts
    should be short.
  - Pages are lossy: when a page is full, the lowest-scored documents fall off. For social
    content that is a feature. Index size is bounded and old or low-value posts age out.
  - Mwmbl has no deletion. We have to add it (see below), and it is the main improvement to
    upstream.

## Architecture

One Cargo workspace producing one binary, `mwmbl-social`, with subcommands:

```
crates/
  tinyindex/    copied mwmbl_core::index + ported TinyIndex: open/create, retrieve(term),
                add(docs), delete(url, terms). No fediverse knowledge. Upstream target.
  tokenize/     Unicode-aware tokeniser + key selection (see below)
  social_index/ SocialPost/SocialAccount item types, index keys, URI lookup, delete by
                re-tokenising, tombstone filtering, the sweep core. The layer above tinyindex.
  store/        SQLite (rusqlite, WAL, bundled) schema + migrations + queries
  apub/         ActivityPub client: HTTP signatures (RFC 9421 + cavage-12), signed GET,
                server actor + WebFinger + NodeInfo (`apub::actor`), signed GET client with per-host
                state (`apub::fetch`: same-host redirects only, id/origin check, backoff; the
                binary persists `Fetcher::host_states` into `hosts`), object parsing
                (`apub::objects`: Note/Page/Article/Person/Group…), a host's nodeinfo
                `software.name` (`apub::nodeinfo`, through `Fetcher::fetch_document`: same
                limits, no id/origin check), signed delivery (`Fetcher::deliver`: POST under
                the same limits, no redirects), inbox signature and key-owner checks
                (`apub::inbox`), the outbound address policy and the shared HTTP client
                builder (`apub::net`, see Outbound requests below), consent evaluation
                (`apub::signatures`: framework-agnostic, operating on a method, a target URI,
                a header lookup and a status. Fixed component set; RFC 9421 signs ed25519 and
                rsa-v1_5-sha256 and also verifies rsa-pss-sha512; cavage-12 is RSA-SHA256 only,
                `hs2019` accepted on verify. Cavage verification requires a covered `date` or
                `(created)` and checks its skew.)
  fasp/         FASP general (registration, provider_info, activation, request signing/
                verification, content-digest) + data_sharing + account_search, trends and
                follow_recommendation handlers
  mwmbl_social/ binary: axum server, background workers, CLI, config
```

**Proposed stack** (the next session should confirm crate choices):

- tokio, axum, reqwest (rustls), rusqlite (bundled)
- ed25519-dalek, rsa (cavage-12 is usually RSA-SHA256 with the actor key), sha2, base64
- serde/serde_json, tracing, clap, figment or toml for config

For RFC 9421, look at the `httpsig` crate. If it doesn't fit, write a narrow implementation:
we only need a fixed component set.

**Process model:** a single process.

- An HTTP server serves the FASP endpoints, the ActivityPub actor and WebFinger, and the search
  API.
- Background tasks handle the fetch queue, index writes, revalidation, and
  subscription/backfill management.
- **All index writes go through one writer task**, fed by a channel. That avoids the fcntl page
  locks Mwmbl uses for multiple processes.
- **Across processes** the writer holds an exclusive lock on `{data_dir}/writer.lock`
  (`File::try_lock`); `serve` refuses to start if it is held. `ingest` and `forget` run
  in-process (own writer and fetcher) when the lock is free. When a server holds it they only
  write SQLite (`ingest` enqueues an `announce`; `forget` writes tombstone and domain-block
  rows), and the server's writer reloads tombstones and domain blocks from SQLite every 30
  seconds; the sweep then removes the copies. `search` and `stats` open the indexes read-only
  and need no lock. `follow` and `unfollow` need no lock either: they write `following` and
  deliver with their own fetcher and our actor's key, and the server's inbox reads `following`
  from SQLite on every delivery (nothing about it is cached in memory).
- Readers use positioned reads concurrently. If a read hits a page mid-write, it gets a
  `PageIoError::Unreadable` and retries. A sharded `RwLock` per page range is the alternative if
  retries prove noisy.

**Storage:** a data directory holding:

- `mwmbl-social.sqlite`
- `posts.tinyindex`
- `accounts.tinyindex`
- `keys/` (our actor RSA key and the per-registration Ed25519 keys)

The number of pages is configurable. Default to something small for self-hosters, such as
2^16 pages = 256 MiB per index file. Make it configurable and support growing the index by
rebuilding from the index itself: read every record from the old file and re-add it to a
larger one.

**Multi-tenancy:** design for many registered servers from day one, since our hosted instance
needs it. A self-hosted install is just one row. A search request's results are not limited to
the calling server's own data. A FASP pools what every connected server shares, and that pooling
is the point.

### Outbound requests

Every host we contact is chosen by someone else: a server being registered (and the FASP base
URL its nodeinfo names), announced and crawl-submitted URIs, followed actors, actor keys for
inbox signature checks, inboxes we deliver to. So no outbound request may reach a non-public
address (SSRF): loopback, private (RFC 1918), CGNAT, link-local (with the 169.254.169.254
metadata address), unspecified, multicast, documentation, benchmarking and reserved IPv4;
anything outside IPv6 global unicast, unique-local and link-local included; and IPv6 forms that
carry one of those IPv4 addresses (mapped, compatible, NAT64, 6to4). `apub::net` holds the one
policy (`is_public`, table-tested):

- Every HTTP client is built from `apub::net::client_builder`, which installs a resolver that
  uses the system resolver and drops non-public addresses, failing when none remain. The check
  is on the addresses the connection is made to, so DNS rebinding can't get between a check and
  the connect. Redirects to non-public IP literals are refused.
- `reqwest` connects to an IP-literal host without resolving it, so `apub::net::check_url`
  refuses those where URLs are accepted: `Fetcher` (every fetch and delivery), registration
  (the server URL, `--fasp-base-url` and the discovered FASP base URL, checked before
  `register` sends anything there) and the `forget` WebFinger lookup.
- A refused request is an error naming the host (`FetchError::NotPublic`, a 400 from the admin
  API); the fetcher starts no backoff for it, since nothing was sent.
- `allow_private_addresses` (default `false`) turns all of it off, for tests (the in-process
  fakes listen on 127.0.0.1) and the Docker dev setups (private networks). Like `allow_http`,
  it is never for production.
- An HTTP proxy from the environment (`HTTPS_PROXY`) resolves names itself, so the filter then
  sees only the proxy's address; enforce the policy at such a proxy.

### Data model (SQLite)

**Principle:** as in Mwmbl, the index is the only record of what is indexed. SQLite holds
operational state and records of explicit user and admin actions. It must not mirror the
index or grow with it: no rows per indexed document or account, and no term lists. Every table
below is bounded by the number of registrations and hosts, by a fixed time window, or by
explicit user and admin actions.

- **`registrations`**: id, the server's **FASP base URL** (`base_url`; this is what the client
  calls, and it is not always the server's own URL, e.g. Mastodon's is `<server>/api/fasp`),
  `serverId` (ours), `faspId` (theirs), our Ed25519 keypair, their public key, status
  (pending/active/revoked), created_at. A registration becomes `active` on the first request
  from that server that verifies; the spec gives us no approval callback. Ids are
  `AUTOINCREMENT`, never reused. `base_url` is unique, so registering a server again replaces
  a dead row for it (revoked, or a `pending` one with no `faspId` that is over 10 minutes old:
  an attempt a crash interrupted) in one transaction, cascading to its children; the new row
  has a new id, key and `serverId`. Only a live registration (active, or pending once the
  server has answered) blocks a new one. Migrations run with foreign keys off and are checked
  with `foreign_key_check` before they commit, as SQLite's table-rebuild procedure needs.
- **`capabilities`**: registration_id, capability id, version, enabled, updated_at.
- **`subscriptions`**: registration_id, remote subscription id, category, type, created_at.
- **`backfills`**: registration_id, remote id, category, state, more_available.
- **`fetch_queue`**: uri, kind hint, reason (announce/backfill/revalidate/crawl), not_before,
  attempts, last_error. It is bounded: deduplicate by URI and cap its length. When it is full,
  backfill and revalidation work is deferred, and announcements take priority.
- **`hosts`**: host, signature scheme that works (9421/cavage), backoff_until, blocked flag,
  nodeinfo `software.name` with its check time (for Lemmy consent and crawl submissions), and a
  cached robots.txt verdict with its fetch time (Stage 3).
- **`domain_blocks`**: admin-configured blocked domains. We neither fetch from these hosts nor
  return results from them.
- **`tombstones`**: object or actor URI, kind (object/author), created_at.
  - Holds deletes and opt-outs that couldn't be applied to the index immediately (see Deletion):
    - FASP delete events whose `uri:` lookup missed
    - author opt-outs
    - domain blocks
  - Deletes found by revalidation never need it.
  - It is mirrored in memory: loaded at startup and updated on each write. Search results and
    sweep records are checked against the in-memory set with a hash lookup.
  - Expect thousands of rows, not millions.
  - Rows are cleared once a full sweep that started after them has finished, so it holds at
    most about one sweep period of entries.
- **`follow_optins`**: actor URI, followed_at. Authors who opted in by following our server
  actor (Stage 3). A record of a user action, bounded by those actions like `forgotten` is by
  admin actions. A row goes when the author unfollows.
- **`following`**: actor URI, inbox, our `Follow` activity's id, state (pending/accepted),
  created_at (when the `Follow` was last sent). Actors our server actor follows: follow-backs of
  opt-ins, and communities and accounts an admin chose. Bounded by those actions. The id
  matches the `Accept` and is what our `Undo` names.
- **`forgotten`**: URIs an admin forgot. Permanent, and bounded by admin actions.
- **`sweep_state`**: the sweep's current page, and when the current and last sweeps started.
- **`subscriptions`** also holds one content `trends` subscription per `data_sharing`
  registration (Stage 4): bounded by registrations like the rest.

**Not in SQLite: the trend state** (Stage 4, `mwmbl_social::trends`). In memory only: post URI
→ hourly `trending` event counts for the last 168 hours (the longest `withinLastHours` the
spec requires), at most `trends_max_uris` URIs (default 5000), the least announced (then least
recently announced) evicted at the cap. So it is bounded by a fixed window and a cap, not by the
index; it holds URIs and counts, never content or term lists; and a restart only empties it.
It is not a record of what is indexed: it records URIs as announced (Mastodon announces trending
statuses whatever their visibility), and is never served without a posts-index lookup.

Never store non-public objects, anywhere.

### Index documents

We keep Mwmbl's **page format**: a zstd frame of a JSON array of tuples, best-first, with
trailing `null`s truncated. We use **our own item types** rather than Mwmbl's `Document`.
`Document` has one timestamp where we need two (published and checked), and no language or
author URI. It would also carry unused fields (`state`, `user_ids`, `source`) onto every page.

`tinyindex` is generic over the item type, like the Python `TinyIndex[T]` with its
`item_factory`:

```rust
trait IndexItem: Sized {
    const ITEM_FACTORY: &'static str; // written to metadata; open() refuses a mismatch
    const TERM_POSITION: usize;       // replaces the hard-coded TERM_INDEX = 4
    fn url(&self) -> &str;            // identity for dedupe/remove on a page
    fn score(&self) -> f64;
    fn to_tuple(&self) -> Vec<serde_json::Value>;
    fn from_tuple(values: Vec<serde_json::Value>) -> Result<Self, PageIoError>;
}
```

Mwmbl's `Document` becomes one implementation, which keeps the crate upstreamable. `has_term`
and `read_index_page` take the term position from the type. The trait also has
`fn term(&self) -> Option<&str>`, since `add` groups items by their term's page.

`SocialPost` and `SocialAccount` live in `social_index`, not `tinyindex`, so `tinyindex` keeps
no fediverse knowledge.

**`tinyindex` API decisions:**
- `TinyIndex<T>` is a cheap-to-clone read handle (`retrieve`, `read_page`, `pages`).
  `IndexWriter<T>` is not `Clone` and its mutators (`add`, `remove`, `write_page`,
  `reset_page`) take `&mut self`, so the single writer task is the only owner of one.
- Merging onto a page deduplicates by (`url`, `term`). `remove(url, terms)` drops every copy
  of `url` from those terms' pages, whatever term the copy carries.
- A page that won't decode is an error for the writer, never an empty page to merge onto.
  Readers retry it a few times, since the writer may be mid-write.
- The key scheme (tokeniser version plus `KeyConfig`, since both decide the keys) is an
  optional opaque `tokenizer` string in the metadata JSON. Opening refuses a mismatch. It is
  omitted when unset, so `Document` indexes keep the exact header Mwmbl's Python reads.

Tuple layouts, with required fields first and optional ones last so they can be truncated:

- **`SocialPost`** (`ITEM_FACTORY = "SocialPost"`):
  `[uri, author_uri, extract, score, term, published, checked, title?, language?]`
  - `uri`: the ActivityPub object id. This is the identity, and it is what FASP returns. The
    human-facing URL isn't stored; clients resolve the URI.
  - `author_uri`: the actor id. Author tombstones match on it.
  - `extract`: plain text of the content with HTML stripped. Includes the content warning text
    and hashtags. Truncated to about 300 chars.
  - `published` and `checked`: Unix seconds. `checked` is when we last fetched or confirmed the
    object, and the sweep uses it.
  - `title`: the `name` of an Article/Page, or a Lemmy post title.
  - `language`: BCP 47, from `contentMap` or the language field.
- **`SocialAccount`** (`ITEM_FACTORY = "SocialAccount"`):
  `[uri, acct, display_name, summary, score, term, checked, languages?]`
  - `acct`: `user@host`. `parse_actor` cuts `preferredUsername` to 64 chars
    (`MAX_USERNAME_CHARS`; Mastodon allows 30, `GoToSocial` 64), and the record cuts `acct` to
    324 chars (`MAX_ACCT_CHARS`: username, `@`, a 253-char host and a `:port`). Both cut on
    char boundaries, at parse time, so the stored `acct` is what keys are derived from.
  - `summary`: the bio as plain text, truncated.
  - `score`: from followers and activity.
  - `languages`: the posting languages we've observed, for `account_search`'s `language`
    filter.

If a field turns out to be needed later, add it at the end as optional. Old pages still decode.

Index keys for accounts include the username, the full `user@host` handle, the host, and the
display-name tokens, so handle searches work. Only the first 8 username tokens
(`MAX_USERNAME_TOKENS`), the first 8 host tokens (`MAX_HOST_TOKENS`) and the first
`KeyConfig::special` handle keys count, so a hostile actor can't be written to thousands of
pages (audit E2). These caps are part of the key scheme: the scheme id went to `social-v2` when
they were added, so an index built before them is refused and must be rebuilt.

**Every item is also indexed under its URI as a key**, written as the term `uri:<uri>`. That
makes it findable by URI alone, which is what deletion needs.

**Invariant:** an item's index keys must be a pure function of its stored fields and the
tokeniser version. For posts that means `title`, `extract` and `uri`; for accounts, `acct`,
`display_name`, `summary` and `uri`. Then re-tokenising a record read from a page gives exactly
the pages it was written to. Don't index terms from text we don't store. The tokeniser is
versioned:
- the version is recorded in the index metadata;
- a version change means rebuilding the index, or keeping the old tokeniser for deletes until
  the index has turned over.

**Scoring:**

- Pages keep documents best-first, so the index-time score decides what survives on a full page.
- For posts, use text quality × engagement × a recency factor computed at write time. Newer
  posts then naturally push out older ones.
- At query time, re-rank the retrieved candidates by term match (all query terms present,
  bigram hits) and, for posts, freshness.
- Port only the simple parts of Mwmbl's `rank.py` and keep the rest for later.

### Deletion (new index capability)

A FASP delete gives us only the object URI. By the time we hear of it, fetching the URI returns
404, 410 or a Tombstone, so we can't re-derive the terms from the source. Following Mwmbl's
approach, we find the document in the index and re-tokenise it.

**`delete(uri)`**, run in the single writer task:

1. Retrieve the page for the term `uri:<uri>`, and find the record whose `url` is the URI.
2. If it's there: re-tokenise its stored fields to get its keys. For each key's page, read it,
   filter the URI out, and rewrite it. That includes the `uri:` page itself.
3. If it isn't there: the `uri:` entry fell off a full page, or the document was never indexed.
   Add a `tombstones` row. Search results filter it out straight away, and the sweep removes
   any copies that remain.

**Lifecycle deletes and `forget` always add a tombstone, even when the lookup found the
record.** An edit whose `uri:` lookup missed leaves copies of the old version under keys the
new version doesn't have. A delete that finds the new version can't see those, so without the
tombstone the old text would stay searchable until the sweep revalidated it. The tombstone
filters it at once and the sweep removes it. Deletes found by revalidation don't need one: the
stale copies they can't see have an old `checked` and get revalidated themselves.

**Revalidation** hands back the stored record. If the object is live and consented, the writer
calls `replace(old, fresh)`: when the keys are unchanged, the add overwrites every copy in
place (merging dedupes by URI and term); otherwise the old copies are removed first.

Implemented in `social_index`: `SocialIndex::{add, find, delete, delete_record, upsert,
replace}`, `Tombstones` (objects, authors, domains with subdomains), and `sweep_page`. A seeded
property test (`social_index/tests/deletion.rs`) checks that deleted documents leave no copy on
any page, including on crowded indexes where `uri:` lookups miss.

**Sweep** (also how the weekly revalidation the spec requires is done):

- A background task walks every page of each index in a cycle, aiming to finish one pass in
  under 7 days. For example, 2^16 pages of 4 KiB is 256 MiB of sequential reads a week.
- For each record on a page:
  - If it matches a tombstone (by URI, or by author for author tombstones): delete it
    everywhere by re-tokenising the record.
  - If its `checked` is more than about 6 days old: queue a revalidation fetch. **The
    queue entry carries the stored record**, which is a few hundred bytes, and the queue is
    bounded. If the object is still live and consented, the record is rewritten with a fresh
    `checked`. If it's gone (404, 410, Tombstone, not public, or consent lost), the record
    is re-tokenised and removed directly, with no URI lookup or tombstone needed.
- A record shows up once per key it was indexed under, so the sweep deduplicates by URI. Keep a
  bounded in-memory set for the current page range, plus the queue's own URI deduplication.
- Records that dropped off every page are simply gone. That is the index's normal lossiness,
  and it's fine.
- Clear tombstones older than the start of the last completed sweep.
  - Tombstones apply to both indexes, so the cutoff is the older of the two indexes' last
    completed sweep start: a tombstone is cleared only once both indexes have finished a full
    pass that began after it was made. A pass that was already running when it was made does
    not count.
  - A page that fails to read is logged with its number and skipped (never reset), so one bad
    page doesn't stall the pass; its tombstones are applied on the next pass.

**What triggers a delete:**

- a lifecycle `delete` event
- an `update` event that turns out to fail consent
- revalidation returning 404 or 410, a Tombstone, or lost consent
- the account no longer being discoverable or indexable. This adds an *author* tombstone: its
  posts are filtered from results immediately and physically removed by the sweep. An author
  tombstone covers posts only. If the account is no longer discoverable, its own record is also
  deleted by URI; if it is still discoverable but not indexable, the account stays searchable.
- a domain block, applied by search-time filtering and the sweep
- an admin CLI command `mwmbl-social forget <uri|acct|domain>`. A forgotten URI goes in a
  permanent `forgotten` table as well as the tombstones, so it is never indexed again after the
  sweep clears its tombstone rows. A forgotten domain is a domain block, also permanent.

**Re-indexing clears an object tombstone.** If a post was made unlisted (removed and
tombstoned) and then public again, the fresh consented fetch clears its object tombstone.
Otherwise the sweep would delete the post that now has consent. Forgotten URIs and authors are
never re-indexed.

**Updates** delete the old record (found by URI lookup) and then add the new one, because the
terms may have changed. Updates and lifecycle events rewrite `checked`, which covers the
spec's rule that a change notification can stand in for a week's recheck.

**Duplicate announcements** of the same URI from several servers are cheap. Merging into a page
deduplicates by `url`, and the fetch queue deduplicates by URI.

### Tokenising

Mwmbl's whitespace split doesn't work for CJK text and keeps punctuation attached to words. Use
Unicode word segmentation (`unicode-segmentation`) with lowercasing and NFKC normalisation.

- **CJK:** emit character bigrams for CJK runs.
- **Social-specific keys:** hashtags (`#rust` → `#rust` and `rust`) and mentioned handles.
- **Key selection:** keep Mwmbl's approach (first N tokens + first N bigrams per field), but
  make N configurable.
- **Stopwords:** keep Mwmbl's English list, and make it per-language later.

Put this in its own crate so it can be offered upstream.

## Stages

Each stage ends with something that runs and has tests. Integration tests use a local Mastodon.
Set up a Docker Compose dev environment with Mastodon with FASP enabled, Postgres and Redis.
Doing this early is part of Stage 0's done criteria.

### Stage 0: foundation

1. Workspace skeleton, CI (fmt, clippy with the same deny list as `mwmbl_core`, tests), and the
   AGPL licence.
2. `tinyindex` crate:
   - Copy `index.rs` and `tests/index.rs`, then port the metadata, create and open code and
     fix the page-size bug.
   - Port the mmh3 key → page mapping with Python-generated test vectors.
   - Add `retrieve(term)`, `add(docs)` (merge with existing, sort by score, pack),
     `remove(url, terms)`, and a page iterator for the sweep.
   - Keep it free of fediverse knowledge. URI lookup, re-tokenising and tombstones live a
     layer up.
   - Make it generic over `IndexItem`, with `term` position and `item_factory` per type.
     Implement Mwmbl's `Document` to keep it compatible; `SocialPost` and `SocialAccount`
     go in `social_index`.
   - Add a compatibility test: a page written by Mwmbl's Python reads back identically as
     `Document`.
3. `tokenize` crate, with tests covering English, German, Japanese and hashtags.
4. `store` crate: schema and migrations (`rusqlite_migration` or hand-rolled).
5. `apub` crate:
   - Server actor (RSA key, `/actor`, WebFinger, `.well-known/nodeinfo` for ourselves).
   - Signed GET with RFC 9421 and cavage-12 fallback, per-host caching, per-host concurrency and
     rate limits, and 429 or Retry-After backoff.
   - Parse Note/Article/Page/Question and Person/Service/Group.
   - Consent evaluation as a pure function with thorough unit tests.
6. Ingestion pipeline: fetch queue worker → consent → tokenize → index writer. Delete by URI
   lookup and re-tokenising, with a tombstone fallback, and filter tombstones at search time.
7. The sweep (page walk, revalidation queueing, tombstone application and clearing), with a
   configurable cycle time so tests can run it quickly.
8. Search API: `GET /api/v1/search/posts?q=` and `/api/v1/search/accounts?q=`, returning JSON.
9. CLI subcommands:
   - `serve`
   - `ingest <uri>`, which queues and fetches one object or actor
   - `search <q>`
   - `forget <…>`
   - `stats`
10. Config file plus env overrides, with sensible defaults.

**Done when** `mwmbl-social ingest https://some.server/@user/123` against the local Mastodon
indexes the post (if consented), `search` finds it, and deleting it upstream then revalidating
removes it.

### Stage 1: FASP `general` + `data_sharing`

1. RFC 9530 content-digest and RFC 9421 Ed25519 sign/verify for FASP requests and responses,
   with clock-drift window checks. Bad signatures get 401.
2. `GET /provider_info`, with name, privacy policy URL(s) and contact taken from config.
3. Registration, initiated by us:
   - Self-hosted: `mwmbl-social register <server-url>`. It discovers `faspBaseUrl` via nodeinfo,
     POSTs `/registration`, stores the result, and prints the fingerprint and completion URL.
   - **Mastodon 4.7.5 does not put `faspBaseUrl` in its nodeinfo metadata**, though the spec
     says it MUST (checked against the e2e environment, 2026-11-15). Its FASP API lives at
     `/api/fasp`. So: use `metadata.faspBaseUrl` if present; otherwise, if nodeinfo's
     `software.name` is `mastodon`, use `<server-url>/api/fasp`; otherwise fail with a clear
     message. `register --fasp-base-url <url>` overrides discovery.
   - Hosted: no sign-up page here. Mwmbl's domain submission flow decides who may register
     and triggers `register` (decided 2026-11-16; see Non-goals).
4. Capability activation endpoints. Turning `data_sharing` on should:
   - create `content` and `account` lifecycle subscriptions;
   - start backfill for both categories (`maxCount` configurable), following continuations
     while `moreObjectsAvailable`.

   Turning it off deletes the registration's local subscription and backfill rows and makes no
   server calls: the capability is already disabled on the server's side, and calling a disabled
   capability's API is forbidden (principle 4), so we do not send `DELETE` for subscriptions.
   Turning it on again starts from scratch. Should a deactivated server's data be purged?
   Decide that; see open questions.
   - Mastodon activates `data_sharing` using only the major version we advertise
     (`POST /capabilities/data_sharing/0/activation`); we accept `0` and `0.1` and record
     the advertised `0.1`. Unknown capability or version gets a (signed) 404.
   - Mastodon returns subscription and backfill ids as JSON numbers, not strings as the spec
     shows; the client accepts both and stores strings. Announcements carry string ids.
   - Implemented in `crates/fasp`: `registration` (discovery and `/registration`), `http` (the
     axum router, request authentication and response signing, driven by the `FaspState`
     trait), `client` (`FaspClient`, which refuses a capability's API unless the store says it
     is enabled, checked on every call) and `data_sharing` (`on_enable`, `on_disable`,
     `on_backfill_announcement`, `resume_backfills`). Backfill continuation happens only when
     the binary says the fetch queue has room; otherwise `more_available` stays set in
     `backfills` and `resume_backfills` picks it up.
5. `POST /data_sharing/v0/announcements`: verify the signature and the registration, enqueue
   the URIs with priority (new/update > backfill > revalidate), and return 204 quickly.
   Announcements are dropped (still 204) when `data_sharing` is not enabled for the
   registration or the source id is not one of its subscriptions or backfills; `trending`
   events are accepted only from a `trends` subscription, and lifecycle events only from a
   `lifecycle` one (Stage 4 routes `trending` to the trend state); at most 1000 `http(s)` URIs are taken per request; a malformed body
   gets 422.
6. Tune the sweep's revalidation against real servers: spread the fetches and respect per-host
   budgets, so a weekly pass doesn't overload a big host. (Done: `mwmbl_social::pacing`.)
   - The worker reads up to 4 due candidates per free slot (at most 256) and passes over,
     leaving queued, any whose host already has the fetcher's `per_host_concurrency` in
     flight, so one host's items never fill the worker's slots waiting on the fetcher. Host
     is `apub::fetch::uri_host_key` (lowercase `host[:port]`, as the fetcher keys hosts);
     a URI without one is started as before.
   - Revalidations to one host start at most once per `revalidate_interval_per_host_ms`
     (default 1000). The worker keeps an in-memory `host -> next start, next free slot` map.
     A due revalidation whose host is not ready is left queued if the slot is under a second
     away (one per host per pass: `not_before` is whole seconds), and otherwise moved with
     `Store::defer` to the next free slot, which then advances one interval. `defer` does not
     touch `attempts` or `last_error`, and only moves rows still queued as revalidations.
     So a host's backlog is laid out in the queue, not polled, and holds no worker slot.
     Announcements and backfill are not budgeted. Hosts whose slots have all passed are
     dropped from the map once it tracks over 10 000. Deferrals are counted in
     `mwmbl_social_revalidations_deferred_total` and logged at debug with the host.
   - `Worker::drain` (tests, one-off runs) is not paced.
7. Accounts: when an account turns off `discoverable` or `indexable`, add an author tombstone
   (see Deletion).
8. Observability: tracing, a `/metrics` endpoint (Prometheus) for queue depth, fetch errors,
   index docs and per-host status, plus `stats`.

**Wiring in `mwmbl_social` (done):**
- The FASP router is mounted on the main listener at the root of our base URL, but only for
  `/provider_info`, `/capabilities/{*rest}` and `/data_sharing/{*rest}` (`fasp_app::FASP_ROUTES`).
  The `fasp` router authenticates every path it sees, so it must not be the fallback: everything
  else, including the 404, stays with the existing routes. A new FASP capability adds its prefix
  to that list.
- `fasp_app::FaspApp` is the `FaspState` (own SQLite connection behind a `Mutex<Store>`).
  Announcements enqueue their URIs in `spawn_blocking` before the 204: kind `Object` for
  `content` and `Actor` for `account`; reason `Announce` for lifecycle events and `Backfill` for
  backfill sources. For backfills `on_backfill_announcement` runs on a spawned task with
  `queue_has_room` = queue below 80% of `queue_cap`. Activation spawns `on_enable`
  (`backfill_max_count`); deactivation runs `on_disable` (local rows only). Mastodon keeps its
  subscriptions when a capability is disabled (seen end to end: a disable/enable left it
  announcing to four), so `on_disable` moves ours to `orphaned_subscriptions`, and the next
  `on_enable`, when calls are allowed again, deletes them on the server before subscribing.
  Re-reading `provider_info` (re-approving) resets Mastodon's capability flags, which disables
  everything.
- A background task, every 60 s, takes each live registration with `data_sharing` enabled:
  if a subscription or backfill is missing (an earlier `on_enable` failed, say because the
  server was unreachable) it retries `on_enable`; otherwise it runs `resume_backfills` while the
  queue is below 50% of the cap. One task per registration at a time; failures are logged with
  the host.
- CLI: `register <server-url> [--fasp-base-url URL]` (prints fingerprint, approval link, next
  step; SQLite and outbound HTTP only, no writer lock; the URL must be `https`, `http` only with
  `allow_http`, without user info), `registrations` (JSON with our key's fingerprint, never
  keys),
  `unregister <id>` (marks revoked, disables its capabilities, deletes its subscription and
  backfill rows; no remote call; its requests get 401). `register` again after `unregister`
  replaces the revoked registration (see `registrations` above). `stats` reports registrations
  by status and enabled capabilities.
- `/metrics` (Prometheus text, hand-written) is served on the main listener only when the
  `metrics` config is true. It is unauthenticated, so firewall it or proxy it to an internal
  address. It reports queue depth (total and by reason), tombstones by kind, domain blocks,
  registrations by status, enabled capabilities, fetch outcomes (`found`, `gone`, `refused`,
  `rate_limited`, `backoff`, `error`; counters in the worker), revalidations deferred by the
  per-host budget, and index sizes. Per-host status
  is not in it yet.
- New config (each with an env override): `privacy_policy_language` (default `en`),
  `fediverse_account`, `backfill_max_count` (default 100), `metrics` (default false),
  `revalidate_interval_per_host_ms` (default 1000; item 6).
- Account opt-outs (item 7) go through `decide`'s author tombstones. A fetched actor document
  now also replaces the worker's cached copy, so a post processed after an opt-out announcement
  cannot be judged against a stale `indexable: true` (the cache lives for an hour).

**Done when** the local Mastodon registers with us, enables data_sharing and backfills. New,
edited and deleted public posts from opted-in accounts show up in or vanish from `search`.
Unlisted posts and non-indexable accounts never appear.

**Status (2026-11-16): met against Mastodon 4.7.5** in dev/mastodon, with `AUTHORIZED_FETCH`:
registration and approval (Mastodon verified our signed `provider_info`), `data_sharing`
activation (2 subscriptions, 2 backfills), backfill indexing alice's public post but not her
unlisted one nor non-indexable bob's posts. A new post appears; an edit applies after the
4-minute delay; a delete removes it and leaves a tombstone; alice turning off `indexable`
removes her posts and keeps her account. Item 6 is done: the worker passes over hosts at
their concurrency limit and lays revalidations out one per host per interval (1 s default). The
hosted sign-up page was dropped: Mwmbl handles it.

### Stage 2: FASP `account_search`

1. `GET /account_search/v0/search` with `term`, `limit` and `language` (RFC 4647 basic
   filtering against the account languages we've observed). It returns actor URIs ranked by
   relevance, with `Link: rel="next"` pagination. Missing `term` gets 422.
2. Relevance:
   - exact handle or host match first
   - then display-name and username term matches
   - then summary matches, weighted by followers and activity
3. Advertise it in `provider_info` and handle its activation.

**Status (items 1-3):** implemented. `fasp::http` serves `GET /account_search/v0/search`: `term`
required (missing or empty is 422), `limit` default 20, capped at 100 (0, negative or
non-numeric is 422), optional `language`, plus our own `offset` for pagination. A `Link:
rel="next"` header is sent only when more results follow. A registration that has not enabled
`account_search` gets 404. It is advertised in `provider_info` and activated at `/0/` or `/0.1/`
without remote calls. The binary's `FaspApp` runs `search_accounts` on `spawn_blocking` over the
writer's accounts reader and shared tombstones, pooled across all registrations, and pages the
ranked list by `offset`/`limit`. It returns actor URIs only. Open question 2 is settled: Mastodon
sends `term` and `limit` only and ignores `Link`.

**Done when** searching for accounts in the local Mastodon's UI returns results from us. This is
the first point where an admin sees value, so after it: write install docs, publish a Docker
image, and try a first external deployment.

**Status (2026-11-16), end to end against Mastodon 4.7.5:** after enabling `account_search`,
Mastodon's own `Fasp::Request` (which verifies our response's digest and signature) calls
`/account_search/v0/search?term=alice&limit=10` and gets alice's actor URI. What isn't shown
yet is the UI payoff: Mastodon only adds accounts it doesn't already know, and every account
in the e2e index came from that same Mastodon. Showing it needs a second server whose accounts
reach us (e.g. a second Mastodon in dev/mastodon sharing data with us), or a real deployment.

**Docker image:** published to GHCR as Mwmbl does (`.github/workflows/build.yml`, a copy of
Mwmbl's): every push to `main` builds and pushes `ghcr.io/mwmbl/social:main` and
`:sha-<commit>`; same-repo pull requests push `:pr-<n>`. Install docs are in `README.md`. A
first external deployment needs a server admin and is left to the human.

### Stage 3: Lemmy and follow opt-in

Lemmy doesn't speak FASP, so content arrives two ways, both through the ActivityPub objects
(never the REST API, which Lemmy's robots.txt disallows under `/api`):

- **Follows (opt-in, fresh):** instance admins opt in, and we follow their communities.
- **Crawl submissions (coverage):** crawling is **not** done in this repo. Mwmbl's distributed
  crawler discovers Lemmy objects (walking community outboxes, scheduling, robots.txt and
  `Crawl-delay`) and submits their **URIs**. We fetch and check each object ourselves.

**Why Lemmy can be crawled at all:** Lemmy is a public forum. lemmy-ui serves the same
robots.txt on every instance, which disallows only UI and private paths (`/login`, `/settings`,
`/search`, `/modlog`, `/api`, …) and allows `/post/`, `/comment/` and `/c/`, with
`Crawl-delay: 60`. There is no `noindex` and no per-user or per-community search opt-out
(checked in lemmy-ui and lemmy, 2026-11-16). That is lemmy-ui `main`; 0.19.13 disallows
`/search/` (with the slash) and `/inbox` but not `/api` or `/signup`, and indents every line
after the first (a template literal), which our parser handles. That robots.txt is the admin's indexing consent.
This does not extend to Mastodon or other microblogging software, whose norms and consent flags
say no.

**Why URIs, not JSON:** the crawler runs on volunteers' machines, so a submitted object could
be forged: a faked `to: Public`, a post attributed to someone who never wrote it, or text
stuffed in to game ranking. Author consent would also need a forgeable actor document. Fetching
from the origin ourselves means nothing new is trusted.

1. Make the server actor able to follow `Group` actors (communities): accept inbox
   deliveries, verify incoming HTTP signatures, and handle
   Create/Update/Delete/Remove/Announce/Undo.
2. Admins choose which opted-in communities (or accounts) to follow: the `follow` command or
   the `follow` config option (built, see below).
3. Crawl submission endpoint (built, see below): an authenticated `POST` taking a batch of
   object URIs, which go on the existing fetch queue (reason `crawl`, lowest priority). Before
   fetching, the worker checks:
   - the host's nodeinfo `software.name` is `lemmy` (otherwise the URI is dropped and logged);
   - the host's robots.txt (cached in `hosts`, refreshed daily) allows the path for our
     User-Agent. If it disallows us, drop the URI;
   - the existing per-host rate limit and backoff, plus the host's `Crawl-delay`.
   The User-Agent names Mwmbl Social and links to a page about it.
4. Map Lemmy `Page` posts and comments to index documents. The post title becomes the document
   title, which makes these good Mwmbl candidates.
5. Consent (decided 2026-11-16), still a pure function with table-driven cases:
   - The object must be addressed `to:` the public collection, as everywhere. This excludes
     private and local-only communities.
   - The rule is chosen by the **author's** host, not the community's. If the author's host
     runs Lemmy, missing `discoverable`/`indexable` count as yes, but an explicit `false`
     still means no. Any other author (e.g. a Mastodon user replying in a Lemmy thread) needs
     the usual FEP-5feb flags.
6. Moderator removals: a `Remove` (or a community or instance removal seen on revalidation)
   tombstones like a delete. Crawled content gets no lifecycle events, so the sweep is the only
   path for its removals.
7. **Follow opt-in for authors on software without FASP** (decided 2026-11-16). GoToSocial is
   the motivating case: it doesn't speak FASP, and its default robots.txt and `X-Robots-Tag:
   noindex` rule out crawling. A server admin can't opt their users in, since consent is per
   author. So individual users opt in:
   - A user follows our server actor. We record them in `follow_optins` and follow them back.
     Their public posts then arrive in our inbox and go on the fetch queue as URIs, fetched from
     the origin like everything else. On follow, backfill from their outbox if it is served.
   - The actor's profile says plainly that following it means "index my public posts", and
     that unfollowing removes them.
   - Consent: following us counts as opt-in **only for flags the actor doesn't carry**. An
     explicit `discoverable: false` or `indexable: false` still wins. Mastodon users already
     have the flags, so a follow changes nothing for them. The consent function stays pure,
     taking "follows us" as an input.
   - Unfollow (`Undo` `Follow`): delete the row, add an author tombstone, and unfollow back.
     Revalidation rechecks the follow like any other consent input.
   - This works for any software without FEP-5feb, not only GoToSocial. Getting FASP into
     GoToSocial is still the better long-term route; raise it with its developers once the
     Mastodon integration can serve as a reference.

**Done when** following a community on a local Lemmy indexes its new posts and comments, and
removes them on delete and mod removal; and submitting URIs for a fake Lemmy host indexes
them, except for paths its robots.txt disallows and replies from non-indexable authors; and a
fake actor without `indexable` that follows us gets its public posts indexed, which are removed
when it unfollows, while one with `indexable: false` stays out.

**Status (2026-11-16): the consent rules of items 5 and 7 are live in the pipeline.** The inbox,
follow opt-in, admin follows, outbox backfill and crawl submissions with robots.txt are built
(below).
- `decide_post`/`decide_account` take the author's `ConsentContext` and use the `_in`
  evaluations. The author tombstone follows `consent::posts_consented` (the effective
  `indexable`), not the parsed flag, so a flagless Lemmy author or follower isn't hidden.
- The worker (`mwmbl_social::context`) works out the context on every fetch of an actor (a
  post's author, or the actor itself), never cached with the author document, so revalidation
  sees an unfollow or a software change. `follows_us` reads `follow_optins` (migration 4).
  `author_on_lemmy` is the nodeinfo `software.name` of the **actor's** host (the `hosts`
  `software` column in the Data model). Both are looked up **only when the actor lacks
  `discoverable` or `indexable`**, so Mastodon authors cost no extra fetch or query.
- Software cache: in memory in front of `hosts.software`/`software_checked_at`. A name is
  trusted for 1 day. A failed lookup for a host never identified is recorded as
  `software = NULL` and retried after 1 hour, counting as not Lemmy meanwhile; one for a host
  whose software was known keeps that name and is retried after 1 hour, so a nodeinfo outage
  during revalidation doesn't remove a Lemmy host's content. The lookup takes the highest nodeinfo 2.x link (shared with
  FASP registration discovery), which must be on the host's own origin.
- Lemmy objects: `language` `{"identifier": …}` is mapped when there is no `contentMap` (`und`
  is none); `Page.name` is the title; comments are `Note`s with `inReplyTo`; a community `Group`
  parses as an account. `stats` reports the number of follow opt-ins.

**Inbox (items 1 and 7), `mwmbl_social::inbox_app`:** `POST /actor/inbox` and the shared
`POST /inbox` (advertised as `endpoints.sharedInbox`) on the main listener.
- Checked before answering: body at most 256 KiB (413), `application/activity+json` or
  `ld+json` (415), a JSON object with `actor` and `type` (400). A sender on a blocked domain or
  forgotten by an admin gets 202 and is ignored, with nothing fetched.
- Signature (`apub::inbox`): RFC 9421 covering `@method`, `@target-uri` (or `@authority` and
  `@path`) and `content-digest`, or cavage-12 covering `(request-target)`, `host`, `digest` and
  `date` or `(created)`; the digest must match the body and the time be within an hour. The
  target URI is `base_url` plus the request path. The `keyId` must be on the actor's origin
  (otherwise it is not fetched). The key is fetched with a signed `GET`, found in the actor's
  `publicKey` (object or array) or a key document with an `owner`, and its owner must be the
  activity's `actor`, on the key's own origin. Otherwise 401. Keys are cached in memory for an
  hour; a cached key that fails is refetched once (rotation). A `Delete` of an actor by itself
  whose key (`<actor>#…`) answers 404/410 is taken as the account's deletion, since its key is
  gone with it.
- Verified deliveries get 202 and are handled on a spawned task. The JSON is never stored,
  indexed or logged: `activities::handle` (pure, table-tested) turns it into effects, which the
  binary applies. Objects are queued (kind from the embedded type, reason `announce`, an
  `Update` 4 minutes out) and fetched from origin; removals go through the writer as
  `RemovePost`/`RemoveAccount` with a tombstone and `AddAuthorTombstone`.
- Effects: `Follow` of us records the opt-in, delivers an `Accept` (object: the `Follow`,
  rebuilt from its id, actor and object), follows back (a `following` row, pending until the
  `Accept`) and queues the actor. `Undo{Follow}` of us (embedded, or by id from an opted-in
  actor) drops the opt-in, adds an author tombstone, delivers `Undo{Follow}` for our follow and
  drops the row, then queues the actor again so one whose own flags consent is shown again.
  `Accept`/`Reject` of our `Follow` (by id, or embedded from us to them) marks it accepted or
  drops it. `Create`/`Update` from a followed actor queue the object if it is on the actor's
  origin or the actor is a followed `Group`. `Announce` from a followed actor unwraps one
  embedded `Create`/`Update`/`Delete`/`Remove`/`Undo`/`Block`, handled as the community's;
  boosts are ignored. `Delete` from any verified actor of an object on its origin (or from a
  followed `Group`) removes it; of the actor itself removes the account. `Remove` from a
  followed `Group` with `target` the group removes the object (moderators-collection targets
  are ignored). `Block` with `removeData: true` from a followed `Group` or the blocked user's
  own instance adds an author tombstone. `Undo{Delete|Remove}` from a followed actor queues the
  object again. The rest is ignored.
- A sender's type (is it a `Group`) and inbox come from the key fetch when the key document is
  the actor, else from a fetch of the actor, cached for an hour. Only an inbox on the actor's
  origin is delivered to.
- Delivery (`mwmbl_social::delivery`, `Fetcher::deliver`): signed `POST` with
  `content-digest` and `Digest`, under the fetcher's per-host limits, backoff and scheme cache
  (cavage on 401/403). A bounded in-memory task: at most 1024 pending (more are dropped), 8 at
  once, retried after 30 s, 5 min and 30 min (or the host's backoff), then given up. **Not
  persistent:** pending deliveries are lost on shutdown. A lost `Accept`, `Follow` or `Undo`
  is acceptable; the next follow or unfollow sends it again.
- Our ids: `{actor}/follows/<ms>-<n>`, `{actor}/accepts/<ms>-<n>`, and `<follow id>/undo`.
  They are not dereferenceable.
- Our actor says in its `summary` that following it means "index my public posts" and that
  unfollowing removes them; `manuallyApprovesFollowers` is false; it stays neither
  `discoverable` nor `indexable`, and anything it sends itself is ignored.
- `stats` reports `following` by state.

**Admin follows (item 2), `mwmbl_social::follows`:**
- `follow <target>`, `unfollow <target>` and `following` (JSON). A target is an actor URI, or
  `!name@host`, `@name@host` or `name@host`, resolved by `WebFinger` (`acct:name@host`; Lemmy
  communities answer it) with a signed `GET` through the fetcher. `follow` refuses blocked
  domains and forgotten URIs, fetches the actor, requires an actor type (`Group`, `Person`,
  `Service`, `Application`, `Organization`) with an inbox on its own origin, records a pending
  row and delivers our `Follow` once, directly (not through the delivery task, which only a
  server runs). A refused delivery drops the row; a transient failure keeps it for the re-send.
  Ids from the CLI are `{actor}/follows/<ms>-<pid>`. `follow` of an actor whose follow is still
  pending sends it again at once, under a new id (Lemmy drops the first `Accept` to a new
  instance, below); of an accepted one, it does nothing.
- `unfollow` delivers `Undo{Follow}` (once; the row goes even if it fails) and removes nothing
  from the index: a community's posts are consented by their authors and host, not by our
  follow, and an account's own flags or opt-in decide for it.
- Config: `follow = ["!rust@lemmy.example", …]` (env `MWMBL_SOCIAL_FOLLOW`, comma-separated;
  each entry is parsed at load). `serve` follows any not in `following` at start-up and every
  6 hours, resolving each handle once per process. Every follow still pending a day after it
  was sent is re-sent (new id, recorded in the row, since Lemmy rejects an activity id it has
  seen) and its `created_at` reset, so at most once a day; this covers
  follow-backs and CLI follows too. Removing a target does not unfollow; on start-up each
  followed actor that is neither configured nor an opt-in is logged with a hint to `unfollow`.

**Outbox backfill, `mwmbl_social::outbox`:** when a pending follow becomes accepted (admin follows
and opt-in follow-backs alike; a repeated `Accept` does nothing), a spawned task reads the
actor's `outbox` (on its own origin): the collection's inline items, or `first`, then `next`,
each page on the actor's origin, at most `outbox_backfill_pages` pages (default 5; 0 turns it
off). From each page it takes the ids of `Note`/`Page`/`Article`/`Question` objects, `Create`s
of them by the actor, and `Announce{Create}` (a Lemmy community relaying a post); boosts
(`Announce` of an object or URI) are skipped. Ids must be on the actor's origin, or anywhere
for a `Group`, as in the inbox. They are queued as kind object, reason `backfill`; it stops
when the queue defers one (full). An outbox that is not served (401/403/404) or a page that
fails is logged and ends the backfill. Only ids are taken; the outbox JSON is never stored or
indexed. Not resumed after a restart.

**Crawl submissions (item 3), `mwmbl_social::crawl_app` and `mwmbl_social::crawl`:**
- `POST /api/v1/crawl/submissions`, body exactly `{"uris": [...]}` (unknown fields, object JSON,
  or more than 1000 URIs: 422; over 1 MiB: 413). `Authorization: Bearer` with one of
  `crawl_tokens` (env `MWMBL_SOCIAL_CRAWL_TOKENS`, comma-separated; each at least 16
  characters, no whitespace; redacted in `Debug`). No tokens: 404. A bad token: 401. Tokens are
  compared by SHA-256 digest in constant time.
- 202 with `{accepted, deferred, rejected}`. Duplicates count once. Rejected: not `https`
  (`http` with `allow_http`), not an absolute URL with a host, user info, longer than 2048, a
  blocked domain, or forgotten. Queued as kind `unknown`, reason `crawl` (priority 3, after
  revalidate). Crawl items are admitted only while the queue is under 50% of `queue_cap`;
  beyond that they are deferred (counted, not queued), and an announce evicts them first. An
  already-queued URI counts as accepted and keeps its reason (a revalidation is not turned
  into a crawl).
- **robots.txt governs crawl-submitted fetches only.** Fetches for FASP announcements and
  backfill, follows, the inbox and revalidation (of crawled content too) are federation, not
  crawling, and are not checked. Nodeinfo and robots.txt fetches are not checked either.
- Worker checks before fetching a crawl item, cheapest first: the host's nodeinfo `software` is
  `lemmy` (the consent software cache; a failed lookup is not Lemmy), else the item is
  completed and logged at info with URI and host; then robots.txt allows the URI's path and
  query, else dropped likewise. robots.txt being unavailable reschedules the item for its
  retry time instead (an hour), counting an attempt.
- robots.txt (`mwmbl_social::robots`, RFC 9309, as Mwmbl's crawler): fetched with
  `Fetcher::fetch_text` (per-host limits and backoff, same-host redirects, first 512 KiB, lossy
  UTF-8), one fetch per host at a time. 2xx: the rules; 4xx except 429: allow all; 429, 5xx,
  backoff or any failure: disallow all, retried after 1 h. Rules are trusted for a day. Groups
  match our product token (`fetch::user_agent` up to the first `/`, `mwmbl-social`),
  case-insensitively and on the token only; every group naming us is merged, else every `*`
  group, else no rules. Longest pattern wins, `Allow` wins ties, `*` and a final `$` are
  understood, paths compare percent-encoded. `Crawl-delay` rounds up, the largest of the merged
  groups', capped at 300 s. Only the rules for us are cached, written back as one `User-agent:
  *` group in `hosts.robots_txt` (`NULL` with a check time: unavailable), in memory in front.
- Each same-host redirect target is checked against the same rules before it is requested
  (`Fetcher::fetch_guarded`), and so is the author's URI before the author fetch; either one
  disallowed drops the item.
- Pacing: crawl items to one host start at most once per `max(Crawl-delay,
  revalidate_interval_per_host_ms)`, budgeted apart from revalidations, by the same pacer
  (held or deferred to a slot). A host's `Crawl-delay` is known once its robots.txt is in
  memory; until then the revalidation interval applies.
- Decide: a crawl item is decided exactly as a backfill (table test), with the author judged
  by its own host's consent rule. An author off the object's origin is not fetched, so such a
  reply is not indexed.
- `/metrics`: `mwmbl_social_queue_depth{reason="crawl"}`,
  `mwmbl_social_crawl_skipped_total{reason="not_lemmy"|"robots_disallowed"|"robots_unavailable"}`
  and `mwmbl_social_crawl_deferred_total`.

**Status (2026-11-16), end to end against Lemmy 0.19.13** (`dessalines/lemmy:0.19.13`, backend
only, Postgres 16, in dev/lemmy): the done-when's Lemmy half passes.
- `follow '!rust@lemmy.test'`: WebFinger, `Follow`, Lemmy's `Accept` (about 25 s later, Lemmy
  delivers in batches), then the outbox backfill queued both existing posts and `search posts`
  found them. A new post and a new comment arrived as `Announce{Create}` and were indexed.
  `unfollow` was accepted too.
- carol deleting her post, an admin's `/api/v3/post/remove` and `/api/v3/comment/remove` each
  arrived as `Announce{Delete}`, and each removed the document and left an object tombstone.
  Lemmy then serves the post as a `Tombstone` with 410.
- Crawl submissions: a post and a comment in a community we don't follow were indexed; `/modlog`
  and `/search/?q=…` were dropped for robots.txt; the items were fetched 60 s apart
  (`Crawl-delay`).
- Item 7 (follow opt-in) was not re-run here; the integration tests cover it.

Fixed in our code (each with a test): a re-sent `Follow` now gets a new id, and `follow` of a
pending actor re-sends it (see the next list for why). Our actor was an `Application`, and Lemmy parses a
follower only as `Person`, `Service` or `Organization`, answering 400 ("unknown variant"), so it
is now a `Service`. Lemmy answers an RFC 9421 signature with 400 (it only parses cavage-12),
not 401, and since it ignores signatures on `GET`s a fetch had cached it as RFC 9421, so the
`Follow` never fell back: a 400 is now retried once with the other scheme and never caches a
scheme. Captured 0.19.13 objects and the community outbox are fixtures in
`crates/apub/tests/consent/lemmy019_*.json`.

Lemmy 0.19 behaviour found end to end:
- **The first `Accept` to a new instance is lost.** Lemmy creates a delivery worker for an
  instance only on its next minutely refresh, and a new worker starts after the newest
  activity, so the `Accept` of the first `Follow` from a server Lemmy has never seen is never
  sent. Following again (a new id) gets an `Accept` about a minute later. Lemmy also answers
  400 to an activity id it has already received.
- The community outbox is one `OrderedCollection` with inline `orderedItems`, each an
  `Announce{Create{Page}}`, and no paging. It holds posts only, the newest 50
  (`FETCH_LIMIT_MAX`), so **comments written before we follow are not backfilled**; only
  crawl submissions can reach them.
- Moderator and admin removals are `Delete` (with the reason in `summary`) announced by the
  community, not `Remove`; `Remove` is only for mods and featured posts.
- Every new post is also announced directly (`Announce` of the `Page`, for Mastodon), which we
  ignore as a boost; the `Announce{Create}` is what queues it.
- Every `Page` carries a `Hashtag` with the community's name (`#rust`, href the post), so the
  community name is indexed as a hashtag.
- A comment's `to` is only the public collection, its `cc` the community and the author; a
  person has no `discoverable` or `indexable`, so the Lemmy consent rule decides.
- **Comments on a removed post stay served** (200) and so stay indexed: Lemmy sends no delete
  for them and revalidation finds them. Decided 2026-11-17: index whatever Lemmy serves, so
  these comments stay indexed; a removal applies to the removed object only.
- `GET`s are never required to be signed: `federation_signed_fetch` only makes Lemmy sign its
  own fetches.
- Only cavage-12 signatures are accepted on `POST` (400 otherwise). Lemmy signs its deliveries
  with cavage over `(request-target)`, `host`, `date` and `digest`, which we verify.
- Lemmy's HTTP client trusts only its compiled-in Mozilla roots (rustls with `webpki-roots`;
  `SSL_CERT_FILE` is ignored), and outside debug mode it refuses any URL whose host resolves
  to a private, loopback or link-local address. dev/lemmy works around both (see its README).
- The backend serves ActivityPub JSON for `/post/…` and `/comment/…` whatever the `Accept`;
  a real deployment's proxy sends only `Accept: application/…` to it, which ours matches.

### Stage 4: rest of FASP discovery

1. `trends` (content). We can use the `trends` subscription type of data_sharing as input.
2. `follow_recommendation`.
3. Take part in drafting the status/post search spec, using our post index as a reference
   implementation. **Out of scope for now** (decided 2026-11-17): nothing is posted on the spec
   repository, Mastodon or anywhere else; notes about it stay in this file.

**Spec facts** (`discovery/trends/v0.1`, `discovery/follow_recommendation/v0.1`, data_sharing,
checked 2026-11-17):
- `trends`: `GET /trends/v0/content`, `/trends/v0/hashtags`, `/trends/v0/links`, all with
  optional `withinLastHours` (positive integer, default 24; a FASP MUST support up to 168),
  `maxCount` (positive integer, default 20) and `language` (RFC 4647 basic filtering). 200 with
  `{"content": [{uri, rank}]}`, `{"hashtags": [{name, rank, examples}]}` or `{"links": [{url,
  rank, examples}]}`, sorted by rank descending. `rank` is 1..=100 (100 = most trending) and
  MUST be comparable between instances of the same software, so their results merge by rank.
  How trends are computed is up to the FASP, which SHOULD document it. No error code is
  specified for bad options.
- data_sharing `trends` subscriptions: `category` must be `content`; optional `threshold`
  `{timeframe (minutes, 15), shares (3), likes (3), replies (3)}`. Announcements carry
  `eventType: trending`.
- `follow_recommendation`: `GET /follow_recommendation/v0/accounts?accountUri=&language=`,
  200 with a JSON array of actor URIs; a missing `accountUri` is 422. The server filters out
  accounts its user already follows or blocks. The account URI may identify the person asking.

**What Mastodon `main` does** (2026-11-17):
- It never calls `trends`. It only *sends* `trending` events (`Fasp::AnnounceTrendWorker`): on
  each favourite, boost or reply of a status by an indexable author, for every active content
  `trends` subscription whose threshold is met (count of that interaction kind within
  `timeframe` ≥ its threshold), one announcement per interaction, one URI each. It checks the
  author's `indexable` but **not the status's visibility**. It stores the threshold in columns
  it compares against, defaulting missing keys to the spec's values only when a `threshold`
  object is sent, so we always send all four.
- It calls `follow_recommendation` from `GET /api/v2/suggestions` (an async refresh,
  `Fasp::FollowRecommendationWorker`): `GET /follow_recommendation/v0/accounts?accountUri=<the
  user's URI>`, no `language`, read as a JSON array; each URI not already known is fetched and
  stored as a recommendation for a day. Known accounts are skipped, so only remote,
  not-yet-seen accounts show up.

**Status (items 1 and 2): implemented** (not yet checked end to end; Mastodon does not call
`trends`).
- `fasp`: `EventType::Trending`; `on_enable(.., trends: Option<TrendsThreshold>)` also creates
  the content `trends` subscription (all four threshold keys, `maxBatchSize` 20). A `trending`
  event is accepted only from a `trends` subscription, lifecycle events only from a
  `lifecycle` one; a `trending` account event is dropped. `fasp::http` serves the three trends
  routes and `follow_recommendation` through `FaspState::{trending_content, trending_hashtags,
  trending_links, follow_recommendations}` (links default to an empty list). Both need their
  capability enabled (else a signed 404), checked before the options. `withinLastHours` and
  `maxCount` must be positive integers (else 422, like `account_search`); `withinLastHours`
  is clamped to 168 and `maxCount` capped at 100. A missing or blank `accountUri` is 422. The
  account URI is never logged.
- Advertised in `provider_info` (`trends` 0.1, `follow_recommendation` 0.1), activated at `/0/`
  or `/0.1/` with no remote call. `FASP_ROUTES` has `/trends/{*rest}` and
  `/follow_recommendation/{*rest}`.
- Binary: with `data_sharing` on, `on_enable` subscribes to trends with
  `trends_timeframe_minutes`/`trends_shares`/`trends_likes`/`trends_replies` (defaults 15, 3,
  3, 3); the 60 s maintenance adds the subscription to registrations that predate it. Each
  `trending` URI counts one event in the trend state (pooled across registrations); one not
  found by `uri:` lookup is queued as kind object, reason **backfill** (so a full queue defers
  it instead of displacing lifecycle work), fetched from its origin and judged for consent.
- **How trends are computed** (documented for admins, as the spec asks): a post's score is the
  number of `trending` events servers sent for it within `withinLastHours` (hour buckets, the
  current hour included). Candidates are taken best first (ties: most recently announced) and
  each looked up in the posts index (at most 500 lookups per request); one that is missing
  (never fetched, refused for consent, or off its page) or tombstoned is skipped, so trends
  never surface anything we could not index. With `language`, a post must carry a matching
  language (posts without one are left out, unlike `account_search`'s accounts). `rank` is
  `1 + 20·log2(1 + score)` in integers, capped at 100 (1 event: 21, 3: 41, 31+: 100): an
  absolute scale, so instances merge. Hashtags: those in the stored title and extract of the
  same trending posts, each scored by the sum of its posts' events, with up to 3 of them as
  examples. Links: always empty, since extracts are stripped of HTML and we keep no link URLs.
- **Follow recommendations:** if the asker's account is in our accounts index (so it is
  discoverable) and not tombstoned, accounts found under the keys of its stored display name
  and bio (never its handle, host or URI), ranked by shared keys and score
  (`social_index::related_accounts`); then authors of the week's trending posts, if their
  account is indexed and not tombstoned. Never the asker; `language` filters as in
  `account_search`; at most 20. An unknown asker gets trending authors only. Nothing is
  fetched for the request.
- Config: `trends_timeframe_minutes`, `trends_shares`, `trends_likes`, `trends_replies` (each
  ≥ 1) and `trends_max_uris` (default 5000), each with an env override.
- Tests: `fasp/tests/discovery.rs` (gating, options, shapes, signatures, 405s),
  `fasp/tests/server.rs::trending_events_are_handed_over_only_from_a_trends_subscription`,
  `fasp/tests/data_sharing.rs::on_enable_with_trends_adds_one_content_trends_subscription_with_the_threshold`,
  `mwmbl_social/tests/discovery.rs` (end to end over signed FASP: trending events build the
  state, a not-yet-indexed trending post is fetched and served, an unlisted one never is,
  ranking, language, hashtags, a deleted post drops out, recommendations, 404s; and a
  domain-blocked post and author still on their pages are not returned), and unit tests in
  `mwmbl_social::trends`.

Mwmbl integration is a separate track; see "Deployment and adoption".

## Deployment and adoption

Decided 2026-11-17. Two goals: get fediverse server owners to connect to a hosted instance
(which raises awareness of Mwmbl and grows its userbase), and show opted-in social content in
Mwmbl search.

**What admins get today.** FASP has no post-search capability yet, so Mastodon calls us only
for `account_search` and `follow_recommendation`, and only behind `EXPERIMENTAL_FEATURES=fasp`.
The pitch is "make your opted-in users' posts findable on Mwmbl, and get fediverse-wide account
search and follow suggestions", not "search for your server". Expect few FASP sign-ups early;
Lemmy follows and crawl submissions from Mwmbl's crawler supply most of the first content.
Consent is the lead message: the fediverse has rejected search projects before.

**Domains.** Following the joinmastodon.org / mastodon.social split:

- **mwmblsocial.org** is the project site: what it is, how consent works, how to self-host, and
  a link to Mwmbl's hosted instance as one option. A static site on GitHub Pages, maintained
  separately; [`docs/site-brief.md`](docs/site-brief.md) describes its content.
- **social.mwmbl.org** is Mwmbl's hosted instance, i.e. its `base_url`. The base URL is in every
  registration and in our actor's URI, so it never changes. Its actor is
  `@mwmbl-social@social.mwmbl.org`.
- **mwmbl.org** holds everything about Mwmbl's instance that has a UI: the sign-up page for
  server admins, the instance's privacy policy, the social search page and the blended results.
  The instance's `privacy_policy_url` and `sign_in_url` point there, and `root_redirect` sends
  `social.mwmbl.org/` there. A self-hoster sets their own.

**What this repo provides for it**, and nothing operator-specific:

- `sign_in_url` config, sent as `signInUrl` in `provider_info`.
- `root_redirect` config: `GET /` answers a redirect to it (404 when unset).
- An admin API, `POST /api/v1/admin/registrations` (and list/revoke), doing what the
  `register`/`registrations`/`unregister` CLI commands do, behind bearer tokens from
  `admin_tokens` (like `crawl_tokens`; none turns it off). It returns the key fingerprint and
  the approval link for the operator's UI to show. Built in `mwmbl_social::admin_app`; the
  commands and the API share `mwmbl_social::registrations`, and the token check is shared with
  crawl submissions (`mwmbl_social::operator`). It runs inside `serve` on the FASP state's
  store connection and HTTP client, and writes only SQLite, like the commands.
  - `POST` takes `{"server_url", "fasp_base_url"?}` and answers 201 with `{id, base_url,
    status, capabilities, fingerprint, created_at, approval_url}` (`base_url` is the server's
    FASP base URL, `approval_url` its `registrationCompletionUri`). 400 for an unusable URL,
    409 with the existing `id` when that server is already registered, 502 when it can't be
    reached, isn't a FASP host or refuses, 422 for any other body.
  - `GET` lists, and `GET`/`DELETE .../{id}` read or revoke one (404 if unknown), with the same
    fields minus `approval_url`: the server sends that link once, and it is not stored, so the
    operator's UI keeps it.
  - Disconnect and reconnect: `POST` after `DELETE` registers the server again, as `register`
    does after `unregister`. The revoked row (with its key, capabilities and subscriptions) is
    replaced by a new registration with a new `id`, key, fingerprint and `serverId`, and the
    201 carries `replaces` with the old `id`, which then gets 404. Only a live registration
    (active, or pending the admin's approval) gets 409. Mastodon refuses a second provider
    for the same base URL until its admin deletes the old one, so the operator's UI should
    say so on disconnect; until then reconnecting gets 502.

**Hosted sign-up (in Mwmbl).** A logged-in Mwmbl user enters their server's URL; Mwmbl
rate-limits per user and per target host, then calls the admin API and shows the fingerprint and
approval link, and later the registration's status. There is no moderation queue: the admin's
approval on their own server proves consent. The rate limits exist because registering someone
else's server leaves a pending provider in their admin panel, and Mastodon refuses a second
registration for the same URL until that one is deleted.

**Search integration (in Mwmbl), at query time.** Mwmbl calls our search API server-side:

1. A social search page on mwmbl.org showing author, snippet, date and a link to the original.
2. A "From the fediverse" block of the top few results on the main results page, under a hard
   timeout and cached like Mwmbl's other external results.
3. Later, social results fed into Mwmbl's ranker, once there is training data for them.

Social documents are **never copied into Mwmbl's index**: a copy would escape our sweep and
tombstones, and break the promise that withdrawn consent removes content promptly.

**Deployment.** social.mwmbl.org runs on Dokku on Mwmbl's server, from the image this repo
publishes (`ghcr.io/mwmbl/social`). The deployment config lives in the Mwmbl repo
(`deploy/social/`), not here, since it is specific to Mwmbl's instance. Secrets are SOPS-encrypted
there. Ansible is deferred until there is more than one server to manage.

## Working method

An Opus session orchestrates the work, and subagents do most of the implementation.

- **Opus defines each stage before it is built.** It fixes the trait signatures and writes the
  acceptance tests, then hands out small tasks of the form "make these pass". Each task covers
  one crate or one endpoint group and has a clear done check: the `CLAUDE.md` gate.
- **Sonnet does most of the implementation:** HTTP handlers, the SQLite schema and queries, the
  CLI, config, the page-code port, and FASP endpoints from the spec.
- **Opus writes these itself, or writes their tests first,** because mistakes in them are quiet
  and costly:
  - consent evaluation and its table of real-world JSON samples;
  - the deletion and sweep core: URI lookup, re-tokenising, tombstones, sweep deduplication,
    and a property test that deleted documents leave no trace on any page;
  - the `IndexItem` trait and tuple layouts.
- **Review:** Opus reviews every change in those areas before anything builds on it. Other work
  gets periodic review.
- **Open-ended interop debugging goes to Opus or the human,** not a looping subagent. That means
  signature mismatches against a real Mastodon, nodeinfo quirks and registration failures.
- **`PLAN.md` stays current.** When a decision changes, Opus updates `PLAN.md` in the same
  change.

**Environment:** Stages 0 to 2 can be built in a cloud session, because every CI test uses
in-process fakes. End-to-end testing against the Docker Compose Mastodon may need a local
machine if the cloud sandbox can't run Docker.

## Testing

Write tests against the spec where appropriate. Aim for at least 80% test coverage.

- **Unit tests:**
  - page format and deletion (tinyindex)
  - tokeniser
  - consent evaluation (table-driven over real-world JSON samples from Mastodon, GoToSocial,
    Misskey, Pleroma and Lemmy, with and without a follow opt-in)
  - signature sign/verify against the RFC 9421 test vectors
  - content-digest
- **Integration tests:** a fake FASP-speaking server in-process (axum) for registration,
  subscriptions and announcements, plus a fake ActivityPub host serving objects, so CI doesn't
  need Mastodon.
- **End-to-end:** Docker Compose environments for manual and scheduled checks: Mastodon
  (dev/mastodon) and Lemmy (dev/lemmy).
- **Crawling:** no bulk crawling of real servers in development or tests. Real-server testing
  only goes through registrations and data_sharing with servers whose admins have opted in.
  In production, real-server traffic also includes fetching crawl-submitted Lemmy URIs
  (Stage 3), which respects robots.txt and per-host limits. robots.txt governs those crawl
  fetches only; federation fetches (FASP, follows, revalidation) are not crawling.

## Open questions

1. **Server actor `preferredUsername`:** `mwmbl-social` is the proposed default. Make it
   configurable.
2. **Mastodon status:** answered from Mastodon `main` (2026-11-15):
   - Behind `EXPERIMENTAL_FEATURES=fasp`. The server side implements registration,
     `data_sharing` (event subscriptions create/destroy, backfill requests and continuations)
     and the debug capability. It calls `account_search` and `follow_recommendation`, and sends
     `trending` events, but never calls `trends` (see Stage 4).
   - `account_search` is called as `GET /account_search/v0/search?term=...&limit=10`, with no
     `language`, and the body is read as a JSON array of actor URIs. `Link` pagination is
     ignored. Mastodon fetches each URI it doesn't already know.
   - Key ids: Mastodon signs its requests to us with `keyid` = the `serverId` we sent at
     registration. We sign ours to it with `keyid` = the `faspId` it returned. It rejects
     requests whose `created` is more than 5 minutes old.
   - Mastodon checks every response we send it, 204s included. The `content-digest` header must
     be exactly `sha-256=:<base64 of SHA-256 of the body>:`, with nothing else in it. The
     response must carry an RFC 9421 signature over `@status` and `content-digest`.
   - **Signed fetches, checked end to end** (dev/mastodon, Mastodon 4.7.5 with
     `AUTHORIZED_FETCH=true`): an unsigned GET gets 401. Our RFC 9421 GET, signed over
     `@method` and `@target-uri` with `created` and `keyid`, gets 200. Mastodon fetches our key
     from the `keyid` (`/actor#main-key`). Stage 0's done-when passes against it: `ingest`
     indexes alice's public post, `search` finds it, and after it is deleted upstream the
     sweep's revalidation gets 404 and removes it, with no tombstone needed.
   - **Edits are cached for 3 minutes.** Mastodon serves a status's ActivityPub JSON from a
     3-minute cache keyed without `updated_at`, and announces an `update` at once. An
     immediate fetch gets the pre-edit text (seen end to end). So `update` events are queued
     with `not_before` 4 minutes out.
   - **Other Mastodon deviations found end to end:** registration answers 200, not 201;
     `event_subscriptions` needs `maxBatchSize` (500 without it); and its responses are signed
     with no `keyid`, so the client verifies them against the server's key from registration.
     A registration attempt that fails after Mastodon has created its provider leaves that
     provider behind, and Mastodon refuses a second one for the same base URL until an admin
     deletes it.
   - Lifecycle announcements: content events only fire while the status is public and the
     author is indexable. So a post that becomes non-public, or an author who turns off
     `indexable`, sends us **no** event for their posts. Account `update` events do fire when
     `discoverable` changes. Weekly revalidation is therefore the only thing that catches those
     consent changes, so the sweep is required, not a backstop. Also check the author's
     `indexable` when an account `update` arrives, and add an author tombstone if it is off.
3. **Data from a deactivated or revoked server:** we don't record which server announced
   what, so purging per server isn't possible. The proposed default is to keep it. It is public
   and opted in, and the sweep keeps rechecking it. An admin can still `forget` a domain.
4. **Lemmy consent policy:** decided 2026-11-16. Index public Lemmy content where the
   instance's robots.txt allows it, choosing the consent rule by the author's host. See Stage 3.
5. **Hosted sign-up:** decided 2026-11-17, replacing the 2026-11-16 decision to gate it through
   Mwmbl's domain submission moderation. Self-serve on mwmbl.org for logged-in Mwmbl users,
   rate-limited, calling the admin registration API. See "Deployment and adoption".
6. **Index sizing defaults** for small self-hosters versus our instance, and whether to keep
   one posts index or shard it by time.
7. **Trend input trust** (Stage 4): every `trending` event counts, pooled across
   registrations, so one registered server could inflate a post's trend score. Registrations
   are admin-approved, so this is accepted for now; per-registration caps or counting
   distinct registrations are the options if it is abused.
8. **Subscribing to trends events** (Stage 4): we subscribe with every `data_sharing`
   registration, even when no registration has `trends` or `follow_recommendation` enabled,
   because trend data is pooled. That costs fetches of trending posts we have not indexed
   (queued as backfill). Should the subscription depend on some registration enabling a
   trends consumer?
9. **Trends end to end:** Mastodon never calls `trends`, so the endpoints are checked against
   our fakes only. Which server will call them, and with which options?
