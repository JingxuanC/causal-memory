"""Causal-memory provider for Hermes.

Maps the Hermes MemoryProvider surface onto the `causal_memory` facade:

- tools      → causal_search / causal_record / causal_trace
- prefetch   → search_memory with a token budget (hybrid recall: the
               seeding layer answers literally; spreading activation adds
               associative lessons the wording doesn't mention)
- system_prompt_block → causal_directory (L0 pointer list)
- sync_turn  → remember() on a daemon thread (NEVER blocks the turn)
- on_memory_write → mirrored into the fact layer (scope="agent")
- on_pre_compress / on_session_end → conservative no-ops (see TODOs:
  LLM-backed distill is the differentiating hook but must not run
  without a configured key)

Storage is profile-isolated: the DB lives under
`<hermes_home>/causal-memory/causal.db` unless config overrides db_path.
"""

from __future__ import annotations

import json
import threading
from pathlib import Path
from typing import Any, Dict, List, Optional

try:  # Hermes installed → subclass the real ABC.
    from agent.memory_provider import MemoryProvider as _MemoryProvider
except Exception:  # Dev/test without Hermes — duck-typed stand-in.
    class _MemoryProvider:  # type: ignore[no-redef]
        """Structural stand-in for agent.memory_provider.MemoryProvider."""


_DEFAULT_PREFETCH_BUDGET = 500

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


class CausalMemoryProvider(_MemoryProvider):
    """Hermes MemoryProvider backed by a local causal-memory store."""

    def __init__(self) -> None:
        self._mem: Any = None
        self._hermes_home: Optional[Path] = None
        self._db_path: Optional[str] = None  # config override; "" / None = auto
        self._prefetch_budget = _DEFAULT_PREFETCH_BUDGET
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
            return mem.record_decision(
                args["decision"],
                args["outcome"],
                args["relation"],
                args["task_tag"],
                confidence_source=args.get("confidence_source"),
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
                {"db_path": self._db_path or "", "prefetch_budget": self._prefetch_budget},
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
        # Conservative no-op: distilling a whole session into lessons needs
        # an LLM key, which is not ours to assume. TODO: when
        # CAUSAL_MEMORY_LLM_* is configured, run a distill pass here.
        return None

    def on_pre_compress(self, messages: list) -> None:
        # Conservative no-op — THE differentiating hook (compaction
        # survival): lessons are already extracted OUTSIDE the context
        # window, so compaction can't lose them. TODO: optionally snapshot
        # about-to-be-compressed messages via remember() when an LLM is
        # configured; silent no-op otherwise.
        return None

    def on_memory_write(self, action: str, target: str, content: str) -> None:
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


def register(ctx: Any) -> CausalMemoryProvider:
    """Hermes entry point (packaged) / directory-layout registration."""
    provider = CausalMemoryProvider()
    register_fn = getattr(ctx, "register_memory_provider", None)
    if callable(register_fn):
        register_fn(provider)
    return provider
