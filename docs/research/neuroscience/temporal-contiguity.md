# Davachi (2006) — Temporal Contiguity and Episodic Encoding

## Full Citation

Davachi, L. (2006). Item, Context and Relational Episodic Encoding in Humans. *Current Opinion in Neurobiology*, 16(6), 693–700. https://doi.org/10.1016/j.conb.2006.10.012

## Abstract

Episodic memory depends on the binding of items to their spatiotemporal context. Davachi reviews neuroimaging evidence showing that the hippocampus and prefrontal cortex differentially support three encoding processes:

1. **Item encoding**: What happened? (medial temporal lobe)
2. **Context encoding**: Where/when did it happen? (hippocampus, parahippocampal cortex)
3. **Relational encoding**: How are items related? (hippocampus, prefrontal cortex)

The paper emphasizes that **temporal contiguity** (events that occur close in time) is the brain's default heuristic for inferring causal relationships — even when no true causal link exists.

## Methodology

Review paper synthesizing:
- Human fMRI studies of encoding (subsequent memory paradigm)
- Patient studies (hippocampal damage impairs temporal order memory)
- Rodent place cell / time cell recordings

The key methodological advance reviewed is the **subsequent memory paradigm**: neural activity during encoding is compared between items that are later remembered vs. forgotten. This isolates the neural signatures of successful episodic encoding.

## Key Findings

### 1. The brain uses temporal contiguity as a causal proxy

> "Temporal proximity serves as a powerful cue for causal inference, even in the absence of mechanistic understanding." (p. 696)

This is a **heuristic**, not a logical rule. The brain's default assumption is:
- A happened before B → A might have caused B
- This is fast and cheap but often wrong

### 2. Theta oscillations segment continuous experience into discrete events

The hippocampal theta rhythm (4–8 Hz) acts as a **temporal sampling rate**. Each theta cycle is a potential "event frame." Events that fall within the same theta cycle are more likely to be bound together in memory.

### 3. Context and item encoding are dissociable but interdependent

- Hippocampal damage → impaired context binding but spared item recognition
- Prefrontal damage → impaired relational binding but spared item/context memory

This suggests a **hierarchical binding architecture**: items → contexts → relations.

## Methodology Critique

| Strength | Limitation |
|---|---|
| Integrates human neuroimaging, patient data, and rodent physiology | Mostly correlational; causality inferred from lesion studies, not manipulation |
| Clear functional dissociation between item/context/relational encoding | Real-world episodic memory is rarely purely item, context, or relational |
| Temporal contiguity heuristic is well-supported | Does not specify how the brain corrects for spurious temporal correlations |

## Connection to `causal-memory`

### 1. Confidence levels encode the temporal→causal hierarchy

Our `confidence_source` levels directly map to the strength of causal evidence:

| Source | Confidence | Biological equivalent | Why |
|---|---|---|---|
| `temporal` | 0.4 | Temporal contiguity heuristic | Weak — just happened in sequence |
| `rule` | 0.7 | Learned causal schema | Medium — matches known pattern |
| `llm_inferred` | 0.6 | Analogy / generalization | Medium — model judges similarity |
| `user_feedback` | 0.95 | Direct intervention confirmation | Strong — human verified cause→effect |

This prevents the system from over-weighting spurious temporal correlations, which is the brain's default (and error-prone) strategy.

### 2. `discovered_at` timestamp as theta-like segmentation

Each `causal_edge` has a `discovered_at` timestamp. Future versions could use this for **temporal clustering**: decisions made within the same "session" (same theta-like window) are more likely to be causally related.

### 3. The "binding" metaphor justifies the schema design

The paper's three-level hierarchy (item → context → relation) justifies our schema:
- **Items** = `chunks` table (decision text, outcome text)
- **Context** = `task_tag`, `discovered_at` (when/where)
- **Relation** = `causal_edges` (how they relate)

This is not accidental — it's a direct architectural translation of the binding hierarchy Davachi describes.

## Reading order

Read this to understand **why confidence levels are necessary**. The brain's default (temporal contiguity) is too weak for reliable causal inference. Our confidence system is the correction mechanism.

---

*Last updated: 2026-07-27*
