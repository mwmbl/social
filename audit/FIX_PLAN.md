# Audit fix plan

How the findings in `AUDIT.md` get fixed. One row per PR. Work goes in order, one PR at a time.

## How each PR is made

- One subagent per PR, run one after another. It gets this file, the finding IDs and the base
  branch, and reads only the `AUDIT.md` sections it needs.
- Branch name: `fix/<ids>-<slug>`, e.g. `fix/a1-key-owner`.
- The fix removes `#[ignore = "AUDIT: ..."]` from the finding's tests, so they now pass. A
  finding with no test gets a new regression test where one is practical.
- Under 500 changed lines (`git diff --stat`). If a fix grows past that, split it.
- Before pushing: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`. A consent change adds a row to the consent table tests (principle 3).
- If the fix changes a design decision, `PLAN.md` changes in the same PR.
- The PR body starts "From Claude:", names the finding IDs, says what changed and why, and
  lists anything for a human to check by hand. Tick the row here in the same PR.
- The subagent reports back in under 150 words: branch, PR URL, diff size, test result, and
  anything it was unsure of.

## Stacking

PR 0 puts the audit (report, ignored tests, fuzz targets, scripts) on `main`. It is over 500
lines, but it changes no behaviour, so it is reviewed as a record rather than line by line.
Once it is merged, every fix PR targets `main` directly, except where a row names another PR
as its base because it edits the same code. Those are stacked, and are retargeted to `main`
when their base merges.

If PR 0 isn't merged quickly, all fix PRs stack on `audit/2026-11-19` instead.

## Decisions (agreed 2026-11-21)

1. **A3 Delete of someone else's object:** ignore it. A Delete is acted on only when
   the sender is the object itself (an actor deleting itself) or the stored record's
   `attributedTo`. When there is no stored record, queue a fetch of the URI instead of adding a
   tombstone, so the origin's 404 or 410 removes it. That also closes E4. Moderator deletions
   by someone other than the author (mainly Lemmy) wait for revalidation, which can take up to
   6 days.
2. **D5 an origin that never answers:** remove content after 3 failed revalidation passes in a
   row (about 18 days at the default sweep). Needs a counter (bounded, per host, in `hosts`).
3. **C1 foreign URIs in announcements:** drop URIs not on the registration's host and log the
   count.
4. **C4 admin API:** admin and crawl routes get their own `admin_listen` address, which
   defaults to `127.0.0.1:8081`, and are never served on the public listener. The crawler will
   be a dedicated one that runs on the same machine or connects through it. Nothing is
   deployed yet, so this breaks no one. Update the Dockerfile, the dev compose files and the
   README to match.
5. **E9 durable inbox removals:** this PR only makes a dead writer stop the process. Queueing
   removals durably before the 202 is left as a later PR.

## PRs

| # | IDs | Change | Base | Done |
|---|---|---|---|---|
| 0 | — | Audit report, ignored tests, fuzz targets, scripts, this plan | main | |
| **Auth and consent (Critical, High)** |||||
| 1 | A1 | Cache `(key, owner)` and run `check_key_owner` on every request | main | |
| 2 | A2 | When the key document isn't the actor, fetch the owner and require it to list the key with the same PEM | 1 | |
| 3 | A3, E4 | First find out why A3's test passes. Then ignore a Delete unless it comes from the object's author (decision 1); for an unknown URI, queue a fetch instead of adding a tombstone | 5 | |
| 4 | A4 | Non-Group `Block` with `removeData` is only accepted from the site actor, or dropped | main | |
| 5 | D1 | Generation number on queue rows; `complete` and `fail` only apply to the generation read | main | |
| 6 | D2, D6 | `IndexPost`, `IndexAccount`, `ClearAuthorTombstone` carry the fetch start time; the writer refuses them when a newer tombstone exists | 5 | |
| 7 | D3 | Act on an author's refusal only when the author `id` is its fetch URI and on the post's origin | main | |
| 8 | D4 | `undo` of a Follow also removes the account record | main | |
| 9 | D5 | Count failed revalidations and remove after the limit (decision 2); update `PLAN.md` | 5 | |
| 10 | D7, D8 | Tombstone whenever the URI lookup misses; require `max_transient_attempts >= REFUSED_ATTEMPTS` | main | ✓ |
| 11 | E9 | Watch the writer thread and stop the process if it ends unexpectedly | main | |
| **SSRF and amplification** |||||
| 12 | B1, F7 | `.no_proxy()` in `client_builder`; warn at start-up when `allow_private_addresses` or `allow_http` is on | main | |
| 13 | B2 | Strip one trailing dot in `host_key` and `uri_host_key` | main | |
| 14 | B5 | FASP client: no redirects, or only same-origin https | main | |
| 15 | C1, C2 | Drop announced URIs not on the registration's host; dedupe `object_uris` in `clean_uris` | main | |
| **Remote DoS** |||||
| 16 | E1, E3 | Cap `q` length and query key count; dedupe special keys with a `HashSet`; cap `tag` in `parse_post`; tighten E3's test | main | |
| 17 | E2 | Truncate `preferredUsername` and `acct`; cap username tokens | main | |
| 18 | C3, E5 | Hard queue ceiling for announces; inbox fetches get their own reason and share of the cap | 5 | |
| 19 | B3, E6 | Bound `Fetcher::hosts` (LRU or TTL); persist only hosts we have a reason to keep, in one transaction | 13 | |
| 20 | A6/B4/E7 (a) | Global outbound semaphore, separate pool for inbox key fetches, per-request deadline across hops | 19 | |
| 21 | A6/B4/E7 (b), E8 | Server header-read timeout, `TimeoutLayer`, concurrency limit; inbox `Content-Length` check before buffering; bound inbox spawns and answer 503 when full | main | |
| 22 | A6/B4/E7 (c) | Cheap signature checks before any key fetch; refetch an evicted key at most once per N minutes | 2 | |
| 23 | E10 | Cap robots rule count and pattern length, or a linear-time matcher | main | |
| 24 | C5 | Metrics use COUNT queries | main | |
| **Secrets and config** |||||
| 25 | A5 | Require the signed `host` to match `base_url` | main | |
| 26 | F1 | Config errors keep line, column and kind only; never the toml source text | main | |
| 27 | F2, F9 | `data_dir` 0700, store files 0600; write `actor.pem` via temp file, fsync and rename | main | |
| 28 | F8 | Log only the outcome variant for a non-matching `Found` | main | |
| 29 | C4 | Separate `admin_listen`, loopback by default (decision 4) | main | |
| **Supply chain** |||||
| 30 | F3, F4, F5 | Pin actions by SHA, add `permissions: contents: read`, don't push PR images; start from `audit/ci-draft/` | main | |
| 31 | F10 | Allow-list `.dockerignore`; pin base images by digest | main | |

Not in this plan: P4's lead L1 (reading the rsa 0.10.0 diff) and L2 (a transient 404 on the
unsigned self-Delete). Those need a human decision first, and are tracked in `AUDIT.md` §3.
