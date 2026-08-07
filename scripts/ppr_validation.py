#!/usr/bin/env python3
"""
PPR validation suite for causal-memory graph.

Experiments:
1. Truncation error: K-hop PPR vs converged PPR (how many hops needed?)
2. Weight normalization: confidence weights vs PPR row-normalized
3. Alpha sweep: teleport probability effect on CausalEval accuracy
4. Forward vs backward PPR complementarity (C1 attribution vs C2 intervention)
5. Spectral analysis: eigenvalue distribution → convergence rate
"""

import json
import sqlite3
import numpy as np
import time
import os
from collections import defaultdict
from pathlib import Path

DB_PATH = os.path.expanduser("~/.local/share/causal-memory/causal.db")
CAUSAL_EVAL_DATA = "benches/causal_eval/data"

def load_graph(db_path):
    """Load causal graph from SQLite into adjacency structures."""
    conn = sqlite3.connect(db_path)
    c = conn.cursor()
    
    # Load chunks as nodes
    nodes = {}
    for row in c.execute("SELECT id, text FROM chunks").fetchall():
        nodes[row[0]] = row[1]
    
    # Load edges
    edges = []
    for row in c.execute("""
        SELECT ce.from_id, ce.to_id, ce.relation, ce.confidence, ce.task_tag
        FROM causal_edges ce
        WHERE ce.valid_to IS NULL
    """).fetchall():
        edges.append({
            'from': row[0], 'to': row[1], 'relation': row[2],
            'confidence': row[3], 'task_tag': row[4]
        })
    
    conn.close()
    
    # Build node index
    node_ids = sorted(nodes.keys())
    id_to_idx = {nid: i for i, nid in enumerate(node_ids)}
    
    return nodes, node_ids, id_to_idx, edges

def build_adjacency(n, edges, id_to_idx, weight_mode='confidence'):
    """Build adjacency matrix.
    
    weight_mode:
    - 'confidence': use edge.confidence directly (current system)
    - 'ppr': row-normalize (standard PPR)
    - 'uniform': equal weight per out-edge
    """
    # Forward adjacency
    out_neighbors = defaultdict(list)
    in_neighbors = defaultdict(list)
    
    for e in edges:
        if e['from'] not in id_to_idx or e['to'] not in id_to_idx:
            continue
        u = id_to_idx[e['from']]
        v = id_to_idx[e['to']]
        
        coeff = {'caused': 1.0, 'enabled': 0.5, 'prevented': -0.3, 
                 'no_effect': 0.0, 'fact': 0.8, 'meta': 0.6}.get(e['relation'], 0.5)
        w = e['confidence'] * coeff
        
        out_neighbors[u].append((v, w, e['relation']))
        in_neighbors[v].append((u, w, e['relation']))
    
    # Build transition matrix
    S = np.zeros((n, n))
    for u in range(n):
        neighbors = out_neighbors.get(u, [])
        if not neighbors:
            continue
        if weight_mode == 'confidence':
            for (v, w, _) in neighbors:
                S[u][v] = w
        elif weight_mode == 'ppr':
            total = sum(abs(w) for (_, w, _) in neighbors)
            if total > 0:
                for (v, w, _) in neighbors:
                    S[u][v] = w / total
        elif weight_mode == 'uniform':
            k = len(neighbors)
            for (v, w, _) in neighbors:
                S[u][v] = 1.0 / k
    
    return S, out_neighbors, in_neighbors

def ppr_power_iter(S, seed_vec, alpha, max_iter=500, tol=1e-8):
    """Compute PPR via power iteration. Returns (scores, iterations_to_converge)."""
    n = S.shape[0]
    p = seed_vec.copy()
    
    for i in range(max_iter):
        new_p = alpha * (S.T @ p) + (1 - alpha) * seed_vec
        
        # Check convergence
        diff = np.max(np.abs(new_p - p))
        p = new_p
        
        if diff < tol:
            return p, i + 1
    
    return p, max_iter

def ppr_truncated(S, seed_vec, alpha, k_hops):
    """Compute K-hop truncated PPR (current system's approach)."""
    n = S.shape[0]
    p = (1 - alpha) * seed_vec.copy()
    current = seed_vec.copy()
    
    for _ in range(k_hops):
        current = alpha * (S.T @ current)
        p += current * (1 - alpha)
    
    return p

