#!/usr/bin/env python3
"""Build a self-contained dashboard of a local causal-memory store.

Opens the store read-only (SQLite ``mode=ro``) and renders one HTML file with
inline CSS/JS and the data embedded: no server, no network calls, opens
offline. Every number on the page is read from the store — nothing is
estimated or invented.

Usage:
    python3 scripts/build_memory_dashboard.py
    python3 scripts/build_memory_dashboard.py --db ~/path/to/causal.db
    python3 scripts/build_memory_dashboard.py --out /tmp/memory-dashboard.html
    python3 scripts/build_memory_dashboard.py --data-only --out /tmp/data.json

The store defaults to ``~/.local/share/causal-memory/causal.db`` (override
with ``--db`` or ``CAUSAL_MEMORY_DB``); the page defaults to
``./memory-dashboard.html``.

The rendered page embeds the actual text of your memories — paths, notes,
whatever you recorded. Treat it like the database itself and do not publish
it as-is; the generator is the thing worth sharing.
"""
from __future__ import annotations

import argparse
import json
import os
import sqlite3
import datetime as dt
from collections import Counter

HERE = os.path.dirname(os.path.abspath(__file__))
TEMPLATE = os.path.join(HERE, "memory_dashboard.tpl.html")
PLACEHOLDER = "/*DATA*/null"

_ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
_ap.add_argument("--db", default=os.environ.get(
    "CAUSAL_MEMORY_DB", "~/.local/share/causal-memory/causal.db"),
    help="path to the causal-memory SQLite store")
_ap.add_argument("--out", default="memory-dashboard.html",
    help="output file (HTML by default, JSON with --data-only)")
_ap.add_argument("--data-only", action="store_true",
    help="write the extracted JSON payload instead of the HTML page")
args = _ap.parse_args()

DB = os.path.expanduser(args.db)
OUT = os.path.abspath(os.path.expanduser(args.out))
if not os.path.isfile(DB):
    raise SystemExit(f"store not found: {DB}\n"
                     "pass --db <path> or set CAUSAL_MEMORY_DB")
TZ = dt.timezone(dt.timedelta(hours=8))


def ts(v):
    if v is None:
        return None
    return dt.datetime.fromtimestamp(int(v), TZ)


def iso(v):
    d = ts(v)
    return d.strftime("%Y-%m-%d %H:%M") if d else None


def day(v):
    d = ts(v)
    return d.strftime("%Y-%m-%d") if d else None


def clip(s, n=110):
    s = " ".join((s or "").split())
    return s if len(s) <= n else s[: n - 1] + "…"


con = sqlite3.connect(f"file:{DB}?mode=ro", uri=True)
con.row_factory = sqlite3.Row
q = lambda sql, *a: con.execute(sql, a).fetchall()

facts = [dict(r) for r in q("select * from agent_facts order by id")]
chunks = [dict(r) for r in q("select * from chunks")]
edges = [dict(r) for r in q("select * from causal_edges order by id")]
recalls = [dict(r) for r in q("select * from recall_audit order by id")]
preds = [dict(r) for r in q("select * from pending_predictions order by id")]
emb = {r["fact_id"]: dict(r) for r in q("select * from agent_facts_embeddings")}
bm25 = q("select count(*) n, count(distinct token) t from bm25_index")[0]
cooc = q("select count(*) n from cooccurrence_edges")[0]["n"]
meta = q("select count(*) n from meta_causal_edges")[0]["n"]
sess = q("select count(*) n from session_logs")[0]["n"]
chunk_emb = q("select count(*) n from chunk_embeddings")[0]["n"]

cmap = {c["id"]: c for c in chunks}
fmap = {f"fact:{f['id']}": f for f in facts}

# ---- per-memory recall hits (which memories came back, how often) ----
hits = Counter()
for r in recalls:
    for item in json.loads(r["results_json"] or "[]"):
        hits[item.get("key", "")] += 1

# ---- resolve an internal id to something readable ----
def label_for(node):
    if not node:
        return {"id": "", "label": "(空)", "kind": "unknown"}
    if node.startswith("fact:"):
        f = fmap.get(node)
        if f:
            return {"id": node, "label": f["key"], "kind": "fact"}
    if node.startswith("causal:"):
        eid = node.split(":", 1)[1]
        e = next((x for x in edges if str(x["id"]) == eid), None)
        if e:
            return {"id": node, "label": clip(cmap.get(e["from_id"], {}).get("text", ""), 70), "kind": "edge"}
    c = cmap.get(node)
    if c:
        kind = "decision" if node.startswith("d") else "outcome"
        return {"id": node, "label": clip(c["text"], 70), "kind": kind}
    return {"id": node, "label": node, "kind": "unknown"}


