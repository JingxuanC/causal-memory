# bench-agent results (trap-world ablation)

- model: deepseek-v4-flash
- temperature: 0.3
- seed: 42
- timestamp: 1785663975
- protocol: task texts never mention traps/solutions; first-exposure failure is expected; B's memory persists across all tasks
- note: the scenario is reproducible; LLM behavior is NOT (model/version dependent)

| group | tasks solved | avg steps | first-exposure trap rate | repeat-mistake rate |
|---|---|---|---|---|
| A (no memory) | 5/8 | 14.4 | 0% (0/3) | 0% (0/5) |

A failure modes: 3 unsolved (step budget exhausted) · 58 invalid-action steps · 0 LLM call failures (after retries)
| B (causal memory) | 4/8 | 14.5 | 0% (0/3) | 0% (0/5) |

B failure modes: 4 unsolved (step budget exhausted) · 67 invalid-action steps · 0 LLM call failures (after retries)

B extras: memory writes 0 · searches 8 · post-search first-action hit rate 0% (0/8)