def experiment_1_truncation_error():
    """Exp 1: How many hops until PPR converges on the production graph?"""
    print("\n" + "="*60)
    print("EXPERIMENT 1: Truncation Error (K-hop vs Converged PPR)")
    print("="*60)
    
    nodes, node_ids, id_to_idx, edges = load_graph(DB_PATH)
    n = len(node_ids)
    print(f"  Graph: {n} nodes, {len(edges)} edges")
    
    S, _, _ = build_adjacency(n, edges, id_to_idx, 'confidence')
    
    # Pick 10 random seed nodes
    rng = np.random.RandomState(42)
    has_outgoing = [i for i in range(n) if np.sum(np.abs(S[i])) > 0]
    if len(has_outgoing) < 10:
        print("  Not enough nodes with outgoing edges")
        return
    seeds = rng.choice(has_outgoing, size=min(10, len(has_outgoing)), replace=False)
    
    print(f"\n  {'Hop':>4}  {'Avg L1 Error':>14}  {'Avg Cosine Sim':>14}  {'Avg Kendall-τ':>14}")
    print("  " + "-"*56)
    
    converged_results = []
    iterations_to_converge = []
    
    for seed_idx in seeds:
        seed_vec = np.zeros(n)
        seed_vec[seed_idx] = 1.0
        
        # Converged PPR
        p_conv, iters = ppr_power_iter(S, seed_vec, alpha=0.85, max_iter=500)
        converged_results.append((seed_idx, p_conv))
        iterations_to_converge.append(iters)
        
        # Truncated at various K
        for k in [1, 2, 3, 5, 8, 15, 30]:
            p_trunc = ppr_truncated(S, seed_vec, alpha=0.85, k_hops=k)
            # Compare top-10 overlap
            top_conv = set(np.argsort(-p_conv)[:10])
            top_trunc = set(np.argsort(-p_trunc)[:10])
            overlap = len(top_conv & top_trunc) / 10.0
            # We'll print aggregated stats below
    
    # Aggregate
    for k in [1, 2, 3, 5, 8, 15, 30]:
        l1_errors = []
        cosine_sims = []
        kendall_taus = []
        top_overlaps = []
        
        for seed_idx, p_conv in converged_results:
            seed_vec = np.zeros(n)
            seed_vec[seed_idx] = 1.0
            p_trunc = ppr_truncated(S, seed_vec, alpha=0.85, k_hops=k)
            
            l1 = np.sum(np.abs(p_trunc - p_conv))
            cos = np.dot(p_trunc, p_conv) / (np.linalg.norm(p_trunc) * np.linalg.norm(p_conv) + 1e-12)
            
            # Top-10 overlap
            top_conv = set(np.argsort(-p_conv)[:10].tolist())
            top_trunc = set(np.argsort(-p_trunc)[:10].tolist())
            overlap = len(top_conv & top_trunc) / 10.0
            
            l1_errors.append(l1)
            cosine_sims.append(cos)
            top_overlaps.append(overlap)
        
        print(f"  {k:>4}  {np.mean(l1_errors):>14.6f}  {np.mean(cosine_sims):>14.6f}  {np.mean(top_overlaps):>14.1%}")
    
    print(f"\n  Convergence: avg {np.mean(iterations_to_converge):.0f} iterations "
          f"(min={np.min(iterations_to_converge)}, max={np.max(iterations_to_converge)})")
    
    # Spectral radius
    try:
        eigvals = np.linalg.eigvals(S)
        spectral_radius = np.max(np.abs(eigvals))
        print(f"  Spectral radius ρ(S) = {spectral_radius:.4f}")
        print(f"  Convergence rate per iteration: ρ(αS) = {0.85 * spectral_radius:.4f}")
        print(f"  Theoretical iterations for 1e-6 convergence: ~{-np.log(1e-6) / np.log(1/(0.85*spectral_radius+1e-12)):.0f}")
    except:
        print("  (Eigenvalue computation failed — graph too large)")
    
    return converged_results

