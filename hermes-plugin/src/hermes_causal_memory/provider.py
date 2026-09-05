"""Causal-memory provider for Hermes.

Maps the Hermes MemoryProvider surface onto the `causal_memory` facade:

- tools      → causal_search / causal_record / causal_trace
- prefetch   → search_memory with a token budget (hybrid recall: the
               seeding layer answers literally; spreading activation adds
               associative lessons the wording doesn't mention)
- system_prompt_block → causal_directory (L0 pointer list)
- sync_turn  → remember() on a daemon thread (NEVER blocks the turn)
- on_memory_write → mirrored into the fact layer (scope="agent")
- on_pre_compress → conservative no-op (LLM-backed distill must not run
  without a configured key)
- on_session_end → cloud auto-commit (P2): when the provider is configured
  with a sync server (server_url + agent_id via `cloud register`) and the
  causal-memory CLI is installed, snapshots the session's recorded lessons
  and pushes them to the agent's cloud remote on a background thread. No
  cloud config / no CLI → silent no-op (never blocks session teardown).

Storage is profile-isolated: the DB lives under
`<hermes_home>/causal-memory/causal.db` unless config overrides db_path.

Per-user isolation (gateway multi-tenant): when `initialize` receives a
`user_id` that is NOT in the shared-users allowlist (config
`shared_user_ids`), the provider opens a tenant DB at
`<hermes_home>/causal-memory/causal_<sha1(user_id)[:16]>.db` instead of the
shared one. Users on the allowlist (default: the Hermes owner) and all
non-gateway contexts (CLI/cron, no user_id) share the main DB. This keeps
each gateway user's facts/lessons physically separate — no cross-user
recall leakage.
"""

from __future__ import annotations

import hashlib
import inspect
import json
import logging
import os
import shutil
import subprocess
import threading
from functools import lru_cache
from pathlib import Path
from typing import Any, Dict, List, Optional

logger = logging.getLogger(__name__)

try:  # Hermes installed → subclass the real ABC.
    from agent.memory_provider import MemoryProvider as _MemoryProvider
except Exception:  # Dev/test without Hermes — duck-typed stand-in.
    class _MemoryProvider:  # type: ignore[no-redef]
        """Structural stand-in for agent.memory_provider.MemoryProvider."""


_DEFAULT_PREFETCH_BUDGET = 500
# Session-end cloud commit: 90s covers a real push; a stuck CLI must not
# outlive the drain join in shutdown() by much.
_SESSION_COMMIT_TIMEOUT_S = 90

# Config schema is deliberately minimal (the guide's explicit advice):
# no secrets, two keys, everything else derived from hermes_home.
_CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "db_path": {
            "type": "string",
            "default": "",
            "description": "SQLite store path (empty = <hermes_home>/causal-memory/causal.db)",
        },
        "prefetch_budget": {
            "type": "integer",
            "default": _DEFAULT_PREFETCH_BUDGET,
            "description": "Max tokens returned per prefetch recall (0 = unlimited)",
        },
        "shared_user_ids": {
            "type": "array",
            "items": {"type": "string"},
            "default": [],
            "description": "Gateway user_ids that share the main causal.db (owner/allowlist). "
            "Users NOT listed get an isolated tenant DB per user_id.",
        },
        "server_url": {
            "type": "string",
            "default": "",
            "description": "Sync server base URL (e.g. https://cm.example.com). "
            "Informational once the remote is provisioned — the CLI resolves "
            "agent_id from the store's own remote config.",
        },
        "agent_id": {
            "type": "string",
            "default": "",
            "description": "Remote name to push session snapshots to — provision once "
            "with `causal-memory cloud register <agent_id> <server_url> --db <this db>` "
            "(cloud) or `remote add <agent_id> <path> --db <this db>` (file). "
            "Empty disables session-end auto-commit.",
        },
        "auto_commit": {
            "type": "boolean",
            "default": True,
            "description": "Run session-commit --push <agent_id> on session end "
            "when cloud is configured and the causal-memory CLI is installed.",
        },
    },
}

