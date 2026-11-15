#!/usr/bin/env bash
# One-time setup of the end-to-end Mastodon: secrets, database, and an admin and a test user.
# Re-running it is safe: it keeps an existing .env and database.
set -euo pipefail
cd "$(dirname "$0")"

run() { docker compose run --rm --no-deps -e SSL_CERT_FILE= web "$@"; }

if [ ! -f .env ]; then
  echo "Generating .env"
  {
    echo "SECRET_KEY_BASE=$(run bundle exec rails secret | tail -1)"
    run bundle exec rails db:encryption:init | grep -E '^ACTIVE_RECORD_ENCRYPTION_' | sed 's/: /=/'
    run bundle exec rails mastodon:webpush:generate_vapid_key | grep -E '^VAPID_'
  } > .env.tmp
  mv .env.tmp .env
fi

docker compose up -d db redis caddy ca
until docker compose exec -T db pg_isready -U postgres >/dev/null 2>&1; do sleep 1; done
docker compose run --rm -e SSL_CERT_FILE= web bundle exec rails db:prepare

tootctl() { docker compose run --rm -e SSL_CERT_FILE= web bin/tootctl "$@"; }
tootctl accounts create admin --email admin@mastodon.test --confirmed --role Owner 2>/dev/null || true
tootctl accounts approve admin 2>/dev/null || true
tootctl accounts create alice --email alice@mastodon.test --confirmed 2>/dev/null || true
tootctl accounts approve alice 2>/dev/null || true

docker compose up -d web sidekiq
echo "Mastodon is starting at https://mastodon.test (port 8443 on localhost)."
echo "Set passwords with: docker compose run --rm web bin/tootctl accounts modify admin --reset-password"
