# Prebuilt Rust application image. Build context must be the repository root.
# BuildKit's TARGETARCH selects the matching Linux musl release directory when
# TRIPLE=auto. Explicit TRIPLE (including empty for target/release) selects a path;
# every selection is checked against the image's actual platform before copying.
# The validator runs on BUILDPLATFORM without executing the application. Python
# stays in the validation stage; the final runtime contains Debian, curl and CAs.
# Dynamic ELF dependencies and native execution require separate runtime tests.
ARG REGISTRY_PREFIX=""
FROM --platform=$BUILDPLATFORM ${REGISTRY_PREFIX}python:3.11-slim AS validate
ARG TARGETOS
ARG TARGETARCH
ARG BIN=websearch
ARG TRIPLE=auto
WORKDIR /inputs
COPY deploy/images/validate_app_binary.py /validate_app_binary.py
COPY target/ target/
RUN python3 /validate_app_binary.py --target-os "$TARGETOS" --arch "$TARGETARCH" \
    --bin "$BIN" --triple "$TRIPLE" --output /validated/app

FROM ${REGISTRY_PREFIX}debian:trixie-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=validate /validated/app /usr/local/bin/app
ENTRYPOINT ["/usr/local/bin/app"]
