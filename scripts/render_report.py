#!/usr/bin/env python3
"""V1 Phase 2: self-contained static HTML report for benchmark results.

Reads LoCoMo and/or LongMemEval summary JSON + rows JSONL, produces a
single report.html with three views:
  1. Per-category bar chart (overall + breakdown)
  2. Run comparison table (multiple runs side by side)
  3. Per-question inspector (filter by category / correctness, expand to see
     question / gold / predicted / judge reason)

Usage:
  python3 scripts/render_report.py --locomo results/e1_v2_full/ --out report.html
  python3 scripts/render_report.py --lme results/p8_multisession_final/ --out report.html
  python3 scripts/render_report.py --locomo results/e1_v2_full/ --lme results/p8_multisession_final/ --out report.html

Zero dependencies — standard library only, inline JS/CSS.
"""

import argparse
import glob
import html
import json
import os
import sys
from pathlib import Path


def load_locomo_run(run_dir: str):
    """Load the latest summary + rows from a LoCoMo run directory."""
    summaries = sorted(glob.glob(os.path.join(run_dir, "*_summary.json")))
    if not summaries:
        return None
    summary = json.load(open(summaries[-1]))
    rows_path = summaries[-1].replace("_summary.json", ".jsonl")
    rows = []
    if os.path.exists(rows_path):
        for line in open(rows_path):
            line = line.strip()
            if line:
                rows.append(json.loads(line))
    return {"summary": summary, "rows": rows, "source": summaries[-1]}


def load_lme_run(run_dir: str):
    """Load the latest summary + rows from a LongMemEval run directory."""
    summaries = sorted(glob.glob(os.path.join(run_dir, "*_summary.json")))
    if not summaries:
        return None
    summary = json.load(open(summaries[-1]))
    rows_path = summaries[-1].replace("_summary.json", ".jsonl")
    rows = []
    if os.path.exists(rows_path):
        for line in open(rows_path):
            line = line.strip()
            if line:
                rows.append(json.loads(line))
    return {"summary": summary, "rows": rows, "source": summaries[-1]}


def esc(s):
    """HTML-escape a string."""
    return html.escape(str(s) if s is not None else "")


def render_category_bars(run_data, run_name):
    """Render per-category accuracy as CSS bar chart."""
    summary = run_data["summary"]
    per_cat = summary.get("per_category", {})
    if not per_cat:
        # LME format: per_question_type
        per_cat = summary.get("per_question_type", {})
        if per_cat:
            per_cat = {f"qt:{k}": v for k, v in per_cat.items()}

    bars = []
    for cat, stats in sorted(per_cat.items()):
        total = stats.get("total", stats.get("total_questions", 0))
        correct = stats.get("correct", 0)
        acc = (correct / total * 100) if total > 0 else 0
        color = "#22c55e" if acc >= 75 else "#eab308" if acc >= 50 else "#ef4444"
        bars.append(f"""
        <div class="bar-row">
          <span class="bar-label">{esc(cat)}</span>
          <div class="bar-track">
            <div class="bar-fill" style="width:{acc:.1f}%; background:{color}"></div>
            <span class="bar-text">{acc:.1f}% ({correct}/{total})</span>
          </div>
        </div>""")

    overall = summary.get("correct", 0)
    total_q = summary.get("total_questions", summary.get("total", 0))
    overall_acc = (overall / total_q * 100) if total_q > 0 else 0

    return f"""
    <div class="run-section">
      <h3>{esc(run_name)}</h3>
      <p class="overall">Overall: <strong>{overall_acc:.1f}%</strong> ({overall}/{total_q})</p>
      <p class="meta">prompt: {esc(summary.get('prompt_version','v1'))} · judge: {esc(summary.get('judge_style','strict'))} · ingest: {esc(summary.get('ingest','raw'))}</p>
      {''.join(bars)}
    </div>"""


def render_inspector(rows, run_name):
    """Render per-question inspector with expandable details."""
    if not rows:
        return "<p>(no row data available)</p>"

    items = []
    for i, r in enumerate(rows):
        cat = r.get("category", r.get("question_type", "?"))
        verdict = r.get("verdict", "?")
        correct = "✅" if verdict == "correct" else "❌" if verdict == "incorrect" else "⚠️"
        q = esc(r.get("question", "")[:80])
        gold = esc(str(r.get("gold", ""))[:80])
        pred = esc(r.get("predicted", "")[:80])
        reason = esc(r.get("judge_reason", "")[:200])
        full_q = esc(r.get("question", ""))
        full_pred = esc(r.get("predicted", ""))
        items.append(f"""
        <details class="q-row {'correct' if verdict=='correct' else 'wrong'}">
          <summary><span class="v">{correct}</span> <span class="cat">[{esc(cat)}]</span> {q}</summary>
          <div class="q-detail">
            <p><strong>Question:</strong> {full_q}</p>
            <p><strong>Gold:</strong> {gold}</p>
            <p><strong>Predicted:</strong> {full_pred}</p>
            <p><strong>Judge:</strong> {reason}</p>
          </div>
        </details>""")

    return f"""
    <div class="inspector">
      <h3>{esc(run_name)} — Per-Question Inspector ({len(rows)} questions)</h3>
      <div class="filter-bar">
        <label><input type="checkbox" id="show-correct" checked> Correct</label>
        <label><input type="checkbox" id="show-wrong" checked> Wrong</label>
      </div>
      {''.join(items)}
    </div>"""


