#!/usr/bin/env python3
"""Audit fact<->chunk entity links against the real store (stdlib only).

Replicates the Rust linker policy in
crates/causal-memory/src/hippocampus/mod.rs (entity_link_facts +
link_fact_node + component_stats) so link counts, df behavior, scope
isolation and graph connectivity can be measured without a Rust build:

  - tokenize() mirrors patterns::tokenizer::tokenize (ASCII words + CJK
    bigrams, STOP_WORDS dropped)
  - LINK_STOPWORDS / FACT_LINK_MIN_TOKENS / FACT_LINK_DF_LIMIT match the
    Rust constants
  - scope_matches() replicates colon-namespace isolation (a fact scoped
    "lme:q1" links only chunks whose task_tag == "q1")
  - node set / edges replicate CausalGraph::from_store: chunks, fact
    nodes "fact:{id}" with text "{key}: {value}", scope hubs
    "scope:{scope}", causal_edges (valid_to IS NULL), scope->fact edges,
    and the bidirectional fact<->chunk links
  - the BFS replicates CausalGraph::component_stats (undirected
    connectivity, isolated singletons counted)

Measured on the real DB (2026-08-26, after the precision fix):
  links 9,764 -> 3,117 (-68%), avg 7.0 -> 2.2 per fact; total valid
  edges 21,151 -> 7,857; components 17 -> 29, largest 1801 -> 1777.
  Sampled link precision 17% -> 33% (strict) / 29% -> 75% (lenient) --
  the strict/lenient judgment is a manual sample; this script
  reproduces the *counts* and can dump links for re-sampling.

Usage:
  python3 scripts/audit_fact_links.py [--db PATH] [--min-tokens N]
      [--df-limit N] [--no-compare] [--sample N]
"""

import argparse
import collections
import os
import random
import sqlite3
import sys

# --- Rust patterns::tokenizer::STOP_WORDS (lowercased words dropped) ------
STOP_WORDS = {
    "a", "an", "the", "to", "and", "or", "of", "in", "on", "for", "with",
    "is", "are", "was", "were", "be", "by", "at", "as", "it", "this",
    "that", "we", "i",
}

# --- Rust hippocampus LINK_STOPWORDS (too generic to drive a link) --------
LINK_STOPWORDS = {
    "user", "project", "code", "build", "using", "used", "want", "like",
    "get", "got", "make", "made", "need", "way", "work", "worked",
    "thing", "stuff", "issue", "problem", "fix", "fixed", "use", "went",
}

DEFAULT_MIN_TOKENS = 3
DEFAULT_DF_LIMIT = 20
MAX_PER_FACT = 8


def tokenize(text):
    """Mirror patterns::tokenizer::tokenize exactly."""
    tokens = []
    ascii_buf = []
    cjk_buf = []

    def flush_ascii():
        w = "".join(ascii_buf)
        ascii_buf.clear()
        if w and w not in STOP_WORDS:
            tokens.append(w)

    def flush_cjk():
        if len(cjk_buf) == 1:
            tokens.append(cjk_buf[0])
        elif len(cjk_buf) > 1:
            tokens.extend("".join(cjk_buf[i : i + 2]) for i in range(len(cjk_buf) - 1))
        cjk_buf.clear()

    for c in text:
        if c.isascii() and c.isalnum():
            flush_cjk()
            ascii_buf.append(c.lower())
        elif c.isalnum():
            flush_ascii()
            cjk_buf.append(c)
        else:
            flush_ascii()
            flush_cjk()
    flush_ascii()
    flush_cjk()
    return tokens


def scope_matches(fact_scope, chunk_tag):
    """Mirror the Rust scope_matches: colon-namespaced scope requires the
    chunk's task_tag to equal the scope suffix; canonical scopes link all."""
    if fact_scope is not None and ":" in fact_scope:
        suffix = fact_scope.rsplit(":", 1)[1]
        return chunk_tag == suffix
    return True


