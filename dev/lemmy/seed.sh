#!/usr/bin/env bash
# Test data through Lemmy's HTTP API: open registration, user carol, community rust, two posts
# and a comment. Prints the ActivityPub ids. Re-running adds more posts and keeps the rest.
set -euo pipefail
cd "$(dirname "$0")"
api() { ./api.sh "$@"; }
tag=$(openssl rand -hex 3)

admin=$(api POST /api/v3/user/login '{"username_or_email":"admin","password":"lemmy-admin-password"}' | jq -r .jwt)
# Open registration, and rate limits high enough for scripted posting.
api PUT /api/v3/site '{"registration_mode":"Open","rate_limit_post":1000,"rate_limit_comment":1000,"rate_limit_register":1000}' "$admin" >/dev/null

login='{"username_or_email":"carol","password":"carol-password-1234"}'
carol=$(api POST /api/v3/user/login "$login" 2>/dev/null | jq -r .jwt) || carol=
if [ -z "$carol" ]; then
  carol=$(api POST /api/v3/user/register \
    '{"username":"carol","password":"carol-password-1234","password_verify":"carol-password-1234","show_nsfw":false}' | jq -r .jwt)
fi

community=$(api GET '/api/v3/community?name=rust' 2>/dev/null | jq -r .community_view.community.id) || community=
if [ -z "$community" ]; then
  community=$(api POST /api/v3/community '{"name":"rust","title":"Rust","description":"The Rust programming language"}' "$carol" \
    | jq -r .community_view.community.id)
fi
echo "community: https://lemmy.test/c/rust ($community)"

post() {
  api POST /api/v3/post "$(jq -nc --arg n "$1" --arg b "$2" --argjson c "$community" '{name:$n, body:$b, community_id:$c}')" "$carol" \
    | jq -r '.post_view.post | "\(.id) \(.ap_id)"'
}
read -r p1 u1 < <(post "Borrow checker tips for beginners $tag" "Lifetimes are easier once you think of them as scopes.")
read -r p2 u2 < <(post "Async runtimes compared $tag" "Tokio, smol and async-std, with benchmarks.")
echo "post: $u1"
echo "post: $u2"
api POST /api/v3/comment "$(jq -nc --argjson p "$p1" '{post_id:$p, content:"Clippy catches most of these mistakes too."}')" "$carol" \
  | jq -r '"comment: \(.comment_view.comment.ap_id)"'
