#!/bin/sh
# Resolves the placeholders in audit/ci-draft/ (DRAFT, audit 2026-11-19, P3).
# Needs network access to github.com and Docker Hub; the audit itself ran offline, so the
# values were left as placeholders. Check each SHA against the action's release page before use.
set -eu

# Actions: `git ls-remote` prints the commit for a tag. For annotated tags the `^{}` line is the
# commit to pin; for branches (dtolnay/rust-toolchain@1.100.0) use refs/heads.
for ref in \
    actions/checkout:refs/tags/v4 \
    Swatinem/rust-cache:refs/tags/v2 \
    docker/setup-buildx-action:refs/tags/v3 \
    docker/metadata-action:refs/tags/v5 \
    docker/login-action:refs/tags/v3 \
    docker/build-push-action:refs/tags/v5 \
    dtolnay/rust-toolchain:refs/heads/1.100.0; do
    repo=${ref%%:*}
    name=${ref#*:}
    sha=$(git ls-remote "https://github.com/$repo" "$name" "$name^{}" | tail -n1 | cut -f1)
    echo "$repo@${name##*/} -> $sha"
done

# Base images: the multi-arch index digest (what `FROM image@sha256:...` should name).
for image in rust:1.100-bookworm debian:bookworm-slim; do
    digest=$(docker buildx imagetools inspect "$image" --format '{{json .Manifest.Digest}}' | tr -d '"')
    echo "$image -> $digest"
done
# Without buildx: docker pull <image> && docker inspect --format '{{index .RepoDigests 0}}' <image>