def experiment_2_weight_modes():
    """Exp 2: Confidence weights vs PPR row-normalized vs uniform."""
    print("\n" + "="*60)
    print("EXPERIMENT 2: Weight Mode Comparison (CausalEval)")
    print("="*60)
    
    # Load CausalEval graphs (which have richer edge types)
    all_results = {}
    
    for mode in ['confidence', 'ppr', 'uniform']:
        print(f"\n  Mode: {mode}")
        
        for gi in range(10):
            json_path = f"{CAUSAL_EVAL_DATA}/graph_{gi}.json"
            if not os.path.exists(json_path):
                continue
            
            bundle = json.load(open(json_path))
            graph = bundle['graph']
            qa_list = bundle.get('qa', [])
            
            # Build graph from JSON
            node_texts = {n['id']: f"{n['person']} {n['action']}" for n in graph['nodes']}
            node_ids = sorted(node_texts.keys())
            id_to_idx = {nid: i for i, nid in enumerate(node_ids)}
            n = len(node_ids)
            
            edges_for_graph = []
            for e in graph['edges']:
                edges_for_graph.append({
                    'from': f"n{e['from']}", 'to': f"n{e['to']}",
                    'relation': e['relation'], 'confidence': 0.8
                })
            
            # Map JSON node ids
            json_id_to_idx = {}
            for node in graph['nodes']:
                json_id_to_idx[node['id']] = len(json_id_to_idx)
            
            edges_remapped = []
            for e in graph['edges']:
                if e['from'] in json_id_to_idx and e['to'] in json_id_to_idx:
                    edges_remapped.append({
                        'from': json_id_to_idx[e['from']],
                        'to': json_id_to_idx[e['to']],
                        'relation': e['relation'],
                        'confidence': 0.8
                    })
            
            n_nodes = len(json_id_to_idx)
            S, _, _ = build_adjacency_from_list(n_nodes, edges_remapped, mode)
            
            # Test each QA
            for qa in qa_list:
                # Find seed nodes by keyword match
                q_lower = qa['question'].lower()
                seeds = []
                for nid, text in node_texts.items():
                    idx = json_id_to_idx.get(nid)
                    if idx is not None:
                        # Simple keyword match
                        text_lower = text.lower()
                        words = [w for w in q_lower.split() if len(w) > 3]
                        if any(w in text_lower for w in words):
                            seeds.append(idx)
                
                if not seeds:
                    continue
                
                # Run PPR
                seed_vec = np.zeros(n_nodes)
                for s in seeds:
                    seed_vec[s] = 1.0 / len(seeds)
                
                ppr_scores, _ = ppr_power_iter(S, seed_vec, alpha=0.85, max_iter=100)
                
                # Evidence hit: are gold nodes in top-k?
                gold_nodes = qa.get('evidence_nodes', [])
                gold_indices = [json_id_to_idx.get(g) for g in gold_nodes if g in json_id_to_idx]
                
                if not gold_indices:
                    continue
                
                top_k = set(np.argsort(-ppr_scores)[:10].tolist())
                hit = any(g in top_k for g in gold_indices)
                
                cat = qa['category']
                key = (mode, cat)
                if key not in all_results:
                    all_results[key] = {'total': 0, 'hits': 0}
                all_results[key]['total'] += 1
                if hit:
                    all_results[key]['hits'] += 1
    
    # Print comparison
    cats = {11: 'C1', 12: 'C2', 13: 'C3', 14: 'C4', 15: 'C5', 16: 'C6', 17: 'C7'}
    print(f"\n  {'Cat':>4}  {'confidence':>12}  {'ppr':>12}  {'uniform':>12}")
    print("  " + "-"*46)
    for cat_id in sorted(cats.keys()):
        row = []
        for mode in ['confidence', 'ppr', 'uniform']:
            key = (mode, cat_id)
            r = all_results.get(key, {'total': 0, 'hits': 0})
            if r['total'] > 0:
                row.append(f"{r['hits']}/{r['total']} ({100*r['hits']//r['total']}%)")
            else:
                row.append("—")
        print(f"  {cats[cat_id]:>4}  {row[0]:>12}  {row[1]:>12}  {row[2]:>12}")

