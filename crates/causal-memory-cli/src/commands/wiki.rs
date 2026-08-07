//! `wiki` subcommand — export causal memory as an Obsidian markdown vault or
//! standalone interactive HTML graph.
//!
//! ## Obsidian mode (`--format obsidian`)
//!
//! Each decision/outcome chunk becomes a markdown note with YAML frontmatter
//! (confidence, relation, task_tag, timestamp). Causal edges become
//! `[[wikilinks]]` with relation annotations, so Obsidian's built-in graph
//! view renders the full causal topology. Facts go into a `Facts/` folder.
//!
//! ## HTML mode (`--format html`, default)
//!
//! A single self-contained `graph.html` using vis-network (via CDN). Nodes
//! are color-coded by relation type; clicking opens a detail panel.

use std::collections::HashMap;
use std::path::PathBuf;

use causal_memory::store::CausalStore;

use crate::get_db_path;

pub fn run_wiki(args: &[String]) -> anyhow::Result<()> {
    let mut out = PathBuf::from("causal-memory-wiki");
    let mut format = "html".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                i += 1;
                out = PathBuf::from(args.get(i).cloned().unwrap_or_default());
            }
            "--format" => {
                i += 1;
                format = args.get(i).cloned().unwrap_or("html".into());
            }
            other => anyhow::bail!("unknown flag {other:?}"),
        }
        i += 1;
    }

    let db_path = get_db_path();
    let store = CausalStore::open(&db_path)?;

    // Load all valid edges
    let edges = store.search_causal(None, None)?;
    let facts = store.list_facts(None, 500)?;

    println!(
        "Exporting {} edges + {} facts as {format} to {}",
        edges.len(),
        facts.len(),
        out.display()
    );

    match format.as_str() {
        "obsidian" => export_obsidian(&store, &edges, &facts, &out)?,
        "html" => export_html(&store, &edges, &facts, &out)?,
        other => anyhow::bail!("unknown format {other:?}; expected obsidian|html"),
    }

    println!("Done. Output: {}", out.display());
    Ok(())
}

// ─── Obsidian markdown export ──────────────────────────────────────────────

fn slugify(text: &str) -> String {
    // Short slug: first ~60 chars, alphanumeric + spaces → dashes
    let mut s: String = text
        .chars()
        .take(60)
        .map(|c| if c.is_alphanumeric() || c == ' ' { c } else { ' ' })
        .collect::<String>()
        .trim()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase();
    // Truncate to avoid filesystem name limits
    let mut end = 50;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    // Prefix with a short hash for uniqueness
    let hash = fnv1a_short(text);
    format!("{s}-{hash}")
}

fn fnv1a_short(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:08x}")
}

