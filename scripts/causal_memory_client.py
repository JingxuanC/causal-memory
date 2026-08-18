#!/usr/bin/env python3
"""
causal_memory — Python client for causal-memory MCP server.

Works with both stdio and HTTP transport modes.

Usage (HTTP):
    from causal_memory import CausalMemoryClient
    cm = CausalMemoryClient.http("http://localhost:9938/mcp")
    cm.record_decision("used Redis mutex", "deadlock", "caused", "caching")
    results = cm.search_causal("cache stampede")

Usage (stdio — spawns the binary):
    cm = CausalMemoryClient.stdio("/path/to/causal-memory")
"""

import json
import subprocess
import threading
import queue
import requests
from typing import Optional, List, Dict, Any


class CausalMemoryClient:
    """Client for the causal-memory MCP server (14 tools)."""

    def __init__(self, transport: str = "http", url: str = "", binary_path: str = "", db_path: str = ""):
        self._transport = transport
        self._url = url
        self._id = 0
        if transport == "stdio":
            self._proc = subprocess.Popen(
                [binary_path] + (["--db", db_path] if db_path else []),
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
            )
            self._initialize()
        elif transport == "http":
            self._initialize()

    @classmethod
    def http(cls, url: str = "http://localhost:9938/mcp") -> "CausalMemoryClient":
        return cls(transport="http", url=url)

    @classmethod
    def stdio(cls, binary_path: str = "causal-memory", db_path: str = "") -> "CausalMemoryClient":
        return cls(transport="stdio", binary_path=binary_path, db_path=db_path)

    def _next_id(self) -> int:
        self._id += 1
        return self._id

    def _call(self, method: str, params: Optional[Dict] = None) -> Dict:
        msg = {
            "jsonrpc": "2.0",
            "id": self._next_id(),
            "method": method,
            "params": params or {},
        }
        if self._transport == "http":
            resp = requests.post(
                self._url,
                json=msg,
                headers={
                    "Content-Type": "application/json",
                    "Accept": "application/json, text/event-stream",
                },
                timeout=30,
            )
            resp.raise_for_status()
            return resp.json()
        else:
            self._proc.stdin.write((json.dumps(msg) + "\n").encode())
            self._proc.stdin.flush()
            line = self._proc.stdout.readline()
            return json.loads(line)

    def _initialize(self):
        self._call("initialize", {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "python-client", "version": "1.0"},
        })

    def _call_tool(self, name: str, arguments: Dict) -> str:
        result = self._call("tools/call", {"name": name, "arguments": arguments})
        content = result.get("result", {}).get("content", [{}])
        if content:
            return content[0].get("text", "")
        return ""

    # ─── 14 MCP tools ─────────────────────────────────────────────

    def record_decision(self, decision: str, outcome: str, relation: str = "caused",
                        task_tag: str = "general", confidence: float = 0.6,
                        confidence_source: str = "llm_inferred") -> str:
        return self._call_tool("record_decision", {
            "decision": decision, "outcome": outcome, "relation": relation,
            "task_tag": task_tag, "confidence": confidence,
            "confidence_source": confidence_source,
        })

    def search_causal(self, query: str, task_tag: Optional[str] = None,
                      topk: int = 10) -> str:
        args = {"query": query, "topk": topk}
        if task_tag:
            args["task_tag"] = task_tag
        return self._call_tool("search_causal", args)

    def record_fact(self, key: str, value: str, scope: str = "user",
                    replace_same_key: bool = False) -> str:
        return self._call_tool("record_fact", {
            "key": key, "value": value, "scope": scope,
            "replace_same_key": replace_same_key,
        })

    def search_facts(self, query: str, scope: Optional[str] = None,
                     topk: int = 10) -> str:
        args = {"query": query, "topk": topk}
        if scope:
            args["scope"] = scope
        return self._call_tool("search_facts", args)

    def search_memory(self, query: str, topk: int = 10) -> str:
        return self._call_tool("search_memory", {"query": query, "topk": topk})

    def trace_cause(self, outcome: str, task_tag: Optional[str] = None) -> str:
        args = {"outcome": outcome}
        if task_tag:
            args["task_tag"] = task_tag
        return self._call_tool("trace_cause", args)

    def trace_cause_chain(self, outcome: str, task_tag: Optional[str] = None,
                          max_hops: int = 5) -> str:
        args = {"outcome": outcome, "max_hops": max_hops}
        if task_tag:
            args["task_tag"] = task_tag
        return self._call_tool("trace_cause_chain", args)

    def invalidate_decision(self, decision_substring: str) -> str:
        return self._call_tool("invalidate_decision", {
            "decision_substring": decision_substring,
        })

    def search_patterns(self, query: str, topk: int = 10) -> str:
        return self._call_tool("search_patterns", {"query": query, "topk": topk})

    def causal_directory(self, limit: int = 20) -> str:
        return self._call_tool("causal_directory", {"limit": limit})

    def intervention_query(self, action: str, task_tag: Optional[str] = None,
                           topk: int = 5) -> str:
        args = {"action": action, "topk": topk}
        if task_tag:
            args["task_tag"] = task_tag
        return self._call_tool("intervention_query", args)

    def counterfactual_query(self, option_a: str, option_b: str,
                             task_tag: Optional[str] = None, topk: int = 5) -> str:
        args = {"option_a": option_a, "option_b": option_b, "topk": topk}
        if task_tag:
            args["task_tag"] = task_tag
        return self._call_tool("counterfactual_query", args)

    def reconstruct_lesson(self, query: str, topk: int = 20, hops: int = 0,
                           task_tag: Optional[str] = None) -> str:
        args = {"query": query, "topk": topk, "hops": hops}
        if task_tag:
            args["task_tag"] = task_tag
        return self._call_tool("reconstruct_lesson", args)

    # ─── Utility ──────────────────────────────────────────────────

    def health(self) -> bool:
        if self._transport == "http":
            base = self._url.rsplit("/", 1)[0]
            try:
                resp = requests.get(f"{base}/health", timeout=5)
                return resp.status_code == 200
            except:
                return False
        return self._proc.poll() is None

    def close(self):
        if self._transport == "stdio" and self._proc:
            self._proc.terminate()


if __name__ == "__main__":
    import sys

    # Quick smoke test against HTTP server
    url = sys.argv[1] if len(sys.argv) > 1 else "http://localhost:9938/mcp"
    cm = CausalMemoryClient.http(url)

    print(f"Health: {cm.health()}")
    print(f"\nSearch 'cargo': {cm.search_causal('cargo', topk=3)[:200]}")
    print(f"\nDirectory: {cm.causal_directory(limit=5)[:200]}")
