# Agenct intranet image: Rust Hub + web assets + pinned DeepSeek adapter.
# Runtime configuration and the database live in the /data volume; no secrets in the image.
ARG RUST_IMAGE=rust:1-bookworm
ARG BASE_IMAGE=debian:bookworm-slim

FROM ${RUST_IMAGE} AS build
WORKDIR /src
COPY hub/Cargo.toml hub/Cargo.lock hub/
COPY hub/src hub/src
COPY hub/tests hub/tests
# include_str!/include_bytes! pull these into the binary.
COPY scripts/extract-pdf.py scripts/extract-pdf.py
COPY scripts/agenct-node.py scripts/agenct-node.py
COPY sandbox sandbox
RUN cargo build --locked --release --manifest-path hub/Cargo.toml

FROM ${BASE_IMAGE}
ARG PIP_INDEX_URL=https://pypi.org/simple
ARG APP_UID=1001
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3 python3-venv curl ca-certificates poppler-utils sqlite3 tini \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --uid ${APP_UID} --create-home --shell /usr/sbin/nologin agenct
WORKDIR /opt/agenct
COPY adapters/dsh adapters/dsh
# Pinned adapter environment outside the data volume (rebuilt with the image).
RUN python3 -m venv /opt/agenct-model/dsh-venv \
 && /opt/agenct-model/dsh-venv/bin/pip install --no-cache-dir --disable-pip-version-check \
      --index-url "${PIP_INDEX_URL}" -r adapters/dsh/requirements.txt
COPY hub/web hub/web
COPY scripts scripts
COPY deploy/docker/entrypoint.py deploy/docker/entrypoint.py
COPY --from=build /src/hub/target/release/agenct-hub hub/target/release/agenct-hub
RUN chown -R root:root /opt/agenct /opt/agenct-model && chmod -R go-w /opt/agenct /opt/agenct-model
USER agenct
ENV LANG=C.UTF-8 PYTHONDONTWRITEBYTECODE=1
VOLUME ["/data"]
# The Hub and the DSH supervisor both shut down cleanly on SIGINT.
STOPSIGNAL SIGINT
ENTRYPOINT ["/usr/bin/tini", "-g", "--", "python3", "/opt/agenct/deploy/docker/entrypoint.py"]
CMD ["run"]
