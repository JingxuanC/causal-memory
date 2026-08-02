# bench-agent results (trap-world ablation)

- model: deepseek-chat
- temperature: 0.3
- seed: 42
- timestamp: 1785660552
- protocol: task texts never mention traps/solutions; first-exposure failure is expected; B's memory persists across all tasks
- note: the scenario is reproducible; LLM behavior is NOT (model/version dependent)

| group | tasks solved | avg steps | first-exposure trap rate | repeat-mistake rate |
|---|---|---|---|---|
| A (no memory) | 4/8 | 12.2 | 33% (1/3) | 20% (1/5) |

A failure modes: 4 unsolved (step budget exhausted) · 0 invalid-action steps · 0 LLM call failures (after retries)
| B (causal memory) | 7/8 | 8.6 | 33% (1/3) | 20% (1/5) |

B failure modes: 1 unsolved (step budget exhausted) · 0 invalid-action steps · 0 LLM call failures (after retries)

B extras: memory writes 0 · searches 8 · post-search first-action hit rate 0% (0/8)