def build_adjacency_from_list(n, edges, weight_mode='confidence'):
    """Build adjacency from edge list with integer indices."""
    out_neighbors = defaultdict(list)
    for e in edges:
        out_neighbors[e['from']].append((e['to'], e['relation'], e.get('confidence', 0.8)))
    
    S = np.zeros((n, n))
    for u in range(n):
        neighbors = out_neighbors.get(u, [])
        if not neighbors:
            continue
        
        if weight_mode == 'confidence':
            for (v, rel, conf) in neighbors:
                coeff = {'caused': 1.0, 'enabled': 0.5, 'prevented': -0.3,
                         'no_effect': 0.0}.get(rel, 0.5)
                S[u][v] = conf * coeff
        elif weight_mode == 'ppr':
            total = sum(abs(conf * {'caused': 1.0, 'enabled': 0.5, 'prevented': -0.3,
                         'no_effect': 0.0}.get(rel, 0.5)) for (_, rel, conf) in neighbors)
            if total > 0:
                for (v, rel, conf) in neighbors:
                    coeff = {'caused': 1.0, 'enabled': 0.5, 'prevented': -0.3,
                             'no_effect': 0.0}.get(rel, 0.5)
                    S[u][v] = conf * coeff / total
        elif weight_mode == 'uniform':
            k = len(neighbors)
            for (v, _, _) in neighbors:
                S[u][v] = 1.0 / k
    
    return S, out_neighbors, None

def experiment_3_alpha_sweep():
    """Exp 3: Optimal teleport probability alpha."""
    print("\n" + "="*60)
    print("EXPERIMENT 3: Alpha (Teleport) Sweep")
    print("="*60)
    
    results = {}
    
    for alpha in [0.3, 0.5, 0.7, 0.85, 0.95]:
        for gi in range(10):
            json_path = f"{CAUSAL_EVAL_DATA}/graph_{gi}.json"
            if not os.path.exists(json_path):
                continue
            
            bundle = json.load(open(json_path))
            graph = bundle['graph']
            qa_list = bundle.get('qa', [])
            
            node_texts = {n['id']: f"{n['person']} {n['action']}" for n in graph['nodes']}
            json_id_to_idx = {n['id']: i for i, n in enumerate(graph['nodes'])}
            n_nodes = len(graph['nodes'])
            
            edges_remapped = []
            for e in graph['edges']:
                if e['from'] in json_id_to_idx and e['to'] in json_id_to_idx:
                    edges_remapped.append({
                        'from': json_id_to_idx[e['from']],
                        'to': json_id_to_idx[e['to']],
                        'relation': e['relation'],
                        'confidence': 0.8
                    })
            
            S, _, _ = build_adjacency_from_list(n_nodes, edges_remapped, 'confidence')
            
            for qa in qa_list:
                q_lower = qa['question'].lower()
                seeds = []
                for nid, text in node_texts.items():
                    text_lower = text.lower()
                    words = [w for w in q_lower.split() if len(w) > 3]
                    if any(w in text_lower for w in words):
                        idx = json_id_to_idx.get(nid)
                        if idx is not None:
                            seeds.append(idx)
                
                if not seeds:
                    continue
                
                seed_vec = np.zeros(n_nodes)
                for s in seeds:
                    seed_vec[s] = 1.0 / len(seeds)
                
                ppr_scores, _ = ppr_power_iter(S, seed_vec, alpha=alpha, max_iter=100)
                
                gold_nodes = qa.get('evidence_nodes', [])
                gold_indices = [json_id_to_idx.get(g) for g in gold_nodes if g in json_id_to_idx]
                
                if not gold_indices:
                    continue
                
                top_k = set(np.argsort(-ppr_scores)[:10].tolist())
                hit = any(g in top_k for g in gold_indices)
                
                cat = qa['category']
                key = (alpha, cat)
                if key not in results:
                    results[key] = {'total': 0, 'hits': 0}
                results[key]['total'] += 1
                if hit:
                    results[key]['hits'] += 1
    
    cats = {11: 'C1', 12: 'C2', 13: 'C3', 14: 'C4', 15: 'C5', 16: 'C6', 17: 'C7'}
    print(f"\n  {'Cat':>4}  {'α=0.3':>8}  {'α=0.5':>8}  {'α=0.7':>8}  {'α=0.85':>8}  {'α=0.95':>8}")
    print("  " + "-"*51)
    for cat_id in sorted(cats.keys()):
        row = []
        for alpha in [0.3, 0.5, 0.7, 0.85, 0.95]:
            key = (alpha, cat_id)
            r = results.get(key, {'total': 0, 'hits': 0})
            if r['total'] > 0:
                row.append(f"{100*r['hits']//r['total']}%")
            else:
                row.append("—")
        print(f"  {cats[cat_id]:>4}  {row[0]:>8}  {row[1]:>8}  {row[2]:>8}  {row[3]:>8}  {row[4]:>8}")
    
    # Overall
    print("  " + "-"*51)
    row = []
    for alpha in [0.3, 0.5, 0.7, 0.85, 0.95]:
        total_hits = sum(results.get((alpha, c), {'hits': 0})['hits'] for c in cats)
        total_all = sum(results.get((alpha, c), {'total': 0})['total'] for c in cats)
        if total_all > 0:
            row.append(f"{100*total_hits//total_all}%")
        else:
            row.append("—")
    print(f"  {'All':>4}  {row[0]:>8}  {row[1]:>8}  {row[2]:>8}  {row[3]:>8}  {row[4]:>8}")

