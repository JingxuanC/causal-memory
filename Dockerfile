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
#    it must be re-declared with ARG to be visible in this stage — without
#    the declaration the fallback silently picks x64 on every arch.
ARG TARGETARCH
ARG ORT_VERSION=1.26.0
# Build-time fetches (this ORT download, the model seed below) may need a
# proxy on restricted networks — GitHub releases included. Scoped to these
# RUN steps only: the standard proxy build-args leak into every RUN and
# break apt (its HTTP repos get 403 from filtering proxies).
ARG FETCH_PROXY=
RUN set -eux; \
    if [ -n "$FETCH_PROXY" ]; then export https_proxy="$FETCH_PROXY" http_proxy="$FETCH_PROXY"; fi; \
    case "${TARGETARCH:-amd64}" in \
      amd64) ort_arch=x64 ;; \
      arm64) ort_arch=aarch64 ;; \
      *) echo "no ONNX Runtime prebuilt for TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    curl -fsSL --retry 3 -o /tmp/ort.tgz \
      "https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/onnxruntime-linux-${ort_arch}-${ORT_VERSION}.tgz"; \
    mkdir -p /opt/onnxruntime; \
    tar -xzf /tmp/ort.tgz -C /opt/onnxruntime --strip-components=1; \
    rm -f /tmp/ort.tgz
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
#    HF_ENDPOINT defaults to DIRECT huggingface.co: the hf-mirror.com mirror
#    omits the Content-Range header that fastembed's chunked downloader
#    requires ("Header Content-Range is missing"), so the mirror cannot
#    warm the cache at all. Build networks that block HF should pass a proxy
#    scoped to THIS step only — the standard proxy build-args leak into
#    every RUN and break apt (HTTP repos get 403 from filtering proxies):
#      docker build --build-arg FETCH_PROXY=http://<proxy> ...
#    Set HF_ENDPOINT only for a mirror that serves range requests.
#    WARM_EMBED=1 (default) FAILS the build when the embedder cannot come
#    up — an image without the semantic layer is otherwise indistinguishable
#    from a working one until the leaderboard says so. Pass
#    `--build-arg WARM_EMBED=0` to accept a BM25-only image instead.
ARG HF_ENDPOINT=
# Pre-seed the hf-hub cache layout with curl: hf-hub's chunked range
# downloader breaks behind filtering proxies (and mirrors that omit
# Content-Range), while plain GET works everywhere. hf-hub's `get()`
# short-circuits on a cache hit, so a fully seeded layout makes the
# --warm-embed step below effectively offline. Layout contract (hf-hub
# 0.5): models--<org>--<name>/refs/main holds the commit hash WITHOUT a
# trailing newline (the ref content is used verbatim as the snapshots dir
# name), snapshots/<commit>/<file> holds the payload.
ARG MODEL_REPO=Xenova/bge-small-en-v1.5
RUN set -eux; \
    if [ -n "$HF_ENDPOINT" ]; then export HF_ENDPOINT="$HF_ENDPOINT"; fi; \
    if [ -n "$FETCH_PROXY" ]; then export https_proxy="$FETCH_PROXY" http_proxy="$FETCH_PROXY"; fi; \
    base="${HF_ENDPOINT:-https://huggingface.co}"; \
    org="${MODEL_REPO%/*}"; name="${MODEL_REPO#*/}"; \
    dir="/fastembed-cache/models--${org}--${name}"; \
    commit=$(curl -fsSL "$base/api/models/$MODEL_REPO" | grep -o '"sha":"[a-f0-9]*"' | head -1 | cut -d'"' -f4); \
    [ -n "$commit" ]; \
    mkdir -p "$dir/refs" "$dir/snapshots/$commit/onnx"; \
    printf %s "$commit" > "$dir/refs/main"; \
    for f in config.json tokenizer.json tokenizer_config.json special_tokens_map.json onnx/model.onnx; do \
      curl -fsSL "$base/$MODEL_REPO/resolve/main/$f" -o "$dir/snapshots/$commit/$f"; \
    done
ARG WARM_EMBED=1
ENV FASTEMBED_CACHE_DIR=/fastembed-cache
# --warm-embed stays as the proof gate: it must initialize the embedder
# from the seeded cache (zero network), failing the build otherwise.
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