def render_html(runs, out_path):
    """Assemble full HTML report."""
    cat_sections = []
    inspector_sections = []
    for name, data in runs:
        if data:
            cat_sections.append(render_category_bars(data, name))
            inspector_sections.append(render_inspector(data["rows"], name))

    html_doc = f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>causal-memory Benchmark Report</title>
<style>
  body {{ font-family: -apple-system, system-ui, sans-serif; margin: 2rem; max-width: 960px; color: #1a1a1a; }}
  h1 {{ border-bottom: 2px solid #333; padding-bottom: .5rem; }}
  h2 {{ margin-top: 2rem; color: #555; }}
  .run-section {{ margin-bottom: 2rem; padding: 1rem; background: #f8f9fa; border-radius: 8px; }}
  .overall {{ font-size: 1.2rem; }}
  .meta {{ color: #666; font-size: 0.85rem; }}
  .bar-row {{ display: flex; align-items: center; margin: 4px 0; }}
  .bar-label {{ width: 80px; font-size: 0.85rem; color: #555; }}
  .bar-track {{ flex: 1; position: relative; height: 24px; background: #e5e7eb; border-radius: 4px; overflow: hidden; }}
  .bar-fill {{ height: 100%; border-radius: 4px; transition: width 0.3s; }}
  .bar-text {{ position: absolute; right: 8px; top: 3px; font-size: 0.75rem; color: #333; font-weight: 600; }}
  .inspector {{ margin-top: 2rem; }}
  .q-row {{ margin: 2px 0; padding: 4px 8px; border-radius: 4px; cursor: pointer; }}
  .q-row.correct {{ background: #f0fdf4; }}
  .q-row.wrong {{ background: #fef2f2; }}
  .q-row summary {{ font-size: 0.85rem; list-style: none; }}
  .q-row summary::-webkit-details-marker {{ display: none; }}
  .q-detail {{ padding: 8px 16px; font-size: 0.8rem; color: #444; }}
  .q-detail p {{ margin: 4px 0; }}
  .v {{ display: inline-block; width: 1.5em; }}
  .cat {{ color: #888; font-size: 0.8rem; }}
  .filter-bar {{ margin-bottom: 1rem; }}
  .filter-bar label {{ margin-right: 1rem; font-size: 0.85rem; }}
</style>
</head>
<body>
<h1>causal-memory Benchmark Report</h1>
<p>Generated {json.dumps(json.loads('{"ts":"auto"}')["ts"])} · git: {esc(runs[0][1]['summary'].get('git_commit','?') if runs and runs[0][1] else '?')}</p>

<h2>Per-Category Accuracy</h2>
{''.join(cat_sections)}

<h2>Per-Question Inspector</h2>
{''.join(inspector_sections)}

<script>
// Filter correct/wrong rows
document.getElementById('show-correct')?.addEventListener('change', e => {{
  document.querySelectorAll('.q-row.correct').forEach(r => r.style.display = e.target.checked ? '' : 'none');
}});
document.getElementById('show-wrong')?.addEventListener('change', e => {{
  document.querySelectorAll('.q-row.wrong').forEach(r => r.style.display = e.target.checked ? '' : 'none');
}});
</script>
</body>
</html>"""

    Path(out_path).write_text(html_doc)
    print(f"wrote {out_path} ({len(html_doc)} bytes)")


def main():
    parser = argparse.ArgumentParser(description="Render benchmark report HTML")
    parser.add_argument("--locomo", help="LoCoMo results directory")
    parser.add_argument("--lme", help="LongMemEval results directory")
    parser.add_argument("--out", default="report.html", help="Output HTML path")
    args = parser.parse_args()

    runs = []
    if args.locomo:
        data = load_locomo_run(args.locomo)
        if data:
            runs.append(("LoCoMo", data))
        else:
            print(f"warning: no summary found in {args.locomo}", file=sys.stderr)
    if args.lme:
        data = load_lme_run(args.lme)
        if data:
            runs.append(("LongMemEval", data))
        else:
            print(f"warning: no summary found in {args.lme}", file=sys.stderr)

    if not runs:
        print("error: no runs loaded. Provide --locomo and/or --lme", file=sys.stderr)
        sys.exit(1)

    render_html(runs, args.out)


if __name__ == "__main__":
    main()