fn export_obsidian(
    _store: &CausalStore,
    edges: &[causal_memory::store::CausalEntry],
    facts: &[causal_memory::store::AgentFact],
    out: &PathBuf,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(out)?;
    std::fs::create_dir_all(out.join("Facts"))?;

    // Collect all unique nodes (by chunk id)
    let mut node_map: HashMap<String, (String, String)> = HashMap::new(); // id → (slug, text)
    for e in edges {
        node_map.entry(e.decision_id.clone()).or_insert_with(|| {
            (
                slugify(&e.decision_text),
                e.decision_text.clone(),
            )
        });
        node_map.entry(e.outcome_id.clone()).or_insert_with(|| {
            (slugify(&e.outcome_text), e.outcome_text.clone())
        });
    }

    // Build adjacency: for each node, what edges connect it?
    let mut outgoing: HashMap<String, Vec<(&causal_memory::store::CausalEntry, bool)>> =
        HashMap::new(); // node_id → [(edge, is_from)]
    for e in edges {
        outgoing
            .entry(e.decision_id.clone())
            .or_default()
            .push((e, true));
        outgoing
            .entry(e.outcome_id.clone())
            .or_default()
            .push((e, false));
    }

    // Write one note per node
    let mut notes_written = 0;
    for (node_id, (slug, text)) in &node_map {
        let mut md = String::new();
        md.push_str(&format!("---\nid: \"{node_id}\"\nslug: \"{slug}\"\n---\n\n"));
        md.push_str(&format!("## {text}\n\n"));

        // Outgoing edges
        let edges_for_node = outgoing.get(node_id).cloned().unwrap_or_default();
        if !edges_for_node.is_empty() {
            md.push_str("### Connections\n\n");
            for (edge, is_from) in &edges_for_node {
                let (target_text, target_slug, arrow) = if *is_from {
                    (
                        &edge.outcome_text,
                        &slugify(&edge.outcome_text),
                        edge.relation.as_str(),
                    )
                } else {
                    (
                        &edge.decision_text,
                        &slugify(&edge.decision_text),
                        edge.relation.as_str(),
                    )
                };
                let icon = match arrow {
                    "caused" => "→",
                    "enabled" => "⇒",
                    "prevented" => "⊣",
                    "no_effect" => "·",
                    _ => "—",
                };
                md.push_str(&format!(
                    "- **{}** {} [[{}|{}]] ({:.0}% confidence)\n",
                    arrow, icon, target_slug, truncate_safe(target_text, 60), edge.confidence * 100.0
                ));
            }
            md.push('\n');
        }

        // Metadata table
        if let Some((edge, _)) = edges_for_node.first() {
            md.push_str("### Details\n\n");
            md.push_str(&format!(
                "| Field | Value |\n|---|---|\n| Task | {} |\n| Relation | {} |\n| Confidence | {:.0}% |\n| Source | {} |\n",
                edge.task_tag.as_deref().unwrap_or("—"),
                edge.relation,
                edge.confidence * 100.0,
                edge.discovered_by,
            ));
        }

        let note_path = out.join(format!("{slug}.md"));
        std::fs::write(&note_path, md)?;
        notes_written += 1;
    }

    // Write facts
    for fact in facts {
        let slug = slugify(&fact.value);
        let mut md = String::new();
        md.push_str(&format!(
            "---\nkey: \"{}\"\nscope: \"{}\"\nconfidence: {:.2}\n---\n\n",
            fact.key, fact.scope, fact.confidence
        ));
        md.push_str(&format!("## {}\n\n", fact.value));
        md.push_str(&format!(
            "- **Category**: {}\n- **Scope**: {}\n- **Source**: {}\n",
            fact.key, fact.scope, fact.source
        ));
        std::fs::write(out.join("Facts").join(format!("{slug}.md")), md)?;
    }

    // Write a README / index
    let readme = format!(
        "# Causal Memory Vault\n\n\
         {} causal edges across {} unique nodes.\n\
         {} facts recorded.\n\n\
         Open this folder in [Obsidian](https://obsidian.md) to see the graph.\n\
         Edge type legend: → caused, ⇒ enabled, ⊣ prevented",
        edges.len(),
        node_map.len(),
        facts.len(),
    );
    std::fs::write(out.join("README.md"), readme)?;

    println!("  {notes_written} notes + {} facts written", facts.len());
    Ok(())
}

// ─── HTML interactive graph ────────────────────────────────────────────────

