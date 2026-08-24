"""Hermes-free tests for the causal-memory provider plugin.

No Hermes installation required: a fake ctx records registration calls, and
the provider is exercised directly (tmp_path stands in for hermes_home).
Requires the `causal_memory` bindings (maturin develop) and the plugin on
the path (pip install -e . --no-deps, or PYTHONPATH=src).
"""

import time
from pathlib import Path

import pytest

from hermes_causal_memory import CausalMemoryProvider, register
from hermes_causal_memory.cli import _stats, register_cli


class FakeCtx:
    def __init__(self):
        self.providers = []
        self.skills = []

    def register_memory_provider(self, provider):
        self.providers.append(provider)

    def register_skill(self, skill):
        self.skills.append(skill)


@pytest.fixture()
def provider(tmp_path):
    p = CausalMemoryProvider()
    p.initialize("session-1", hermes_home=tmp_path)
    return p


def test_register_with_fake_ctx():
    ctx = FakeCtx()
    provider = register(ctx)
    assert ctx.providers == [provider]
    assert provider.name == "causal-memory"


def test_register_without_registration_hook():
    # A minimal ctx (older Hermes or a bare object) must not explode.
    provider = register(object())
    assert provider.name == "causal-memory"


def test_is_available_offline():
    # Bindings are installed in this venv; the check must not touch network.
    assert CausalMemoryProvider().is_available() is True


def test_initialize_creates_profile_isolated_db(tmp_path):
    CausalMemoryProvider().initialize("s", hermes_home=tmp_path)
    assert (tmp_path / "causal-memory" / "causal.db").exists()


def test_initialize_requires_hermes_home():
    with pytest.raises(ValueError):
        CausalMemoryProvider().initialize("s")


def test_tool_schemas_cover_three_tools(provider):
    names = {s["function"]["name"] for s in provider.get_tool_schemas()}
    assert names == {"causal_search", "causal_record", "causal_trace"}


def test_record_search_trace_roundtrip(provider):
    out = provider.handle_tool_call(
        "causal_record",
        {
            "decision": "used Redis mutex for cache stampede protection",
            "outcome": "deadlock under load when the holder crashed",
            "relation": "caused",
            "task_tag": "concurrency",
            "confidence_source": "rule",
        },
    )
    assert "Recorded" in out

    hits = provider.handle_tool_call(
        "causal_search", {"query": "Redis mutex cache", "detail_level": "l1"}
    )
    assert "Redis mutex" in hits

    trace = provider.handle_tool_call(
        "causal_trace", {"outcome": "deadlock under load when the holder crashed"}
    )
    assert "Redis mutex" in trace

    with pytest.raises(ValueError):
        provider.handle_tool_call("causal_nope", {})


def test_prefetch_respects_configured_budget(provider):
    for i in range(6):
        provider.handle_tool_call(
            "causal_record",
            {
                "decision": f"deployed cache variant {i} without warmup",
                "outcome": f"cold start latency spike {i}",
                "relation": "caused",
                "task_tag": "deploy",
            },
        )
    provider.save_config({"prefetch_budget": 150}, provider._hermes_home)
    out = provider.prefetch("cache warmup deploy", session_id="s")
    assert "truncated (token budget)" in out


def test_config_schema_is_minimal(provider):
    schema = provider.get_config_schema()
    assert set(schema["properties"]) == {"db_path", "prefetch_budget"}
    assert "key" not in str(schema["properties"]).lower()  # no secrets


def test_sync_turn_is_nonblocking_and_lands(provider):
    t0 = time.monotonic()
    provider.sync_turn(
        "I prefer pnpm over npm for monorepos",
        "Noted — pnpm it is for this workspace",
        session_id="s",
    )
    elapsed = time.monotonic() - t0
    assert elapsed < 0.5, f"sync_turn blocked for {elapsed:.2f}s"

    provider.wait_pending()
    hits = provider.handle_tool_call("causal_search", {"query": "pnpm monorepos"})
    assert "pnpm" in hits


def test_on_memory_write_mirrors_to_facts(provider):
    provider.on_memory_write("write", "editor_preference", "neovim")
    facts = provider._mem.search_facts(query="editor_preference", scope="agent")
    assert "neovim" in facts


def test_system_prompt_block_lists_directory(provider):
    provider.handle_tool_call(
        "causal_record",
        {
            "decision": "switched apk sources to Aliyun mirrors",
            "outcome": "alpine build stabilized",
            "relation": "caused",
            "task_tag": "docker",
        },
    )
    block = provider.system_prompt_block()
    assert "Aliyun" in block


def test_compression_and_session_hooks_are_safe_noops(provider):
    # Must not raise, must not write anything unexpected.
    assert provider.on_pre_compress([{"role": "user", "content": "x"}]) is None
    assert provider.on_session_end([{"role": "user", "content": "x"}]) is None


def test_shutdown_drains_and_reinit_works(provider, tmp_path):
    provider.sync_turn("shutdown drain check user turn", "shutdown drain check reply")
    provider.shutdown()
    with pytest.raises(RuntimeError):
        provider.handle_tool_call("causal_search", {"query": "x"})
    # Re-initialize on the same home: data persists.
    provider.initialize("s2", hermes_home=tmp_path)
    hits = provider.handle_tool_call("causal_search", {"query": "shutdown drain check"})
    assert "shutdown drain check" in hits


def test_cli_stats(provider, tmp_path, capsys):
    provider.handle_tool_call(
        "causal_record",
        {
            "decision": "skipped backup before migration",
            "outcome": "data loss during rollback",
            "relation": "caused",
            "task_tag": "db",
        },
    )

    class Args:
        db = str(tmp_path / "causal-memory" / "causal.db")

    assert _stats(Args()) == 0
    out = capsys.readouterr().out
    assert "causal edges (valid): 1" in out
    assert "caused=1" in out


def test_cli_register_cli_wires_subparser():
    class Sub:
        def __init__(self):
            self.parsers = {}

        def add_parser(self, name, help=None):
            self.parsers[name] = self
            return self

        def add_subparsers(self, dest=None):
            return self

        def add_argument(self, *a, **k):
            pass

        def set_defaults(self, **k):
            self.defaults = k

    sub = Sub()
    register_cli(sub)
    assert "causal-memory" in sub.parsers
    assert sub.defaults["func"] is _stats
