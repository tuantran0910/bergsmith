# Bergsmith application images (BS-020.T1).
#
#   docker build --target coordinator -t bergsmith-coordinator .
#   docker build --target worker      -t bergsmith-worker .
#   docker build --target web         -t bergsmith-web .
#
# Base images are pinned by digest. The coordinator image contains the Rust
# binary only; the web image contains nginx and the built UI; the worker image
# contains the worker binary.

# syntax=docker/dockerfile:1

FROM node:24-alpine@sha256:6a1c190e17215d844926b0677f4bf87273ff7d5e54db06d3b5b5863ab0b09a3d AS web-build
WORKDIR /src/web
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-fund --no-audit
COPY web/ ./
RUN npm run build

# The UI is its own image. The development Compose stack serves it with nginx
# and proxies /api, /auth, and /healthz to the coordinator, so the coordinator
# image contains no static assets.
FROM nginx:1.29-alpine@sha256:5616878291a2eed594aee8db4dade5878cf7edcb475e59193904b198d9b830de AS web
COPY deploy/docker/nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=web-build /src/web/dist /usr/share/nginx/html
EXPOSE 8080

FROM rust:1.94.1-bookworm@sha256:6ae102bdbf528294bc79ad6e1fae682f6f7c2a6e6621506ba959f9685b308a55 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates crates
COPY bin bin
RUN cargo build --release --locked --bin bergsmith-coordinator --bin bergsmith-worker

FROM debian:bookworm-slim@sha256:7ff6ba31527b9e336162fa061746c366c4b2032f4d2cff2078bea43f78efe3f8 AS runtime
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates libssl3 curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /var/lib/bergsmith bergsmith
USER 10001
WORKDIR /var/lib/bergsmith

FROM runtime AS coordinator
COPY --from=build /src/target/release/bergsmith-coordinator /usr/local/bin/bergsmith-coordinator
ENTRYPOINT ["bergsmith-coordinator"]
CMD ["serve", "--config", "/etc/bergsmith/coordinator.json"]

FROM runtime AS worker
COPY --from=build /src/target/release/bergsmith-worker /usr/local/bin/bergsmith-worker
ENTRYPOINT ["bergsmith-worker"]
CMD ["run", "--config", "/etc/bergsmith/worker.json", "--token-file", "/run/secrets/worker-token"]