def experiment_4_fwd_bwd():
    """Exp 4: Forward vs backward PPR complementarity."""
    print("\n" + "="*60)
    print("EXPERIMENT 4: Forward vs Backward PPR Complementarity")
    print("="*60)
    
    results = {'fwd_only': defaultdict(lambda: {'total': 0, 'hits': 0}),
               'bwd_only': defaultdict(lambda: {'total': 0, 'hits': 0}),
               'union': defaultdict(lambda: {'total': 0, 'hits': 0}),
               'intersect': defaultdict(lambda: {'total': 0, 'hits': 0})}
    
    for gi in range(10):
        json_path = f"{CAUSAL_EVAL_DATA}/graph_{gi}.json"
        if not os.path.exists(json_path):
            continue
        
        bundle = json.load(open(json_path))
        graph = bundle['graph']
        qa_list = bundle.get('qa', [])
        node_texts = {n['id']: f"{n['person']} {n['action']}" for n in graph['nodes']}
        json_id_to_idx = {n['id']: i for i, n in enumerate(graph['nodes'])}
        n_nodes = len(graph['nodes'])
        
        edges_remapped = []
        for e in graph['edges']:
            if e['from'] in json_id_to_idx and e['to'] in json_id_to_idx:
                edges_remapped.append({
                    'from': json_id_to_idx[e['from']],
                    'to': json_id_to_idx[e['to']],
                    'relation': e['relation'],
                    'confidence': 0.8
                })
        
        S_fwd, _, _ = build_adjacency_from_list(n_nodes, edges_remapped, 'confidence')
        S_bwd = S_fwd.T  # Transpose = reverse edges
        
        for qa in qa_list:
            q_lower = qa['question'].lower()
            seeds = []
            for nid, text in node_texts.items():
                text_lower = text.lower()
                words = [w for w in q_lower.split() if len(w) > 3]
                if any(w in text_lower for w in words):
                    idx = json_id_to_idx.get(nid)
                    if idx is not None:
                        seeds.append(idx)
            
            if not seeds:
                continue
            
            seed_vec = np.zeros(n_nodes)
            for s in seeds:
                seed_vec[s] = 1.0 / len(seeds)
            
            ppr_fwd, _ = ppr_power_iter(S_fwd, seed_vec, alpha=0.85, max_iter=100)
            ppr_bwd, _ = ppr_power_iter(S_bwd, seed_vec, alpha=0.85, max_iter=100)
            
            gold_nodes = qa.get('evidence_nodes', [])
            gold_indices = [json_id_to_idx.get(g) for g in gold_nodes if g in json_id_to_idx]
            
            if not gold_indices:
                continue
            
            top_fwd = set(np.argsort(-ppr_fwd)[:10].tolist())
            top_bwd = set(np.argsort(-ppr_bwd)[:10].tolist())
            top_union = set(np.argsort(-(ppr_fwd + ppr_bwd))[:10].tolist())
            top_intersect = top_fwd & top_bwd
            
            cat = qa['category']
            for mode_name, top_set in [('fwd_only', top_fwd), ('bwd_only', top_bwd),
                                        ('union', top_union)]:
                results[mode_name][cat]['total'] += 1
                if any(g in top_set for g in gold_indices):
                    results[mode_name][cat]['hits'] += 1
    
    cats = {11: 'C1', 12: 'C2', 13: 'C3', 14: 'C4', 15: 'C5', 16: 'C6', 17: 'C7'}
    print(f"\n  {'Cat':>4}  {'C1/C5 target':>14}  {'fwd':>8}  {'bwd':>8}  {'fwd+bwd':>8}")
    print("  (C1=attribution→bwd, C2=intervention→fwd)")
    print("  " + "-"*46)
    for cat_id in sorted(cats.keys()):
        fwd = results['fwd_only'].get(cat_id, {'total': 0, 'hits': 0})
        bwd = results['bwd_only'].get(cat_id, {'total': 0, 'hits': 0})
        uni = results['union'].get(cat_id, {'total': 0, 'hits': 0})
        
        def pct(r):
            return f"{100*r['hits']//r['total']}%" if r['total'] > 0 else "—"
        
        note = ""
        if cat_id == 11: note = "← attribution"
        elif cat_id == 12: note = "← intervention"
        
        print(f"  {cats[cat_id]:>4}  {'':>14}  {pct(fwd):>8}  {pct(bwd):>8}  {pct(uni):>8}  {note}")

