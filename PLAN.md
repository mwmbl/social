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
  a key fingerprint. Self-hosted installs do this with a CLI command. The hosted instance needs a
  minimal sign-up page, which comes later (see Stage 1).
- **Mwmbl integration is postponed** until the index has real content.
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
                server actor + WebFinger, object parsing (Note/Page/Article/Person/Group…),
                consent evaluation
                (`apub::signatures`: framework-agnostic, operating on a method, a target URI,
                a header lookup and a status. Fixed component set; RFC 9421 signs ed25519 and
                rsa-v1_5-sha256 and also verifies rsa-pss-sha512; cavage-12 is RSA-SHA256 only,
                `hs2019` accepted on verify. Cavage verification requires a covered `date` or
                `(created)` and checks its skew.)
  fasp/         FASP general (registration, provider_info, activation, request signing/
                verification, content-digest) + data_sharing + account_search handlers
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

### Data model (SQLite)

**Principle:** as in Mwmbl, the index is the only record of what is indexed. SQLite holds
operational state only. It must not mirror the index or grow with it: no per-document or
per-account rows, and no term lists. Every table below is bounded by the number of registrations
and hosts, or by a fixed time window.

- **`registrations`**: id, server base URL, `serverId` (ours), `faspId` (theirs), our Ed25519
  keypair, their public key, status (pending/active/revoked), created_at.
- **`capabilities`**: registration_id, capability id, version, enabled, updated_at.
- **`subscriptions`**: registration_id, remote subscription id, category, type, created_at.
- **`backfills`**: registration_id, remote id, category, state, more_available.
- **`fetch_queue`**: uri, kind hint, reason (announce/backfill/revalidate), not_before,
  attempts, last_error. It is bounded: deduplicate by URI and cap its length. When it is full,
  backfill and revalidation work is deferred, and announcements take priority.
- **`hosts`**: host, signature scheme that works (9421/cavage), backoff_until, blocked flag.
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
- **`sweep_state`**: the sweep's current page, and when the current and last sweeps started.

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
  - `acct`: `user@host`.
  - `summary`: the bio as plain text, truncated.
  - `score`: from followers and activity.
  - `languages`: the posting languages we've observed, for `account_search`'s `language`
    filter.

If a field turns out to be needed later, add it at the end as optional. Old pages still decode.

Index keys for accounts include the username, the full `user@host` handle, the host, and the
display-name tokens, so handle searches work.

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

**What triggers a delete:**

- a lifecycle `delete` event
- an `update` event that turns out to fail consent
- revalidation returning 404 or 410, a Tombstone, or lost consent
- the account no longer being discoverable or indexable. This adds an *author* tombstone: its
  posts are filtered from results immediately and physically removed by the sweep. An author
  tombstone covers posts only. If the account is no longer discoverable, its own record is also
  deleted by URI; if it is still discoverable but not indexable, the account stays searchable.
- a domain block, applied by search-time filtering and the sweep
- an admin CLI command `mwmbl-social forget <uri|acct|domain>`

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
   - Hosted: a minimal server-rendered sign-up page doing the same. This can come at the end of
     the stage, after the CLI works.
4. Capability activation endpoints. Turning `data_sharing` on should:
   - create `content` and `account` lifecycle subscriptions;
   - start backfill for both categories (`maxCount` configurable), following continuations
     while `moreObjectsAvailable`.

   Turning it off unsubscribes and stops calls. Should a deactivated server's data be purged?
   Decide that; see open questions.
5. `POST /data_sharing/v0/announcements`: verify the signature and the registration, enqueue
   the URIs with priority (new/update > backfill > revalidate), and return 204 quickly.
6. Tune the sweep's revalidation against real servers: spread the fetches and respect per-host
   budgets, so a weekly pass doesn't overload a big host.
7. Accounts: when an account turns off `discoverable` or `indexable`, add an author tombstone
   (see Deletion).
8. Observability: tracing, a `/metrics` endpoint (Prometheus) for queue depth, fetch errors,
   index docs and per-host status, plus `stats`.

**Done when** the local Mastodon registers with us, enables data_sharing and backfills. New,
edited and deleted public posts from opted-in accounts show up in or vanish from `search`.
Unlisted posts and non-indexable accounts never appear.

### Stage 2: FASP `account_search`

