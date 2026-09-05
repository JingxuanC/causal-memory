# Causal Memory — common dev tasks.
# Note: crates/causal-memory-py is a maturin cdylib; plain cargo release
# builds cover the Rust crates only (see workspace Cargo.toml).

PY_CRATE := crates/causal-memory-py

# Prefer the repo-local Rust toolchain (.cargo/bin) if present.
# NOTE: invoke cargo via the absolute $(CARGO) path — Apple make 3.81's
# direct-exec fast path ignores the PATH we reassign below.
LOCAL_CARGO := $(CURDIR)/.cargo/bin
ifneq ($(wildcard $(LOCAL_CARGO)/cargo),)
CARGO := $(LOCAL_CARGO)/cargo
PATH := $(LOCAL_CARGO):$(PATH)
export PATH
else
CARGO := cargo
endif

.PHONY: all build release install uninstall test test-all check fmt fmt-check lint clippy \
        bench bench-locomo bench-longmemeval bench-memora bench-memory \
        bench-causal-eval run-cli py-dev py-build py-test clean help

all: build ## Default: debug build of the Rust crates

build: ## Debug build (default workspace members)
	$(CARGO) build

release: ## Release build (default workspace members)
	$(CARGO) build --release

# Install the release binaries onto PATH (default ~/.local/bin, which is on
# PATH in this shell; override with `make install PREFIX=/usr/local`).
PREFIX ?= $(HOME)/.local
BINS ?= causal-memory

install: release ## Release build, then copy BINS into $(PREFIX)/bin (on PATH)
	@mkdir -p $(PREFIX)/bin
	for b in $(BINS); do \
		cp target/release/$$b $(PREFIX)/bin/$$b && echo "installed $(PREFIX)/bin/$$b"; \
	done

uninstall: ## Remove BINS from $(PREFIX)/bin
	for b in $(BINS); do rm -f $(PREFIX)/bin/$$b && echo "removed $(PREFIX)/bin/$$b"; done

test: ## Run tests for the whole workspace (dev linking incl. py crate)
	$(CARGO) test --workspace

test-all: test py-test ## Rust tests + Python bindings tests

check: ## Fast type check without codegen
	$(CARGO) check --workspace

fmt: ## Format all Rust code
	$(CARGO) fmt --all

fmt-check: ## Verify formatting (CI)
	$(CARGO) fmt --all -- --check

lint: clippy ## Alias for clippy

clippy: ## Run clippy with workspace lints
	$(CARGO) clippy --workspace --all-targets

# The repo has no #[bench]/criterion targets — `cargo bench` would run
# nothing. Benchmarks are harness binaries (declared as [[bin]] in
# crates/causal-memory-cli/Cargo.toml pointing at benches/**/main.rs).
# Only the retrieval micro-benchmark is self-contained; the eval harnesses
# need an LLM key (DEEPSEEK_API_KEY + LOCOMO_LLM_API/LOCOMO_LLM_MODEL) and
# their datasets under benches/<name>/data.
# DB defaults to the standard causal-memory data location (the repo-root
# causal.db is empty and only useful with DB=... pointing elsewhere).
DB ?= $(HOME)/.local/share/causal-memory/causal.db

bench: ## Retrieval micro-benchmark vs vector/keyword on DB=$(DB)
	$(CARGO) run --release -p causal-memory-cli --bin causal-memory-bench -- $(DB)

bench-locomo: ## LoCoMo eval harness (needs LLM key; pass ARGS="...")
	$(CARGO) run --release -p causal-memory-cli --bin causal-memory-locomo -- $(ARGS)

bench-longmemeval: ## LongMemEval harness (needs LLM key; pass ARGS="...")
	$(CARGO) run --release -p causal-memory-cli --bin causal-memory-longmemeval -- $(ARGS)

bench-memora: ## Memora harness (needs LLM key; pass ARGS="...")
	$(CARGO) run --release -p causal-memory-cli --bin causal-memory-memora -- $(ARGS)

bench-memory: ## Memory harness (needs LLM key; pass ARGS="...")
	$(CARGO) run --release -p causal-memory-cli --bin causal-memory-bench-memory -- $(ARGS)

bench-causal-eval: ## Causal-eval harness (needs LLM key; pass ARGS="...")
	$(CARGO) run --release -p causal-memory-cli --bin causal-memory-causal-eval -- $(ARGS)

run-cli: build ## Run the CLI binary (pass args via ARGS="...")
	$(CARGO) run -p causal-memory-cli -- $(ARGS)

py-dev: ## Build & install Python bindings into the venv (maturin develop)
	cd $(PY_CRATE) && maturin develop

py-build: ## Build Python wheel (maturin)
	cd $(PY_CRATE) && maturin build --release

py-test: ## Run Python binding tests (requires py-dev first)
	cd $(PY_CRATE) && python -m pytest

clean: ## Remove build artifacts
	$(CARGO) clean

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'
