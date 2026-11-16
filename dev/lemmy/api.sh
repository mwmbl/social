#!/usr/bin/env bash
# Call Lemmy's HTTP API from the host, through Caddy on port 8444, trusting our root CA.
#   ./api.sh METHOD PATH [JSON] [JWT]
# Prints the response body; fails on an HTTP error (after printing its body to stderr).
set -euo pipefail
cd "$(dirname "$0")"
method=$1 path=$2 body=${3:-} jwt=${4:-}
args=(-sS --noproxy '*' --resolve lemmy.test:8444:127.0.0.1 --cacert tls/root.crt
      -X "$method" -H 'Content-Type: application/json' -w '\n%{http_code}')
[ -n "$jwt" ] && args+=(-H "Authorization: Bearer $jwt")
[ -n "$body" ] && args+=(--data "$body")
out=$(curl "${args[@]}" "https://lemmy.test:8444$path")
code=${out##*$'\n'}
out=${out%$'\n'*}
if [ "$code" -ge 400 ]; then echo "$method $path: HTTP $code: $out" >&2; exit 1; fi
printf '%s\n' "$out"
