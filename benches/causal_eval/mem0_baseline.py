#!/usr/bin/env python3
"""mem0 baseline runner for CausalEval.

Ingests the SAME conversations as the causal-memory harness, uses mem0's own
extraction + retrieval pipeline, then feeds mem0's retrieved memories through
the SAME answer + judge LLM pipeline (same DeepSeek model, same prompts).

Outputs JSONL in the same ResultRow format as the Rust harness so results are
directly comparable.

Usage:
    DEEPSEEK_API_KEY=... python3 mem0_baseline.py --data benches/causal_eval/data
"""

import argparse
import json
import os
import sys
import glob
import time

# ─── LLM client (same DeepSeek, same prompts as Rust harness) ──────────────

import requests

DEEPSEEK_API = os.environ.get("LOCOMO_LLM_API", "https://api.deepseek.com/v1")
DEEPSEEK_KEY = os.environ.get("DEEPSEEK_API_KEY") or os.environ.get("CAUSAL_MEMORY_LLM_KEY")
DEEPSEEK_MODEL = os.environ.get("LOCOMO_LLM_MODEL", "deepseek-chat")

ANSWER_SYSTEM = (
    "You are answering a question using retrieved memories from past work "
    "conversations between two colleagues. Follow these steps IN ORDER.\n\n"
    "## Step 1: SCAN ALL MEMORIES\n"
    "Read EVERY memory. Details are often scattered across the whole list.\n\n"
    "## Step 2: ENTITY VERIFICATION\n"
    "Only use memories about the correct person.\n\n"
    "## Step 3: COMBINE AND REASON\n"
    "Combine facts across memories. For causal questions: identify what caused "
    "what, what prevented what, and what the person now believes.\n\n"
    "## Step 4: COMMIT AND ANSWER\n"
    'Give a direct, specific answer after "ANSWER:". Never say "not specified" '
    "when a memory contains the information. Keep the final answer short.\n"
    '- IRON RULE: your response MUST contain the marker "ANSWER:" followed by '
    "the final answer as the LAST line."
)

JUDGE_SYSTEM = (
    "You are an impartial judge evaluating whether a predicted answer "
    "correctly answers a question about past work conversations. "
    'Respond with ONLY a JSON object (no markdown): {"verdict": "correct" '
    'or "incorrect", "reason": "<one sentence>"}'
)


def llm_chat(system: str, user: str, max_tokens: int = 400, json_mode: bool = False) -> str:
    url = f"{DEEPSEEK_API.rstrip('/')}/chat/completions"
    payload = {
        "model": DEEPSEEK_MODEL,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        "max_tokens": max_tokens,
        "temperature": 0.0,
    }
    if json_mode:
        payload["response_format"] = {"type": "json_object"}

    for attempt in range(3):
        try:
            resp = requests.post(
                url,
                json=payload,
                headers={"Authorization": f"Bearer {DEEPSEEK_KEY}"},
                timeout=60,
            )
            if resp.status_code == 429:
                time.sleep(1 << attempt)
                continue
            resp.raise_for_status()
            data = resp.json()
            content = data["choices"][0]["message"]["content"].strip()
            if content:
                return content
        except Exception as e:
            if attempt == 2:
                raise
            time.sleep(1 << attempt)
    return ""


def extract_json(raw: str):
    import re

    s = raw.strip()
    s = re.sub(r"^```(?:json)?", "", s).strip()
    s = re.sub(r"```$", "", s).strip()
    start = s.find("{")
    if start < 0:
        return None
    for i in range(len(s) - 1, start, -1):
        if s[i] == "}":
            try:
                return json.loads(s[start : i + 1])
            except Exception:
                continue
    return None


# ─── Token matching (same logic as Rust harness) ───────────────────────────

STOP = {
    "with", "without", "before", "after", "during", "under", "into", "from",
    "were", "was", "the", "and", "that", "this", "have", "has", "had", "did",
    "they", "their", "there", "them",
}


def key_tokens(text: str) -> list[str]:
    return [
        w.lower()
        for w in re.split(r"[^a-zA-Z0-9]+", text)
        if len(w) >= 4 and w.lower() not in STOP
    ]


