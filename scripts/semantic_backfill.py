#!/usr/bin/env python3
"""Semantic embedding backfill + RRF fusion for LoCoMo benchmark.

This script does offline what the Rust harness would do at runtime:
1. Embed all chunks in a distill DB using a local sentence-transformer model
2. Write embeddings into edge_embeddings table (blob format matching Rust)
3. At QA time, embed the query and do cosine ranking, then RRF-fuse with BM25

Usage as backfill (one-time per DB):
  python3 scripts/semantic_backfill.py backfill --db benches/locomo/db/conv_0_distill.db

The Rust harness's search_causal_semantic() already reads edge_embeddings.
After backfill, running the harness with CAUSAL_MEMORY_EMBED_* env vars
configured will use the semantic path automatically.
"""

import argparse
import sqlite3
import struct
import sys
from pathlib import Path


def vec_to_blob(vec):
    """Match Rust's vec_to_blob: little-endian f32 array."""
    return struct.pack(f'<{len(vec)}f', *vec)


def blob_to_vec(blob):
    """Match Rust's blob_to_vec."""
    n = len(blob) // 4
    return list(struct.unpack(f'<{n}f', blob))


def backfill(db_path, model_name='all-MiniLM-L6-v2'):
    """Embed all chunks and store in edge_embeddings table."""
    from sentence_transformers import SentenceTransformer

    print(f"Loading model: {model_name}...")
    model = SentenceTransformer(model_name)
    print(f"  dims: {model.get_sentence_embedding_dimension()}")

    conn = sqlite3.connect(db_path)

    # Get all chunks
    chunks = conn.execute("SELECT id, text FROM chunks ORDER BY id").fetchall()
    print(f"Embedding {len(chunks)} chunks...")

    # Batch encode
    texts = [t for _, t in chunks]
    embeddings = model.encode(texts, batch_size=64, show_progress_bar=True)

    # Write to edge_embeddings (keyed by a synthetic edge_id = rowid of chunk)
    # The Rust search_causal_semantic joins edge_embeddings on edge_id = causal_edges.id
    # But chunks don't have edge_ids directly. We need to find edges that reference
    # each chunk as from_id or to_id, and attach the embedding to that edge.
    #
    # Strategy: for each chunk, find edges where from_id = chunk_id,
    # and store the embedding under that edge's id.
    written = 0
    for (chunk_id, _text), emb in zip(chunks, embeddings):
        # Find edges where this chunk is the 'from' endpoint
        edges = conn.execute(
            "SELECT id FROM causal_edges WHERE from_id = ? LIMIT 1", (chunk_id,)
        ).fetchall()
        if not edges:
            # Try as 'to' endpoint
            edges = conn.execute(
                "SELECT id FROM causal_edges WHERE to_id = ? LIMIT 1", (chunk_id,)
            ).fetchall()
        if not edges:
            continue

        edge_id = edges[0][0]
        blob = vec_to_blob(emb)
        conn.execute(
            "INSERT OR REPLACE INTO edge_embeddings (edge_id, model, vector, created_at) "
            "VALUES (?, ?, ?, ?)",
            (edge_id, model_name, blob, 0)
        )
        written += 1

    # Also embed facts
    facts = conn.execute(
        "SELECT id, key, value FROM agent_facts WHERE valid_to IS NULL"
    ).fetchall()
    if facts:
        print(f"Embedding {len(facts)} facts...")
        fact_texts = [f"{k} {v}" for _, k, v in facts]
        fact_embs = model.encode(fact_texts, batch_size=64, show_progress_bar=True)
        for (fact_id, _, _), emb in zip(facts, fact_embs):
            blob = vec_to_blob(emb)
            conn.execute(
                "INSERT OR REPLACE INTO agent_facts_embeddings (fact_id, model, vector, created_at) "
                "VALUES (?, ?, ?, ?)",
                (fact_id, model_name, blob, 0)
            )

    conn.commit()
    conn.close()
    print(f"Done: {written} edge embeddings + {len(facts)} fact embeddings written to {db_path}")


def main():
    parser = argparse.ArgumentParser(description="Semantic embedding backfill")
    sub = parser.add_subparsers(dest="cmd")

    bf = sub.add_parser("backfill", help="Embed all chunks in a DB")
    bf.add_argument("--db", required=True, help="Path to SQLite DB")
    bf.add_argument("--model", default="all-MiniLM-L6-v2", help="Model name")

    args = parser.parse_args()
    if args.cmd == "backfill":
        backfill(args.db, args.model)
    else:
        parser.print_help()


if __name__ == "__main__":
    main()
