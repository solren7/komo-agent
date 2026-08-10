# No `# syntax=docker/dockerfile:1` directive on purpose: it makes BuildKit
# fetch the external dockerfile frontend image, which a locked-down registry
# mirror (e.g. fnOS's docker.fnnas.com) 401s on. The daemon's built-in
# frontend (Docker 23+) already supports the `RUN --mount=type=cache` used
# below, so nothing here needs the external frontend.
#
# Multi-stage build for the komo gateway.
#   builder : rust toolchain + protoc (feishu's protobuf is compiled at build
#             time), produces the release binary
#   runtime : debian-slim + CA certs (TLS to Telegram / Home Assistant / the
#             LLM API needs a trust store) + libssl3 (the wechat channel's
#             `wechatbot` crate pulls reqwest's native-tls, which dynamically
#             links libssl on Linux — without it the binary won't even load) +
#             tzdata (reminders and the briefing run on local time — set TZ) +
#             git (`komo skills install owner/repo` shells out to `git clone`;
#             only the single-file `…/SKILL.md` form uses the built-in HTTP
#             client) + curl / python3 / uv (nothing in komo calls any of
#             them — skills do, since a skill is largely "here is the command
#             line for this API"; see the runtime stage for the size tradeoff)
#
# Build for the NAS's architecture, NOT your laptop's. On Apple Silicon:
#   docker buildx build --platform linux/amd64 -t ghcr.io/solren7/komo:latest --push .
# Deployment lives in compose.yaml (registry pull by default; a Dockhand git
# stack sets KOMO_PULL_POLICY=build to build natively on the NAS).

# ---- builder ----------------------------------------------------------------
FROM rust:trixie AS builder

# protoc for lark-websocket-protobuf's build script. `libprotobuf-dev` is
# required too: it ships the well-known protos (google/protobuf/descriptor.proto)
# under /usr/include that protoc resolves imports against — `protobuf-compiler`
# alone omits them, so the build fails with "descriptor.proto: File not found".
# (pin the rust tag, e.g. rust:1.90-bookworm, for fully reproducible builds.)
RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler libprotobuf-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .

# Cache the cargo registry and target dir across builds (BuildKit). The target
# dir is a cache mount, so the binary must be copied out to a real layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked \
    && cp target/release/komo /usr/local/bin/komo

# ---- runtime ----------------------------------------------------------------
FROM debian:trixie-slim AS runtime

# Measured on trixie-slim/amd64: base layer 31 MB, +5 MB for curl, +35 MB for
# git (liberror-perl is a hard dependency, not a recommend, so it comes along
# either way). --no-install-recommends saves a further 4 MB — worth keeping,
# but git is the real cost. Drop git if you only ever install single-file
# skills by raw SKILL.md URL, which uses the built-in HTTP client.
#
# python3 is the full interpreter, NOT python3-minimal: minimal omits the ssl
# module, so every HTTPS call from a skill script would fail. It costs 11 MB
# here (the expensive shared libs are already in for other reasons).
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates tzdata libssl3 git curl python3 \
    && rm -rf /var/lib/apt/lists/*

# uv, for skills that need third-party packages (the system python3 above only
# covers stdlib-only scripts). Copied from the official image — two static musl
# binaries, +21 MB, no installer script and no network fetch at run time. Bump
# the tag to upgrade. If your registry mirror can't reach ghcr.io (fnOS's
# docker.fnnas.com 401s on it, same reason the dockerfile frontend is avoided
# above), swap this for a GitHub release tarball or drop the line — nothing in
# komo itself depends on uv.
COPY --from=ghcr.io/astral-sh/uv:0.12.3 /uv /uvx /usr/local/bin/

COPY --from=builder /usr/local/bin/komo /usr/local/bin/komo

# All durable state (config.toml, .env, state.db / kanban.db / memory.db, logs)
# lives here — mount a TrueNAS dataset to it so nothing is lost on redeploy.
ENV KOMO_HOME=/data
VOLUME ["/data"]

# The gateway is outbound-only (Telegram long-poll, Feishu WS, LLM API, LAN HA)
# — no inbound port to EXPOSE. `komo gateway` runs in the foreground; Docker's
# restart policy replaces launchd. One-off CLI ops bypass the entrypoint, e.g.
#   docker exec komo komo pair approve <code>

# "Running" ≠ alive: probe the gateway's loopback /health (via the rendezvous
# file in $KOMO_HOME) so a wedged gateway flips the container unhealthy.
HEALTHCHECK --interval=60s --timeout=5s --start-period=30s --retries=3 \
    CMD ["komo", "health"]

ENTRYPOINT ["komo"]
CMD ["gateway"]
