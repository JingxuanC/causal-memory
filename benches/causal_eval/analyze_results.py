#!/usr/bin/env python3
"""CausalEval result analyzer — per-category accuracy vs baselines.

Usage: python3 analyze_results.py <results.jsonl> [--full20]
"""
import json, sys, collections

rows = [json.loads(l) for l in open(sys.argv[1])]
if "--full20" not in sys.argv:
    n_graphs = max(r["graph"] for r in rows) + 1
    rows = [r for r in rows if r["graph"] < 10] if n_graphs > 10 else rows

names = {11: "C1 attribution", 12: "C2 intervention", 13: "C3 counterfactual",
         14: "C4 inhibition", 15: "C5 temporal", 16: "C6 transfer", 17: "C7 update"}
v12 = {11: 90, 12: 70, 13: 90, 14: 90, 15: 100, 16: 20, 17: 50}
mem0 = {11: 90, 12: 40, 13: 80, 14: 50, 15: 90, 16: 30, 17: 80}

by_cat = collections.defaultdict(lambda: [0, 0])
ev = collections.defaultdict(lambda: [0, 0])
errors = empty_retrieval = 0
for r in rows:
    c = r["category"]
    if r["verdict"] == "error":
        errors += 1
        continue
    if not r["retrieved_ids"]:
        empty_retrieval += 1
    by_cat[c][1] += 1
    ev[c][1] += 1
    if r["verdict"] == "correct":
        by_cat[c][0] += 1
    if r["evidence_hit"]:
        ev[c][0] += 1

tc = sum(v[0] for v in by_cat.values())
tn = sum(v[1] for v in by_cat.values())
print(f"questions: {tn}  errors: {errors}  empty-retrieval: {empty_retrieval}")
print(f"OVERALL: {100*tc/tn:.0f}%  [{tc}/{tn}]")
print(f"{'category':<20} {'acc':>6} {'ev_hit':>7} {'v12':>5} {'mem0':>5} {'Δv12':>6}")
for c in sorted(by_cat):
    k, n = by_cat[c]
    acc = 100 * k / n
    eh = 100 * ev[c][0] / ev[c][1]
    print(f"{names[c]:<20} {acc:>5.0f}% {eh:>6.0f}% {v12[c]:>4}% {mem0[c]:>4}% {acc-v12[c]:>+5.0f}pp")
