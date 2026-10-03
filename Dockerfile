# Agent Memory Challenge (AMC/01) — causal-memory submission image.
#
# Builds and runs the Add/Search integration server (`causal-memory-amc`)
# on the shared memory facade — the same retrieval pipeline as the MCP
# server and Python bindings (BM25 inverted index + optional semantic +
# entity boost + hop expansion + RRF fusion).
#   POST /add      store memory chunks (one store per user_id)
#   POST /search   return ordered memory evidence
#   GET  /health   liveness + the live embedding model (null = BM25-only)
#
# Build:  docker build -t causal-memory-amc .
# Run:    docker run -p 8787:8787 -v amc-data:/data causal-memory-amc
#         (per-user DBs live under /data/stores; override with AMC_DB_DIR.
#          Write strategy defaults to raw — no LLM keys on the platform;
#          AMC_WRITE_MODE=distill needs CAUSAL_MEMORY_LLM_* env.)
#
# Verify the semantic layer after a run — this is the whole point of the
# build-time warm-up below:
#   curl -s localhost:8787/health     # expect embedding: BAAI/bge-small-en-v1.5
# `embedding: null` means the process is BM25-only (see the server's WARN).

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

# ── ONNX Runtime (Batch 0): `local-embed` builds against `ort-load-dynamic`
#    (see crates/causal-memory/Cargo.toml — static linking fails on
#    CLT-only macOS), so the shared library is dlopen'd from the system at
#    runtime. Nothing ships it: without this step the local embedder cannot
#    initialize in the image at all, no matter how the model cache is set up.
#    Debian trixie's own package (libonnxruntime1.21) is older than the line
#    this build targets, so take the official prebuilt release — the same
#    1.26 the macOS dev setup runs. TARGETARCH (BuildKit) picks the archive;
#    it defaults to amd64 for the legacy builder.
ARG ORT_VERSION=1.26.0
RUN set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64) ort_arch=x64 ;; \
      arm64) ort_arch=aarch64 ;; \
      *) echo "no ONNX Runtime prebuilt for TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/ort.tgz \
      "https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/onnxruntime-linux-${ort_arch}-${ORT_VERSION}.tgz"; \
    mkdir -p /opt/onnxruntime; \
    tar -xzf /tmp/ort.tgz -C /opt/onnxruntime --strip-components=1; \
    rm /tmp/ort.tgz
# OpenMP runtime the prebuilt library links against.
RUN apt-get update \
 && apt-get install -y --no-install-recommends libgomp1 \
 && rm -rf /var/lib/apt/lists/*
ENV ORT_DYLIB_PATH=/opt/onnxruntime/lib/libonnxruntime.so

# ── Semantic layer, part 1: download the ONNX model into the fastembed cache
#    AT BUILD TIME. The runtime sets FASTEMBED_CACHE_DIR to a path on the
#    mounted volume, and LocalEmbedder::new() refuses to download into a
#    directory that does not exist — a fresh volume therefore used to leave
#    the server BM25-only for its whole lifetime (shared_embedder() is a
#    OnceLock: a failed init is never retried). Warming here makes runtime
#    downloads zero.
#    HF_ENDPOINT defaults to the mirror: huggingface.co is unreachable from
#    the environments this image is built in (the repo already records a
#    150s stall on the direct host), and a build-time download is exactly
#    where that would bite.
#    WARM_EMBED=1 (default) FAILS the build when the embedder cannot come
#    up — an image without the semantic layer is otherwise indistinguishable
#    from a working one until the leaderboard says so. Pass
#    `--build-arg WARM_EMBED=0` to accept a BM25-only image instead.
ARG HF_ENDPOINT=https://hf-mirror.com
ENV HF_ENDPOINT=$HF_ENDPOINT
ARG WARM_EMBED=1
ENV FASTEMBED_CACHE_DIR=/fastembed-cache
RUN ./target/release/causal-memory-amc --warm-embed \
    || { [ "$WARM_EMBED" = "0" ] \
         && echo "warn: building a BM25-only image (WARM_EMBED=0)"; }

# ── Runtime: Debian trixie (non-slim) ships ca-certificates + libssl3 AND a
#    GCC-14 libstdc++ — the ONNX Runtime prebuilt needs the newer C++
#    symbols (__cxa_call_terminate, _M_replace_cold) that bookworm's
#    libstdc++ lacks. ───────────────────────────────────────────────────────
FROM debian:trixie
COPY --from=builder /build/target/release/causal-memory-amc /usr/local/bin/causal-memory-amc

# ONNX Runtime shared library + its OpenMP dependency (see the builder).
COPY --from=builder /opt/onnxruntime/lib/ /usr/local/lib/onnxruntime/
RUN apt-get update \
 && apt-get install -y --no-install-recommends libgomp1 \
 && rm -rf /var/lib/apt/lists/*
ENV ORT_DYLIB_PATH=/usr/local/lib/onnxruntime/libonnxruntime.so

# Semantic layer, part 2: ship the warmed cache inside the image at the path
# the runtime reads. `docker run -v amc-data:/data` seeds a NEW volume from
# the image's /data content, so a first run starts with the model already
# present; the mkdir covers a bind-mounted (empty) /data, where the server
# would otherwise have to download on first use — or, worse, not initialize
# at all (the code creates that directory before init for the same reason).
COPY --from=builder /fastembed-cache /data/fastembed-cache
RUN mkdir -p /data/fastembed-cache /data/stores

# FASTEMBED_CACHE_DIR points at the volume so a model downloaded there
# survives container restarts; the image copy above is what makes a fresh
# volume work.
ENV AMC_DB_DIR=/data/stores \
    AMC_PORT=8787 \
    AMC_WRITE_MODE=raw \
    FASTEMBED_CACHE_DIR=/data/fastembed-cache
# CAUSAL_MEMORY_EMBED_WRITE (write-time chunk vectors) is left to the server's
# own default — on for the AMC binary, off for everything else. Set it to 0
# here or at `docker run -e` to serve BM25-only.
VOLUME /data
EXPOSE 8787

CMD ["sh", "-c", "causal-memory-amc --db-dir \"$AMC_DB_DIR\" --port \"$AMC_PORT\" --write-mode \"$AMC_WRITE_MODE\""]
