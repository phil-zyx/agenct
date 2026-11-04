# Operator must supply an audited, digest-pinned base with /usr/bin/python3, /usr/bin/git
# and all required project tools. This Dockerfile installs nothing and is not built automatically.
ARG BASE_IMAGE
FROM ${BASE_IMAGE}
COPY --chmod=0555 runner.py /opt/agenct/runner.py
USER 65534:65534
WORKDIR /work
ENTRYPOINT ["/usr/bin/python3", "-I", "/opt/agenct/runner.py"]
