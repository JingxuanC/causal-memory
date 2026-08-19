#!/usr/bin/env python3
"""PersonaMem benchmark harness — Tencent-style memory-system evaluation.

Protocol (mirrors TencentDB Agent Memory's evaluation methodology):
- continuous long-horizon sessions, not single-turn QA: each persona's
  context is a full multi-session conversation history (32k/128k tokens)
- dual metrics: accuracy AND token cost (and wall-clock time)

Two conditions per question:
- baseline : the agent reads the (truncated) full context and answers —
              long-context recall. token = context length.
- memory   : a persona-level distillation pass extracts the user profile
              into causal-memory facts (record_fact), then each question is
              answered by retrieving only relevant facts (search_facts).
              token = distill input + retrieval+answer input.

The agent is the Claude Code CLI (headless) with the causal-memory MCP
server attached (~/.claude.json mcpServers.causal-memory). The MCP DB is
isolated to a temp file so the eval never touches a real store.

Data: PersonaMem (bowen-upenn, https://huggingface.co/datasets/bowen-upenn/PersonaMem)
      questions_{32k,128k}.csv + shared_contexts_{32k,128k}.jsonl
      -> fetch with ./fetch_data.sh

Usage:
  python3 harness.py --version 128k --personas 3 --per-persona 5 --mode both
  (writes results/run_<version>_<ts>.json and prints a summary)
"""
import csv, json, subprocess, os, sys, argparse, re, time, ast

CLAUDE = "claude"
OUT_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "results")
DATA_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data")
EVAL_DB = "/tmp/personamem_eval2/causal.db"

def load_contexts(version):
    ctx = {}
    with open(os.path.join(DATA_DIR, f"shared_contexts_{version}.jsonl")) as f:
        for line in f:
            d = json.loads(line)
            for cid, msgs in d.items():
                ctx[cid] = msgs
    return ctx

def fmt_context(msgs, end_idx=None, max_chars=800000, content_trunc=0):
    """Render messages; content_trunc caps each message (GLM context limit)."""
    out, total = [], 0
    for m in (msgs if end_idx is None else msgs[:end_idx]):
        content = m.get("content", "")
        if isinstance(content, list):
            content = " ".join(str(c.get("text", c)) if isinstance(c, dict) else str(c) for c in content)
        if content_trunc and len(content) > content_trunc:
            content = content[:content_trunc]
        line = f"[{m.get('role', '?')}] {content}"
        out.append(line)
        total += len(line)
        if total > max_chars:
            break
    return "\n".join(out)

def rough_tokens(s):
    """Deterministic CJK-aware token yardstick (relative numbers matter)."""
    t = sum(1 for ch in s if ord(ch) > 0x2E80)
    t += len(s) // 4
    return max(t, 1)

def run_claude(prompt, timeout=900):
    """Run one headless claude call; prompt goes via stdin ONLY (argv would
    exceed ARG_MAX for 128k contexts). Isolates the causal-memory MCP DB."""
    env = dict(os.environ)
    env["CAUSAL_MEMORY_DB"] = EVAL_DB
    t0 = time.time()
    p = subprocess.run(
        [CLAUDE, "-p", "--output-format", "text", "--allowedTools", "mcp__causal-memory__*"],
        input=prompt, capture_output=True, text=True, timeout=timeout, env=env)
    return p.stdout, time.time() - t0

def parse_options(raw):
    raw = raw.strip()
    try:
        return json.loads(raw)
    except Exception:
        pass
    try:
        v = ast.literal_eval(raw)
        if isinstance(v, list):
            return v
    except Exception:
        pass
    parts = re.findall(r"\('\(?[a-z]\).*?'\)", raw)
    return [p for p in parts if p] or [raw]