def experiment_5_spectral():
    """Exp 5: Spectral analysis of the production graph."""
    print("\n" + "="*60)
    print("EXPERIMENT 5: Spectral Analysis (Production Graph)")
    print("="*60)
    
    nodes, node_ids, id_to_idx, edges = load_graph(DB_PATH)
    n = len(node_ids)
    print(f"  Graph: {n} nodes, {len(edges)} edges")
    
    S, _, _ = build_adjacency(n, edges, id_to_idx, 'confidence')
    
    # For large graphs, use sparse eigenvalue computation
    if n > 500:
        print(f"  Graph has {n} nodes — computing top-k eigenvalues...")
        from scipy.sparse.linalg import eigs
        from scipy.sparse import csr_matrix
        S_sparse = csr_matrix(S)
        
        try:
            # Top 10 eigenvalues by magnitude
            eigvals, _ = eigs(S_sparse, k=min(10, n-2), which='LM')
            eigvals = sorted(eigvals, key=lambda x: abs(x), reverse=True)
            
            print(f"\n  Top-10 eigenvalues (by magnitude):")
            for i, ev in enumerate(eigvals):
                print(f"    λ_{i+1} = {ev:.6f}  (|λ| = {abs(ev):.6f})")
            
            spectral_radius = abs(eigvals[0])
            print(f"\n  Spectral radius ρ = {spectral_radius:.6f}")
            
            for alpha in [0.5, 0.7, 0.85]:
                rho_alpha = alpha * spectral_radius
                if rho_alpha < 1:
                    iters_needed = int(np.ceil(np.log(1e-6) / np.log(rho_alpha)))
                    print(f"  α={alpha}: ρ(αS)={rho_alpha:.4f} < 1 → converges in ~{iters_needed} iterations")
                else:
                    print(f"  α={alpha}: ρ(αS)={rho_alpha:.4f} ≥ 1 → MAY NOT CONVERGE")
            
            # Spectral gap
            if len(eigvals) >= 2:
                gap = abs(eigvals[0]) - abs(eigvals[1])
                print(f"\n  Spectral gap (|λ1| - |λ2|) = {gap:.6f}")
                if gap > 0.1:
                    print("  → Wide gap: fast convergence (well-connected graph)")
                else:
                    print("  → Narrow gap: slow convergence (graph has bottlenecks)")
        
        except Exception as e:
            print(f"  Eigenvalue computation failed: {e}")
    else:
        print("  Graph too small for spectral analysis")

def main():
    print("╔══════════════════════════════════════════════════════════╗")
    print("║   Causal Memory: PPR Validation Suite                   ║")
    print("╚══════════════════════════════════════════════════════════╝")
    
    start = time.time()
    
    # Exp 1: Truncation error on production graph
    exp1_results = experiment_1_truncation_error()
    
    # Exp 2: Weight modes on CausalEval
    experiment_2_weight_modes()
    
    # Exp 3: Alpha sweep
    experiment_3_alpha_sweep()
    
    # Exp 4: Forward vs backward
    experiment_4_fwd_bwd()
    
    # Exp 5: Spectral analysis
    experiment_5_spectral()
    
    elapsed = time.time() - start
    print(f"\n{'='*60}")
    print(f"All experiments completed in {elapsed:.1f}s")
    print(f"{'='*60}")

if __name__ == "__main__":
    main()
