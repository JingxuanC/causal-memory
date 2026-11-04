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


def test_per_user_db_isolation(tmp_path):
    """Gateway users off the shared allowlist get isolated tenant DBs."""
    p_owner = CausalMemoryProvider()
    p_owner.initialize(
        "s-owner", hermes_home=tmp_path, user_id="6ee4d376",
        config={"shared_user_ids": ["6ee4d376"]},
    )
    p_tenant = CausalMemoryProvider()
    p_tenant.initialize(
        "s-tenant", hermes_home=tmp_path, user_id="395e8f36",
        config={"shared_user_ids": ["6ee4d376"]},
    )
    p_other = CausalMemoryProvider()
    p_other.initialize(
        "s-other", hermes_home=tmp_path, user_id="b9g9a36b",
        config={"shared_user_ids": ["6ee4d376"]},
    )
    # Owner → shared main DB; tenants → distinct per-user DBs.
    assert p_owner._resolved_db().name == "causal.db"
    assert p_tenant._resolved_db().name != p_owner._resolved_db().name
    assert p_tenant._resolved_db().name != p_other._resolved_db().name
    # Tenant A writes a fact; tenant B must not see it.
    p_tenant._mem.record_fact("private", "395e8f36的私有记忆", scope="user")
    hits = p_other._mem.search_facts("395e8f36的私有记忆", scope="user", limit=5)
    assert "私有记忆" not in hits
    # Owner (shared DB) is unaffected by tenant writes.
    owner_hits = p_owner._mem.search_facts("395e8f36的私有记忆", scope="user", limit=5)
    assert "私有记忆" not in owner_hits


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
    assert set(schema["properties"]) == {
        "db_path",
        "prefetch_budget",
        "shared_user_ids",
        "server_url",
        "agent_id",
        "auto_commit",
    }
    assert "key" not in str(schema["properties"]).lower()  # no secrets


def test_cloud_config_persists_and_reloads(tmp_path):
    p = CausalMemoryProvider()
    p.initialize(
        "s", hermes_home=tmp_path,
        config={
            "server_url": "https://cm.example.com",
            "agent_id": "athena",
            "auto_commit": True,
        },
    )
    p.save_config(
        {
            "server_url": "https://cm.example.com",
            "agent_id": "athena",
            "auto_commit": True,
        },
        tmp_path,
    )
    p2 = CausalMemoryProvider()
    p2.initialize("s2", hermes_home=tmp_path)
    assert p2._server_url == "https://cm.example.com"
    assert p2._agent_id == "athena"
    assert p2._auto_commit is True


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


def test_l0_from_messages_first_user_and_counts():
    from hermes_causal_memory.provider import _l0_from_messages

    msgs = [
        {"role": "system", "content": "you are x"},
        {"role": "user", "content": "  部署灰度到 30% 观察一小时  "},
        {"role": "assistant", "content": "done"},
    ]
    l0 = _l0_from_messages(msgs)
    assert l0.startswith("hermes session (3 turns): 部署灰度到 30% 观察一小时")
    assert len(l0) <= 256 and "\n" not in l0


def test_l0_from_messages_fallback_and_caps():
    from hermes_causal_memory.provider import _l0_from_messages

    assert _l0_from_messages([]) == "hermes session (0 turns)"
    assert _l0_from_messages([{"role": "assistant", "content": "hi"}]) == "hermes session (1 turns)"
    long_user = "x" * 500
    l0 = _l0_from_messages([{"role": "user", "content": long_user}])
    assert len(l0) <= 256 and l0.endswith("…")


def test_session_end_noop_without_cloud_config(provider, monkeypatch):
    # No server_url/agent_id → nothing runs, nothing raises.
    def _boom(*a, **k):
        raise AssertionError("must not invoke the CLI without cloud config")

    monkeypatch.setattr(provider, "_run_cli", _boom)
    assert provider.on_session_end([{"role": "user", "content": "x"}]) is None


def test_session_end_noop_when_cli_missing(tmp_path, monkeypatch):
    p = CausalMemoryProvider()
    p.initialize(
        "s", hermes_home=tmp_path,
        config={"server_url": "https://cm.example.com", "agent_id": "athena"},
    )
    monkeypatch.delenv("CAUSAL_MEMORY_CLI", raising=False)
    monkeypatch.setattr("hermes_causal_memory.provider.shutil.which", lambda *_a, **_k: None)
    assert p.on_session_end([{"role": "user", "content": "x"}]) is None


def test_session_end_spawns_commit_with_expected_args(tmp_path, monkeypatch):
    p = CausalMemoryProvider()
    p.initialize(
        "s", hermes_home=tmp_path,
        config={"server_url": "https://cm.example.com", "agent_id": "athena"},
    )
    # NB: must NOT be tmp_path/"causal-memory" — that name is the provider's
    # store directory (created by initialize).
    fake_cli = tmp_path / "cm-cli"
    fake_cli.write_text("#!/bin/sh\nexit 0\n")
    fake_cli.chmod(0o755)
    monkeypatch.setenv("CAUSAL_MEMORY_CLI", str(fake_cli))

    captured = {}

    class FakeProc:
        returncode = 0
        stdout = "pushed 1 commit(s)\n"
        stderr = ""

    def fake_run(cmd, **kwargs):
        captured["cmd"] = list(cmd)
        return FakeProc()

    monkeypatch.setattr("hermes_causal_memory.provider.subprocess.run", fake_run)

    msgs = [{"role": "user", "content": "让我修复一下回滚策略"}, {"role": "assistant", "content": "ok"}]
    assert p.on_session_end(msgs) is None
    p.wait_pending()

    cmd = captured["cmd"]
    assert cmd[0] == str(fake_cli)
    assert cmd[1] == "session-commit"
    assert cmd[2] == "-m"
    assert cmd[3].startswith("hermes session (2 turns): 让我修复一下回滚策略")
    assert cmd[4] == "--push" and cmd[5] == "athena"
    assert cmd[6] == "--db" and cmd[7].endswith("causal.db")


def test_session_end_failure_logs_but_never_raises(tmp_path, monkeypatch, caplog):
    import logging

    p = CausalMemoryProvider()
    p.initialize(
        "s", hermes_home=tmp_path,
        config={"server_url": "https://cm.example.com", "agent_id": "athena"},
    )
    fake_cli = tmp_path / "cm-cli"
    fake_cli.write_text("#!/bin/sh\nexit 0\n")
    fake_cli.chmod(0o755)
    monkeypatch.setenv("CAUSAL_MEMORY_CLI", str(fake_cli))

    def failing_run(*a, **k):
        raise RuntimeError("connection refused")

    monkeypatch.setattr("hermes_causal_memory.provider.subprocess.run", failing_run)
    with caplog.at_level(logging.WARNING, logger="hermes_causal_memory"):
        assert p.on_session_end([{"role": "user", "content": "x"}]) is None
        p.wait_pending()
    assert "session-commit failed" in caplog.text
