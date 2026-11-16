# Mwmbl Social

A search engine for the fediverse that a server's admin can run for their own server, as a
FASP (Fediverse Auxiliary Service Provider). It indexes only public posts and accounts whose
authors opted in (`discoverable`, and `indexable` for posts), and removes them when that changes.

It currently provides:

- **`data_sharing`**: your server tells us about public posts and accounts, and we fetch and
  index them.
- **`account_search`**: your server's account search can include results from us.
- A JSON search API at `/api/v1/search/posts` and `/api/v1/search/accounts`, for testing.

There is no end-user UI. In FASP the fediverse server is the UI. The design and the plan are
in [PLAN.md](PLAN.md). It is licensed under the AGPL-3.0.

**Status:** early. It has been tested end to end against Mastodon 4.7.5 (see `dev/mastodon`),
but not yet in a real deployment.

## Requirements

- Mastodon 4.5 or later with `EXPERIMENTAL_FEATURES=fasp` set. Other servers may work if they
  implement the FASP specs.
- A public HTTPS URL for Mwmbl Social, served by a reverse proxy (Caddy, nginx). Your
  fediverse server must be able to reach it, and it must be able to reach the fediverse.
- About 256 MiB of disk for the default index sizes, and very little CPU and memory.

## Install

With Docker:

```sh
docker build -t mwmbl-social .
docker run -d --name mwmbl-social -p 127.0.0.1:8080:8080 -v mwmbl-social:/data \
  -e MWMBL_SOCIAL_BASE_URL=https://search.example.org \
  mwmbl-social
```

Or from source, with Rust 1.100:

```sh
cargo build --release -p mwmbl_social
MWMBL_SOCIAL_BASE_URL=https://search.example.org ./target/release/mwmbl-social serve
```

Then point your reverse proxy at `127.0.0.1:8080` for `https://search.example.org`. The URL must
be served at the root of its host.

## Connect your server

1. Register:

   ```sh
   docker exec mwmbl-social mwmbl-social register https://your.server
   ```

   It prints a key fingerprint and an approval link.
2. Open the link (Mastodon: Administration > Fediverse Auxiliary Service Providers). Check that
   the fingerprint matches, then approve.
3. On the same page, enable the capabilities you want: `data_sharing` to send us content, and
   `account_search` to use our account search. Enabling `data_sharing` starts a backfill of
   recent public content.

`mwmbl-social registrations` lists registrations and `mwmbl-social unregister <id>` revokes
one.

## Configuration

Configure it with a TOML file (`--config` or `MWMBL_SOCIAL_CONFIG`). Every field can also be
set from the environment as `MWMBL_SOCIAL_<FIELD>`, for example `MWMBL_SOCIAL_BASE_URL`.

| Field | Default | |
|---|---|---|
| `base_url` | (required) | Our public HTTPS URL |
| `listen` | `127.0.0.1:8080` | |
| `data_dir` | `./data` | SQLite, the two index files and our keys |
| `name` | `Mwmbl Social` | Shown to admins in their FASP list |
| `contact_email`, `privacy_policy_url`, `fediverse_account` | unset | Shown in `provider_info` |
| `actor_username` | `mwmbl-social` | Our server actor's `preferredUsername` |
| `posts_pages`, `accounts_pages` | 65536, 16384 | Index size in 4 KiB pages; full pages drop their lowest-scored items |
| `sweep_period_secs` | 6 days | One full revalidation pass over each index |
| `fetch_concurrency` | 8 | |
| `metrics` | `false` | Serve Prometheus text at `/metrics`. It is public, so firewall or proxy it |

## Removing content

- `mwmbl-social forget <post or actor URI>` removes something for good.
- `mwmbl-social forget user@host` resolves the account and removes it and its posts.
- `mwmbl-social forget example.com` blocks a whole domain.

Deletes and opt-outs from connected servers apply automatically. Everything indexed is
re-checked at least weekly, as the FASP spec requires.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`dev/mastodon` has a Docker Compose Mastodon with FASP enabled for end-to-end testing. Read
[CLAUDE.md](CLAUDE.md) for the rules the code follows.