edge_ref = Counter()
for e in edges:
    edge_ref[e["from_id"]] += 1
    edge_ref[e["to_id"]] += 1
bm25_terms = {r["chunk_id"]: r["n"] for r in q("select chunk_id, count(*) n from bm25_index group by chunk_id")}

edge_rows = []
for e in edges:
    src = cmap.get(e["from_id"], {})
    dst = cmap.get(e["to_id"], {})
    edge_rows.append({
        "id": e["id"],
        "from_id": e["from_id"],
        "to_id": e["to_id"],
        "from_text": " ".join((src.get("text") or "").split()),
        "to_text": " ".join((dst.get("text") or "").split()),
        "relation": e["relation"],
        "confidence": e["confidence"],
        "access": e["access_count"],
        "last_access": iso(e["last_accessed_at"]),
        "event_time": iso(e["event_time"]),
        "task_tag": e["task_tag"] or "",
        "polarity": e["outcome_polarity"] or "",
        "discovered_by": e["discovered_by"],
        "invalid": e["valid_to"] is not None,
        "hit": hits.get(f"causal:{e['id']}", 0),
    })

# ---- daily series over the store's whole life, zero-filled ----
all_days = sorted({day(r["created_at"]) for r in facts}
                  | {day(c["created_at"]) for c in chunks}
                  | {day(e["event_time"]) for e in edges}
                  | {day(r["created_at"]) for r in recalls})
if all_days:
    d0 = dt.date.fromisoformat(all_days[0])
    d1 = dt.date.fromisoformat(all_days[-1])
    dates = []
    cur = d0
    while cur <= d1:
        dates.append(cur.isoformat())
        cur += dt.timedelta(days=1)
else:
    dates = []

def by_day(seq, key):
    c = Counter(day(x[key]) for x in seq)
    return [c.get(d, 0) for d in dates]

facts_new = by_day(facts, "created_at")
chunks_new = by_day(chunks, "created_at")
edges_new = Counter(day(e["event_time"]) for e in edges)
edges_new = [edges_new.get(d, 0) for d in dates]

def cum(xs):
    out, acc = [], 0
    for x in xs:
        acc += x
        out.append(acc)
    return out

# ---- distributions ----
def dist(seq, key, null_label="(未记录)"):
    c = Counter((x[key] if x[key] not in (None, "") else null_label) for x in seq)
    return [{"name": k, "n": v} for k, v in c.most_common()]

rel_dist = dist(edges, "relation")
pol_dist = dist(edges, "outcome_polarity")
disc_dist = dist(edges, "discovered_by")
tag_dist = [t for t in dist(edges, "task_tag") if t["name"] != "(未记录)"]
scope_dist = dist(facts, "scope")
source_dist = dist(facts, "source")
fact_tag_dist = []  # facts carry no task_tag

# confidence histograms (bins of 0.1)
def conf_hist(seq, key="confidence"):
    bins = [0] * 10
    for x in seq:
        v = x[key]
        if v is None:
            continue
        bins[min(9, max(0, int(v * 10)))] += 1
    return bins

# access-count histogram (0..max)
max_acc = max([e["access_count"] for e in edges] or [0])
acc_hist = [0] * (max_acc + 1)
for e in edges:
    acc_hist[e["access_count"]] += 1

# ---- top accessed memories ----
top_edges = sorted(edge_rows, key=lambda e: (-e["access"], -e["confidence"]))[:15]
for t in top_edges:
    t["from_short"] = clip(t["from_text"], 62)
    t["to_short"] = clip(t["to_text"], 62)

# ---- recall events ----
recall_rows = []
for r in recalls:
    results = json.loads(r["results_json"] or "[]")
    recall_rows.append({
        "id": r["id"],
        "time": iso(r["created_at"]),
        "query": r["query"],
        "tag": r["task_tag"] or "",
        "mode": r["mode"],
        "server": r["server"],
        "results": r["result_count"],
        "activated": r["activated_nodes"],
        "latency": round(r["latency_ms"], 1),
        "kinds": dict(Counter(label_for(x.get("key", ""))["kind"] for x in results)),
    })