def load_db(db_path):
    """Load chunks, causal edges and facts exactly like from_store."""
    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    cur = conn.cursor()

    # chunks: (id, text); task_tag filled later from causal_edges
    chunks = {}
    for cid, ctext in cur.execute(
        "SELECT id, text FROM chunks ORDER BY created_at ASC"
    ):
        chunks[cid] = {"text": ctext, "task_tag": None, "idx": len(chunks)}

    # causal edges (valid): from/to ids + task_tag (first tag per node wins)
    edges = []  # (from_id, to_id, relation, weight)
    for fid, tid, rel, conf, task_tag in cur.execute(
        "SELECT from_id, to_id, relation, confidence, task_tag"
        " FROM causal_edges WHERE valid_to IS NULL ORDER BY event_time ASC"
    ):
        edges.append((fid, tid, rel, conf))
        for node_id in (fid, tid):
            if node_id in chunks and chunks[node_id]["task_tag"] is None and task_tag:
                chunks[node_id]["task_tag"] = task_tag

    # facts (valid): (fact_id, key, value, scope, confidence)
    facts = []
    for fid, key, value, scope, conf in cur.execute(
        "SELECT id, key, value, scope, confidence FROM agent_facts"
        " WHERE valid_to IS NULL"
    ):
        facts.append((fid, key, value, scope, conf))

    # meta + cooccurrence edges (tables may not exist / be empty)
    meta = []
    if cur.execute(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table'"
        " AND name='meta_causal_edges'"
    ).fetchone()[0]:
        meta = list(
            cur.execute(
                "SELECT from_id, to_id, confidence FROM meta_causal_edges"
                " WHERE valid_to IS NULL"
            )
        )
    cooc = []
    if cur.execute(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table'"
        " AND name='cooccurrence_edges'"
    ).fetchone()[0]:
        cooc = list(cur.execute("SELECT from_id, to_id, weight FROM cooccurrence_edges"))

    conn.close()
    return chunks, edges, facts, meta, cooc


def link_facts(chunks, facts, min_tokens, df_limit):
    """Mirror entity_link_facts. Returns (links, per_fact, df_histogram)
    where links is a list of (fact_text, fact_scope, chunk_id, chunk_text,
    overlap)."""
    # inverted token -> chunk ids (chunk nodes only, distinct tokens)
    token_to_chunks = collections.defaultdict(set)
    for cid, c in chunks.items():
        for tok in set(tokenize(c["text"])):
            token_to_chunks[tok].add(cid)

    links = []
    per_fact = []
    for fid, key, value, scope, conf in facts:
        fact_text = f"{key}: {value}"
        fact_tokens = {
            t
            for t in set(tokenize(fact_text))
            if t not in LINK_STOPWORDS
            and (df_limit is None or len(token_to_chunks.get(t, ())) <= df_limit)
        }
        overlap = collections.Counter()
        for tok in fact_tokens:
            for cid in token_to_chunks.get(tok, ()):
                if scope_matches(scope, chunks[cid]["task_tag"]):
                    overlap[cid] += 1
        linked = [
            (cid, n)
            for cid, n in overlap.items()
            if n >= min_tokens
        ]
        # Rust sorts by node INDEX (chunk load order), not by id string.
        linked.sort(key=lambda kv: (-kv[1], chunks[kv[0]]["idx"]))
        linked = linked[:MAX_PER_FACT]
        per_fact.append(len(linked))
        for cid, n in linked:
            links.append((fact_text, scope, cid, chunks[cid]["text"], n))
    return links, per_fact


def component_stats(nodes, edges):
    """Mirror CausalGraph::component_stats over undirected connectivity."""
    adj = {n: set() for n in nodes}
    for a, b in edges:
        if a in adj and b in adj:
            adj[a].add(b)
            adj[b].add(a)
    visited = set()
    comps = []
    for start in nodes:
        if start in visited:
            continue
        stack = [start]
        visited.add(start)
        size = 0
        while stack:
            n = stack.pop()
            size += 1
            for m in adj[n]:
                if m not in visited:
                    visited.add(m)
                    stack.append(m)
        comps.append(size)
    max_c = max(comps) if comps else 0
    isolated = sum(1 for c in comps if c == 1)
    return len(comps), max_c, isolated


