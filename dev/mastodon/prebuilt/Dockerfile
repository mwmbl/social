# For environments where `docker build` can't reach crates.io (e.g. behind an intercepting
# proxy): package a binary built on the host. Build it with
#   cargo build --release -p mwmbl_social && cp target/release/mwmbl-social dev/mastodon/prebuilt/
# The host must have a glibc no newer than this image's (Ubuntu 24.04).
FROM ubuntu:24.04
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY mwmbl-social /usr/local/bin/mwmbl-social
ENV MWMBL_SOCIAL_DATA_DIR=/data MWMBL_SOCIAL_LISTEN=0.0.0.0:8080
VOLUME /data
ENTRYPOINT ["mwmbl-social"]
CMD ["serve"]
