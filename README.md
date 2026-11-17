# Mwmbl Social

A search engine for the fediverse that a server's admin can run for their own server, as a
FASP (Fediverse Auxiliary Service Provider). It indexes only public posts and accounts whose
authors opted in (`discoverable`, and `indexable` for posts), and removes them when that changes.

It currently provides:

- **`data_sharing`**: your server tells us about public posts and accounts, and we fetch and
  index them.
- **`account_search`**: your server's account search can include results from us.
- **`trends`**: trending posts and hashtags, from the `trending` events connected servers
  send us, limited to posts we have indexed (see PLAN.md Stage 4 for how they are ranked).
  Links are not reported. Mastodon does not query trends yet.
- **`follow_recommendation`**: accounts to follow, related by the words of the asking
  account's name and bio, then authors of trending posts. Mastodon shows these as follow
  suggestions.
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

With Docker, using the image built from `main` (`ghcr.io/mwmbl/social:main`; every commit is
also tagged `sha-<commit>`):

```sh
docker run -d --name mwmbl-social -p 127.0.0.1:8080:8080 -v mwmbl-social:/data \
  -e MWMBL_SOCIAL_BASE_URL=https://search.example.org \
  ghcr.io/mwmbl/social:main
```

Or build the image yourself with `docker build -t mwmbl-social .`.

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
   `account_search`, `trends` and `follow_recommendation` to use them. Enabling `data_sharing`
   starts a backfill of recent public content and subscribes to trending events.

`mwmbl-social registrations` lists registrations and `mwmbl-social unregister <id>` revokes
one. To offer sign-up from your own site, use the [admin API](#admin-api) instead.

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
| `sign_in_url` | unset | Where server admins sign in to manage their account with you; sent as `signInUrl` in `provider_info` |
| `root_redirect` | unset | `GET /` answers a 302 to this URL; unset, `/` is a 404 |
| `actor_username` | `mwmbl-social` | Our server actor's `preferredUsername` |
| `posts_pages`, `accounts_pages` | 65536, 16384 | Index size in 4 KiB pages; full pages drop their lowest-scored items |
| `sweep_period_secs` | 6 days | One full revalidation pass over each index |
| `fetch_concurrency` | 8 | |
| `revalidate_interval_per_host_ms` | 1000 | Revalidation fetches to one host start at most once per this interval; the rest are rescheduled. 0 turns it off. Announcements and backfill are not limited by it |
| `trends_timeframe_minutes`, `trends_shares`, `trends_likes`, `trends_replies` | 15, 3, 3, 3 | Threshold of our trends subscription: a server reports a post once it has this many shares, likes or replies within the timeframe |
| `trends_max_uris` | 5000 | Most post URIs kept in the in-memory trend state (one week of hourly counts) |
| `metrics` | `false` | Serve Prometheus text at `/metrics`. It is public, so firewall or proxy it |
| `follow` | `[]` | Communities or accounts to follow, e.g. `["!rust@lemmy.example"]`; env: comma-separated |
| `outbox_backfill_pages` | 5 | Outbox pages read when a follow is accepted; 0 turns it off |
| `crawl_tokens` | `[]` | Bearer tokens for crawl submissions (at least 16 characters each); none turns the endpoint off. Env: comma-separated |
| `admin_tokens` | `[]` | Bearer tokens for the admin API, as for `crawl_tokens`; none turns it off. Env: comma-separated |

## Following communities

Lemmy doesn't speak FASP, so its communities are followed instead. Follow only communities
whose instance admins agreed to be indexed.

- `mwmbl-social follow '!rust@lemmy.example'` follows a community (or `@user@host`, or an actor
  URI). Its new posts then reach our inbox, and once it accepts, its recent posts are read from
  its outbox (up to `outbox_backfill_pages` pages).
- `mwmbl-social unfollow '!rust@lemmy.example'` stops following. What is indexed stays.
- `mwmbl-social following` lists what we follow and whether it accepted.
- Or list them under `follow` in the config: `serve` follows any new ones at start-up and every
  6 hours. Removing one from the list does not unfollow it.

The server must be running to receive the community's acceptance.

## Crawl submissions

Mwmbl's crawler finds Lemmy posts and comments and sends us their URIs, never their JSON:

```sh
curl -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"uris": ["https://lemmy.example/post/1"]}' https://social.example/api/v1/crawl/submissions
# 202 {"accepted": 1, "deferred": 0, "rejected": 0}
```

- At most 1000 URIs, `https` only. Duplicates count once. URIs on blocked domains or forgotten
  are rejected. Crawl URIs only use the fetch queue while it is under half full; the rest are
  deferred (counted, not queued), so resubmit them later.
- 404 when no `crawl_tokens` are set, 401 on a bad token, 422 for any other body.
- Before fetching, we check the host's nodeinfo says Lemmy (anything else is dropped) and that
  its robots.txt allows the URI for `mwmbl-social`. Fetches to a host are spaced by its
  `Crawl-delay` (at most 300 s). Authors still need consent; see PLAN.md Stage 3.

## Admin API

For an operator's own sign-up page: it does what `register`, `registrations` and `unregister`
do. Set `admin_tokens` to turn it on, and call it server-side only.

```sh
curl -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"server_url": "https://mastodon.example"}' https://social.example/api/v1/admin/registrations
# 201 {"id": 4, "base_url": "https://mastodon.example/api/fasp", "status": "pending",
#      "capabilities": [], "fingerprint": "…", "created_at": 1763337600,
#      "approval_url": "https://mastodon.example/admin/…"}
```

- Show the admin the `fingerprint` and `approval_url`. The approval link is only in this
  response, so keep it. `fasp_base_url` may be added when nodeinfo doesn't lead to it.
- 400 for an unusable URL (`https` only), 409 (with the existing `id`) if the server is already
  registered, 502 if it can't be reached or doesn't accept the registration, 422 for any other
  body.
- `GET /api/v1/admin/registrations` lists registrations, `GET .../{id}` shows one (its `status`
  becomes `active` once the server first calls us), and `DELETE .../{id}` revokes it. Unknown
  ids get 404.
- 404 for every route when no `admin_tokens` are set, 401 on a bad token.

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
