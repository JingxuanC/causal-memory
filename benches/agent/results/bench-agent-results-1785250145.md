# bench-agent results (trap-world ablation)

- model: glm-4-plus
- temperature: 0.3
- seed: 42
- timestamp: 1785250145
- protocol: task texts never mention traps/solutions; first-exposure failure is expected; B's memory persists across all tasks
- note: the scenario is reproducible; LLM behavior is NOT (model/version dependent)

| group | tasks solved | avg steps | first-exposure trap rate | repeat-mistake rate |
|---|---|---|---|---|
| A (no memory) | 6/6 | 2.7 | 67% (2/3) | 67% (2/3) |

A failure modes: 0 unsolved (step budget exhausted) · 0 invalid-action steps · 0 LLM call failures (after retries)
| B (causal memory) | 6/6 | 4.0 | 67% (2/3) | 33% (1/3) |

B failure modes: 0 unsolved (step budget exhausted) · 0 invalid-action steps · 0 LLM call failures (after retries)

B extras: memory writes 2 · searches 7 · post-search first-action hit rate 57% (4/7)
