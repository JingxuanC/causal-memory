# bench-agent results (trap-world ablation)

- model: deepseek-v4-flash
- temperature: 0.3
- seed: 42
- timestamp: 1785663921
- protocol: task texts never mention traps/solutions; first-exposure failure is expected; B's memory persists across all tasks
- note: the scenario is reproducible; LLM behavior is NOT (model/version dependent)

| group | tasks solved | avg steps | first-exposure trap rate | repeat-mistake rate |
|---|---|---|---|---|
