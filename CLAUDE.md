# Mwmbl Social

A fediverse search engine written in Rust. Fediverse server owners deploy it as a FASP
(Fediverse Auxiliary Service Provider), and Mwmbl also runs a hosted instance. It is licensed
under AGPL-3.0.

**Read `PLAN.md` before starting any task.** It is the design and the stage plan. If a decision
changes, update `PLAN.md` in the same change. Don't let the code and the plan drift apart.

## Build and check

Every change must pass these before it counts as done:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Every crate's `lib.rs` and `main.rs` carries the same lint deny list as Mwmbl's `mwmbl_core`:
`unwrap_used`, `expect_used`, `panic`, `indexing_slicing`, `string_slice`, `todo`,
`unimplemented`, `unreachable`, `exit`, and `clippy::pedantic`. Don't add `#[allow]` to get
past them. Restructure the code instead, and if you truly can't, justify the allow in a comment
next to it.

## Principles

These are easy to break by accident. Breaking them is a design bug, not a style issue.

1. **The index is the only record of what is indexed.** SQLite holds bounded operational state
   only: registrations, subscriptions, the fetch queue, hosts, tombstones and the sweep
   position. It never gets a row per document or per account, and never stores term lists.
   The one exception is `follow_optins`, which is consent state bounded by user actions (see
   Stage 3 in `PLAN.md`).
2. **Index keys are a pure function of an item's stored fields plus the tokeniser version.**
   Deletion works by reading the stored record and re-tokenising it. Never index a term derived
   from text the item doesn't store. Changing the tokeniser output means bumping its version.
3. **Consent comes before anything else.** Only fully public objects are indexed (check `to:`
   for the public collection; unlisted and quiet-public objects don't qualify). The author must
   have `discoverable: true`, and posts also need `indexable: true` (FEP-5feb). When in doubt,
   don't index.
   - Lemmy is the one exception: if the **author's** host runs Lemmy, missing flags count as
     yes but an explicit `false` still means no. The rule follows the author, not the
     community (see Stage 3 in `PLAN.md`).
   - An author who follows our server actor has opted in for any flag their actor doesn't
     carry. An explicit `false` still wins, and unfollowing withdraws consent.
   - Consent evaluation is a pure function with table-driven tests. A change to it needs a test
     case for the situation it covers.
   - Never store non-public objects anywhere, including logs.
4. **Only call a server's FASP APIs for capabilities that server has enabled.**
   Beyond objects announced over FASP (and their revalidation), the only real-server fetches
   are for followed communities and accounts, and crawl-submitted Lemmy URIs. Those fetches respect robots.txt and per-host limits. Crawling
   itself (discovery, scheduling) lives in Mwmbl's crawler, not here. Never index JSON a client
   submits; fetch it from its origin.
5. **All index writes go through the single writer task.** Nothing else writes to an index file.
6. **`tinyindex` stays free of fediverse knowledge.** It is meant to be upstreamed to Mwmbl, and
   must keep reading pages written by Mwmbl's Python as `Document`. URI lookup, re-tokenising
   and tombstones belong a layer above it.

## Code layout

See the Architecture section of `PLAN.md`. Crates live under `crates/`: `tinyindex`,
`tokenize`, `store`, `apub`, `fasp`, `mwmbl_social` (the binary).

Reference code:
- **Page format:** `https://github.com/mwmbl/mwmbl/tree/main/rust/mwmbl_core` is the origin of `tinyindex`.
- **Mwmbl's Python index:** `https://github.com/mwmbl/mwmbl/blob/main/mwmbl/tinysearchengine/indexer.py` (key hashing:
  `mmh3.hash(key, signed=False) % num_pages`) and `https://github.com/mwmbl/mwmbl/blob/main/mwmbl/indexer/index.py`
  (tokenising).
- **FASP specs:** github.com/mastodon/fediverse_auxiliary_service_provider_specifications.
  Treat the spec as the source of truth over any summary of it, including `PLAN.md`'s.

## Testing

- **Unit tests** go next to the code. Protocol pieces need known-answer tests: RFC 9421 test
  vectors, content-digest, and mmh3 vectors generated from Python.
- **Integration tests** use in-process fakes, an axum FASP server and an ActivityPub host, so
  CI never needs network access or Mastodon.
- **End-to-end testing** uses the Docker Compose Mastodon dev environment.
- **Never fetch real fediverse servers in bulk** from tests or during development. In
  development, real-server traffic only goes through registrations with servers whose admins
  have opted in.

## Conventions

- **Errors:** `thiserror` for library crates, `anyhow` only in the binary. Errors must say what
  failed and with which URI or host.
- **Logging:** use `tracing`. Log URIs and hosts, never post content.
- **Dependencies:** keep them few, and pin them in the workspace `Cargo.toml`. A new dependency
  needs a reason in the commit message.
- **Commits:** keep them small and focused, with a message explaining why.
