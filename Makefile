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

.PHONY: all build release test test-all check fmt fmt-check lint clippy \
        bench run-cli py-dev py-build py-test clean help

all: build ## Default: debug build of the Rust crates

build: ## Debug build (default workspace members)
	$(CARGO) build

release: ## Release build (default workspace members)
	$(CARGO) build --release

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

bench: ## Run benchmarks
	$(CARGO) bench

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