1. `GET /account_search/v0/search` with `term`, `limit` and `language` (RFC 4647 basic
   filtering against the account languages we've observed). It returns actor URIs ranked by
   relevance, with `Link: rel="next"` pagination. Missing `term` gets 422.
2. Relevance:
   - exact handle or host match first
   - then display-name and username term matches
   - then summary matches, weighted by followers and activity
3. Advertise it in `provider_info` and handle its activation.

**Done when** searching for accounts in the local Mastodon's UI returns results from us. This is
the first point where an admin sees value, so after it: write install docs, publish a Docker
image, and try a first external deployment.

### Stage 3: Lemmy

Ingest through federation, not the REST API:

1. Make the server actor able to follow `Group` actors (communities): accept inbox
   deliveries, verify incoming HTTP signatures, and handle
   Create/Update/Delete/Announce/Undo.
2. Admins configure which communities or instances to follow.
3. Map Lemmy `Page` posts and comments to index documents. The post title becomes the document
   title, which makes these good Mwmbl candidates.
4. Consent: Lemmy has no `indexable` flag, so decide on a policy (see open questions).

**Done when** following a community on a local Lemmy indexes its new posts and comments, and
removes them on delete.

### Stage 4: rest of FASP discovery

1. `trends` (content). We can use the `trends` subscription type of data_sharing as input.
2. `follow_recommendation`.
3. Take part in drafting the status/post search spec, using our post index as a reference
   implementation.

Mwmbl integration is a separate track. It can start any time after Stage 1, once there is real
content: an authenticated search API for Mwmbl, and a decision on merging and ranking.

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
    Misskey and Pleroma)
  - signature sign/verify against the RFC 9421 test vectors
  - content-digest
- **Integration tests:** a fake FASP-speaking server in-process (axum) for registration,
  subscriptions and announcements, plus a fake ActivityPub host serving objects, so CI doesn't
  need Mastodon.
- **End-to-end:** a Docker Compose Mastodon for manual and scheduled checks.
- **Crawling:** no bulk crawling of real servers in development. Real-server testing only goes
  through registrations and data_sharing with servers whose admins have opted in.

## Open questions

1. **Server actor `preferredUsername`:** `mwmbl-social` is the proposed default. Make it
   configurable.
2. **Mastodon status:** answered from Mastodon `main` (2026-11-15):
   - Behind `EXPERIMENTAL_FEATURES=fasp`. The server side implements registration,
     `data_sharing` (event subscriptions create/destroy, backfill requests and continuations)
     and the debug capability. It calls `account_search` and `follow_recommendation`.
   - `account_search` is called as `GET /account_search/v0/search?term=...&limit=10`, with no
     `language`, and the body is read as a JSON array of actor URIs. `Link` pagination is
     ignored. Mastodon fetches each URI it doesn't already know.
   - Key ids: Mastodon signs its requests to us with `keyid` = the `serverId` we sent at
     registration. We sign ours to it with `keyid` = the `faspId` it returned. It rejects
     requests whose `created` is more than 5 minutes old.
   - Mastodon checks every response we send it, 204s included. The `content-digest` header must
     be exactly `sha-256=:<base64 of SHA-256 of the body>:`, with nothing else in it. The
     response must carry an RFC 9421 signature over `@status` and `content-digest`.
   - Lifecycle announcements: content events only fire while the status is public and the
     author is indexable. So a post that becomes non-public, or an author who turns off
     `indexable`, sends us **no** event for their posts. Account `update` events do fire when
     `discoverable` changes. Weekly revalidation is therefore the only thing that catches those
     consent changes, so the sweep is required, not a backstop. Also check the author's
     `indexable` when an account `update` arrives, and add an author tombstone if it is off.
3. **Data from a deactivated or revoked server:** we don't record which server announced
   what, so purging per server isn't possible. The proposed default is to keep it. It is public
   and opted in, and the sweep keeps rechecking it. An admin can still `forget` a domain.
4. **Lemmy consent policy:** index all public content from communities on instances that
   federate with us, or require something further? Also respect `noindex`-style
   instance-level signals where they exist.
5. **Hosted sign-up:** who can register with our hosted instance, and do we moderate or limit
   it?
6. **Index sizing defaults** for small self-hosters versus our instance, and whether to keep
   one posts index or shard it by time.
