# Agent Memory Challenge (AMC/01) — causal-memory submission image.
#
# Builds and runs the Add/Search integration server (`causal-memory-amc`).
# The platform (or any host) starts the documented entrypoint and evaluates
# the exposed HTTP contract:
#   POST /add      store memory chunks (user_id-isolated)
#   POST /search   return ordered memory evidence
#   GET  /health   liveness
#
# Build:  docker build -t causal-memory-amc .
# Run:    docker run -p 8787:8787 -v amc-data:/data causal-memory-amc
#         (the DB lives at /data/amc.db; override with AMC_DB)

# ── Builder: compile the workspace, take only the amc binary ───────────────
FROM rust:1.92-bookworm AS builder
WORKDIR /build
COPY . .
# The workspace lints deny correctness issues; release profile is LTO+stripped.
RUN cargo build --release --bin causal-memory-amc

# ── Runtime: Debian bookworm (non-slim) already ships ca-certificates and
#    libssl3, which the linked HTTP stack needs — no apt in the image. ──────
FROM debian:bookworm
COPY --from=builder /build/target/release/causal-memory-amc /usr/local/bin/causal-memory-amc

ENV AMC_DB=/data/amc.db \
    AMC_PORT=8787
VOLUME /data
EXPOSE 8787

CMD ["sh", "-c", "causal-memory-amc --db \"$AMC_DB\" --port \"$AMC_PORT\""]
