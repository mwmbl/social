# Security audit: mwmbl/social

| | |
|---|---|
| Scope | The whole workspace (`crates/apub`, `fasp`, `mwmbl_social`, `social_index`, `store`, `tinyindex`, `tokenize`), the Dockerfile, `.github/workflows/`, `dev/`, and the dependency tree |
| Date | 2026-11-19 to 2026-11-20 |
| Branch / commit | `audit/2026-11-19` at `f82525eb354206325c78a88a297c019904d20d2a` (reviewer branches A-F, P3 and P4 merged) |
| Method | Phase 1: six parallel code reviewers, each with its own area: A signatures and inbox, B SSRF and outbound requests, C FASP, registration and admin, D consent, E hostile input and remote DoS, F secrets and deployment. Each wrote `#[ignore = "AUDIT: ..."]` tests that fail while the finding stands. P3 ran the supply-chain tooling and wrote the fuzz targets and CI drafts. P4 wrote the audit scripts and the manual checklist. |
| Threat-model categories | 1 consent, 2 SSRF and amplification, 3 auth bypass, 4 secret leaks, 5 remote DoS, 6 supply chain |

> **Caveat: Phase 2 (an independent attempt to disprove each finding) was skipped at the
> owner's request, to save cost.** So every finding below was reported by a reviewer and has
> not been independently verified. The evidence is either an ignored test that **FAILS** under
> `cargo test --workspace -- --ignored` (the test reproduces the finding), a test that
> **PASSES** (the finding is not reproduced), or **none** (code reading only).

