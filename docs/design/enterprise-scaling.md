# Enterprise Scaling: measured bottlenecks, trigger points, and the optimization ladder

> Status: design + measurement (2026-09-05). Numbers below are **measured** on a
> synthetic store (see `crates/causal-memory/examples/scale_probe.rs`), not
> extrapolated guesses. Host: 8 GB macOS laptop; `--release` not used.

## TL;DR

- At **tens of millions of nodes** the current resident-graph architecture
  breaks **before storage does**. Storage is never the first bottleneck.
- The pain order is: **resident whole-store graph (rebuild cadence + RAM)** →
  **full-table candidate scans** → **unbounded in-memory caches** → (later)
  disk. A graph *database* is **not** required at any of these sizes — the
  access pattern (bounded 1-2 hop recall over a sparse graph) is relational
  SQL's home turf.
- Every optimization below has a **measured trigger size**. Do not optimize
  ahead of the trigger; do decide the *architecture* (graph-as-projection)
  now, because it is cheap today and expensive to retrofit.

## 1. How the active engine actually works (recap, code-grounded)

The active recall engine is the **unified engine**: `Memory` (memory facade,
used by MCP / python bindings / the Hermes provider) holds a resident
whole-store graph:

```
Memory (memory/mod.rs:58)
  graph: Mutex<Option<CausalGraph>>          // whole store, in RAM
  open → CausalGraph::from_store(&store)     // full load at open (mod.rs:84)
  write → mark_graph_dirty                   // ≥5 writes OR 30s (mod.rs:26-27)
  next search → maybe_rebuild_graph → full from_store reload (mod.rs:240-264)
  recall  → graph.spreading_activation_seeded (memory/unified.rs:78)
```

`CausalGraph` (hippocampus/mod.rs:82) is compact CSR + SoA (**good choice**),
but it holds **every node's full text** (`node_text: Vec<String>`) and is
**one full copy per frontend** (each MCP server / python process / Hermes
session builds its own).

`store/retrieve/` (BM25 / semantic / entity-hop) is the **legacy direct-store
path** — do not mistake it for the active engine.

## 2. Measurement method

`crates/causal-memory/examples/scale_probe.rs <nodes> [fanout=4]`:
1. bulk-inserts synthetic chunks + causal edges (raw rows, faithful schema)
2. times `CausalGraph::from_store` (the resident-graph build)
3. times a full candidate id-scan and a cold full-text hydrate (raw SQL
   mirrors of the entity-path costs)
4. times one `spreading_activation` call

Run as `/usr/bin/time -l target/debug/examples/scale_probe 1000000 4` to
capture RSS. **Caveat:** the probe query never seeded (hits=0 — all-lowercase
synthetic text yields no entity tokens), so spread times are the *seed-scan*
lower bound; real propagation costs more. 5M nodes ≈ 7.5 GB peak footprint —
do not run 10M on an 8 GB host.

## 3. Measured numbers

| metric | 50k nodes / 0.2M edges | 1M / 4M | 5M / **20M** | growth (5× data) |
|---|---|---|---|---|
| `from_store` rebuild | 4.9 s | 107 s | **676 s (11.3 min)** | 6.3× — superlinear |
| memory footprint (macOS peak) | ~0.12 GB | 1.6 GB | **7.5 GB** | ~linear |
| full id-scan (candidate sweep) | 0.02 s | 2.1 s | **18.4 s** | 8.7× — superlinear |
| cold full-text hydrate | 0.35 s | 12.9 s | **114 s** | 8.9× |
| spread seed-scan (no seed) | 0.03 s | 0.6 s | **20.7 s** | 34× |
| db file | 37 MB | 779 MB | 4.0 GB | ~linear — disk is fine |

Linear extrapolation to 50M nodes / ~200M edges: **~2 h per full rebuild**,
**~70 GB+ per resident instance**, **minutes per full-candidate query**,
~40 GB single-file db (fine on disk).

## 4. Bottleneck order (corrected reality)

