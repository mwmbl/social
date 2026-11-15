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
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --home /data mwmbl
COPY --from=build /src/target/release/mwmbl-social /usr/local/bin/mwmbl-social
ENV MWMBL_SOCIAL_DATA_DIR=/data \
    MWMBL_SOCIAL_LISTEN=0.0.0.0:8080
RUN mkdir -p /data && chown mwmbl /data
USER mwmbl
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["mwmbl-social"]
CMD ["serve"]