fn export_html(
    _store: &CausalStore,
    edges: &[causal_memory::store::CausalEntry],
    facts: &[causal_memory::store::AgentFact],
    out: &PathBuf,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(out.parent().unwrap_or(out))?;

    // Collect nodes
    let mut node_ids: HashMap<String, usize> = HashMap::new();
    let mut nodes_json = Vec::new();
    let mut edges_json = Vec::new();

    let relation_color = |r: &str| -> &str {
        match r {
            "caused" => "#e74c3c",
            "enabled" => "#2ecc71",
            "prevented" => "#3498db",
            "no_effect" => "#95a5a6",
            _ => "#f39c12",
        }
    };

    let mut add_node = |id: &str, text: &str, node_ids: &mut HashMap<String, usize>| -> usize {
        if let Some(&idx) = node_ids.get(id) {
            return idx;
        }
        let idx = node_ids.len();
        node_ids.insert(id.to_string(), idx);
        nodes_json.push(format!(
            r#"{{"id":{idx},"label":{},"title":{}}}"#,
            json_string(&truncate_safe(text, 30)),
            json_string(text),
        ));
        idx
    };

    for e in edges {
        let from_idx = add_node(&e.decision_id, &e.decision_text, &mut node_ids);
        let to_idx = add_node(&e.outcome_id, &e.outcome_text, &mut node_ids);
        let color = relation_color(&e.relation);
        let dashes = e.relation == "prevented";
        edges_json.push(format!(
            r#"{{"from":{from_idx},"to":{to_idx},"color":{{"color":"{color}"}},"label":{},"dashes":{dashes},"title":{}}}"#,
            json_string(&e.relation),
            json_string(&format!(
                "{} ({:.0}%)",
                e.relation,
                e.confidence * 100.0
            )),
        ));
    }

    // Add fact nodes (different shape)
    let mut fact_nodes_json = Vec::new();
    for fact in facts {
        let idx = nodes_json.len() + fact_nodes_json.len();
        let bg = "#fef9e7";
        let border = "#f39c12";
        fact_nodes_json.push(format!(
            "{{\"id\":{idx},\"label\":{},\"title\":{},\"shape\":\"box\",\"color\":{{\"background\":\"{bg}\",\"border\":\"{border}\"}}}}",
            json_string(&truncate_safe(&fact.value, 25)),
            json_string(&format!("FACT: {}", fact.value)),
        ));
    }

    let html = format!(
        r##"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>Causal Memory Graph</title>
<script src="https://unpkg.com/vis-network/standalone/umd/vis-network.min.js"></script>
<style>
body {{ margin:0; padding:0; font-family:-apple-system,sans-serif; }}
#network {{ width:100%; height:100vh; }}
#sidebar {{
  position:fixed; right:0; top:0; width:350px; height:100vh;
  background:#fff; border-left:1px solid #ddd; padding:20px;
  overflow-y:auto; display:none; box-shadow:-2px 0 8px rgba(0,0,0,.1);
}}
#sidebar h3 {{ margin-top:0; }}
.legend {{
  position:fixed; bottom:20px; left:20px; background:rgba(255,255,255,.9);
  padding:12px 16px; border-radius:8px; font-size:13px;
  box-shadow:0 2px 8px rgba(0,0,0,.1); z-index:1000;
}}
.legend-item {{ display:flex; align-items:center; gap:6px; margin:4px 0; }}
.legend-dot {{ width:16px; height:3px; border-radius:2px; }}
.stat {{ position:fixed; top:20px; left:20px; font-size:13px; color:#666; z-index:1000; }}
</style>
</head>
<body>
<div class="stat">{edges_count} edges · {nodes_count} nodes · {facts_count} facts</div>
<div class="legend">
  <div class="legend-item"><div class="legend-dot" style="background:#e74c3c"></div> caused</div>
  <div class="legend-item"><div class="legend-dot" style="background:#2ecc71"></div> enabled</div>
  <div class="legend-item"><div class="legend-dot" style="background:#3498db;border-top:2px dashed #3498db"></div> prevented</div>
  <div class="legend-item"><div class="legend-dot" style="background:#95a5a6"></div> no_effect</div>
  <div class="legend-item"><div class="legend-dot" style="background:#f39c12;width:12px;height:12px;border-radius:2px"></div> fact</div>
</div>
<div id="network"></div>
<div id="sidebar"><h3 id="sb-title"></h3><div id="sb-content"></div></div>
<script>
var nodes = new vis.DataSet([{nodes_data}{facts_data}]);
var edges_data = new vis.DataSet([{edges_data}]);
var container = document.getElementById('network');
var data = {{ nodes: nodes, edges: edges_data }};
var options = {{
  nodes: {{ shape: 'dot', size: 16, font: {{ size: 12 }} }},
  edges: {{ font: {{ size: 10, strokeWidth: 0, background: 'rgba(255,255,255,.7)' }}, arrows: {{ to: {{ enabled: true, scaleFactor: 0.5 }} }}, smooth: {{ type: 'continuous' }} }},
  physics: {{ stabilization: {{ iterations: 200 }}, barnesHut: {{ gravitationalConstant: -3000 }} }},
  interaction: {{ hover: true, tooltipDelay: 200 }}
}};
var network = new vis.Network(container, data, options);
network.on('click', function(params) {{
  if (params.nodes.length > 0) {{
    var node = nodes.get(params.nodes[0]);
    document.getElementById('sb-title').textContent = node.title || node.label;
    document.getElementById('sb-content').textContent = '';
    document.getElementById('sidebar').style.display = 'block';
  }} else {{
    document.getElementById('sidebar').style.display = 'none';
  }}
}});
</script>
</body>
</html>"##,
        edges_count = edges.len(),
        nodes_count = node_ids.len(),
        facts_count = facts.len(),
        nodes_data = nodes_json.join(","),
        facts_data = if fact_nodes_json.is_empty() { String::new() } else { format!(",{}", fact_nodes_json.join(",")) },
        edges_data = edges_json.join(","),
    );

    let html_path = if out.is_dir() || out.extension().is_none() {
        out.join("graph.html")
    } else {
        out.clone()
    };
    std::fs::write(&html_path, html)?;
    println!("  HTML graph: {}", html_path.display());
    Ok(())
}

// ─── Helpers ───────────────────────────────────────────────────────────────

fn truncate_safe(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Walk char boundaries to avoid splitting a multibyte char
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}