Line numbers come from the reviewers' branches and may be a few lines off on the merged
commit. To run every audit test: `cargo test --workspace -- --ignored`. Of the 25 audit tests,
23 fail and 2 pass (A3, E3). Without `--ignored`, `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass. One
flaky, timing-based test, `discovery.rs::trending_announcements_feed_trends_and_follow_recommendations`,
timed out once under parallel load and then passed on rerun. It is not related to the audit.

## 1. Summary

The findings are sorted by threat-model category and then by severity. Where reviewers
reported the same issue, the entry lists both IDs.

| ID(s) | Sev | Cat | Title | Where | Test |
|---|---|---|---|---|---|
| A1 | Critical | 1 (3) | A cached key is accepted for any actor on its origin, so anyone there can forge a Follow opt-in, Undo or Delete as another user | `mwmbl_social/src/inbox_app.rs:291-308` | `audit_a1_a_cached_key_signs_for_another_actor_on_its_origin`: FAILS |
| D1 | High | 1 | A delete or opt-out announced while the same URI is in flight is lost | `store/src/queue.rs:202`, `mwmbl_social/src/worker.rs:776,787` | `a_delete_announced_while_the_create_is_in_flight_is_not_lost`: FAILS |
| D2 | High | 1 | A stale `IndexPost` brings back a post that the inbox deleted | `mwmbl_social/src/writer.rs:530-545`, `inbox_app.rs:466` | `an_index_decision_from_before_an_inbox_delete_does_not_resurrect_the_post`: FAILS |
| D3 | Medium | 1 (5) | A post's author document can name any account on any host, which we then remove and tombstone | `mwmbl_social/src/decide.rs:233-243`, `apub/src/consent.rs:194` | `a_post_cannot_make_us_remove_an_account_on_another_host`: FAILS |
| D4 | Medium | 1 | After an unfollow, a follower-consented account stays indexed if the refetch fails | `mwmbl_social/src/activities.rs:290-310`, `worker.rs:776-788` | `an_unfollow_withdraws_a_flagless_account_even_if_its_refetch_fails`: FAILS |
| D5 | Medium | 1 | Content whose origin never answers again stays searchable forever | `mwmbl_social/src/decide.rs:198,207`, `worker.rs:776-788` | `content_whose_origin_never_answers_again_is_eventually_removed`: FAILS |
| E9 | Medium (conditional) | 1 (5) | If the writer thread panics, nothing reports it, and inbox removals that were answered 202 are dropped | `mwmbl_social/src/writer.rs:354-361`, `inbox_app.rs:395-398` | none |
| D6 | Low | 1 | A stale cached author document can clear a fresh author tombstone | `mwmbl_social/src/worker.rs:520-545`, `decide.rs:158-162` | none |
| D7 | Low | 1 | Removing a carried record leaves the newer copies when the `uri:` copy has been evicted | `mwmbl_social/src/writer.rs:556-572` | none |
| D8 | Low | 1 | With `max_transient_attempts` below 3, a 401 or 403 never removes anything | `mwmbl_social/src/decide.rs:30,106`, `config.rs:365` | none |
| B1 = F6 | Medium | 2 | Proxy environment variables bypass the SSRF filter | `apub/src/net.rs:195-214` | `an_environment_proxy_does_not_bypass_the_address_filter`: FAILS |
| C1 | Medium | 2 | Announced URIs aren't tied to the announcing server's host, so 1 request causes 1000 fetches from third parties | `mwmbl_social/src/fasp_app.rs:798-814` | `announced_uris_on_another_host_are_not_fetched`: FAILS |
| B2 | Low | 2 | A trailing-dot hostname (or wildcard DNS) gets its own per-host limits and backoff | `apub/src/fetch.rs:397-403` | `a_trailing_dot_does_not_make_a_new_host`: FAILS |
| B5 | Low | 2 | The FASP client follows cross-host and https-to-http redirects | `apub/src/net.rs:202-213` | none |
| F7 | Low | 2 | No warning is logged when `allow_private_addresses` or `allow_http` is on | `mwmbl_social/src/config.rs:105-110`, `cli.rs:222-223` | none |
| A2 | High | 3 (1) | A key document that the actor never lists is trusted to sign for that actor | `apub/src/inbox.rs:150-213`, `inbox_app.rs:257-264` | `audit_a2_a_key_document_the_actor_does_not_list_signs_for_it`: FAILS |
| A3 | High | 3 | Any user on an origin can delete any other user's post (**unresolved**) | `mwmbl_social/src/activities.rs:245-249`, `inbox_app.rs:466` | `audit_a3_anyone_on_an_origin_can_delete_anothers_post`: **PASSES** |
| A4 | High | 3 | Any user on an origin can tombstone another user there with `Block`+`removeData` | `mwmbl_social/src/activities.rs:257-267` | `audit_a4_anyone_on_an_origin_can_tombstone_another_author`: FAILS |
| A5 | Low | 3 | Cavage signatures aren't bound to our host, so they can be replayed from other servers | `apub/src/inbox.rs:106-115` | `audit_a5_a_cavage_signature_for_another_host_verifies_here`: FAILS |
| C4 | Low | 3 | The admin API shares the public listener and only checks token length, not entropy | `mwmbl_social/src/cli.rs:408-421`, `config.rs:129-143` | none |
| F1 | Medium | 4 | Config parse errors echo admin and crawl tokens to stderr | `mwmbl_social/src/config.rs:23-27,254-257`, `cli.rs:164` | `a_mistyped_token_list_is_not_echoed_in_the_config_error`, `a_misspelt_token_key_is_not_echoed_in_the_config_error`: FAIL |
| F2 | Medium | 4 | The SQLite store holding the FASP Ed25519 private keys is world-readable (0644) | `store/src/lib.rs:71-75`, `mwmbl_social/src/cli.rs:555` | `the_store_holding_registration_keys_is_private`: FAILS |
| F8 = D9 | Low | 4 | Debug logging writes whole remote JSON documents to the log | `mwmbl_social/src/inbox_app.rs:344-346` | none |
| E4 | High | 5 (1) | Anyone can grow the tombstone table without limit, and the whole table is reloaded every 30 s | `mwmbl_social/src/activities.rs:245`, `writer.rs:559-561,722-739` | none |
| E1 | High | 5 | One unauthenticated search can cost about 20,000 page reads | `tokenize/src/lib.rs:151-165`, `mwmbl_social/src/http.rs:139-167` | `query_keys_are_bounded`: FAILS |
| A6 = B4 = E7 | Medium | 5 (2) | No inbound or outbound concurrency limits or timeouts, and the inbox key fetch runs inline while the request body is held | `apub/src/fetch.rs:509-532`, `inbox_app.rs:740,772`, `cli.rs:435` | `outbound_requests_have_a_global_limit`: FAILS (B4 part only) |
| B3 = E6 | Medium | 5 | Per-host state for any host a stranger names is never evicted, and every such host is rewritten to SQLite each tick | `apub/src/fetch.rs:377,471-479`, `mwmbl_social/src/worker.rs:999-1010` | `per_host_state_is_bounded`: FAILS |
| C3 = E5 | Medium | 5 | `queue_cap` doesn't limit announces, from FASP peers or from any stranger's Follow | `store/src/queue.rs:141-176`, `activities.rs:192-212` | `announcements_do_not_grow_the_queue_past_its_cap`, `announces_cannot_grow_the_queue_past_its_cap`: FAIL |
| E2 | Medium | 5 | An account record is stored under one key per username token, with no cap | `social_index/src/keys.rs:125-130`, `mwmbl_social/src/records.rs:121` | `account_keys_are_bounded`: FAILS |
| E3 | Medium | 5 | `special_keys` is quadratic in distinct hashtags and mentions (**unresolved**) | `tokenize/src/special.rs:44-49` | `special_keys_is_not_quadratic`: **PASSES** |
| E8 | Low-Medium | 5 (1) | The inbox moves writer back-pressure into an unbounded number of spawned tasks | `mwmbl_social/src/inbox_app.rs:776-777` | none |
| C5 = L4 | Low | 5 (4) | `/metrics`, when enabled, is public and reads whole tables on every scrape | `mwmbl_social/src/metrics.rs:79-80`, `cli.rs:422-429` | none |
| E10 | Low | 5 | robots.txt matching is O(rule bytes × path length) for a hostile host | `mwmbl_social/src/robots.rs:107-142` | `robots_matching_is_cheap_on_hostile_rules`: FAILS |
| F9 | Low | 5 | The actor key file isn't written atomically, so a crash can leave a key file that blocks every later start | `mwmbl_social/src/keys.rs:100-104` | none |
| F3 | Medium | 6 | GitHub Actions are pinned by mutable tags, in the job that holds `packages: write` | `.github/workflows/build.yml:12-44`, `ci.yml:12-15` | none |
| F4 | Low | 6 | `ci.yml` has no `permissions:` block | `.github/workflows/ci.yml` | none |
| F5 | Low | 6 | Same-repo PR builds are pushed to GHCR, although a comment says they aren't | `.github/workflows/build.yml:43-44` | none |
| F10 | Low | 6 (4) | `.dockerignore` is minimal, and the base images are pinned by tag only | `.dockerignore`, `Dockerfile:3,7,10` | none |
| C2 | Medium (hosted) / Low | other: integrity | A URI repeated in one `trending` announcement is counted once per repeat | `mwmbl_social/src/fasp_app.rs:451-462` | `a_uri_repeated_in_one_trending_announcement_counts_once`: FAILS |

Count (39 after merging duplicates): 1 Critical, 7 High, 15 Medium, 1 Low-Medium, 15 Low.
Two of the High and Medium findings (A3, E3) are not reproduced by their tests, and E4 depends
on the same code path as A3 (see §3).

**Context for categories 2, 3 and 5 (from C):** PLAN.md:1231-1233 makes hosted sign-up
self-serve for any logged-in Mwmbl user, and a registration becomes `active` on the first
request that verifies (`fasp/src/http/auth.rs:99-108`). So in hosted mode a registered FASP
peer is an untrusted stranger who runs a small fake "Mastodon". PLAN.md:1236-1239 accepts trend
inflation on the grounds that "registrations are admin-approved", which is not true in hosted
mode. This affects C1, C2 and C3.

## 2. Findings in detail

### Category 1: consent

#### A1: A cached key is accepted for any actor on its origin (Critical; also category 3)
- **Where:** `crates/mwmbl_social/src/inbox_app.rs:291-308`. `verify()` checks only
  `same_origin(key_id, actor)`. On a key-cache hit it calls `verify_post` and returns `Ok`.
  `check_key_owner(&info, actor)` runs only inside `fetch_key` (line 261), that is, on a cache
  miss. The cache (line 144) is keyed by `key_id` alone and doesn't record the owner.
- **Scenario:** Eve has an account on `m.example`. She sends us any properly signed activity
  as herself, which caches `https://m.example/users/eve#main-key` for an hour. She then POSTs
  `{"type":"Follow","actor":"https://m.example/users/bob","object":<our actor>}`, signed with
  her own key and `keyId=…/eve#main-key`. We answer 202 and process it as coming from Bob. We
  record `RecordFollowOptin` for Bob, so any consent flag Bob's actor leaves out now counts as
  yes. We also Accept, follow Bob back and fetch him. She can send Undo, Delete (which removes
  Bob's account), Accept or Reject as Bob in the same way.
- **Test:** `audit_a1_a_cached_key_signs_for_another_actor_on_its_origin` in
  `crates/mwmbl_social/tests/inbox.rs`. Run
  `cargo test -p mwmbl_social --test inbox audit_a1 -- --ignored`.
- **Fix:** cache `(key, owner)` and run `check_key_owner` on every request, whether or not
  the key was cached. Alternatively, key the cache by `(key_id, actor)`.

#### D1: A delete announced while the same URI is in flight is lost (High)
- **Where:** `crates/store/src/queue.rs:202` (`complete` deletes by URI). `queue.rs:~127-138`
  folds a re-enqueue of an already-queued URI into the existing row and returns
  `AlreadyQueued`. `crates/mwmbl_social/src/worker.rs:776,787` (`finish` calls
  `complete(uri)`).
- **Scenario:** a FASP `create` for post P is queued, and the worker claims it and fetches P
  while P is still public. The author then deletes P, and their server announces `delete` for P.
  `enqueue` finds the row still there, because it is in flight, and returns `AlreadyQueued`. The
  worker applies `IndexPost`, and then `finish` runs
  `DELETE FROM fetch_queue WHERE uri = P`, which drops the delete. P stays searchable until it
  is revalidated, which can be up to 6 days later (`revalidate_after_secs`). The same race loses
  an account `update` that turns off `indexable` or `discoverable`, and an `update` that makes a
  post non-public. The inbox's `Create` and `Update` handling, which also queues fetches, has
  the same race.
- **Test:** `a_delete_announced_while_the_create_is_in_flight_is_not_lost` in
  `crates/mwmbl_social/tests/audit_d.rs`. Run
  `cargo test -p mwmbl_social --test audit_d -- --ignored`. This runs all of D's tests.
- **Fix:** give each row a generation number (or bump `enqueued_at`) when a duplicate arrives,
  and make `complete` and `fail` conditional on the generation that was read
  (`DELETE ... WHERE uri=? AND gen=?`). Otherwise, keep a "dirty" flag and re-queue the row.

#### D2: A stale index decision brings back a post that the inbox deleted (High)
- **Where:** `crates/mwmbl_social/src/writer.rs:530-545`. `IndexPost` clears the object
  tombstone unconditionally and upserts. `crates/mwmbl_social/src/inbox_app.rs:466` applies an
  inbox `Delete` straight to the writer, outside the queue.
- **Scenario:** a `Create` or an announcement makes the worker fetch P while it is public. Then
  the author's inbox `Delete` arrives and is applied, so P is removed and tombstoned. After that
  the worker's `IndexPost`, decided from the earlier fetch, is applied. It clears the tombstone
  and indexes the deleted P again, and P stays served until revalidation (up to 6 days). The
  window is the time taken by the post fetch plus the author fetch. That is a few seconds, or
  longer against slow hosts, and posting and then deleting within seconds is common. The same
  race applies when an `AddAuthorTombstone` from an inbox `Block` or Undo Follow races an
  in-flight `ClearAuthorTombstone` (see D6).
- **Test:** `an_index_decision_from_before_an_inbox_delete_does_not_resurrect_the_post` in
  `crates/mwmbl_social/tests/audit_d.rs`.
- **Fix:** carry the fetch's start time in `IndexPost`, `IndexAccount` and
  `ClearAuthorTombstone`. The writer refuses them, or keeps the tombstone, when the tombstone
  row is newer than the fetch.

#### D3: A post's author document can name any account, which we then remove and tombstone (Medium; also category 5)
- (Reviewer severity. Lead note: this is a wider version of A4. A4 needs the same origin, but
  D3 lets any host remove any account on any other host, so it is arguably at least as severe
  as A4, which is rated High.)
- **Where:** `crates/mwmbl_social/src/decide.rs:233-243` together with
  `crates/apub/src/consent.rs:194`. `evaluate_post_in` checks the author's flags before it
  checks that the author's `id` matches `attributedTo` and is on the post's origin. A
  `NotDiscoverable` refusal then calls `account_actions(author_json, …)`, which acts on whatever
  `id` the author document claims. `decide_account` does check `id == uri` (decide.rs:261), but
  this path doesn't.
- **Scenario:** evil.example serves `/notes/1`, which is public with
  `attributedTo: evil.example/users/x`. It serves `/users/x` as
  `{"id": "https://victim.example/users/v", "type": "Person", "discoverable": false}`. Once the
  note is announced, crawled or delivered, the decision is
  `RemoveAccount{victim, tombstone: true}` plus `AddAuthorTombstone{victim}`. The victim's
  account disappears, all of the victim's posts are hidden at once, and the sweep then deletes
  them from the pages. Nothing clears this until the victim's actor is fetched again.
- **Test:** `a_post_cannot_make_us_remove_an_account_on_another_host` in
  `crates/mwmbl_social/tests/audit_d.rs`.
- **Fix:** in `decide_post`, call `account_actions` only when the author document's `id`
  equals the URI it was fetched from (`author_of(json)`) and is on the post's origin.
  Alternatively, move the `AuthorMismatch` and `OriginMismatch` checks ahead of
  `evaluate_account_in` in consent.rs.

#### D4: An unfollow doesn't withdraw a follower-consented account if the refetch fails (Medium)
- **Where:** `crates/mwmbl_social/src/activities.rs:290-310`. `undo` emits
  `RemoveFollowOptin`, `AddAuthorTombstone`, `Unfollow` and a `Fetch` of the actor. Only that
  fetch can remove the account record, and author tombstones don't cover accounts
  (`social_index/src/keys.rs`, where `author_uri()` returns `None` for accounts). In
  `worker.rs:776-788`, giving up after `max_transient_attempts` completes the item and does
  nothing else.
- **Scenario:** a GoToSocial user with no `discoverable` flag follows us, so we index their
  account, and later unfollows. Their host is down or rate-limiting when we refetch them (500,
  timeouts, 429 or backoff). After 10 attempts, about 17 hours of backoff, the worker gives up.
  The account stays in account search and follow recommendations until the next account
  revalidation, up to 6 days later. The author tombstone hides their posts, but not their
  account.
- **Test:** `an_unfollow_withdraws_a_flagless_account_even_if_its_refetch_fails` in
  `crates/mwmbl_social/tests/audit_d.rs`.
- **Fix:** in `undo`, also remove the account record (`RemoveAccount`, without an object
  tombstone). The refetch adds it back if the actor's own flags consent.

#### D5: Content whose origin never answers again is served forever (Medium)
- **Where:** `crates/mwmbl_social/src/decide.rs:198,207`, where every `Transient` result is a
  `Retry`, and `worker.rs:776-788`, where giving up removes nothing. `checked` isn't updated,
  so each sweep pass queues the record again, it fails again, and the worker gives up again.
- **Scenario:** a server shuts down (its DNS is gone, or it answers 5xx or times out), or it
  starts firewalling us. Its users can no longer withdraw consent through it, and we can't
  confirm their consent. Every post and account from that server stays searchable
  indefinitely, because only `Refused` (401 or 403) escalates to removal. Principle 3 says
  "when in doubt, don't index".
- **Test:** `content_whose_origin_never_answers_again_is_eventually_removed` in
  `crates/mwmbl_social/tests/audit_d.rs` (5 failed passes).
- **Fix:** count consecutive failed revalidations, for example in the carried record or a
  `hosts` column, and remove the content after N passes or after a maximum time since the last
  successful check.

#### E9: A writer-thread panic goes unnoticed, and the service keeps serving while dropping inbox removals (Medium, conditional; also category 5)
- (Reviewer severity, conditional: E found no reachable panic. See §6.)
- **Where:** `crates/mwmbl_social/src/writer.rs:354-361` is a plain `std::thread` with no
  `catch_unwind`, joined only at shutdown (`:376-382`). Callers get `WriterError::Stopped`
  (`:236-239`). At `inbox_app.rs:395-398`, an inbox removal that fails is only logged, and the
  sender has already had a 202, so it won't resend. There is no health endpoint. Background
  tokio tasks (`cli.rs` `tasks.spawn`) are also joined only at shutdown.
- **Scenario:** any future panic in the writer leaves `/api/v1/search` serving the last index
  state forever. `Delete`, `Undo{Follow}` and account deletions are answered 202 and then
  thrown away, so content whose author withdrew consent stays searchable.
- **Test:** none.
- **Fix:** watch the writer thread's `JoinHandle` (or `is_finished()`) and exit the process if
  the thread ends unexpectedly. Make inbox removals durable by queueing them before answering
  202.

#### D6: A stale author document can clear a fresh author tombstone (Low)
- **Where:** `crates/mwmbl_social/src/worker.rs:520-545` (`fetch_author` reads the 1-hour
  author cache), `decide.rs:158-162` (`ClearAuthorTombstone` when the cached document
  consents), and `ClearAuthorTombstone` in `writer.rs`, which has no ordering check.
- **Scenario:** post P by A is being processed, and the worker reads A's cached document with
  `indexable: true`. At the same moment A's account update is processed: the cache is refreshed
  and `AddAuthorTombstone` is applied. P's decision is applied after that and sends
  `IndexPost(P)` and `ClearAuthorTombstone(A)`, which undoes the opt-out until the next
  revalidation. Sequential processing is already covered (`refresh_author`,
  `tests/optout.rs:81`), but concurrent processing isn't.
- **Test:** none (it would need a concurrency harness).
- **Fix:** the same as D2.

#### D7: Removing a carried record leaves copies when the `uri:` copy fell off its page (Low)
- **Where:** `crates/mwmbl_social/src/writer.rs:556-572`. `RemovePost { record: Some(old) }`
  deletes the carried old version by its keys and then by URI. When the URI lookup is
  `NotFound`, it adds a tombstone only if `record.is_none()`.
- **Scenario:** the sweep carries version 1 of P. Before the revalidation runs, an update
  indexes version 2 under new keys, and P's `uri:` copy is later evicted from a full page. The
  revalidation finds P gone or non-public and removes version 1's keys, but misses version 2's
  copies. No tombstone is added, so version 2 stays searchable for up to 6 days.
- **Test:** none.
- **Fix:** add a tombstone whenever the URI lookup is `NotFound`, whether or not a record was
  carried.

#### D8: With `max_transient_attempts` below 3, a 401 or 403 never removes anything (Low, depends on config)
- **Where:** `crates/mwmbl_social/src/decide.rs:30,106` (`REFUSED_ATTEMPTS = 3`, counted
  against `item.attempts`), `worker.rs:782` (give up at `max_transient_attempts`), and
  `config.rs:365`, which accepts any value of 1 or more.
- **Scenario:** with `max_transient_attempts = 2`, a post whose host now answers 403 is retried
  once and then given up on without being removed. This repeats on every pass, so the post is
  served forever.
- **Test:** none.
- **Fix:** require `max_transient_attempts >= REFUSED_ATTEMPTS` in config validation, or remove
  the item on give-up after a `Refused`.

### Category 2: SSRF and amplification

#### B1 = F6: Proxy environment variables bypass the SSRF filter (Medium)
- (B rated this Medium and F rated it Low, as a deployment hazard. This report keeps the
  higher rating.)
- **Where:** `crates/apub/src/net.rs:195-214`. `client_builder` never calls `.no_proxy()`.
  reqwest 0.13.5 adds `ProxyMatcher::system()` whenever `auto_sys_proxy` is set, which it is by
  default. On Linux that reads `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and `NO_PROXY`, in
  either case. The `system-proxy` feature is off, but that only controls macOS and Windows
  system settings. All three clients are built through this function (`apub/src/fetch.rs:445`,
  `mwmbl_social/src/cli.rs:500,720`). The module doc comment (net.rs:21-22) admits the problem,
  but nothing enforces it, and README.md:106 doesn't mention it.
- **Scenario:** the deployment has `HTTPS_PROXY` set, which is common in k8s and corporate
  egress setups or when it is inherited from the host. A stranger POSTs to `/inbox` with
  `keyId=https://metadata.google.internal/...`, or with any name that points at 10.x or
  169.254.169.254. An announced or crawl-submitted URI works too. The name never reaches
  `PublicResolver`, because the proxy resolves it and fetches it, so the signed GET reaches the
  internal service. Only `check_url`'s IP-literal check still applies, and nothing logs that
  the filter is off.
- **Test:** `an_environment_proxy_does_not_bypass_the_address_filter` in
  `crates/apub/tests/audit_b_proxy.rs`. Run
  `cargo test -p apub --test audit_b --test audit_b_proxy -- --ignored`. The fake proxy
  receives `GET http://metadata.google.internal/computeMetadata/v1/instance/`.
- **Fix:** call `.no_proxy()` in `client_builder`. If a proxy is ever needed, make it explicit
  config and document that the proxy must do its own filtering. Alternatively, refuse to start
  when a proxy variable is set and `allow_private_addresses` is false.

#### C1: Announced URIs aren't tied to the announcing registration's host (Medium)
- **Where:** `crates/mwmbl_social/src/fasp_app.rs:798-814`. `on_announcement` queues
  `object_uris` unchanged, through `enqueue_all` (`fasp_app.rs:232-258`).
  `crates/fasp/src/http/handlers.rs:161-198` checks only that the source id belongs to the
  registration, and `crates/fasp/src/announcement.rs:106-116` checks only the scheme and length.
- **Scenario:** registration X POSTs a signed `content`/`new` announcement listing 1000 URIs on
  `victim.example`, `other.example` and so on. Every URI is queued with `FetchReason::Announce`
  and fetched from its origin. Announce fetches skip robots.txt, and `pacing.rs:270` doesn't
  pace Announce or Backfill fetches. So one signed request of 1 MB makes us send 1000 GETs to
  third parties. The per-host limits bound each host, but nothing bounds the number of hosts.
  Consent still holds, because the objects are fetched from their origin and judged there. In
  hosted mode, X can be anyone (see the context in §1).
- **Test:** `announced_uris_on_another_host_are_not_fetched` in
  `crates/mwmbl_social/tests/audit_c.rs`. Run
  `cargo test -p mwmbl_social --test audit_c -- --ignored`.
- **Fix:** drop announced URIs whose host isn't the registration's server host (or a host list
  the server declares), or count and log them. Mastodon only announces local objects. At the
  least, cap how many distinct foreign hosts one registration can make us fetch.

#### B2: A trailing-dot hostname gets its own per-host limits (Low)
- **Where:** `crates/apub/src/fetch.rs:397-403` (`host_key`). `victim.example.` and
  `victim.example` are different keys. So are a name and its IP literal, and so are wildcard
  subdomains. Each key gets its own concurrency limit (2), 250 ms pacing and backoff. The
  worker's pacing groups by the same key (`uri_host_key`), so C1's announcements are affected
  too.
- **Scenario:** a stranger's inbox POSTs alternate `keyId` between `https://victim.example/k`
  and `https://victim.example./k`. That doubles the rate we send to the victim, and the second
  form ignores any backoff the victim set with a 429 or 5xx. An attacker who controls a
  wildcard DNS zone pointing at the victim's IP gets an unlimited number of keys.
- **Test:** `a_trailing_dot_does_not_make_a_new_host` in `crates/apub/tests/audit_b.rs`.
- **Fix:** strip one trailing dot in `host_key` and `uri_host_key`, or refuse such URLs. To stop
  wildcard-DNS amplification, add a global outbound rate limit or key the limits by resolved IP.

#### B5: The FASP and registration client follows cross-host redirects and https-to-http downgrades (Low, informational)
- **Where:** `crates/apub/src/net.rs:202-213`. The redirect policy used by `http_client`
  (`crates/mwmbl_social/src/cli.rs:499`) only re-runs `check_url`, which checks IP literals, on
  each hop. The resolver still filters names, so this is not SSRF.
- **Scenario:** a registered server redirects our signed FASP calls to any public host,
  including over `http://`, even when `allow_http` is false. The RFC 9421 signature headers are
  forwarded to the redirect target. Responses still have to verify with the server's key.
- **Test:** none.
- **Fix:** use `Policy::none()` for the FASP client, or allow only same-origin https redirects.

#### F7: No warning is logged when `allow_private_addresses` or `allow_http` is on (Low)
- **Where:** `crates/mwmbl_social/src/config.rs:105-110`, applied at `cli.rs:222-223,500`.
  There is no `tracing::warn!` for these flags, unlike the `/metrics` warning at `cli.rs:429`.
  Both dev compose files set `MWMBL_SOCIAL_ALLOW_PRIVATE_ADDRESSES: "true"`
  (`dev/mastodon/docker-compose.yml:91`, `dev/lemmy/docker-compose.yml:68`).
- **Scenario:** an operator bases their production compose file on `dev/*/docker-compose.yml`
  and keeps that line. `client_builder` then returns a bare builder (`net.rs:197-199`), so the
  SSRF filter is off and nothing in the logs says so.
- **Test:** none.
- **Fix:** log a `warn!` for each flag when `serve` starts. Consider refusing
  `allow_private_addresses` unless the host in `base_url` is a `.test` name.

### Category 3: auth bypass

#### A2: A key document that the actor never lists is trusted to sign for it (High; also category 1)
- (Reviewer severity, which assumes the precondition: JSON that a user controls, served from
  the instance's own origin.)
- **Where:** `crates/apub/src/inbox.rs:150-213` (`find_public_key`, `check_key_owner`) and
  `crates/mwmbl_social/src/inbox_app.rs:257-264`. When the fetched key document isn't the
  actor, its own `owner` field is believed as long as it is on the same origin. Nothing fetches
  the owner actor to confirm that its `publicKey` lists this key id, although Mastodon does make
  that check. The fetcher doesn't check the response content type either
  (`apub/src/fetch.rs:968-991`).
- **Scenario:** on a server where users can get arbitrary JSON served from the instance's
  origin (for example, file uploads on the same domain), Eve uploads
  `{"id":"https://m.example/files/x.json","publicKey":{"id":"…/x.json#k","owner":"https://m.example/users/bob","publicKeyPem":<Eve's key>}}`.
  She then signs a Follow as Bob with `keyId=…/x.json#k`. We answer 202 and record Bob's
  opt-in.
- **Test:** `audit_a2_a_key_document_the_actor_does_not_list_signs_for_it` in
  `crates/mwmbl_social/tests/inbox.rs`.
- **Fix:** when the key document's `id` isn't the actor, fetch the owner actor and require that
  its `publicKey` (an object or an array) contains this key id with the same PEM.

#### A3: Any user on an origin can delete any other user's post from the index (High, UNRESOLVED)
- **Where:** `crates/mwmbl_social/src/activities.rs:56-58` (`speaks_for` means the same
  origin) and `:245-249` (`Delete` of another object leads to `RemoveObject`). That effect is
  applied at `inbox_app.rs:466` as `RemovePost { tombstone: true }`, with no re-fetch, and
  `writer.rs:553-573` applies it as a delete plus an object tombstone. PLAN.md:818 describes
  this behaviour as the design.
- **Scenario:** Eve on `m.example` sends a signed
  `Delete{object: https://m.example/users/bob/statuses/1}`. Bob's live post is removed and
  tombstoned, and Eve could do the same to every post on her instance. An `object` naming
  another *actor* URI on the same origin tombstones that account's URI too.
- **Test:** `audit_a3_anyone_on_an_origin_can_delete_anothers_post` in
  `crates/mwmbl_social/tests/inbox.rs`. **It PASSES**, so the finding is not demonstrated. See
  §3.
- **Fix:** act on a Delete only if the object is confirmed gone at its origin (a fetch that
  returns 404, 410 or a Tombstone, as Mastodon does), or if the sender is the stored record's
  author.

#### A4: Any user on an origin can tombstone every post by another user there with `Block` and `removeData` (High)
- (Reviewer severity. It lasts until the victim's actor is fetched again.)
- **Where:** `crates/mwmbl_social/src/activities.rs:257-267`.
  `same_origin(object_id, sender.actor)` lets anyone on the instance send this, not just an
  instance admin or the site actor.
- **Scenario:** Eve sends `{"type":"Block","actor":eve,"object":bob,"removeData":true}`. That
  adds `AddAuthorTombstone{bob}`, which hides all of Bob's posts until a later fetch of Bob's
  actor clears it (`decide.rs:157-160`).
- **Test:** `audit_a4_anyone_on_an_origin_can_tombstone_another_author` in
  `crates/mwmbl_social/tests/inbox.rs`.
- **Fix:** accept a non-Group `Block` with `removeData` only from the instance's site or admin
  actor (for Lemmy, the site actor from nodeinfo or `/`), or drop the non-Group case.

#### A5: Cavage signatures aren't bound to our host, so they can be replayed from other servers (Low)
- **Where:** `crates/apub/src/inbox.rs:106-115`. The required `host` header isn't compared
  with anything. `(request-target)` contains only the path, and `receive`
  (`inbox_app.rs:764-771`) never compares the Host header with `base_url`. Mastodon and we both
  use `/inbox` as the shared inbox.
- **Scenario:** an attacker runs a server that receives Alice's cavage-signed delivery (for
  example, because the attacker follows Alice). Within an hour, the attacker forwards the
  identical request to our `/inbox`, and it verifies. Only activities whose meaning doesn't
  depend on the recipient are affected. RFC 9421 signatures aren't affected.
- **Test:** `audit_a5_a_cavage_signature_for_another_host_verifies_here`, a unit test in
  `crates/apub/src/inbox.rs`. Run `cargo test -p apub --lib audit_a5 -- --ignored`.
- **Fix:** require the received `host` value to equal the authority of our `base_url`.

#### C4: The admin API shares the public listener (Low, design)
- **Where:** `crates/mwmbl_social/src/cli.rs:408-421`. There is a single listener (the
  Dockerfile sets `0.0.0.0:8080` at line 21), and the admin routes, the crawl routes and
  optionally `/metrics` are merged into the public router. The bearer token is the only
  protection, and 401s aren't rate-limited. `Tokens::validate` (`config.rs:129-143`) requires a
  length of at least 16 but doesn't check entropy, so `aaaaaaaaaaaaaaaa` is accepted.
- **Scenario:** an operator who sets a weak 16-character token exposes registration and
  revocation to online guessing over the public port. Revoking a registration (DELETE) cuts
  that server off.
- **Test:** none (a configuration issue).
- **Fix:** add an optional `admin_listen` address that defaults to loopback, or refuse to serve
  the admin API on the public listener unless that is explicitly allowed.

### Category 4: secret leaks

#### F1: Config parse errors echo admin and crawl tokens to stderr (Medium)
- **Where:** `crates/mwmbl_social/src/config.rs:23-27`, where `ConfigError::Parse` wraps
  `toml::de::Error`, and `:254-257`. The error is printed by `cli.rs:164`: `Config::load(...)?`
  returns it from `main`, and anyhow prints its full Display to stderr, which ends up in
  container logs or journald.
- **Scenario:** the operator writes `admin_tokens = "s3cret..."` as a string (the way the env
  override `MWMBL_SOCIAL_ADMIN_TOKENS` is written), or misspells the key
  (`admin_token = [...]`, which `deny_unknown_fields` at `config.rs:41` refuses). The Display of
  `toml`'s error quotes the offending source line, and serde's `invalid type: string "..."`
  quotes the value. So the token goes to the log on every restart attempt. The redacting
  `Debug` on `Tokens` doesn't help here.
- **Tests:** `a_mistyped_token_list_is_not_echoed_in_the_config_error` and
  `a_misspelt_token_key_is_not_echoed_in_the_config_error` in
  `crates/mwmbl_social/tests/audit_f.rs`. Run
  `cargo test -p mwmbl_social --test audit_f -- --ignored`.
- **Fix:** reduce the toml error to its line, column and kind of error (`e.span()` plus a
  generic reason), and never print the Display of `source`. Alternatively, load tokens only
  from the environment or from a separate file.

#### F2: The SQLite store holding every FASP registration's Ed25519 private key is world-readable (Medium)
- (Reviewer severity: Medium for bare-metal or shared-host installs, Low inside the container.)
- **Where:** `crates/store/src/lib.rs:71-75` calls `Connection::open` without setting a mode,
  so the file gets SQLite's default of 0644 minus the umask. `crates/mwmbl_social/src/cli.rs:555`
  creates `data_dir` with `create_dir_all`, giving 0755. The key is in the `our_private_key`
  column (`store/src/schema.rs:15`). The `-wal` and `-shm` files are affected too. By contrast,
  `keys.rs:35-55` uses 0700 and 0600 for `actor.pem`. In the main checkout, F observed
  `data/mwmbl-social.sqlite`, `-wal` and `-shm` as `-rw-r--r--`.
- **Scenario:** any local user, or another service on the host, reads the database and takes a
  registration's Ed25519 seed. They can then sign FASP requests to that Mastodon server as us
  (`fasp/src/client.rs:91`) or forge our signed responses (`fasp/src/http/respond.rs:88`).
- **Test:** `the_store_holding_registration_keys_is_private` in
  `crates/mwmbl_social/tests/audit_f.rs`.
- **Fix:** create `data_dir` with mode 0700 and create the database file with mode 0600
  (OpenOptions mode) before `Connection::open`. Alternatively, set `umask(0o077)` at start-up.

#### F8 = D9: Debug logging writes whole remote JSON documents to the log (Low)
- (F rated this Low. D rated it Informational, because actor documents are public.)
- **Where:** `crates/mwmbl_social/src/inbox_app.rs:344-346`. The catch-all arm
  `other => tracing::debug!(actor, outcome = ?other, ...)` also matches
  `FetchOutcome::Found { json }` when `json.id != actor`. `FetchOutcome`'s Debug prints the
  whole `json` (`apub/src/fetch.rs:219-224`).
- **Scenario:** the log level includes `mwmbl_social=debug`, which both dev compose files set
  (`RUST_LOG: info,mwmbl_social=debug,apub=debug`). A stranger's inbox POST names an actor URI
  whose server returns a document with a different `id`, such as a non-public Note. The whole
  document, up to the fetch body cap, is logged. That breaks the project rule against logging
  post content.
- **Test:** none.
- **Fix:** log only the variant name or the status for a non-matching `Found`.

### Category 5: remote DoS

#### E4: A stranger can grow the tombstone table without limit, and it is reloaded in full every 30 s (High; also category 1)
- (Reviewer severity. Lead note: E4 relies on the same path as A3, where a signed Delete of any
  object on the sender's origin is acted on. A3's test doesn't reproduce that, and E4 has no
  test, so E4 stays unconfirmed until A3 is resolved. See §3.)
- **Where:** `crates/mwmbl_social/src/activities.rs:245` accepts a `Delete` of any object on
  the sender's origin, and the sender doesn't have to be followed. `writer.rs:559-561` adds the
  tombstone before checking the index (see also `writer.rs:500-505`). `writer.rs:456-458` and
  `:722-739`, triggered from `cli.rs:60,314`, reload the whole table every 30 s
  (`ReloadTombstones`). Tombstones are only cleared after a full sweep cycle (`sweep.rs:203`,
  6 days by default, `config.rs:168`).
- **Scenario:** an attacker who runs one server, or controls one account on a server, sends
  signed `{"type":"Delete","actor":"https://evil/u","object":"https://evil/o/<n>"}` to `/inbox`
  with a fresh `n` each time. The key is fetched once and then cached, so each request costs us
  only an RSA verify. Each request adds a SQLite row, a `HashSet` entry in both the writer and
  the search process's copy, and an index delete. At 100 requests per second that is about 52M
  rows before the first expiry. Every 30 s the writer thread re-reads all of them while holding
  the tombstone write lock. That blocks every search (`search.rs:89`), and real removals wait
  behind it in the writer queue.
- **Test:** none (it needs the signed-inbox harness).
- **Fix:** add tombstones only for URIs that were indexed or queued (the writer already does an
  index lookup), or cap tombstones per origin host. Make the reload incremental.

#### E1: One unauthenticated search request costs one page read per word of a ~64 KiB query (High)
- **Where:** `crates/tokenize/src/lib.rs:151-165`. `query_keys` takes every non-stopword
  token, every bigram, and every hashtag or mention, with no cap.
  `crates/social_index/src/search.rs:29-61` (`gather`) does a pread, a zstd decompression and a
  JSON parse of a 4 KiB page for each key. `crates/mwmbl_social/src/http.rs:139-150` doesn't
  check the length of `q`. At `http.rs:153-167`, each search runs in `spawn_blocking` with no
  concurrency limit, and tokio's blocking pool has 512 threads.
- **Scenario:** `GET /api/v1/search/posts?q=w0+w1+...+w9999`, about 60 KB, fits under the
  `http` crate's URI limit of 65,534 bytes. It produces 20,000 keys and so 20,000 page reads
  and decodes. A few hundred such requests in parallel fill the blocking pool, and every search
  and every other `spawn_blocking` task stalls.
- **Test:** `query_keys_are_bounded` in `crates/tokenize/tests/audit_e.rs`. Run
  `cargo test -p tokenize --test audit_e -- --ignored`.
- **Fix:** cap the length of `q` (say 512 bytes) and the number of query keys (say 32), and add
  a concurrency limit for searches.

#### A6 = B4 = E7: No inbound or outbound concurrency limits or timeouts, and inbox key fetches run inline (Medium; also category 2)
- (This merges A6, B4 and E7. P4's lead L3, that unsigned inbox POSTs cause outbound key
  fetches, is covered here and in B2 and B3.)
- **Where:**
  - `crates/apub/src/fetch.rs:509-532`: `admit` has only a per-host semaphore and no global
    cap (B4).
  - The inbox handler reads the body (up to 256 KiB, `inbox_app.rs:54,740`) and then awaits
    `verify -> fetch_key -> fetcher.fetch` inline (`inbox_app.rs:243,316,772`), queued behind
    the per-host semaphore (`fetch.rs:618-640`). Failed key fetches aren't cached (A6).
  - `crates/mwmbl_social/src/cli.rs:435` uses `axum::serve(listener, app)`. axum 0.8.9 builds
    hyper with no `Timer`, so hyper's default 30 s `header_read_timeout` never takes effect.
    There is no tower `TimeoutLayer`, `ConcurrencyLimitLayer` or connection cap anywhere. FASP
    buffers up to 1 MiB for any path before auth (`fasp/src/http/mod.rs:30,197-201`), and crawl
    also buffers up to 1 MiB (`crawl_app.rs:42,172`) (E7).
- **Scenario:**
  - (a) An attacker opens N concurrent inbox POSTs. Each names a key on a different slow host
    that the attacker controls (a wildcard zone or slow-loris servers), or on one host that
    answers just under the client timeout, so no backoff starts. Each request holds its body
    and an outbound socket, and a fetch can take up to 20 s × 2 (the scheme retry) × 4 hops =
    160 s. 10k requests hold about 2.5 GB. File descriptors and memory run out, and the worker,
    deliveries and follows stall, because they share the same fetcher.
  - (b) Slowloris: the attacker opens 50k connections with `Content-Length: 1048576` to a FASP
    path and trickles one byte a minute. Nothing ever times out.
  - (c) Each forged request that names a real victim's keyId evicts the cached key
    (`inbox_app.rs:309-314`) and forces a refetch from the victim's host. Per-host pacing caps
    that at about 4 GET/s per victim host, so the traffic is 1:1, not amplified.
- **Test:** `outbound_requests_have_a_global_limit` in `crates/apub/tests/audit_b.rs`. It
  covers part (a) only: with 64 slow hosts, all 64 connections are open at once, and the test
  asserts at most 32. Parts (b) and (c) have no test.
- **Fix:**
  - Run the cheap checks (Date and created skew, the digest against the body, parsing the
    signature header) before any fetch.
  - Add a global semaphore in `Fetcher`, with separate pools for inbox key fetches and worker
    fetches, and a per-request deadline that covers all hops.
  - Refetch an evicted key at most once per N minutes.
  - Serve through `hyper_util` with a `TokioTimer` and a header-read timeout, and add
    `TimeoutLayer` and a global `ConcurrencyLimitLayer` or `LoadShed`.
  - On the inbox, reject a `Content-Length` over the cap before buffering the body, as FASP
    does.

#### B3 = E6: Per-host state grows without limit for hosts a stranger names (Medium; breaks principle 1)
- **Where:** `crates/apub/src/fetch.rs:377,471-479`. `Fetcher::hosts` gets an entry for every
  host that is parsed, including refused addresses (`admit` at `:559`/`:631-632`), and nothing
  ever removes one. `note_scheme` (`:794`) remembers a scheme for any status below 500 except
  400 and 429, so a 404 counts. A DNS or connect failure sets a backoff (`:927-928`).
  `persist_hosts` (`crates/mwmbl_social/src/worker.rs:999-1010`, scheduled from `cli.rs:62`)
  writes one autocommit upsert per known host every 60 s. Rows are never deleted from `hosts`
  (`store/src/hosts.rs:189-209`). The same pattern appears in `context.rs:76` (the software
  cache) and `crawl.rs:105-107` (the robots `memory` and `fetching` maps).
- **Scenario:** a stranger sends inbox POSTs with `actor` and `keyId` on
  `https://<random>.evil.example/`, where the wildcard zone serves 404s or doesn't resolve. No
  valid signature is needed, because the key fetch comes first. Each POST adds a permanent
  `HostEntry` and a permanent `hosts` row. After 10^6 requests, `persist_hosts` does a million
  SQLite writes a minute, each in its own transaction with an fsync, while holding the store
  connection.
- **Test:** `per_host_state_is_bounded` in `crates/apub/tests/audit_b.rs`. It names 300 hosts,
  and 293 are kept.
- **Fix:** add an LRU cap on `Fetcher::hosts`, or evict entries with no backoff and no scheme
  after a TTL. Persist only hosts we have our own reason to keep (registered, followed,
  subscribed or queued), and batch the writes into one transaction.

#### C3 = E5: `queue_cap` doesn't limit announces (Medium; breaks principle 1)
- **Where:** `crates/store/src/queue.rs:141-176`. When the queue is full, an `Announce` item
  evicts one row with `priority > 0`. If there is no such row, it is inserted anyway, and
  `fasp_app.rs:229-231` documents this. There are two sources of announces:
  - FASP announcements from a registered peer (C3).
  - A `Follow` of our actor from *any* verified actor (E5). It yields `Effect::Fetch`, which
    `inbox_app.rs:449-459` queues as `FetchReason::Announce`
    (`activities.rs:192-212`). The Follow also records a `follow_optins` row and makes us follow
    back, and after that the actor's `Create`s queue announces too (`activities.rs:220`).
- **Scenario:**
  - A registered peer posts announcements of 1000 fresh URIs per request, which don't have to
    exist, as many times as it likes. Each URI becomes a SQLite row, and the disk fills.
  - Alternatively, an attacker with wildcard DNS, or with one server and many actors, sends
    Follows. Each adds a queue row, `follow_optins` and `following` rows, and an outbound
    Accept and Follow. The attacker's rows compete with real FASP announcements at priority 0.
- **Tests:** `announcements_do_not_grow_the_queue_past_its_cap`
  (`crates/mwmbl_social/tests/audit_c.rs`: cap 10, 100 rows) and
  `announces_cannot_grow_the_queue_past_its_cap` (`crates/store/tests/audit_e.rs`, run
  `cargo test -p store --test audit_e -- --ignored`).
- **Fix:**
  - Set a hard ceiling (for example 2 × cap), past which announces are dropped or deferred.
  - Give each registration its own share of the queue.
  - Give inbox-originated fetches their own reason and priority, with a share of the cap like
    `CRAWL_QUEUE_PERCENT`.

#### E2: An account record is stored under one key per username token, with no limit (Medium)
- **Where:** `crates/social_index/src/keys.rs:125-130`. `tokenize(user)`, `tokenize(host)` and
  `special_keys("@acct")` aren't capped by `KeyConfig`, unlike `field_keys`.
  `crates/mwmbl_social/src/records.rs:121` copies `acct` without truncating it.
  `crates/apub/src/objects.rs:250,277` puts no length limit on `preferredUsername`.
- **Scenario:** a hostile server serves a `discoverable: true` actor with
  `preferredUsername: "a0-a1-...-a1999"` and `followers.totalItems: 10^12`. It can get the
  actor fetched by sending a Follow (see C3 = E5). The writer then writes a copy of the record
  to about 2,000 account pages, which costs about 2,000 page rewrites on the single writer
  thread. On full pages, the actor's huge score makes it evict real accounts
  (`index.rs:411`).
- **Test:** `account_keys_are_bounded` in `crates/social_index/tests/audit_e.rs`. Run
  `cargo test -p social_index --test audit_e -- --ignored`.
- **Fix:** truncate `preferredUsername` and `acct` (Mastodon caps usernames at 30 characters),
  and cap username tokens as `first_tokens` does.

#### E3: `special_keys` is quadratic in distinct hashtags and mentions (Medium, UNRESOLVED)
- **Where:** `crates/tokenize/src/special.rs:44-49` does a linear scan for each key
  (`if !keys.contains(&key)`). This runs on every search query (`lib.rs:163`) and on stored
  fields. Similar O(n²) dedupes are at `apub/src/objects.rs:224`,
  `social_index/src/keys.rs:58` and `mwmbl_social/src/records.rs:48-55`.
- **Scenario:** an unauthenticated search for 60 KB of `#t0 #t1 ...` (about 10k hashtags)
  costs about 5×10^7 string comparisons. A fetched object (up to 1 MiB) with about 30k
  `Hashtag` entries costs about 4.5×10^8 comparisons in `hashtags()`.
- **Test:** `special_keys_is_not_quadratic` in `crates/tokenize/tests/audit_e.rs`. It requires
  60k hashtags to take under 500 ms. **It PASSES**, so the finding is not demonstrated. See §3.
- **Fix:** dedupe with a `HashSet`, cap special keys on queries, and cap the `tag` array in
  `parse_post` (say at 64).

#### E8: The inbox moves writer back-pressure into unbounded spawned tasks (Low-Medium; also category 1)
- **Where:** `crates/mwmbl_social/src/inbox_app.rs:776-777` calls
  `tokio::spawn(app.process(...))` for each accepted delivery, with no bound. The task goes
  through `:395-397` to `writer.rs:234-237`, which waits on a bounded `mpsc` channel of 256.
- **Scenario:** a flood of signed Deletes (see E4) is answered 202 at once. The work goes into
  detached tasks, and each holds the full parsed activity (up to 256 KiB) while it waits for the
  writer channel. Memory grows with the backlog, and a real `Delete` waits behind all of it.
- **Test:** none.
- **Fix:** bound the inbox work queue with a semaphore before `spawn`, and answer 503 or 429
  when it is full so that honest senders retry.

#### C5 = L4: `/metrics`, when enabled, is unauthenticated and does an unbounded store read on each scrape (Low; also category 4)
- **Where:** `crates/mwmbl_social/src/metrics.rs:79-80`. Each `GET /metrics` calls
  `list_tombstones()`, which loads every tombstone, and `list_registrations` plus a capability
  query per registration. It is served on the public listener (`cli.rs:422-429`). It is off by
  default, and turning it on logs a warning.
- **Scenario:** with `metrics = true` and no proxy rule, strangers can scrape it in a loop. Each
  scrape takes the store mutex and reads whole tables, and the registration counts reveal how
  many servers use the instance. No hosts, URIs, ids or tokens leak.
- **Test:** none.
- **Fix:** use COUNT queries instead of loading lists, and/or serve metrics on a separate
  listener.

#### E10: robots.txt matching is O(rule bytes × path length) for a hostile host (Low)
- **Where:** `crates/mwmbl_social/src/robots.rs:107-142` (single-star backtracking),
  `robots.rs:220-227` (`allows` runs every rule), and `crawl.rs:29` (robots.txt files up to
  512 KiB are allowed).
- **Scenario:** a host whose objects we fetch, which can be any stranger once we follow them
  back (see C3 = E5), serves 120 rules of `/*aaaa…(4000 characters)b` and object URIs with
  4,000-character paths. Each robots check then costs about 10^9 byte comparisons, on the async
  runtime.
- **Test:** `robots_matching_is_cheap_on_hostile_rules` in
  `crates/mwmbl_social/tests/audit_e_robots.rs`. Run
  `cargo test -p mwmbl_social --test audit_e_robots -- --ignored`.
- **Fix:** cap the number of rules and the pattern length, or use a linear-time wildcard
  matcher.

#### F9: The actor key file isn't written atomically (Low; a little of category 4)
- **Where:** `crates/mwmbl_social/src/keys.rs:100-104`. The file is created with `create_new`
  (0600) and then written with `write_all`, at the final path, with no temp file or rename.
- **Scenario:** the process is killed between the create and the write (by OOM, a
  `docker stop` timeout or a power loss). `actor.pem` is left empty or partial, and every later
  start fails at `keys.rs:62-67` until someone deletes the file by hand. This fails closed.
- **Test:** none.
- **Fix:** write to `actor.pem.tmp` (0600, `create_new`), fsync it, `rename` it into place, and
  fsync the directory.

### Category 6: supply chain

#### F3: GitHub Actions are pinned by mutable tags, in the job that holds `packages: write` (Medium)
- **Where:** `.github/workflows/build.yml:12-14` (`permissions: packages: write`) and the
  actions at `:16,19,24,33,40`: `actions/checkout@v4`, `docker/setup-buildx-action@v3`,
  `docker/metadata-action@v5`, `docker/login-action@v3` and `docker/build-push-action@v5`.
  `.github/workflows/ci.yml:12-15` uses `dtolnay/rust-toolchain@1.100.0` and
  `Swatinem/rust-cache@v2`.
- **Scenario:** a tag is moved or compromised (as happened with tj-actions/changed-files). The
  action then runs with a `GITHUB_TOKEN` that can push to `ghcr.io/mwmbl/social`, and it pushes
  a backdoored `:main`. README.md:36-43 tells operators to run that image, and it holds their
  actor key and FASP keys.
- **Test:** none.
- **Fix:** pin every `uses:` to a full commit SHA, with a version comment, and let Dependabot
  bump them. A draft is in `audit/ci-draft/`.

#### F4: `ci.yml` has no `permissions:` block (Low)
- **Where:** `.github/workflows/ci.yml`. The file has no top-level or job-level
  `permissions:`.
- **Scenario:** on `push` to any branch, the job gets the repository's default token scope.
  That scope is read/write for repos created before February 2023 or using the legacy setting.
  Any build script or proc macro in the dependency tree could then write to the repo. Whether
  this applies depends on a repo setting the reviewer couldn't see.
- **Test:** none.
- **Fix:** add `permissions: contents: read` at the top level.

#### F5: Same-repo PR builds are pushed to GHCR, although a comment says they aren't (Low)
- **Where:** `.github/workflows/build.yml:43-44`. The comment says "Don't push pull-request
  builds to GHCR", but the condition is
  `github.event_name == 'push' || github.event.pull_request.head.repo.full_name == github.repository`.
- **Scenario:** any PR from a branch in this repo publishes `ghcr.io/mwmbl/social:pr-N` and
  `:sha-<long>` from unreviewed code, under the official image name. To someone pinning by SHA,
  `sha-` tags from `main` and from PRs look the same. PRs from forks can't push.
- **Test:** none.
- **Fix:** use `push: ${{ github.event_name == 'push' }}`, or push PR images to a separate
  repository name.

#### F10: `.dockerignore` is minimal, and the base images are pinned by tag only (Low; also category 4)
- **Where:** `.dockerignore` lists only `target`, `data`, `dev` and `.git`. `Dockerfile:3,10`
  use `rust:1.100-bookworm` and `debian:bookworm-slim` without digests, and `Dockerfile:7` runs
  `COPY . .`.
- **Scenario:** a local `docker build .` from a working checkout sends `.claude/` (including
  worktrees and `settings.local.json`) into the build stage, along with any root-level
  `config.toml` holding tokens, `*.pem`, `*.sqlite` and `.env` files. The published image
  doesn't contain them, because the final stage copies only the binary. But the build-stage
  layer does, and it goes to any `cache-to` exporter. `build.yml:49` uses `type=gha,mode=max`,
  but CI builds from a clean checkout, so nothing is exposed there today. Unpinned base images
  mean a rebuilt image can change under the same Dockerfile.
- **Test:** none.
- **Fix:** add `.claude`, `*.pem`, `*.sqlite*`, `*.tinyindex`, `.env*`, `config*.toml` and
  `*.md` to `.dockerignore`, or allow-list only `Cargo.*`, `crates/` and `rust-toolchain.toml`.
  Pin both `FROM` lines by `@sha256:`. A patch is in `audit/ci-draft/Dockerfile.base-pins.patch`.

### Other: integrity

#### C2: A URI repeated in one `trending` announcement counts once per repeat (Medium in hosted mode, otherwise Low)
- **Where:** `crates/mwmbl_social/src/fasp_app.rs:451-462`. `record_trending` calls
  `trends.record(uri)` for every entry, with no dedupe (`trends.rs:87-102`).
  `announcement.rs:106` `clean_uris` doesn't dedupe either.
- **Scenario:** a single signed announcement from any registration with `objectUris: [P × 40]`
  takes post P to rank 100. Trends are pooled across registrations, so P becomes the top
  trending post served to every other server, and its author is pushed into everyone's
  `follow_recommendation` results (`fasp_app.rs:584-597`). Repeating the URI makes inflation
  about 40 times cheaper, and with self-serve sign-up the "admin-approved" premise doesn't hold.
- **Test:** `a_uri_repeated_in_one_trending_announcement_counts_once` in
  `crates/mwmbl_social/tests/audit_c.rs`. It fails with `rank: 100`, where 21 is expected.
- **Fix:** dedupe `object_uris` in `clean_uris`. For trends, count distinct registrations per
  URI per hour, or cap the events per registration.

## 3. Unresolved

The owner must check these before relying on this report for them.

- **A3 (reported High): the test PASSES.** The code path looks vulnerable. `speaks_for` means
  the same origin (`activities.rs:56-58`), so a Delete of another object on that origin becomes
  `RemoveObject` (`activities.rs:245-249`), and `inbox_app.rs:466` writes `RemovePost` with
  `tombstone: true` and no re-fetch. Yet `audit_a3_anyone_on_an_origin_can_delete_anothers_post`
  still finds Bob's post after Eve's Delete. There are three possible explanations:
  - an earlier guard blocks the activity, for example in `handle` or `dispatch`, where the
    `object` is a `{"id","type":"Tombstone"}` object rather than a string;
  - `settle()` returns before the writer applies the removal;
  - the search side doesn't see the change yet. E's report says the search process keeps its
    own tombstone copy and reloads it every 30 s (`cli.rs:60,314`).

  Until this is settled, treat A3 as possibly live and fix it anyway (the fix is cheap).
- **E4 (reported High): no test, and it depends on A3's path.** If whatever stops A3's test
  also stops a Delete of a fresh same-origin URI, E4's growth path is closed. If it is only
  timing, E4 stands.
- **E3 (reported Medium): the test PASSES.** On the test machine, 60k hashtags took less than
  the 500 ms threshold, so the quadratic cost isn't demonstrated at that size. The
  `contains` scan in the code is still O(n²), and the threshold may simply be too loose.
- **Findings with no test (code reading only):** E9, D6, D7, D8, B5, F7, C4, F8 = D9, E8, C5,
  F9, F3, F4, F5 and F10, plus parts (b) and (c) of A6 = B4 = E7. Of these, E9 is conditional
  on a panic that no reviewer found a way to reach. F4 depends on a GitHub repo setting that
  couldn't be checked offline.
- **D2 and D6 are races.** D's tests show them by ordering, not by timing under the real run
  loop.
- **A2's precondition is unchecked.** No one checked whether real Mastodon, GoToSocial,
  Misskey or Akkoma instances serve user-controlled JSON from the instance origin.
- **P4 leads, not confirmed:**
  - L1: recently released versions of rsa 0.10.0 (2026-11-08), rusqlite 0.43.0, zstd 0.13.5
    and toml 1.2.1. rsa handles the actor key and signature checks, so its 0.10.0 diff deserves
    a look.
  - L2: the unsigned self-Delete accepts a 404 as "gone", as well as a 410
    (`activities.rs:164`, `fetch.rs:960`). While a host briefly 404s a real actor URI, anyone
    can remove that account, drop its follow opt-in and add an author tombstone. This fails
    safe for consent. A judged that this path holds, because it only removes an actor whose own
    URI is gone (§6), but nobody checked the transient-404 case or how long the tombstone lasts.

## 4. Rejected findings

None were rejected, because Phase 2 didn't run. The reviewers themselves withdrew or
downgraded these items:

- B: WebFinger for `forget` reads the whole body before its 1 MiB check (`cli.rs:736-742`). This
  is admin-triggered and bounded by the 20 s timeout, so B rated it Low and didn't report it.
- C: FASP has no nonce or replay cache, so requests can be replayed within 5 minutes. Every
  endpoint is idempotent, so C didn't count it as a finding.
- D: unregistering or deactivating a server doesn't purge its content. This is a documented
  decision (PLAN.md open question 3), not a bug.
- D9: rated Informational because actor documents are public. Merged into F8.
- A6(c): the key-eviction reflection is 1:1, not amplified.
- F2: rated Low inside the container. E9: conditional, because no reachable panic was found.
  C2: Low outside hosted mode.
- F: the TLS-roots question was resolved. The system store is used and `SSL_CERT_FILE` is
  honoured.
- P3: the `toml_parser` unsafe code is compiled out (its `unsafe` feature is off), and the
  duplicate `syn` versions are build-time only.

## 5. Security claims checked and found to hold

**Signatures and inbox (A)**
- The key host must match the activity actor's origin before any fetch
  (`inbox_app.rs:292-297`, covered by `forged_deliveries_get_401_and_change_nothing`).
- On a cache miss, the key owner must equal `actor` and be on the key's origin
  (`inbox.rs:198-213`, `inbox_app.rs:261`). The cache-hit path is broken (A1).
- `handle` drops activities whose `actor` isn't the verified sender, and activities from our
  own actor (`activities.rs:134-139`). Wrapped activities are acted on as the sender, which is
  a followed Group.
- Follow, Undo{Follow}, Accept and Reject only affect the sender's own rows, and Accept and
  Reject must match our Follow (`activities.rs:275-330`).
- Algorithm confusion isn't possible:
  - cavage accepts only `rsa-sha256`, `hs2019` or no algorithm, and always verifies RSA PKCS#1
    v1.5 (`cavage.rs:410-417`);
  - in RFC 9421 the key variant fixes the algorithm, and a declared `alg` must equal it
    (`rfc9421.rs:447-455`, `inbox.rs:84-87`);
  - Ed25519 can't be substituted for an RSA key.
- Required components:
  - cavage requires `(request-target)`, `host`, `digest`, and either `date` or `(created)`
    (`inbox.rs:111`, `cavage.rs:418-442`);
  - RFC 9421 requires `@method`, `content-digest`, `created`, and either `@target-uri` or
    `@authority`+`@path` (`inbox.rs:88-103`, `rfc9421.rs:423-431`);
  - FASP requires `@method`, `@target-uri` and `content-digest` (`fasp/src/http/auth.rs:94-98`).
- Digests are always checked against the body, with SHA-256 required (`integrity.rs:541-601`).
  If a header appears several times, the instances are combined.
- Date and created must be within ±3600 s, and `expires` is honoured (`mod.rs:121-143`,
  `cavage.rs:428-449`, `rfc9421.rs:430-438`).
- The parsers reject duplicate params, unterminated quotes, bad escapes and an empty `headers`
  (`cavage.rs:354-392`). The SF parser uses checked slices, and its lookup insists on
  `signer.key_id` (`inbox.rs:94-96,112`). CR/LF in header values is refused (`mod.rs:77`), and
  non-UTF-8 headers are dropped (`inbox_app.rs:759-763`).
- When verification fails, nothing is processed or logged beyond the actor and key-id URIs
  (`inbox_app.rs:788-791`). The only exception is the self-Delete with a gone key, which removes
  only an actor whose own URI is gone (`inbox_app.rs:778-787`, `activities.rs:164-171`,
  `tombstones.rs:37-44`; see L2).
- The `rsa` 0.10 crate bounds RSA keys at 4096 bits.
- The RFC 9421 known-answer tests use the RFC's real B.1.1, B.1.2 and B.1.4 keys and the B.2.1,
  B.2.3 and B.2.6 signatures (`crates/apub/tests/fixtures/`, `tests/signatures.rs:173-238`).
  The cavage C.1-C.3 vectors are present too. These were checked against memory, not a fresh
  copy of the RFC.
- FASP: the keyid maps to a registration, revoked registrations are refused, only Ed25519 is
  accepted, and the digest is checked (`fasp/src/http/auth.rs:60-113`).

**Outbound requests and SSRF (B)**
- Every non-test reqwest client is built through `client_builder` (`fetch.rs:445`,
  `cli.rs:500,720`).
- `PublicResolver` filters the addresses actually connected to, so DNS rebinding doesn't get
  through (`net.rs:173-189`). IP literals are caught by `check_url` (`fetch.rs:539`,
  `registrations.rs:51,94`, `cli.rs:713`, and every redirect hop at `net.rs:209`).
- `is_public` (`net.rs:59-115`) refuses every private, reserved, mapped and transition range,
  including NAT64, 6to4 and Teredo. Odd IPv4 literal forms are normalised first, and
  `localhost` is refused (`net.rs:323-327,373`).
- Fetcher redirects are manual, at most 3, same `host_key` only, and the only scheme change
  allowed is to https (`fetch.rs:701-774`). `deliver` follows no redirects.
- Only https is accepted, plus http when `allow_http` is set (`fetch.rs:547`). Userinfo is
  rejected in crawl submissions and server URLs (`crawl_app.rs:64-77`, `registrations.rs:47`).
- Bodies are capped at 1 MiB while streaming (`fetch.rs:1003`), with a 20 s total timeout. FASP
  and registration have their own caps and timeouts.
- No decompression: reqwest is built with `default-features = false, features = ["rustls"]`.
- `allow_private_addresses` defaults to false (`config.rs:184`, `fetch.rs:160`). Only the dev
  compose files turn it on.
- Delivery inboxes must be on the actor's origin (`inbox_app.rs:725`), and a key must be on the
  actor's origin before it is fetched (`inbox_app.rs:290`).

**FASP, registration and admin (C)**
- There is no public registration endpoint. Registrations are created only outbound
  (`fasp/src/registration.rs:205`), from the CLI or the admin API behind the token
  (`admin_app.rs:233-236`).
- FASP keyids are random 24-character `serverId`s (`registration.rs:186-190`), the resolver only
  returns the key for that id (`auth.rs:96-98`), and `created` must be within ±300 s.
  `@target-uri` is rebuilt from the base URL, never from `Host` (`auth.rs:30-37`).
- Registrations are isolated from each other. Source ids must belong to the registration
  (`handlers.rs:139-158`), and a `delete` event only causes a re-fetch from the origin
  (`fasp_app.rs:798-806`).
- Re-registration replaces only revoked rows, or stale pending rows, and the replacement gets a
  new key, serverId and id (`registration.rs:46-55,232-246`, `schema.rs:166-190`).
- Registration URLs are checked against non-public addresses before anything is sent
  (`registrations.rs:87-104`).
- Tokens are compared as SHA-256 digests in constant time (`operator.rs:79-89`), redacted in
  Debug, and only accepted as `Authorization: Bearer`. The admin and crawl tokens are separate,
  and with no tokens configured the routes answer 404.
- Registration private keys are never returned; the JSON has only the fingerprint
  (`registrations.rs:118-136`).
- Metrics labels are fixed names, with no host, URI, id or token, and `/metrics` is unrouted
  when it is off.
- Crawl submissions take only URIs (`deny_unknown_fields`, `crawl_app.rs:46-50`), never object
  JSON.
- The routers can't overlap silently, because axum `merge` panics at start-up.

**Consent (D)**
- Only `to` counts for public addressing, so unlisted, followers-only and direct posts are
  refused (`apub/src/consent.rs:20-24,128-132`).
- `attributedTo` must resolve to exactly one id, which must equal the author's `id` on the same
  origin (`consent.rs:190-203`). Authors are never fetched off the post's origin. The
  post-refusal path is broken (D3).
- Boosts are never consent. Delivered JSON is never indexed: objects are always re-fetched from
  their origin.
- An explicit `false`, `null` or string flag always means no (`consent.rs:120-126`,
  `context.rs:37-40`).
- The Lemmy rule follows the author's host, and nodeinfo is read only from the actor's own
  origin (`apub/src/nodeinfo.rs:31,110`).
- The follow opt-in is read on every decision (`context.rs:114-126`), and `Undo{Follow}`
  deletes it and adds an author tombstone.
- Forgotten URIs are refused by `IndexPost`, `IndexAccount` and `ClearAuthorTombstone`
  (`writer.rs:532,580,630`), reloaded at start-up, and refused in crawl submissions
  (`crawl_app.rs:135-145`).
- Search, trends and recommendations all filter with `Tombstones::blocks`.
- A change to the tokeniser or key config is refused when the index is opened
  (`social_index/src/index.rs:97-109`).
- Tombstones are retired only after a full pass of both indexes (`sweep.rs:190-207`).
- The queue holds only carried index records, the inbox stores no activities, and fetch errors
  carry no bodies.

**Hostile input (E)**
- There are no explicit panics, `unsafe` or indexing outside tests. E's inventory found no
  reachable panic, and every reachable arithmetic site is bounded.
- zstd page bombs are bounded (`tinyindex/src/page.rs:76-110`).
- There are no response decompression bombs, and every fetch body is capped.
- serde_json's 128-level recursion limit is active.
- No regex runs on untrusted input.
- The HTML-to-text converter is linear (`objects.rs:439-578`).
- Post keys are bounded at 30 (`tokenize/src/lib.rs:46-53,129-145`).
- Search `limit` is clamped to 100.
- The inbox key and actor caches are bounded at 10,000 entries with a TTL
  (`inbox_app.rs:58,96-109`).
- The writer channel is bounded at 256.
- Every POST handler has a body cap: inbox 256 KiB, FASP and crawl 1 MiB, admin 16 KiB.

**Secrets and deployment (F)**
- The actor key is generated with the OS RNG and written 0600 with `create_new` into a 0700
  directory, and a loose existing file gets a warning (`keys.rs:35-82`).
- The RSA and Ed25519 signing keys and `Registration` redact themselves in Debug, and
  `Registration` isn't `Serialize`.
- Tokens are never logged: there is no TraceLayer and no `#[instrument]`. Token lists from the
  environment never reach an error message.
- The Docker image is multi-stage, runs as non-root `10001:10001`, and can only write `/data`.
  Outside Docker the default `listen` is `127.0.0.1:8080` (`config.rs:150`). `allow_http` and
  `allow_private_addresses` default to false.
- The workflows have no `pull_request_target` and no expressions inside `run:`, and fork PRs
  can't push.
- `dev/` holds only throwaway credentials on loopback-bound networks.
- `data/` has never been committed. Git history contains only the published RFC and cavage test
  keys.

## 6. Tool results

- **cargo audit** (0.23.0, advisory DB of 2026-11-19, 256 crates): 0 vulnerabilities and 0
  warnings.
- **cargo deny** (`cargo deny --locked check`, with the new `deny.toml`): advisories, licenses
  and sources are ok.
  - **bans FAILED**, but only on `wildcard` path dependencies in `mwmbl_social`, `fasp` and
    `social_index`. This is a config issue, not a supply-chain finding. `deny.toml` already sets
    `allow-wildcard-paths = true`, but cargo-deny applies that only to crates that aren't
    published. Add `publish = false` to the workspace crates' `Cargo.toml` files.
  - There is one duplicate, `syn` 2 and 3, which is build-time only.
  - 7 entries in the licence allow list are unused and can be trimmed.
- **unsafe:** the workspace crates contain no `unsafe` at all, but none of them has
  `#![forbid(unsafe_code)]`. P3 suggests adding it so that this is guaranteed rather than
  accidental.
  - In dependencies, P3 counted `unsafe` with grep. The heaviest users are expected for their
    kind: aws-lc-sys FFI, tokio, libc, rusqlite and bundled SQLite, hashbrown, memchr, zstd and
    curve25519-dalek.
  - P3 checked the odd ones and found nothing unusual: `toml_parser`'s unsafe code is compiled
    out, `utf_types` and `cpubits`/`ctutils`/`cmov` are expected, `openssl-probe` only calls
    `set_var`, and the `rustls-platform-verifier` FFI is for Android only.
  - **cargo-geiger was not run.**
- **Build scripts and proc-macros:** all 249 registry packages come from crates.io, with no git
  sources. The 14 proc-macro crates are all well known.
  - No build script makes network calls. aws-lc-sys, libsqlite3-sys and zstd-sys compile bundled
    C code, and the rest probe the compiler or set cfg flags.
  - The TLS crypto provider is aws-lc-rs, a large C dependency, not ring (P4 L5). `ring`,
    `quinn` and `jni` are in the lockfile but aren't compiled for Linux.
- **CI drafts** (under `audit/ci-draft/`; **not applied**, and `.github/` is unchanged):
  - `workflows/ci.yml` adds a supply-chain job (`cargo audit --deny warnings` and
    `cargo deny --locked check`), a daily schedule, `permissions: contents: read`,
    `persist-credentials: false` and `--locked`.
  - `workflows/build.yml` SHA-pins every action and adds provenance and an SBOM.
  - The action SHAs are **placeholders**. Fill them in with `resolve-pins.sh`.
  - `Dockerfile.base-pins.patch` pins the base images by digest, also with placeholders.
  - `dependabot.yml` covers cargo, `/fuzz`, github-actions and docker.

## Fuzzing

Run by the lead auditor on 2026-11-20 with `cargo +nightly fuzz run <target> -- -max_total_time=180`. Each target ran for **3 minutes**, cut from the planned 10 to save cost. **No crashes, timeouts or OOMs**, so there were no regression tests to add.

| Target | Runs | Coverage (edges) | Corpus |
|---|---|---|---|
| cavage_signature | 4,635,087 | 537 | 861 |
| rfc9421_structured_fields | 853,035 | 1,384 | 985 |
| digest_headers | 4,726,160 | 957 | 1,069 |
| tinyindex_page | 1,255,146 | 2,038 | 1,729 |
| activity_json | 3,565,211 | 1,951 | 2,679 |

Three minutes is a smoke test. Run each target longer (`-max_total_time=3600`) before relying on these results. The corpora are in `fuzz/corpus/`, untracked.

## 7. Not assessed

- Load and throughput: A6 = B4 = E7 and E1 weren't load-tested. E's timings come from single
  requests on a shared, busy machine.
- E4, E6, E7(b) and E9 have no automated tests. They need the signed-inbox harness or the real
  `serve` wiring in `cli.rs`. The D2 and D6 races weren't tested under the real run loop.
- A2's precondition (servers that serve user-controlled JSON from their origin), because
  checking it would mean contacting real servers.
- The RFC 9421 vectors were checked against memory, not against a fresh copy of the RFC.
- `pacing.rs` and the worker's queue limits for FASP announcements, beyond the grouping by
  `uri_host_key`.
- The production deployment's real proxy, DNS and environment, which decides whether B1 = F6 is
  live.
- FASP `data_sharing` backfill paths, which are authenticated and so outside the threat model.
- Whether real Mastodon ever announces non-local URIs, which affects the fix for C1. This wasn't
  checked against the spec text.
- The sign-up rate limiting on the operator's site (mwmbl.org), which isn't in this repo.
- Whether Lemmy serves mod-removed posts as public.
- The repo's default `GITHUB_TOKEN` permission and branch protection (F4), and the contents of
  the published `ghcr.io/mwmbl/social` images.
- cargo-geiger was not run, and the CI drafts have placeholder SHAs.
- `egress_capture_net.sh` was only syntax-checked and never run against Docker.
  `deps_age_net.py`'s 30-day fresh-release rule was only syntax-checked.

## 8. Deployment hardening note

This note starts from F §4, with additions from B, C and E. Each item cites the file:line or
finding it comes from.

### Must

- Run with `allow_private_addresses = false`, which is the default (`config.rs:184`). With
  `true`, `client_builder` returns a bare builder with no resolver filter and no redirect check
  (`apub/src/net.rs:197-199`). Don't copy it from `dev/*/docker-compose.yml` (F7).
- Run with `allow_http = false` (`config.rs:106`). B5 is worse if http is allowed.
- Unset `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY`, in both cases, for this container. Use
  `env -u`, or don't inherit them in k8s. Any one of them switches off the SSRF filter until B1
  = F6 is fixed (`net.rs:21-22,195-214`).
- Set `base_url` to the public https origin, served at the root. FASP rebuilds `@target-uri`
  from it (`fasp/src/http/mod.rs:10-11`), and the fix for A5 relies on it.
- Restrict `/api/v1/admin/*` (`admin_app.rs:42-44`) at the reverse proxy or firewall, to the
  operator's sign-up backend only. Restrict `/api/v1/crawl/submissions` (`crawl_app.rs:38`) to
  the Mwmbl crawler's addresses (C4).
- If `metrics = true`, block `/metrics` (`metrics.rs:303`) at the proxy and scrape it over
  loopback. It is unauthenticated and does a full-table read per scrape (C5 = L4,
  `cli.rs:422-429`).
- Limit, at the reverse proxy:
  - request bodies: inbox 256 KiB, FASP and crawl 1 MiB, admin 16 KiB, so oversized bodies are
    rejected before the app buffers them (E7);
  - header and read timeouts, to stop slowloris (E7);
  - concurrent connections per client IP, and the rate per client IP on `/inbox`,
    `/actor/inbox` and both search APIs. The app has no limits of its own (A6 = B4 = E7, E1,
    `cli.rs:408-421`).
- If tokens are used, make each one at least 32 random characters (only 16 are enforced,
  `config.rs:114`, C4), use a different token for admin and crawl, and write them as a TOML
  **array** (F1).
- Create the data directory with mode 0700, owned by `10001:10001`, so the SQLite store and its
  `-wal`/`-shm` files aren't world-readable (F2, `store/src/lib.rs:71-75`).
- Pin the image by digest or `sha-<commit>` from `main`, not `:main` (F3, F5, README.md:36).

### Must not

- Don't set `MWMBL_SOCIAL_ALLOW_PRIVATE_ADDRESSES` or `MWMBL_SOCIAL_ALLOW_HTTP` (F7).
- Don't run with `RUST_LOG` at `mwmbl_social=debug` or `apub=debug` in production. Keep the
  default `info` (`cli.rs:174`), because debug logs whole remote documents (F8 = D9).
- Don't set `SSL_CERT_FILE` or `SSL_CERT_DIR`. Only the dev setups need them, and they replace
  the system roots.
- Don't publish the container port on 0.0.0.0. The image listens on 0.0.0.0
  (`Dockerfile:21-22`), so publish it as `-p 127.0.0.1:8080:8080` behind the proxy.
- Don't let the proxy rewrite or strip the `Signature`, `Signature-Input`, `Content-Digest`,
  `Digest` or `Date` headers, re-encode the body, or change `Host` or the path, because the
  signatures cover them.
- Don't log `Authorization` headers in proxy access logs.
- Don't pass tokens with `docker run -e` if `docker inspect` is visible to others. Use a secret
  store or a 0600 config file.
- Don't build images locally from a working checkout until `.dockerignore` is extended (F10).

### Recommended

- Container flags:
  ```
  docker run -d --name mwmbl-social \
    --user 10001:10001 --read-only --cap-drop ALL --security-opt no-new-privileges:true \
    --pids-limit 256 --memory 1g \
    -p 127.0.0.1:8080:8080 -v mwmbl-social:/data \
    -e MWMBL_SOCIAL_BASE_URL=https://search.example.org \
    ghcr.io/mwmbl/social@sha256:<digest>
  ```
  These flags are compatible with the code, which writes only to `data_dir`. The memory and pid
  limits also cap the damage from E8 and B3 = E6.
- Expose only the public routes: `/actor`, `/actor/inbox`, `/inbox`, `/.well-known/webfinger`,
  `/.well-known/nodeinfo`, `/nodeinfo/2.1`, `/api/v1/search/posts` and
  `/api/v1/search/accounts` (`http.rs:54-59`, `inbox_app.rs:210-211`). Also expose the FASP
  routes (`fasp_app.rs:51-58`), and `/` if `root_redirect` is set.
- Cap the length of the search query `q` at the proxy (for example 1-2 KB) until E1 is fixed.
- Terminate TLS at the proxy and redirect HTTP to HTTPS.
- There is no health route. Probe `GET /.well-known/nodeinfo` (`http.rs:56`). Alert on the
  writer stopping (E9) by watching for `WriterError::Stopped` in the logs.
- Watch disk use of the SQLite store (the `hosts`, `fetch_queue` and `tombstones` tables) until
  B3 = E6, C3 = E5 and E4 are fixed.
- Keep `max_transient_attempts` at 3 or more (D8).
- In hosted mode, rate-limit and review self-serve registrations, because registered peers can
  trigger C1, C2 and C3.

## 9. Pointers

- `audit/scripts/` holds the audit scripts: `find_literals.py`, `deps.py`, `deps_age_net.py`
  (uses the network), `surface.py`, `git_review.py` and `egress_capture_net.sh` (uses the
  network, not run yet).
- `audit/MANUAL_CHECKLIST.md` is a manual checklist that takes about 30 minutes.
- `deny.toml` is the cargo-deny policy. It needs `publish = false` on the workspace crates
  (§6).
- `fuzz/` holds the cargo-fuzz targets `activity_json`, `cavage_signature`, `digest_headers`,
  `rfc9421_structured_fields` and `tinyindex_page`.
- `audit/ci-draft/` holds the hardened workflows, `dependabot.yml`, the base-image pin patch
  and `resolve-pins.sh`. None of it is applied.
- The audit tests are the `#[ignore = "AUDIT: ..."]` tests in
  `crates/mwmbl_social/tests/{inbox,audit_c,audit_d,audit_f,audit_e_robots}.rs`,
  `crates/apub/tests/{audit_b,audit_b_proxy}.rs`, `crates/apub/src/inbox.rs`,
  `crates/tokenize/tests/audit_e.rs`, `crates/store/tests/audit_e.rs` and
  `crates/social_index/tests/audit_e.rs`. Run them all with
  `cargo test --workspace -- --ignored`.