# ---- health ----
last_write = max([r["created_at"] for r in facts] + [c["created_at"] for c in chunks]
                 + [e["event_time"] for e in edges])
now = dt.datetime.now(TZ)
stale_days = (now.date() - ts(last_write).date()).days
unresolved = sum(1 for p in preds if p["resolved_at"] is None)
invalid_edges = sum(1 for e in edges if e["valid_to"] is not None)

payload = {
    "as_of": now.strftime("%Y-%m-%d %H:%M"),
    "db_path": DB,
    "db_size": os.path.getsize(DB),
    "db_mtime": dt.datetime.fromtimestamp(os.path.getmtime(DB), TZ).strftime("%Y-%m-%d %H:%M"),
    "span": {"first": iso(min([f["created_at"] for f in facts] + [c["created_at"] for c in chunks])),
             "last": iso(last_write)},
    "counts": {
        "facts": len(facts), "chunks": len(chunks), "edges": len(edges),
        "recalls": len(recalls), "access_total": sum(e["access_count"] for e in edges),
        "access_edges": sum(1 for e in edges if e["access_count"] > 0),
        "embedded": len(emb), "preds": len(preds), "unresolved": unresolved,
        "invalid": invalid_edges, "cooc": cooc, "meta": meta, "session": sess,
        "chunk_emb": chunk_emb, "bm25_tokens": bm25["t"], "bm25_rows": bm25["n"],
        "stale_days": stale_days,
        "recall_hits": sum(hits.values()),
    },
    "daily": {
        "dates": dates,
        "facts_new": facts_new, "chunks_new": chunks_new, "edges_new": edges_new,
        "facts_cum": cum(facts_new), "chunks_cum": cum(chunks_new), "edges_cum": cum(edges_new),
        "access_cum": None,
    },
    "dist": {
        "relation": rel_dist, "polarity": pol_dist, "discovered": disc_dist,
        "tags": tag_dist[:12], "scope": scope_dist, "source": source_dist,
        "fact_conf": conf_hist(facts), "edge_conf": conf_hist(edges),
        "access_hist": acc_hist,
    },
    "top_edges": top_edges,
    "recalls": recall_rows,
    "edges": [{k: e[k] for k in ("id", "relation", "confidence", "access", "event_time",
                                 "task_tag", "polarity", "discovered_by", "hit", "invalid", "last_access")}
              | {"from_short": clip(e["from_text"], 90), "to_short": clip(e["to_text"], 90)}
              for e in sorted(edge_rows, key=lambda x: x["id"])],
    "facts": [{
        "id": f["id"], "key": f["key"], "value": " ".join((f["value"] or "").split()),
        "scope": f["scope"], "source": f["source"], "confidence": f["confidence"],
        "created": iso(f["created_at"]), "updated": iso(f["updated_at"]),
        "embedded": f["id"] in emb,
    } for f in facts],
    "chunks": [{
        "id": c["id"],
        "text": " ".join((c["text"] or "").split()),
        "kind": "决策" if c["id"].startswith("d") else "结果",
        "created": iso(c["created_at"]),
        "q": c["q_value"],
        "edges": edge_ref.get(c["id"], 0),
        "terms": bm25_terms.get(c["id"], 0),
        "sparse": c["sparse_code"] is not None,
    } for c in sorted(chunks, key=lambda x: x["created_at"])],
    "health": {
        "embed_cover": round(len(emb) / len(facts) * 100, 1) if facts else 0.0,
        "chunk_emb": chunk_emb,
        "invalid": invalid_edges,
        "unresolved": unresolved,
        "stale_days": stale_days,
    },
}

if args.data_only:
    with open(OUT, "w", encoding="utf-8") as fh:
        json.dump(payload, fh, ensure_ascii=False, indent=1)
else:
    with open(TEMPLATE, encoding="utf-8") as fh:
        template = fh.read()
    if PLACEHOLDER not in template:
        raise SystemExit(f"template has no {PLACEHOLDER} placeholder: {TEMPLATE}")
    blob = json.dumps(payload, ensure_ascii=False, separators=(",", ":")).replace("</", "<\\/")
    with open(OUT, "w", encoding="utf-8") as fh:
        fh.write(template.replace(PLACEHOLDER, blob))

print(json.dumps(payload["counts"], ensure_ascii=False))
print("days:", len(dates), "…", dates[-1] if dates else "-")
print("top access:", [(e["id"], e["access"]) for e in top_edges[:5]])
print("wrote", OUT, os.path.getsize(OUT), "bytes")
