# Agenct v2 Hub: single Rust binary + static native-ESM web UI.
ARG RUST_IMAGE=rust:1-bookworm
ARG BASE_IMAGE=debian:bookworm-slim

FROM ${RUST_IMAGE} AS build
WORKDIR /src
COPY hub/Cargo.toml hub/Cargo.lock hub/
COPY hub/src hub/src
COPY hub/tests hub/tests
COPY node/agenct-node.py node/agenct-node.py
RUN cargo build --locked --release --manifest-path hub/Cargo.toml

FROM ${BASE_IMAGE}
ARG APP_UID=1001
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --uid ${APP_UID} --create-home --shell /usr/sbin/nologin agenct
WORKDIR /opt/agenct
COPY hub/web hub/web
COPY --from=build /src/hub/target/release/agenct-hub /usr/local/bin/agenct-hub
RUN mkdir -p /data && chown -R agenct:agenct /data
USER agenct
ENV AGENCT_BIND=0.0.0.0:8080 \
    AGENCT_DATA_DIR=/data \
    AGENCT_WEB_DIR=/opt/agenct/hub/web
VOLUME ["/data"]
EXPOSE 8080
STOPSIGNAL SIGINT
ENTRYPOINT ["/usr/local/bin/agenct-hub"]