1. **Rebuild cadence** — `maybe_rebuild_graph` fires a **full** `from_store`
   every ≥5 writes or 30 s. At 5M nodes that is an 11-minute reload triggered
   every 30 s of active writing: the store throttles itself. First cliff is
   much earlier: rebuilds pass ~30 s somewhere around 300-500k edges.
2. **Resident memory** — `node_text` keeps the whole corpus in RAM; one full
   graph per frontend multiplies it. ~linear growth ⇒ ~70 GB/instance at 50M.
3. **Full-table candidate scans** — the entity path is O(all valid edges)
   per query (id sweep + per-edge overlap). 2.1 s already at 4M edges.
4. Unbounded caches (`entity_cache`, per-edge token Arcs) — grows forever
   with edge count; needs a bound before it becomes the OOM trigger.
5. Storage / graph-database questions — **not** bottlenecks at these sizes.

## 5. Measured trigger table (optimize when crossed, not before)

| optimization | trigger (measured) | notes |
|---|---|---|
| T0 — rebuild policy: make full rebuild a *fallback*; serve reads from a stale graph + incremental patches (write-path patch machinery already exists, mod.rs:190-236) and rebuild only when a store-resolved seed is missing (that branch already exists, mod.rs:252) | **now-ish**: rebuild > ~1 s (≈ 100k edges) makes the 30 s cadence self-defeating under active writes | lowest risk, highest leverage; no data-layout change |
| T0b — bound `entity_cache` (+ graph token caches) | any long-running process | pure win; correctness already guaranteed by immutability |
| T1 — move `node_text` out of the resident graph (store text by id; fetch on demand; LRU for hot texts) | **~1-2M nodes** (footprint > ~1-2 GB) | removes the dominant RAM term |
| T2 — share one read-mostly graph per store/tenant across frontends (`Arc<RwLock<…>>`) instead of one full copy per Memory | when >1 frontend (MCP + python + Hermes) runs against one store | removes the multiplier |
| T3 — ANN / inverted-entity index for the candidate path | **~4M edges** (id-scan already 2.1 s) | unrelated to graph DBs |
| T4 — cap resident graph; fall back to DB wave queries / hosted tier beyond N nodes | 10M+ nodes | see §7 |

## 6. Do we need "a graph engine / graph data" at enterprise scale?

No — we need the **algorithm** (spreading activation is the product's
differentiator) and a **projection strategy**, not the current *form*.

- Graph **data** stays relational (chunks + causal_edges + meta edges):
  sparse graph, bounded shallow traversal — relational tables + indexes are
  the right home at every size measured here. A graph database only pays off
  for unbounded-depth hot traversal or whole-graph analytics; neither is in
  the recall hot path.
- Graph **engine** becomes a *projection* of the store, not a private
  per-process resident structure: the store (SQLite/PG edge tables) is the
  single source of truth; `CausalGraph` is a rebuildable projection
  (`from_store` is already event-sourcing-friendly; git-sync commit objects
  make the projection portable across backends).

Tiered deployment (also the product tiering): edge/local keeps a **bounded
hot graph** (millisecond associative recall); the hosted tier serves cold
recall from the relational layer with on-demand materialization (LRU hot
regions per tenant); whole-graph analytics lives offline / in a hosted graph
store. `Memory::disable_spread` already gives the product a
with/without-engine switch (ablation → tiering).

## 7. Open items / next measurements

- 10M-node point on a bigger host (expect ~15 GB footprint, ~25-35 min
  rebuild) to confirm superlinear fits.
- Distinguish rebuild *peak* vs *steady-state* RSS (peak includes build-time
  duplicates — may argue for building into a fresh graph then swapping).
- FTS5 (BM25 seed path) behavior at ~40 GB single-file scale.
- Real propagation cost once seeding actually hits (query that seeds).

## 8. Observability to add (before acting on any tier)

- `from_store` build duration + RSS (per rebuild)
- graph-staleness: rebuilds triggered per interval; time-between-rebuilds
- candidate-scan latency (entity path) — p95
- cache sizes / hit rates (`entity_cache`, any text cache)
- spread query latency + activated-node counts (histogram exists:
  `recall_activated_nodes`)
