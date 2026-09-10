#!/usr/bin/env python3
"""Capture real outputs for the 30s danger-warning demo (fresh DB, no mocks).

Run from the repo root:  python3 scripts/capture_demo30.py
Requires: target/release/causal-memory (cargo build --release) + requests.
Writes /tmp/demo30_out.json for scripts/render_demo30.py.
"""
import json, os, sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(REPO, "scripts"))
os.environ["CAUSAL_MEMORY_DB"] = "/tmp/demo30.db"
from causal_memory_client import CausalMemoryClient

# fresh store
for f in ("/tmp/demo30.db", "/tmp/demo30.db-shm", "/tmp/demo30.db-wal"):
    if os.path.exists(f):
        os.remove(f)

cm = CausalMemoryClient.stdio(os.path.join(REPO, "target/release/causal-memory"))

# Seed the historical lessons (the "memory" the agent will be warned with).
# NOTE: the outcome text must contain a failure signal word ("failed",
# "error", "crash", ...) — the polarity heuristic has no "broke".
cm.record_decision(
    "Skipped the test suite and pushed straight to main to save 4 minutes",
    "production login failed for 40 minutes; emergency rollback and postmortem",
    "caused", "deployment", 0.9, "user_feedback")
cm.record_decision(
    "Ran the full test suite before every push to main",
    "caught a broken DB migration before merge three times last quarter",
    "prevented", "deployment", 0.8, "user_feedback")
cm.record_decision(
    "Added a pre-push hook running the fast test subset",
    "no direct-main breakage since adoption; deploys stay green",
    "caused", "deployment", 0.8, "llm_inferred")

out = {
    "intervention": cm.intervention_query("skip tests and push directly to main"),
    "search": cm.search_causal("push to main without running tests"),
}
cm.close()
json.dump(out, open("/tmp/demo30_out.json", "w"))
print("=== intervention ===")
print(out["intervention"][:1200])
print("=== search ===")
print(out["search"][:800])
