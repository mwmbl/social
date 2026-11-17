# Build: docker build -t mwmbl-social .
# Run:   docker run -v mwmbl-social:/data -e MWMBL_SOCIAL_BASE_URL=https://social.example mwmbl-social
FROM rust:1.100-bookworm AS build
WORKDIR /src
# Use the image's own 1.100.0 rather than letting rust-toolchain.toml pull clippy and rustfmt.
ENV RUSTUP_TOOLCHAIN=1.100.0
COPY . .
RUN cargo build --release -p mwmbl_social

FROM debian:bookworm-slim
# ca-certificates: outbound HTTPS (signed fetches, FASP calls) verifies against system roots.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# A fixed uid and gid, so a deployment can chown a bind-mounted data directory to 10001:10001
# before the first run. 10001 is above the uids hosts usually give their first users.
RUN groupadd --system --gid 10001 mwmbl \
    && useradd --system --uid 10001 --gid mwmbl --home /data --no-create-home mwmbl
COPY --from=build /src/target/release/mwmbl-social /usr/local/bin/mwmbl-social
ENV MWMBL_SOCIAL_DATA_DIR=/data \
    MWMBL_SOCIAL_LISTEN=0.0.0.0:8080
RUN mkdir -p /data && chown 10001:10001 /data
USER 10001:10001
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["mwmbl-social"]
CMD ["serve"]
