#!/usr/bin/env python3
"""Offline embedding backfill using ZhiPu embedding-3 API.

Embeds all chunks and facts in a LoCoMo/LME distill DB and writes them
into edge_embeddings / agent_facts_embeddings tables in the blob format
that Rust's search_causal_semantic / search_facts_semantic expects.

Usage:
  python3 scripts/embed_backfill.py --db benches/locomo/db/conv_0_distill.db
  python3 scripts/embed_backfill.py --db-dir benches/locomo/db --pattern 'conv_*_distill.db'
"""
import argparse
import requests
import sqlite3
import struct
import sys
import time
from pathlib import Path

API_URL = "https://open.bigmodel.cn/api/paas/v4/embeddings"
API_KEY = "267a25f814c5452290ff6e602e2344f2.MFJfoMrC5V4kyUZo"
MODEL = "embedding-3"
BATCH_SIZE = 16


def embed_batch(texts):
    """Call ZhiPu embedding API for a batch of texts."""
    headers = {
        "Authorization": f"Bearer {API_KEY}",
        "Content-Type": "application/json",
    }
    resp = requests.post(
        API_URL,
        headers=headers,
        json={"model": MODEL, "input": texts},
        timeout=30,
    )
    if resp.status_code == 429:
        time.sleep(3)
        resp = requests.post(
            API_URL,
            headers=headers,
            json={"model": MODEL, "input": texts},
            timeout=30,
        )
    resp.raise_for_status()
    data = resp.json()
    return [d["embedding"] for d in data["data"]]


def vec_to_blob(vec):
    """Match Rust vec_to_blob: little-endian f32 array."""
    return struct.pack(f"<{len(vec)}f", *vec)


def backfill_db(db_path):
    conn = sqlite3.connect(db_path)
    conn.execute("PRAGMA journal_mode=WAL")

    # --- Embed chunks → attach to their first edge ---
    chunks = conn.execute("SELECT id, text FROM chunks ORDER BY id").fetchall()
    print(f"  {len(chunks)} chunks")

    written = 0
    for i in range(0, len(chunks), BATCH_SIZE):
        batch = chunks[i:i+BATCH_SIZE]
        texts = [t for _, t in batch]
        try:
            embs = embed_batch(texts)
        except Exception as e:
            print(f"    batch {i} failed: {e}, retrying once...")
            time.sleep(2)
            try:
                embs = embed_batch(texts)
            except:
                print(f"    batch {i} skipped")
                continue

        for (chunk_id, _), emb in zip(batch, embs):
            # Find an edge that references this chunk
            edge = conn.execute(
                "SELECT id FROM causal_edges WHERE from_id = ? LIMIT 1", (chunk_id,)
            ).fetchone()
            if not edge:
                edge = conn.execute(
                    "SELECT id FROM causal_edges WHERE to_id = ? LIMIT 1", (chunk_id,)
                ).fetchone()
            if not edge:
                continue
            conn.execute(
                "INSERT OR REPLACE INTO edge_embeddings (edge_id, model, vector, created_at) VALUES (?,?,?,?)",
                (edge[0], MODEL, vec_to_blob(emb), 0),
            )
            written += 1

        if (i // BATCH_SIZE) % 5 == 0:
            print(f"    {i+len(batch)}/{len(chunks)} chunks embedded")
        time.sleep(0.1)  # rate limit

    # --- Embed facts ---
    facts = conn.execute(
        "SELECT id, key, value FROM agent_facts WHERE valid_to IS NULL"
    ).fetchall()
    print(f"  {len(facts)} facts")
    for i in range(0, len(facts), BATCH_SIZE):
        batch = facts[i:i+BATCH_SIZE]
        texts = [f"{k} {v}" for _, k, v in batch]
        try:
            embs = embed_batch(texts)
        except Exception as e:
            print(f"    fact batch {i} failed: {e}")
            continue
        for (fact_id, _, _), emb in zip(batch, embs):
            conn.execute(
                "INSERT OR REPLACE INTO agent_facts_embeddings (fact_id, model, vector, created_at) VALUES (?,?,?,?)",
                (fact_id, MODEL, vec_to_blob(emb), 0),
            )
        time.sleep(0.1)

    conn.commit()
    conn.close()
    print(f"  done: {written} edge embeddings + {len(facts)} fact embeddings")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", help="Single DB path")
    parser.add_argument("--db-dir", help="Directory of DBs")
    parser.add_argument("--pattern", default="conv_*_distill.db", help="Glob pattern")
    args = parser.parse_args()

    if args.db:
        print(f"Backfilling {args.db}")
        backfill_db(args.db)
    elif args.db_dir:
        dbs = sorted(Path(args.db_dir).glob(args.pattern))
        print(f"Found {len(dbs)} DBs matching {args.pattern}")
        for db in dbs:
            print(f"\n=== {db.name} ===")
            backfill_db(str(db))
    else:
        parser.print_help()


if __name__ == "__main__":
    main()
