# Goyal & Bengio (2022) — Inductive Biases for Deep Learning of Higher-Level Cognition

## Full Citation

Goyal, A., & Bengio, Y. (2022). Inductive Biases for Deep Learning of Higher-Level Cognition. *Proceedings of the Royal Society A*, 478(2266), 20210068. https://doi.org/10.1098/rspa.2021.0068

## Abstract

Current deep learning excels at System 1 cognition (fast, intuitive, pattern-matching) but struggles with System 2 cognition (slow, deliberate, structured reasoning). Goyal and Bengio argue that bridging this gap requires **new inductive biases** — architectural priors that encourage the learning of:

- **Object-centric representations**: entities with persistent identity
- **Relational reasoning**: structured relationships between entities
- **Causal models**: explicit cause-effect representations
- **Systematic generalization**: composing known primitives in novel ways

The paper proposes a research program for building neural architectures that can learn and manipulate these higher-level structures without explicit symbolic programming.

## Methodology

This is a **position paper** with theoretical arguments and illustrative experiments:

1. **Theoretical analysis**: Reviewing why standard neural networks (MLPs, Transformers) fail at System 2 tasks (causal reasoning, planning, counterfactuals).

2. **Inductive bias taxonomy**: Categorizing architectural priors by the cognitive function they support:
   - Attention → selective binding
   - Recurrence → sequential reasoning
   - Graph networks → relational reasoning
   - Causal models → intervention and counterfactual reasoning

3. **Illustrative experiments**: Small-scale demonstrations showing that networks with appropriate inductive biases outperform generic architectures on structured reasoning tasks.

## Key Findings

### 1. System 2 cognition requires explicit structure

> "Higher-level cognition involves manipulating structured representations: objects, relations, variables, and rules. Current neural networks lack the inductive biases to learn these structures from raw data." (p. 3)

This is the central claim. Bengio argues that **end-to-end training on raw tokens/pixels will never yield System 2 cognition** — the inductive biases are too weak. Instead, the architecture must explicitly support:
- Variable binding
- Relational reasoning
- Causal intervention

### 2. Attention is not enough

Transformers use attention for "soft" variable binding, but this is:
- **Shallow**: binding is temporary (within a single forward pass)
- **Implicit**: the model never explicitly represents "Object A has Property P"
- **Non-causal**: attention weights are correlational, not causal

System 2 requires **persistent, explicit, causal** representations.

### 3. Causal models as a target representation

The paper identifies **causal Bayesian networks** as the appropriate target representation for causal reasoning:
- Nodes = variables (objects, states)
- Edges = causal relationships
- Do-calculus = intervention reasoning
- Counterfactuals = hypothetical reasoning

The challenge is to learn these structures from data without human-provided causal graphs.

### 4. The role of inductive biases

Goyal and Bengio distinguish three types of inductive bias:
1. **Architectural**: hard-coded into the network structure (e.g., graph convolutional networks)
2. **Optimization**: encouraged by the training objective (e.g., contrastive learning)
3. **Data**: provided by the training distribution (e.g., curriculum learning)

For causal reasoning, **architectural biases** are most important — the network must be designed to represent and manipulate causal graphs.

## Methodology Critique

| Strength | Limitation |
|---|---|
| Clearly articulates the System 1 / System 2 gap | Position paper, not empirical; claims are not rigorously tested |
| Proposes concrete architectural directions | Some proposals (e.g., causal graph neural networks) are computationally expensive |
| Grounded in cognitive science (Kahneman's dual-process theory) | Does not provide a training recipe for learning causal models from data |
| Influential in shaping the field's research agenda | May underestimate the capabilities of scaled-up Transformers (counter-evidence from GPT-4) |

## Connection to `causal-memory`

### 1. `causal-memory` as an architectural inductive bias

This paper is the **theoretical justification** for externalizing causal structure into an explicit graph rather than hoping the LLM "learns" it from context.

Bengio's argument:
> "If you want causal reasoning, you need explicit causal representations."

Our implementation:
> "Instead of trying to teach the LLM causality, we give it an explicit causal graph to query."

This is not giving up on Bengio's research program — it is **acknowledging the current reality**: LLMs do not have persistent causal representations. `causal-memory` provides the missing substrate.

### 2. The LLM + causal graph architecture

Our architecture maps directly to Goyal & Bengio's proposed System 2 stack:

| Goyal & Bengio component | `causal-memory` equivalent |
|---|---|
| Object-centric representations | `chunks` table (decision chunks, outcome chunks) |
| Relational reasoning | `causal_edges` table (explicit relations) |
| Causal models | Graph traversal (`trace_cause`, `trace_cause_chain`) |
| Systematic generalization | `meta_causal_edges` (cross-task pattern abstraction) |
| Intervention reasoning | Future: `counterfactual_query` tool |

The LLM provides System 1 (pattern matching, language generation). `causal-memory` provides System 2 (structured causal reasoning).

### 3. Why externalization beats end-to-end learning

Bengio argues that causal models should be *learned* by the network. We argue that for LLM agents, causal models should be *externalized* for three reasons:

1. **Persistence**: LLM weights are frozen; causal knowledge must be stored externally to survive model updates
2. **Inspection**: External graphs are auditable; learned causal representations are not
3. **Compositionality**: External graphs can be shared between agents; learned representations cannot

This is a **pragmatic compromise**, not a rejection of Bengio's vision. Future agents may learn causal structures end-to-end. Until then, externalization is the only way to get reliable causal reasoning.

### 4. Graph networks as the retrieval mechanism

The paper advocates for **graph neural networks (GNNs)** as the architectural bias for relational reasoning. Our SQLite-backed causal graph is a lightweight, interpretable approximation:
- GNN: learns vector representations of nodes and edges; supports differentiable reasoning
- `causal-memory`: stores symbolic edges; supports explicit traversal

Future versions could hybridize: store causal edges in SQLite for persistence, but index them with a GNN for fast semantic retrieval.

## Reading order

Read this to understand **why externalizing causal structure is architecturally sound** — not just a workaround for current LLM limitations, but a principled design decision aligned with the field's long-term direction.

---

*Last updated: 2026-07-27*
