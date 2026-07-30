# Wang et al. (2024) — A Survey on Large Language Model based Autonomous Agents

## Full Citation

Wang, L., Ma, C., Feng, X., Zhang, Z., Yang, H., Zhang, J., Chen, Z., Tang, J., Chen, X., Lin, Y., Zhao, W. X., Wei, Z., & Wen, J.-R. (2024). A Survey on Large Language Model based Autonomous Agents. *Frontiers of Computer Science*, 18(6), 186345. https://doi.org/10.1007/s11704-024-40231-1

## Abstract

This survey provides a comprehensive overview of LLM-based autonomous agents, covering their architecture (planning, memory, tool use), applications (coding, science, daily life), and evaluation benchmarks. The paper identifies memory as one of the three core components of agent architecture (alongside planning and tool use) and surveys existing memory implementations.

Key memory-related findings:
- Most agent memory systems use **retrieval-augmented generation (RAG)** paradigms
- Memory is typically divided into short-term (context window) and long-term (vector DB)
- **No existing system stores causal relationships as a primary data structure**
- Memory evaluation is underdeveloped — most benchmarks test recall, not causal reasoning

## Methodology

This is a **survey paper**, not an empirical study. The methodology is:

1. **Systematic literature review**: the authors searched arXiv, ACL Anthology, NeurIPS, ICML, and ICLR for papers on LLM agents published 2022–2023.

2. **Taxonomy construction**: papers were categorized by:
   - Agent architecture (single-agent vs. multi-agent)
   - Memory mechanism (short-term, long-term, hybrid)
   - Application domain (coding, science, web, embodied)

3. **Benchmark comparison**: existing evaluation benchmarks were compared on dimensions like task diversity, memory requirements, and causal reasoning demands.

## Key Findings

### 1. Memory is a core component but under-theorized

> "While memory modules are ubiquitous in agent architectures, their design is often ad hoc, lacking a unified theoretical framework." (p. 8)

Most memory systems are engineering solutions (vector DB + prompt injection) without grounding in cognitive science or formal memory theory.

### 2. RAG dominates, but RAG is not enough

The survey identifies three memory paradigms:
- **RAG-based**: retrieve relevant text chunks, inject into context (LangChain, LlamaIndex)
- **Parametric**: fine-tune the LLM to "remember" (expensive, inflexible)
- **Hybrid**: combine RAG with structured storage (generative agents, MemGPT)

None of these paradigms support **causal traversal** ("what caused X?" → "what caused that cause?").

### 3. Evaluation benchmarks do not test causal memory

Existing benchmarks test:
- **Recall**: "Did the agent remember fact X from 10 turns ago?"
- **Consistency**: "Does the agent's answer match its previous statements?"
- **Planning**: "Can the agent decompose a task into steps?"

None test:
- **Causal attribution**: "Why did the agent make decision Y?"
- **Counterfactual reasoning**: "What would have happened if the agent had chosen Z?"
- **Experience transfer**: "Can the agent apply a lesson from task A to task B?"

### 4. Multi-agent memory sharing is unexplored

The survey notes that multi-agent systems (AutoGen, MetaGPT) each maintain separate memory stores. There is no mechanism for:
- Sharing causal lessons across agents
- Resolving conflicting causal beliefs between agents
- Building a collective causal model

## Methodology Critique

| Strength | Limitation |
|---|---|
| Comprehensive coverage of 2022–2023 agent literature | Rapid field evolution means some recent work (2024) is omitted |
| Clear taxonomy helps navigate the space | Taxonomy is somewhat arbitrary; some systems cross categories |
| Identifies memory evaluation as a critical gap | Does not propose solutions for the identified gaps |
| Well-structured and accessible | Some sections are descriptive rather than analytical |

## Connection to `causal-memory`

### 1. Market gap confirmation

This survey is our **primary evidence** that causal memory is a genuine gap, not a feature that existing systems "just haven't gotten around to yet." The authors explicitly state that no surveyed system stores causal relationships as a primary data structure.

This originally justified `causal-memory`'s existence as a complementary layer. As of v0.9+ the positioning has shifted: the gap this survey identified remains the **architectural center** of the system, but no longer its **boundary** — causal-memory is growing into a complete memory system with a causal core, covering factual and temporal memory on the same graph.

### 2. RAG vs. causal graph: architectural distinction

| RAG-based memory | Causal graph memory |
|---|---|
| Stores: text chunks | Stores: decision→outcome edges |
| Retrieves: semantic similarity | Retrieves: graph traversal |
| Query: "Find text like X" | Query: "Find causes of X" |
| Structure: flat (vector space) | Structure: directed graph |
| Compression: vector embedding | Compression: edge abstraction |

`causal-memory` is not "RAG with extra metadata." It is a **different data structure** for a different query type.

### 3. Evaluation gap → our benchmark roadmap

The survey's finding that no benchmark tests causal memory directly shapes our v0.4 roadmap:
- **LongMemEval integration**: test causal recall vs. factual recall
- **Causal reasoning benchmark**: design probe questions that require causal traversal
- **Cross-task transfer benchmark**: measure whether lessons from task A improve performance on task B

### 4. Multi-agent memory sharing (v0.5+)

The survey's observation that multi-agent systems have isolated memory stores maps to our v0.5 roadmap:
- **Causal graph export/import**: agents can share causal subgraphs
- **Collective causal model**: multiple agents contribute to a shared causal graph
- **Conflict resolution**: when agents disagree about causality, the system flags contradictions for human review

## Reading order

Read this to understand **where causal-memory fits in the agent memory landscape**. It confirms that we are not reinventing Mem0 — we are filling a hole that Mem0 (by design) does not address.

---

*Last updated: 2026-07-27*
