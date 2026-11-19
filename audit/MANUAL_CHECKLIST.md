# Manual audit checklist

About 30 minutes. Run everything from the repo root. Every script is read-only and offline
except the two whose names end in `_net`; their headers say exactly what they contact.
Each script is under about 75 lines, so read it before you run it (2 minutes each).

## 1. `python3 audit/scripts/surface.py` (8 min)

The routes table, then outbound HTTP, subprocesses, file writes and environment reads.

- Every `/api/v1/admin/*` and `/api/v1/crawl/*` route must show `auth=yes`. Their check is
  `tokens.reject(` (bearer token; with no tokens configured the API answers 404).
- `/inbox` and `/actor/inbox` must show `auth=yes` (`app.verify(`). Then open `receive` in
  `crates/mwmbl_social/src/inbox_app.rs` and confirm that the only branch that acts without
  a verified signature is the `KeyGone` self-delete. Note that `apub::fetch` treats 404 as
  "gone" as well as 410.
- The FASP line is `ANY_SERVICE <pattern> ... dispatch auth=yes`. Check that every
  `FASP_ROUTES` pattern listed below it is one you expect, and that every `FASP dispatch arm`
  in `handlers.rs` is a FASP spec endpoint. All of them sit behind `authenticate()`, so there
  is no unauthenticated FASP endpoint (registration is started from our side, through the
  admin API).
- These routes are public on purpose and should be the only `auth=NO` ones: `/actor`,
  `/.well-known/webfinger`, `/.well-known/nodeinfo`, `/nodeinfo/2.1`, `/api/v1/search/*`,
  the `/` redirect closure, and `/metrics`. `/metrics` only exists when `metrics = true`, and
  it is on the main listener, so check your proxy blocks it. Any other `NO` or `NOT FOUND`
  is a problem.
- Outbound HTTP: the only `Client::builder` should be `apub/src/net.rs` (`client_builder`,
  which refuses non-public addresses). For each `.send()` site (fetch.rs, fasp client.rs,
  registration.rs, cli.rs), open it and check that the client it uses came from
  `net::client_builder` and not from `reqwest::Client::new()`.
- Subprocesses: expect none.
- File writes: expect only the data dir (`cli.rs`, `writer.rs`), the key file (`keys.rs`,
  check it sets mode 0600) and `tinyindex/src/index.rs`. No other code should write files.
- Environment reads: expect only `CARGO_PKG_*` at compile time, `MWMBL_SOCIAL_CONFIG` (clap)
  and `MWMBL_SOCIAL_<FIELD>` for every config field (`config.rs`). Nothing else, for example
  no `AWS_*`, `HOME` or proxy variables read directly.

## 2. `python3 audit/scripts/find_literals.py` (5 min)

- Read the **UNEXPECTED** section line by line. It should only hold ActivityStreams and
  nodeinfo schema URLs, `ghcr.io` in the workflow, the repository URL, `0.0.0.0` in the
  Dockerfile and `127.0.0.1` as a default listen address. Any other host, IP, email or blob
  in production code is a hidden endpoint or secret. Look at it.
- In the **EXPECTED** counts, PEM blocks should appear only in `tests/` fixtures, test JSON,
  and the `#[cfg(test)]` part of `keys.rs`. A real-looking key anywhere else is a leak.
- `crates/apub/src/net.rs` has many IPs. They should be the private and reserved ranges it
  refuses. Skim them for a range that is missing (for example `169.254.0.0/16`, `fd00::/8`,
  `::ffff:0:0/96`).

## 3. `python3 audit/scripts/deps.py` (4 min)

- "Not from crates.io" must be 0.
- "Name within edit distance 2 of a popular crate": expect only `base64ct`, `serdect`
  (RustCrypto) and `rustix` (Bytecode Alliance). Anything new needs its repository checked.
- Build scripts and proc-macros: each one runs code on your machine at compile time. Expect
  well-known ones (serde, thiserror, ring, aws-lc-sys, libsqlite3-sys, zstd-sys, the windows
  target crates). `aws-lc-rs`, `quinn` and `jni` come in through reqwest's `rustls` feature.
  Decide if you want them (`cargo tree -i aws-lc-rs` shows the path).

## 4. `python3 audit/scripts/deps_age_net.py` (2 min, contacts crates.io)

- "Flagged" should only list recent releases of long-established crates. On 2026-11-19 these
  were rsa 0.10.0, rusqlite 0.43.0, zstd 0.13.5 and toml 1.2.1, all released in November 2026.
  For each, check that the repository URL is the one you know, and skim the release diff on
  that repository if the crate handles keys or untrusted input (rsa above all).
- Any crate whose first publish is under 6 months old, or with under 100k downloads, is a red
  flag for an invented or typosquatted dependency.

## 5. `python3 audit/scripts/git_review.py` (6 min)

- Authors: expect only `Claude <noreply@anthropic.com>` and you. An unknown author or
  email is the first thing to chase.
- For `signatures/*`, `inbox*`, `net.rs`, `fetch.rs`, `crates/fasp/*`, `admin_app.rs`,
  `operator.rs`, `consent.rs`, `decide.rs`, `keys.rs`, `Dockerfile` and the workflows, read
  the subjects of the newest commits. Any subject that doesn't explain a security-relevant
  change (for example "tidy" or "fix tests" on `consent.rs`) deserves `git show <hash>`.
- "Tracked .rs files nothing references" must be empty. A file there is dead code, or code
  that something includes in a way the script can't see.
- "Other tracked files whose name appears in no other tracked file": expect `ci.yml` (GitHub
  finds it by convention), `generate.py` (regenerates the tinyindex fixtures) and
  `rfc9421_rsa_pss_private.pem` (the RFC 9421 test key, which is public). Anything else
  deserves a look.

## 6. `audit/scripts/egress_capture_net.sh <container> [minutes]` (5 min plus capture time)

The name ends in `_net` because it pulls `nicolaka/netshoot` and does reverse-DNS lookups.
Start the dev Mastodon environment first. The script prints the steps from
`dev/mastodon/README.md` and doesn't start anything itself. Then run it, for example
`audit/scripts/egress_capture_net.sh mwmbl-social-e2e-social-1 10`, while you post, edit and
delete as alice.

- Every destination should be Caddy's address on the e2e network (reverse DNS
  `...caddy...`, port 443, which is `mastodon.test`) or Docker DNS (`127.0.0.11:53`).
- "DNS lookups" should only name `mastodon.test` (and `social.test` if we fetch ourselves).
  Any public IP, other hostname, port 80, or UDP/443 (QUIC) is an undeclared call home.
- The staging run uses `MWMBL_SOCIAL_ALLOW_PRIVATE_ADDRESSES=true` (see the compose file), so
  this capture does not test the private-address block. That is covered by surface.py and
  the `net.rs` tests.
