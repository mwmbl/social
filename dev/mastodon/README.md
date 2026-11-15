# End-to-end environment

Mastodon 4.7.5 with `EXPERIMENTAL_FEATURES=fasp`, Postgres, Redis, and Mwmbl Social, all
behind Caddy. Caddy serves `https://mastodon.test` and `https://social.test` with its own
internal CA, and both Mastodon and Mwmbl Social trust that CA through `SSL_CERT_FILE`, so
signed fetches and FASP calls go over real HTTPS. Nothing here contacts real fediverse servers.

```sh
./setup.sh                     # secrets, database, users admin and alice; starts Mastodon
docker compose up -d --build social
```

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