_TOOL_SCHEMAS: List[Dict[str, Any]] = [
    {
        "type": "function",
        "function": {
            "name": "causal_search",
            "description": (
                "Search ALL causal-memory layers at once: flat facts (preferences, "
                "tech stack, config) AND decision→outcome lessons, fused by RRF. "
                "Call BEFORE non-trivial decisions to recall relevant experience."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Natural language query"},
                    "task_tag": {
                        "type": "string",
                        "description": "Task category filter for causal episodes",
                    },
                    "detail_level": {
                        "type": "string",
                        "enum": ["l0", "l1", "l2"],
                        "description": "l0 pointer / l1 overview / l2 full (default)",
                    },
                    "max_tokens": {
                        "type": "integer",
                        "description": "Max output tokens (0 = unlimited, default 0)",
                    },
                },
                "required": ["query"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "causal_record",
            "description": (
                "Record a decision and its observed outcome as a causal lesson. "
                "Call AFTER acting on a decision and observing the result, "
                "especially when the outcome was surprising or educational."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "decision": {"type": "string"},
                    "outcome": {"type": "string"},
                    "relation": {
                        "type": "string",
                        "enum": ["caused", "enabled", "prevented", "no_effect"],
                    },
                    "task_tag": {"type": "string", "description": "Task category"},
                    "context": {
                        "type": "string",
                        "description": (
                            "Short description of the situation the decision was "
                            "made in (environment, constraints, key parameters). "
                            "Same task_tag + context => comparable branch for "
                            "counterfactuals. Always set it when multiple options "
                            "were weighed."
                        ),
                    },
                    "confidence_source": {
                        "type": "string",
                        "enum": ["temporal", "rule", "llm_inferred", "user_feedback"],
                        "description": "Evidence source (default llm_inferred)",
                    },
                },
                "required": ["decision", "outcome", "relation", "task_tag"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "causal_trace",
            "description": (
                "When something went wrong, trace back which past decision could "
                "have caused it. Use for post-mortem analysis."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "outcome": {
                        "type": "string",
                        "description": "Description of the bad outcome",
                    },
                },
                "required": ["outcome"],
            },
        },
    },
]


@lru_cache(maxsize=4)
def _record_supports_context(mem: Any) -> bool:
    """True when the installed causal-memory binding accepts `context=`.

    The kwarg exists on the workspace/facade builds and on wheels released
    after py-v0.9.2; the unpinned PyPI dependency may resolve older. Cached
    per binding object (the pyo3 module or an instance).
    """
    try:
        params = inspect.signature(mem.record_decision).parameters
        return "context" in params
    except (TypeError, ValueError):  # exotic callables — fail closed
        return False


def _l0_from_messages(messages: list) -> str:
    """≤256-char single-line L0 for the session-commit message.

    Best-effort from an OpenAI-style message list: first user text (first
    ~140 chars) + turn count. Hosts with richer summaries can pass better
    text via their own plumbing — this only has to be honest and stable.
    """
    first_user = ""
    for m in messages or []:
        content = m.get("content") if isinstance(m, dict) else None
        if m.get("role") == "user" and isinstance(content, str) and content.strip():
            first_user = content.strip()
            break
    n = len(messages or [])
    if first_user:
        head = first_user.replace("\n", " ")[:140]
        ellipsis = "…" if len(first_user) > 140 else ""
        msg = f"hermes session ({n} turns): {head}{ellipsis}"
    else:
        msg = f"hermes session ({n} turns)"
    return msg[:256]


class CausalMemoryProvider(_MemoryProvider):
    """Hermes MemoryProvider backed by a local causal-memory store."""

    def __init__(self) -> None:
        self._mem: Any = None
        self._hermes_home: Optional[Path] = None
        self._db_path: Optional[str] = None  # config override; "" / None = auto
        self._prefetch_budget = _DEFAULT_PREFETCH_BUDGET
        self._shared_user_ids: List[str] = []
        self._server_url: str = ""
        self._agent_id: str = ""
        self._auto_commit: bool = True
        self._user_id: str = ""
        self._threads: List[threading.Thread] = []
        self._lock = threading.Lock()

    # ── required surface ────────────────────────────────────────────────

    @property
    def name(self) -> str:
        return "causal-memory"

    def is_available(self) -> bool:
        """No network: availability == the bindings import."""
        try:
            import causal_memory  # noqa: F401
        except Exception:
            return False
        return True

    def initialize(self, session_id: str, **kwargs: Any) -> None:
        hermes_home = kwargs.get("hermes_home")
        if hermes_home is None:
            raise ValueError("initialize requires hermes_home in kwargs")
        self._hermes_home = Path(hermes_home)
        # config passed via kwargs wins over values persisted by save_config;
        # when Hermes doesn't thread a config, reload our own config.json so
        # db_path overrides survive restarts (save_config is otherwise
        # write-only).
        cfg = kwargs.get("config") or self._read_persisted_config()
        self._apply_config(cfg)
        # Per-user isolation: gateway sessions carry a user_id; non-gateway
        # contexts (CLI/cron/subagent) pass none → shared main DB.
        self._user_id = str(kwargs.get("user_id") or "")
        self._open()

    def get_tool_schemas(self) -> List[Dict[str, Any]]:
        return _TOOL_SCHEMAS

    def handle_tool_call(self, tool_name: str, args: Dict[str, Any], **kwargs: Any) -> str:
        mem = self._require_mem()
        if tool_name == "causal_search":
            return mem.search_memory(
                args["query"],
                task_tag=args.get("task_tag"),
                detail_level=args.get("detail_level"),
                max_tokens=args.get("max_tokens"),
            )
        if tool_name == "causal_record":
            # `context` landed on the pyo3 facade after the last published
            # wheel (py-v0.9.2). The dependency is unpinned, so feature-
            # detect instead of passing the kwarg unconditionally — an
            # older wheel would raise TypeError on EVERY causal_record
            # call (review finding on PR #19).
            kwargs: Dict[str, Any] = {
                "confidence_source": args.get("confidence_source"),
            }
            if _record_supports_context(mem):
                kwargs["context"] = args.get("context")
            return mem.record_decision(
                args["decision"],
                args["outcome"],
                args["relation"],
                args["task_tag"],
                **kwargs,
            )
        if tool_name == "causal_trace":
            return mem.trace_cause(args["outcome"])
        raise ValueError(f"unknown tool: {tool_name}")

    def get_config_schema(self) -> Dict[str, Any]:
        return _CONFIG_SCHEMA

    def save_config(self, values: Dict[str, Any], hermes_home: Any) -> None:
        self._hermes_home = Path(hermes_home)
        self._apply_config(values)
        # Persist so the next session starts with the same config.
        cfg_dir = self._hermes_home / "causal-memory"
        cfg_dir.mkdir(parents=True, exist_ok=True)
        (cfg_dir / "config.json").write_text(
            json.dumps(
                {
                    "db_path": self._db_path or "",
                    "prefetch_budget": self._prefetch_budget,
                    "shared_user_ids": self._shared_user_ids,
                    "server_url": self._server_url,
                    "agent_id": self._agent_id,
                    "auto_commit": self._auto_commit,
                },
                indent=2,
            )
        )
        if self._mem is not None:
            self._open()  # reopen in case db_path changed

    # ── optional hooks ──────────────────────────────────────────────────

    def system_prompt_block(self) -> str:
        """L0 pointer directory — compact enough to pin in the prompt."""
        if self._mem is None:
            return ""
        return self._mem.causal_directory(limit=20)

    def prefetch(self, query: str, *, session_id: str = "") -> str:
        """Hybrid recall: literal seeding + associative spreading, budgeted."""
        if self._mem is None:
            return ""
        return self._mem.search_memory(query, max_tokens=self._prefetch_budget)

    def sync_turn(
        self,
        user: str,
        assistant: str,
        *,
        session_id: str = "",
        messages: Optional[list] = None,
    ) -> None:
        """MUST NOT block the turn: remember() runs on a daemon thread."""
        if self._mem is None:
            return
        text = f"user: {user}\nassistant: {assistant}"
        t = threading.Thread(target=self._remember_safe, args=(text,), daemon=True)
        with self._lock:
            self._threads.append(t)
        t.start()

    def on_session_end(self, messages: list) -> None:
        """Cloud snapshot at the real session boundary (P2 auto-commit).

        Runs `causal-memory session-commit -m <L0> --push <agent_id>
        --db <resolved db>` on a daemon thread — the same CLI path a human
        would run, reusing commit/L0/push machinery and its idempotency
        (nothing recorded this session → "nothing to commit", no-op).

        Silent no-op when any prerequisite is missing: no agent_id (provision
        the remote once — `cloud register <agent_id> <server_url>` for cloud,
        or `remote add <agent_id> <path>` for a file remote), auto_commit
        disabled, or the causal-memory CLI binary absent (override for dev:
        CAUSAL_MEMORY_CLI env var). A session hook must never raise or block
        teardown.
        """
        if not (self._auto_commit and self._agent_id):
            return None
        cli = self._cli_binary()
        if cli is None:
            logger.info("causal-memory CLI not found — skipping cloud session-commit")
            return None
        db = self._resolved_db()
        msg = _l0_from_messages(messages)
        cmd = [
            cli,
            "session-commit",
            "-m",
            msg,
            "--push",
            self._agent_id,
            "--db",
            str(db),
        ]
        t = threading.Thread(target=self._commit_safe, args=(cmd,), daemon=True)
        with self._lock:
            self._threads.append(t)
        t.start()
        return None

    def on_pre_compress(self, messages: list) -> None:
        # Conservative no-op — THE differentiating hook (compaction
        # survival): lessons are already extracted OUTSIDE the context
        # window, so compaction can't lose them. TODO: optionally snapshot
        # about-to-be-compressed messages via remember() when an LLM is
        # configured; silent no-op otherwise.
        return None

    def on_memory_write(
        self,
        action: str,
        target: str,
        content: str,
        metadata: Optional[dict] = None,
    ) -> None:
        """Mirror Hermes-native memory writes into the fact layer."""
        if self._mem is None:
            return
        self._mem.record_fact(
            str(target),
            str(content),
            scope="agent",
            replace_same_key=True,
        )

    def shutdown(self) -> None:
        """Drain pending remember threads, then release the store."""
        with self._lock:
            threads, self._threads = self._threads, []
        for t in threads:
            t.join(timeout=10)
        self._mem = None

    # ── test/support helpers (not part of the Hermes surface) ───────────

    def wait_pending(self, timeout: float = 10.0) -> None:
        """Join outstanding sync_turn threads (tests; shutdown without reset)."""
        with self._lock:
            threads = list(self._threads)
        for t in threads:
            t.join(timeout=timeout)

    # ── internals ───────────────────────────────────────────────────────

    def _apply_config(self, values: Dict[str, Any]) -> None:
        if not values:
            return
        db_path = values.get("db_path")
        if db_path is not None:
            self._db_path = db_path or None
        budget = values.get("prefetch_budget")
        if budget is not None:
            self._prefetch_budget = int(budget)
        shared = values.get("shared_user_ids")
        if shared is not None:
            self._shared_user_ids = [str(u) for u in shared]
        server_url = values.get("server_url")
        if server_url is not None:
            self._server_url = str(server_url or "")
        agent_id = values.get("agent_id")
        if agent_id is not None:
            self._agent_id = str(agent_id or "")
        auto_commit = values.get("auto_commit")
        if auto_commit is not None:
            self._auto_commit = bool(auto_commit)

    def _read_persisted_config(self) -> Dict[str, Any]:
        """Load config.json written by save_config (missing/corrupt → {})."""
        assert self._hermes_home is not None
        try:
            raw = (self._hermes_home / "causal-memory" / "config.json").read_text()
            cfg = json.loads(raw)
            return cfg if isinstance(cfg, dict) else {}
        except Exception:
            return {}

    def _resolved_db(self) -> Path:
        # Per-user isolation: a gateway user_id that is NOT on the shared
        # allowlist gets its own tenant DB. Shared users (owner) and
        # non-gateway contexts (empty user_id) use the main DB.
        if self._user_id and self._user_id not in self._shared_user_ids:
            assert self._hermes_home is not None
            digest = hashlib.sha1(self._user_id.encode("utf-8")).hexdigest()[:16]
            return self._hermes_home / "causal-memory" / f"causal_{digest}.db"
        if self._db_path:
            return Path(self._db_path).expanduser()
        assert self._hermes_home is not None
        return self._hermes_home / "causal-memory" / "causal.db"

    def _open(self) -> None:
        from causal_memory import CausalMemory

        db = self._resolved_db()
        db.parent.mkdir(parents=True, exist_ok=True)
        self._mem = CausalMemory(str(db))

    def _require_mem(self) -> Any:
        if self._mem is None:
            raise RuntimeError("provider not initialized (call initialize first)")
        return self._mem

    def _remember_safe(self, text: str) -> None:
        try:
            self._mem.remember(text)
        except Exception:
            pass  # background hook — never raise into a turn

    def _cli_binary(self) -> Optional[str]:
        """causal-memory CLI binary: CAUSAL_MEMORY_CLI override, else PATH."""
        exe = os.environ.get("CAUSAL_MEMORY_CLI")
        if exe and Path(exe).is_file():
            return exe
        return shutil.which("causal-memory")

    def _run_cli(self, cmd: List[str]) -> None:
        """Run the session-commit subprocess; log failures, never raise."""
        proc = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=_SESSION_COMMIT_TIMEOUT_S,
        )
        if proc.returncode != 0:
            tail = (proc.stderr or proc.stdout or "").strip().splitlines()[-3:]
            logger.warning(
                "causal-memory session-commit exited %s: %s",
                proc.returncode,
                " | ".join(tail) or "(no output)",
            )

    def _commit_safe(self, cmd: List[str]) -> None:
        try:
            self._run_cli(cmd)
        except Exception as e:  # noqa: BLE001 — background hook, never raise
            logger.warning("causal-memory session-commit failed: %s", e)


def register(ctx: Any) -> CausalMemoryProvider:
    """Hermes entry point (packaged) / directory-layout registration."""
    provider = CausalMemoryProvider()
    register_fn = getattr(ctx, "register_memory_provider", None)
    if callable(register_fn):
        register_fn(provider)
    return provider