def text_covers(text: str, tokens: list[str]) -> bool:
    if not tokens:
        return False
    lower = text.lower()
    return sum(1 for t in tokens if t in lower) >= max(1, len(tokens) // 2 + 1)


# ─── Main ───────────────────────────────────────────────────────────────────

import re


def run_single_graph(graph_path: str, topk: int):
    """Run mem0 baseline on a single graph (subprocess-isolated for Qdrant)."""
    with open(graph_path) as f:
        bundle = json.load(f)

    graph = bundle["graph"]
    convs = bundle.get("conversations", [])
    qas = bundle.get("qa", [])
    graph_id = graph["id"]

    # ── Configure mem0 ──
    from mem0 import Memory
    from mem0.configs.base import MemoryConfig

    qdrant_path = f"/tmp/mem0_causal_eval_g{graph_id}"
    import shutil
    shutil.rmtree(qdrant_path, ignore_errors=True)
    history_db = f"/tmp/mem0_history_g{graph_id}.db"
    if os.path.exists(history_db):
        os.remove(history_db)

    config = MemoryConfig(
        vector_store={
            "provider": "qdrant",
            "config": {
                "collection_name": f"causal_eval_g{graph_id}",
                "embedding_model_dims": 384,
                "path": qdrant_path,
            },
        },
        llm={
            "provider": "openai",
            "config": {
                "model": DEEPSEEK_MODEL,
                "api_key": DEEPSEEK_KEY,
                "openai_base_url": DEEPSEEK_API,
            },
        },
        embedder={
            "provider": "fastembed",
            "config": {"model": "BAAI/bge-small-en-v1.5"},
        },
        history_db_path=history_db,
    )

    m = Memory(config)

    # ── Ingest conversations ──
    user_id = f"person_g{graph_id}"
    n_added = 0
    for conv in convs:
        for session in conv.get("sessions", []):
            turns = session.get("turns", [])
            messages = []
            for turn in turns:
                messages.append(
                    {"role": "user" if turn["speaker"] == turns[0]["speaker"] else "assistant",
                     "content": turn["text"]}
                )
            if messages:
                result = m.add(messages, user_id=user_id)
                if result.get("results"):
                    n_added += len(result["results"])

    print(f"  mem0 extracted {n_added} memories", file=sys.stderr)

    # ── Precompute evidence tokens ──
    evidence_tokens = {}
    for node in graph["nodes"]:
        evidence_tokens[node["id"]] = key_tokens(node["action"])

    # ── Run questions ──
    for qa in qas:
        cat = qa["category"]
        question = qa["question"]
        gold = qa["answer"]

        hits = m.search(question, filters={"user_id": user_id}, top_k=topk)

        memory_lines = []
        retrieved_texts = []
        for hit in hits.get("results", []):
            mem_text = hit.get("memory", "")
            if mem_text:
                memory_lines.append(f"- {mem_text}")
                retrieved_texts.append(mem_text)

        memories_str = (
            "\n".join(memory_lines) if memory_lines else "(no memories retrieved)"
        )

        gold_toks = [
            evidence_tokens[n]
            for n in qa.get("evidence_nodes", [])
            if n in evidence_tokens
        ]
        evidence_hit = any(
            any(text_covers(rt, gt) for rt in retrieved_texts)
            for gt in gold_toks
        )

        answer_user = f"Memories:\n{memories_str}\n\nQuestion: {question}\nAnswer:"
        try:
            raw = llm_chat(ANSWER_SYSTEM, answer_user, max_tokens=400)
            predicted = raw.rsplit("ANSWER:", 1)[-1].strip() if "ANSWER:" in raw else raw.strip()
        except Exception as e:
            predicted = ""
            print(f"  answer error: {e}", file=sys.stderr)

        verdict = "error"
        judge_user = (
            f"Question: {question}\nGold answer: {gold}\n"
            f'Predicted answer: {predicted}\n\nThe prediction is "correct" '
            'if it conveys the same information as the gold answer '
            '(wording may differ); otherwise "incorrect".'
        )
        for attempt in range(3):
            try:
                raw_j = llm_chat(JUDGE_SYSTEM, judge_user, max_tokens=512, json_mode=True)
                v = extract_json(raw_j)
                if v and v.get("verdict"):
                    vd = v["verdict"].lower()
                    if vd in ("correct", "incorrect"):
                        verdict = vd
                        break
            except Exception:
                pass
            time.sleep(1 << attempt)

        row = {
            "graph": graph_id,
            "category": cat,
            "question": question,
            "gold": gold,
            "predicted": predicted,
            "verdict": verdict,
            "evidence_hit": evidence_hit,
            "memory_count": len(memory_lines),
        }
        print(json.dumps(row))

    # Clean up Qdrant lock
    try:
        m.vector_store.client.close()
    except Exception:
        pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--data", default="benches/causal_eval/data")
    parser.add_argument("--topk", type=int, default=20)
    parser.add_argument("--graph", type=str, default=None, help="single graph path (subprocess mode)")
    args = parser.parse_args()

    if not DEEPSEEK_KEY:
        print("ERROR: DEEPSEEK_API_KEY not set", file=sys.stderr)
        sys.exit(1)

    # ── Subprocess mode: run a single graph ──
    if args.graph:
        run_single_graph(args.graph, args.topk)
        return

    # ── Orchestrator mode: spawn subprocess per graph ──
    import subprocess

    graph_files = sorted(glob.glob(os.path.join(args.data, "graph_*.json")))
    print(f"Loaded {len(graph_files)} graphs", file=sys.stderr)

    all_results = []
    for gi, graph_path in enumerate(graph_files):
        graph_id = gi
        print(f"\n=== graph {graph_id} ===", file=sys.stderr)
        result = subprocess.run(
            [sys.executable, __file__, "--graph", graph_path, "--topk", str(args.topk),
             "--data", args.data],
            capture_output=True,
            text=True,
            env={**os.environ},
        )
        # Parse stdout for JSON lines
        for line in result.stdout.strip().split("\n"):
            line = line.strip()
            if line.startswith("{") and '"verdict"' in line:
                try:
                    all_results.append(json.loads(line))
                except Exception:
                    pass
        # Print stderr
        for line in result.stderr.strip().split("\n"):
            print(f"  {line}", file=sys.stderr)

    # Output all results
    for row in all_results:
        print(json.dumps(row))

    print(f"\nDone. {len(all_results)} results.", file=sys.stderr)


if __name__ == "__main__":
    main()
