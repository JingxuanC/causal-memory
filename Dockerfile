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
FROM rust:1.92-trixie AS builder
WORKDIR /build
COPY . .
# The workspace lints deny correctness issues; release profile is LTO+stripped.
# local-embed: offline fastembed (bge-small-en-v1.5, ONNX) for semantic fusion.
# CARGO_BUILD_JOBS: cap compilation parallelism — ONNX Runtime's C++ build is
# memory-hungry and can OOM small Docker VMs (3.8GB default) at full -jN.
ARG CARGO_BUILD_JOBS=2
ENV CARGO_BUILD_JOBS=$CARGO_BUILD_JOBS
RUN cargo build --release --bin causal-memory-amc --features local-embed

# ── Runtime: Debian trixie (non-slim) ships ca-certificates + libssl3 AND a
#    GCC-14 libstdc++ — the bundled ONNX Runtime was built with GCC 13+ and
#    its C++ symbols (__cxa_call_terminate, _M_replace_cold) do not exist in
#    bookworm's libstdc++. No apt in the image. ─────────────────────────────
FROM debian:trixie
COPY --from=builder /build/target/release/causal-memory-amc /usr/local/bin/causal-memory-amc

ENV AMC_DB=/data/amc.db \
    AMC_PORT=8787
VOLUME /data
EXPOSE 8787

CMD ["sh", "-c", "causal-memory-amc --db \"$AMC_DB\" --port \"$AMC_PORT\""]