def parse_answer(out, n):
    if not out:
        return None
    low = out.lower()
    m = re.search(r"\(\s*([a-h])\)", low)
    if m: return m.group(1)
    m = re.search(r"^\s*([a-h])\s*[\)\.:]", low, re.M)
    if m: return m.group(1)
    for ch in "abcdefgh"[:n]:
        if re.search(r"\b" + ch + r"\b", low):
            return ch
    return None

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--version", default="32k", choices=["32k", "128k"])
    ap.add_argument("--personas", type=int, default=3)
    ap.add_argument("--per-persona", type=int, default=5)
    ap.add_argument("--mode", choices=["baseline", "memory", "both"], default="both")
    args = ap.parse_args()

    os.makedirs(OUT_DIR, exist_ok=True)
    os.makedirs("/tmp/personamem_eval2", exist_ok=True)
    rows = list(csv.DictReader(open(os.path.join(DATA_DIR, f"questions_{args.version}.csv"))))
    ctx = load_contexts(args.version)
    personas = sorted(set(r["persona_id"] for r in rows))[:args.personas]

    picked = []
    for pid in personas:
        pr = [r for r in rows if r["persona_id"] == pid]
        by_type = {}
        for r in pr:
            by_type.setdefault(r["question_type"], []).append(r)
        take = []
        types = list(by_type.keys())
        while len(take) < args.per_persona and any(by_type.values()):
            for t in types:
                if by_type[t] and len(take) < args.per_persona:
                    take.append(by_type[t].pop(0))
        picked.extend(take)

    print(f"EVAL: {len(picked)} questions, {args.version}, {args.personas} personas", flush=True)

    distilled = {}
    results = []
    for i, r in enumerate(picked):
        pid = r["persona_id"]
        cid = r["shared_context_id"]
        msgs = ctx.get(cid, [])
        end = int(r["end_index_in_shared_context"])
        q = r["user_question_or_message"]
        opts = parse_options(r["all_options"])
        gold = r["correct_answer"].strip("() ").strip()
        opt_text = "\n".join(f"({chr(97+j)}) {o}" for j, o in enumerate(opts))
        qblock = f"USER QUESTION: {q}\n\nOPTIONS:\n{opt_text}\n\nAnswer with ONLY the option letter in parentheses, e.g. (c)."

        if args.mode in ("baseline", "both"):
            transcript = fmt_context(msgs, end, content_trunc=500)  # GLM ctx limit
            prompt = f"Here is a user's conversation history. Read it and answer the question.\n\n{transcript}\n\n{qblock}"
            out, elapsed = run_claude(prompt, timeout=900)
            pred = parse_answer(out, len(opts))
            results.append({"mode": "baseline", "persona": pid, "type": r["question_type"],
                            "question": q, "gold": gold, "pred": pred, "ok": pred == gold,
                            "tokens": rough_tokens(prompt), "seconds": round(elapsed, 1)})
            print(f"[{i+1}/{len(picked)}] baseline p{pid} {r['question_type'][:20]:20} gold={gold} pred={pred} {'OK' if pred==gold else 'x'} (ctx~{int(r['context_length_in_tokens'])}, {elapsed:.0f}s)", flush=True)

        if args.mode in ("memory", "both"):
            if pid not in distilled:
                full = fmt_context(msgs)
                d_prompt = f"""You are building a persistent user profile memory for one user. Read their ENTIRE conversation history.
Extract the user's stable and evolving traits: preferences, facts, history timeline, and reasoning.
For EACH distinct fact, call the causal-memory MCP tool record_fact (key='preference' or 'fact' or 'event').
Extract up to 15 facts. Be thorough — the whole profile must be captured. Then STOP.
Conversation:\n{full[:30000]}"""
                run_claude(d_prompt, timeout=1200)
                distilled[pid] = rough_tokens(d_prompt)
                print(f"    (distilled persona {pid}: {distilled[pid]} tokens)", flush=True)
            a_prompt = f"""You maintain a user profile in causal-memory. First call search_facts with: {q}
Then, using ONLY the retrieved facts, answer. If facts are insufficient say UNKNOWN.
{qblock}"""
            out, elapsed = run_claude(a_prompt)
            pred = parse_answer(out, len(opts))
            results.append({"mode": "memory", "persona": pid, "type": r["question_type"],
                            "question": q, "gold": gold, "pred": pred, "ok": pred == gold,
                            "tokens": distilled.get(pid, 0) + rough_tokens(a_prompt), "seconds": round(elapsed, 1)})
            print(f"[{i+1}/{len(picked)}] memory   p{pid} {r['question_type'][:20]:20} gold={gold} pred={pred} {'OK' if pred==gold else 'x'} ({elapsed:.0f}s)", flush=True)

    # ---- aggregate ----
    summary = {"version": args.version, "personas": args.personas, "questions": len(picked),
               "conditions": {}}
    for mode in ("baseline", "memory"):
        rr = [x for x in results if x["mode"] == mode]
        if not rr: continue
        ok = sum(1 for x in rr if x["ok"])
        tok = sum(x["tokens"] for x in rr)
        sec = sum(x["seconds"] for x in rr)
        summary["conditions"][mode] = {
            "correct": ok, "total": len(rr), "accuracy": round(100.0*ok/len(rr), 1),
            "est_tokens": tok, "est_tokens_per_q": tok // max(len(rr), 1),
            "seconds": round(sec, 0)}
    summary["per_type"] = {}
    types = sorted(set(x["type"] for x in results))
    for t in types:
        entry = {}
        for mode in ("baseline", "memory"):
            rr = [x for x in results if x["mode"] == mode and x["type"] == t]
            if rr:
                ok = sum(1 for x in rr if x["ok"])
                entry[mode] = f"{ok}/{len(rr)}"
        if entry:
            summary["per_type"][t] = entry
    summary["items"] = results

    ts = time.strftime("%Y%m%d_%H%M%S")
    out_path = os.path.join(OUT_DIR, f"run_{args.version}_{ts}.json")
    with open(out_path, "w") as f:
        json.dump(summary, f, ensure_ascii=False, indent=2)
    print(f"\n=== SUMMARY (accuracy + token + time) ===")
    for mode, s in summary["conditions"].items():
        print(f"{mode}: {s['correct']}/{s['total']} = {s['accuracy']}% | est tokens: {s['est_tokens']} ({s['est_tokens_per_q']}/q) | time: {s['seconds']:.0f}s")
    print(f"\nper-type: {json.dumps(summary['per_type'], ensure_ascii=False)}")
    print(f"wrote {out_path}")

if __name__ == "__main__":
    main()