def run_config(chunks, edges, facts, meta, cooc, min_tokens, df_limit):
    links, per_fact = link_facts(chunks, facts, min_tokens, df_limit)
    fid_by_text = {f"{key}: {value}": f"fact:{fid}" for fid, key, value, scope, conf in facts}
    scope_nodes = sorted({f[3] for f in facts})
    nodes = list(chunks.keys()) + [f"scope:{s}" for s in scope_nodes] + [f"fact:{f[0]}" for f in facts]
    und = [(a, b) for a, b, _r, _w in edges]
    for s in scope_nodes:
        und.extend(
            (f"scope:{s}", f"fact:{fid}")
            for fid, key, value, scope, conf in facts
            if scope == s
        )
    und.extend((a, b) for a, b, _w in meta)
    und.extend((a, b) for a, b, _w in cooc)
    for ftxt, fscope, cid, ctxt, n in links:
        und.append((cid, fid_by_text[ftxt]))
    comps, max_c, isolated = component_stats(nodes, und)
    total_valid = len(edges) + len(facts) + len(meta) + len(cooc) + 2 * len(links)
    return links, per_fact, comps, max_c, isolated, total_valid


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--db", default=os.path.expanduser(
        "~/.local/share/causal-memory/causal.db"))
    ap.add_argument("--min-tokens", type=int, default=DEFAULT_MIN_TOKENS)
    ap.add_argument("--df-limit", type=int, default=DEFAULT_DF_LIMIT,
                    help="0/negative disables the df filter")
    ap.add_argument("--no-compare", action="store_true")
    ap.add_argument("--sample", type=int, default=0)
    args = ap.parse_args()

    if not os.path.exists(args.db):
        sys.exit(f"no DB at {args.db}")

    df_limit = args.df_limit if args.df_limit and args.df_limit > 0 else None
    chunks, edges, facts, meta, cooc = load_db(args.db)

    links, per_fact, comps, max_c, isolated, total_valid = run_config(
        chunks, edges, facts, meta, cooc, args.min_tokens, df_limit
    )
    out = []
    out.append(f"DB: {args.db}")
    out.append(f"chunks={len(chunks)} facts={len(facts)} scopes={len({f[3] for f in facts})}"
               f" causal_edges={len(edges)} meta={len(meta)} cooc={len(cooc)}")
    out.append("")
    out.append(f"config: min_tokens={args.min_tokens} df_limit={df_limit or 'none'}")
    out.append(f"  fact<->chunk links : {len(links)}  (avg {len(links)/max(len(facts),1):.1f} per fact)")
    out.append(f"  facts with >=1 link: {sum(1 for p in per_fact if p > 0)}/{len(facts)}")
    out.append(f"  total valid edges  : {total_valid}")
    out.append(f"  components/largest : {comps} / {max_c}  (isolated {isolated})")

    if not args.no_compare:
        olinks, oper_fact, ocomps, omax_c, oiso, ototal = run_config(
            chunks, edges, facts, meta, cooc, 2, None
        )
        out.append("")
        out.append("comparison vs pre-fix config (min_tokens=2, no df filter):")
        out.append(f"  links            : {len(olinks)} -> {len(links)}  "
                   f"({(len(links)/len(olinks) - 1)*100:+.0f}%)")
        out.append(f"  total valid edges: {ototal} -> {total_valid}")
        out.append(f"  components       : {ocomps} -> {comps}  (largest {omax_c} -> {max_c})")

    if args.sample and args.sample > 0:
        out.append("")
        out.append(f"random sample of {args.sample} links (for manual precision judgment):")
        rng = random.Random(20260826)
        for ftxt, fscope, cid, ctxt, n in rng.sample(links, min(args.sample, len(links))):
            out.append(f"  [{n} tok] {ftxt!r}  <->  {ctxt!r}")

    print("\n".join(out))


if __name__ == "__main__":
    main()
