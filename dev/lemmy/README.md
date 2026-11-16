# End-to-end environment: Lemmy

Lemmy 0.19.13 (the backend, `dessalines/lemmy`), its Postgres, and Mwmbl Social, all behind
Caddy. Caddy serves `https://lemmy.test` and `https://social.test` from a root CA that
`setup.sh` makes, and both sides trust it, so follows, deliveries and fetches go over real
HTTPS with real signatures. Nothing here contacts real fediverse servers. It runs next to
dev/mastodon (port 8444 here, 8443 there).

```sh
./setup.sh      # root CA, a patched copy of Lemmy's binary (below), Postgres, Caddy, Lemmy
./seed.sh       # admin opens registration; carol, community rust, two posts and a comment
docker compose up -d --build social
```

`setup.sh` needs `openssl`, `python3` and `curl` on the host; `seed.sh` also needs `jq`.
If `docker build` can't download crates (an intercepting proxy, as in the cloud sandbox), build
on the host and use dev/mastodon's prebuilt image:

```sh
cargo build --release -p mwmbl_social && cp ../../target/release/mwmbl-social ../mastodon/prebuilt/
docker compose -f docker-compose.yml -f docker-compose.prebuilt.yml up -d --build social
```

## What is unusual here, and why

- **Lemmy trusts only its compiled-in roots.** Its HTTP client is reqwest with rustls and
  `webpki-roots`, so `SSL_CERT_FILE` and the system store are ignored. `setup.sh` therefore
  makes a P-256 root CA whose subject and key have exactly the encoded lengths of one
  compiled-in root (Amazon Root CA 3), gives that root to Caddy (`pki` in the `Caddyfile`), and
  `trust.py` overwrites Amazon Root CA 3's bytes with ours in a copy of the binary
  (`tls/lemmy_server`, mounted over the image's). Nothing else in the binary changes. Mwmbl
  Social trusts the same root through `SSL_CERT_FILE`, as in dev/mastodon.
- **Lemmy refuses hosts on private addresses** (activitypub_federation outside debug mode:
  "Localhost is only allowed in debug mode", for any private, loopback or link-local IP). The
  compose network is on TEST-NET-2, `198.51.100.0/24`, which is reserved for documentation and
  never routed, so `social.test` resolves to an address Lemmy accepts.
- **No lemmy-ui and no pict-rs.** Lemmy starts without pict-rs (`image_mode: None`). lemmy-ui
  is what serves `/robots.txt` in a real deployment, so Caddy serves `robots.txt` here, a
  verbatim copy of what lemmy-ui 0.19.13 sends (including its indented lines), and sends
  everything else to the backend. A real deployment's proxy sends `Accept: application/...`
  requests and every `POST` to the backend and the rest to lemmy-ui.
- Federation is open (no allowlist), which is Lemmy's default. `seed.sh` opens registration
  and raises the post and comment rate limits so scripting works.

## Checks (Stage 3)

```sh
docker compose exec social mwmbl-social follow '!rust@lemmy.test'   # Accept, then outbox backfill
docker compose exec social mwmbl-social search posts borrow
```

Lemmy delivers in batches, so allow up to a minute for an activity to arrive. Then post,
comment, delete and remove with `./api.sh` (`./api.sh METHOD PATH [JSON] [JWT]`; log in with
`./api.sh POST /api/v3/user/login '{"username_or_email":"carol","password":"carol-password-1234"}'`):
`/api/v3/post`, `/api/v3/comment`, `/api/v3/post/delete`, `/api/v3/post/remove` (as admin,
password `lemmy-admin-password`), `/api/v3/comment/remove`.

Crawl submissions (token `dev-crawl-token-0123456789`):

```sh
curl --noproxy '*' --resolve social.test:8444:127.0.0.1 --cacert tls/root.crt \
  -H 'Authorization: Bearer dev-crawl-token-0123456789' -H 'Content-Type: application/json' \
  -d '{"uris":["https://lemmy.test/post/4","https://lemmy.test/modlog"]}' \
  https://social.test:8444/api/v1/crawl/submissions
```

robots.txt says `Crawl-delay: 60`, so crawl items to lemmy.test are fetched a minute apart.

Useful commands:

```sh
docker compose exec social mwmbl-social stats
docker compose logs -f social lemmy
docker compose down -v         # throw everything away (tls/ stays; delete it for a new CA)
```

Findings from running it are recorded in `PLAN.md`, Stage 3.
