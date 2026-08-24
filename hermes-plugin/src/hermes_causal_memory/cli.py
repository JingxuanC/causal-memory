"""Hermes CLI integration: `hermes causal-memory stats`.

Read-only, stdlib-only (sqlite3 URI mode) — works whether or not the
provider has been initialized in this process.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path
from typing import Any


def register_cli(subparser: Any) -> None:
    parser = subparser.add_parser(
        "causal-memory", help="causal-memory provider commands"
    )
    sub = parser.add_subparsers(dest="causal_memory_command")
    stats = sub.add_parser("stats", help="Show causal-memory store statistics")
    stats.add_argument(
        "--db",
        default=None,
        help="Store path (default: <hermes_home>/causal-memory/causal.db)",
    )
    stats.set_defaults(func=_stats)


def _default_db() -> Path:
    import os

    home = os.environ.get("HERMES_HOME", str(Path.home() / ".hermes"))
    return Path(home) / "causal-memory" / "causal.db"


def _stats(args: Any) -> int:
    db = Path(args.db).expanduser() if args.db else _default_db()
    if not db.exists():
        print(f"no causal-memory store at {db}")
        return 1
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        counts = {}
        for table, label in [
            ("chunks", "chunks"),
            ("causal_edges", "causal edges (valid)"),
            ("agent_facts", "facts (valid)"),
            ("meta_causal_edges", "mined patterns (valid)"),
        ]:
            where = " WHERE valid_to IS NULL" if table != "chunks" else ""
            counts[label] = conn.execute(
                f"SELECT COUNT(*) FROM {table}{where}"
            ).fetchone()[0]
        relations = conn.execute(
            "SELECT relation, COUNT(*) FROM causal_edges "
            "WHERE valid_to IS NULL GROUP BY relation ORDER BY 2 DESC"
        ).fetchall()
        q_distinct = conn.execute(
            "SELECT COUNT(DISTINCT q_value) FROM chunks"
        ).fetchone()[0]
    finally:
        conn.close()
    print(f"causal-memory store: {db}")
    for label, n in counts.items():
        print(f"  {label}: {n}")
    print("  relations: " + ", ".join(f"{r}={n}" for r, n in relations))
    print(f"  distinct q_values: {q_distinct} (>1 means sleep consolidation ran)")
    return 0
