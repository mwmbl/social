# End-to-end environment

Mastodon 4.7.5 with `EXPERIMENTAL_FEATURES=fasp`, Postgres, Redis, and Mwmbl Social, all
behind Caddy. Caddy serves `https://mastodon.test` and `https://social.test` with its own
internal CA, and both Mastodon and Mwmbl Social trust that CA through `SSL_CERT_FILE`, so
signed fetches and FASP calls go over real HTTPS. Nothing here contacts real fediverse servers.

```sh
./setup.sh                     # secrets, database, users admin and alice; starts Mastodon
docker compose run --rm web bin/rails runner /seed/seed.rb   # alice and bob, and their posts
docker compose up -d --build social
```

Mastodon runs with `AUTHORIZED_FETCH=true`, so every fetch we make must carry a signature it
accepts.

If `docker build` can't download crates (an intercepting proxy, as in the cloud sandbox), build
on the host and package that binary instead:

```sh
cargo build --release -p mwmbl_social && cp ../../target/release/mwmbl-social prebuilt/
docker compose -f docker-compose.yml -f docker-compose.prebuilt.yml up -d --build social
```

The Stage 0 check: `docker compose exec social mwmbl-social ingest <alice's public post URI>`,
then `mwmbl-social search posts garlic`. Delete the post in Mastodon. Run the server with
`MWMBL_SOCIAL_SWEEP_PERIOD_SECS=40 MWMBL_SOCIAL_REVALIDATE_AFTER_SECS=1` and the next sweep
removes it.

Reach Mastodon from the host at `https://localhost:8443` with a `Host: mastodon.test` header,
or add `127.0.0.1 mastodon.test social.test` to `/etc/hosts` and use port 8443.

Useful commands:

```sh
docker compose run --rm web bin/tootctl accounts modify alice --reset-password
docker compose exec social mwmbl-social stats
docker compose logs -f social sidekiq
docker compose down -v         # throw everything away
```

Findings from running it are recorded in `PLAN.md` (for example, Mastodon does not advertise
`faspBaseUrl` in its nodeinfo).
