"""Smoke tests for the causal-memory Python bindings.

Runs fully offline: no LLM / embedding endpoints configured, so every call
exercises the BM25 / degraded paths (by design — the same fallback behavior
the MCP server has). `maturin develop` must have been run first.
"""

import pytest

from causal_memory import CausalMemory


@pytest.fixture()
def mem(tmp_path):
    return CausalMemory(str(tmp_path / "test.db"))


def test_open_and_in_memory(tmp_path):
    mem = CausalMemory(str(tmp_path / "a.db"))
    assert mem is not None
    ephemeral = CausalMemory.in_memory()
    assert ephemeral is not None


def test_record_and_search_causal(mem):
    out = mem.record_decision(
        "used Redis mutex for cache stampede protection",
        "deadlock under load because the mutex holder crashed",
        "caused",
        "concurrency",
        confidence_source="rule",
    )
    assert "Recorded" in out

    hits = mem.search_causal(query="Redis mutex cache")
    assert "Redis mutex" in hits


def test_record_decision_defaults(mem):
    out = mem.record_decision("skipped tests", "shipped broken code", "caused", "testing")
    assert "Recorded" in out


def test_facts_roundtrip(mem):
    out = mem.record_fact("tech_stack", "Rust + SQLite", scope="user", confidence=0.9)
    assert "Recorded fact" in out

    hits = mem.search_facts(query="tech stack")
    assert "Rust + SQLite" in hits

    # No query → list most recent.
    listing = mem.search_facts()
    assert "Rust + SQLite" in listing


def test_record_fact_invalid_scope(mem):
    out = mem.record_fact("k", "v", scope="galaxy")
    assert "Invalid scope" in out


def test_replace_same_key_retires(mem):
    mem.record_fact("package_manager", "npm", scope="user")
    out = mem.record_fact("package_manager", "pnpm", scope="user", replace_same_key=True)
    assert "Recorded fact" in out
    hits = mem.search_facts(query="package manager")
    assert "pnpm" in hits


def test_search_memory_unified(mem):
    mem.record_fact("editor", "neovim", scope="user")
    mem.record_decision("used vim macros", "edit speed doubled", "caused", "editor")
    out = mem.search_memory("editor")
    assert "unified" in out
    assert "neovim" in out or "vim macros" in out


def test_invalidate_decision(mem):
    mem.record_decision("deployed on Friday", "weekend outage", "caused", "deploy")
    before = mem.search_causal(query="Friday deploy")
    assert "Friday" in before

    out = mem.invalidate_decision(1, reason="not actually causal")
    assert "Invalidated edge #1" in out

    after = mem.search_causal(query="Friday deploy")
    assert "Friday" not in after

    # Double invalidation is a no-op with a clear message.
    again = mem.invalidate_decision(1)
    assert "already invalidated" in again


def test_trace_cause(mem):
    mem.record_decision(
        "configured Redis without expiry",
        "OOM killed the service",
        "caused",
        "caching",
    )
    out = mem.trace_cause("OOM killed the service")
    assert "Redis" in out


def test_trace_cause_chain(mem):
    mem.record_decision("no cache TTL", "cache grew unbounded", "caused", "caching")
    mem.record_decision("cache grew unbounded", "service OOM crashed", "caused", "caching")
    out = mem.trace_cause_chain("service OOM crashed", max_depth=3)
    assert "chain" in out.lower()


def test_intervention_query_prevented(mem):
    mem.record_decision("added rate limiting", "API abuse", "prevented", "api")
    out = mem.intervention_query("added rate limiting", max_depth=2)
    # With a precedent present we get chains or an honest empty notice —
    # either way it must not error and must mention the action.
    assert "rate limiting" in out


def test_counterfactual_query(mem):
    mem.record_decision("used redis mutex for cache", "deadlock under load", "caused", "concurrency")
    mem.record_decision("switched to channel ownership", "race fixed, all tests pass", "caused", "concurrency")
    out = mem.counterfactual_query("redis mutex", "channel")
    assert "contrastive/empirical counterfactual" in out
    assert "Comparing recorded evidence" in out


def test_reconstruct_lesson_no_llm(mem):
    mem.record_decision("used redis mutex for cache", "deadlock under load", "caused", "concurrency")
    out = mem.reconstruct_lesson("redis mutex")
    assert "Causal subgraph" in out
    # No LLM configured in tests → honest degraded output.
    assert "CAUSAL_MEMORY_LLM" in out


def test_remember_no_llm(mem):
    out = mem.remember("user: I prefer tabs over spaces")
    # Without an LLM endpoint, remember stores raw and says so.
    assert "no LLM" in out or "Stored raw" in out


def test_causal_directory(mem):
    mem.record_decision("used Redis mutex", "deadlock", "caused", "concurrency")
    out = mem.causal_directory(limit=10)
    assert "Redis mutex" in out


def test_search_patterns_empty(mem):
    out = mem.search_patterns(query="anything")
    assert "No cross-task patterns" in out


def test_tilde_expansion(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path))
    mem = CausalMemory("~/cm-test/sub/causal.db")
    assert (tmp_path / "cm-test" / "sub" / "causal.db").exists()
    assert mem is not None
