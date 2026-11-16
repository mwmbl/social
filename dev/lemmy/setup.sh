#!/usr/bin/env bash
# One-time setup of the end-to-end Lemmy: a root CA for Caddy, a copy of Lemmy's binary that
# trusts it, then Postgres, Caddy and Lemmy. Needs openssl and python3 on the host.
# Re-running it is safe: it keeps an existing CA and database.
set -euo pipefail
cd "$(dirname "$0")"
image=dessalines/lemmy:0.19.13

mkdir -p tls
if [ ! -f tls/root.crt ]; then
  echo "Generating the root CA"
  openssl ecparam -name prime256v1 -genkey -noout -out tls/root.key
  openssl req -x509 -new -key tls/root.key -subj "$(python3 trust.py gen-subject)" -days 3650 \
    -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign,cRLSign \
    -out tls/root.crt
  chmod 644 tls/root.key  # Caddy's container user reads it; this is a throwaway dev CA.
  rm -f tls/lemmy_server
fi
if [ ! -f tls/lemmy_server ]; then
  echo "Patching Lemmy's compiled-in roots"
  docker image inspect "$image" >/dev/null 2>&1 || docker pull "$image"
  docker run --rm --entrypoint cat "$image" /usr/local/bin/lemmy_server > tls/lemmy_server.orig
  python3 trust.py patch tls/lemmy_server.orig tls/lemmy_server tls/root.crt
  chmod 755 tls/lemmy_server
  rm tls/lemmy_server.orig
fi

docker compose up -d postgres caddy ca lemmy
echo "Waiting for Lemmy"
until ./api.sh GET /api/v3/site >/dev/null 2>&1; do sleep 2; done
echo "Lemmy is up at https://lemmy.test (port 8444 on localhost). Seed it with ./seed.sh."
