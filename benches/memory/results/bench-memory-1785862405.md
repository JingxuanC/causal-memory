# bench-memory results

- model: deepseek-chat
- temperature: 0
- seed: 42
- topk: 5
- protocol: LLM-distilled write path, deterministic retrieval scoring (no judge LLM)
- note: the scenario is reproducible; LLM extraction is NOT (model/version dependent — expect run-to-run variance in the extraction metrics)

## LLM extraction quality (write path)

| metric | score |
|---|---|
| fact/preference recall@5 | 80% (4/5) |
| causal recall@5 | 100% (4/4) |
| relation accuracy | 75% (3/4) |
| decision attachment | 75% (3/4) |
| supersession detected | false — extracted as event/edge, not fact (new value present in 1 chunks) |
| extracted items | 14 (5 facts, 9 edges) · 0 distill failures |

## Read-path mechanics (deterministic)

| metric | score |
|---|---|
| multi-hop chain recall | 100% (2/2) |
| forward-simulation recall@5 | 25% (1/4) |
| trap warning detected | true |
| reversibility (restore) | superseded=true restored=true |

## Efficiency

| metric | value |
|---|---|
| avg context tokens/query | 54 |

## Extraction fidelity (what the LLM wrote)

| kind | text |
|---|---|
| fact | User's deployments use Kubernetes for their infrastructure |
| causal | A SQL injection attempt was blocked last month (around May 2026) |
| lesson | Adding input validation prevents SQL injection attacks (learned from blocking an attempt in May 2026) |
| preference | User switched from almond milk to oat milk in their coffee on 2026-06-24, finding almond milk boring. |
| event | User upgraded the session cache from Redis 7.2.4 to Redis 7.4 on 2026-06-28, resulting in much better hit rates; the old |
| preference | User has been using vim for all their editing lately and finds it fast once you get used to it |
| preference | User prefers dark mode everywhere because light themes hurt their eyes |
| causal | Data was lost during the rollback |
| lesson | Never run a migration without a backup (learned from data loss during rollback on 2026-07-06) |
| causal | The rollback restored everything successfully |
| event | User's team upgraded to PostgreSQL 16 in production on 2026-07-10 |
| causal | Production crash occurred last week (around 2026-07-03), requiring a two-hour rollback |
| lesson | Always run tests before deploying (learned from production crash on 2026-07-03) |
| fact | User's session cache runs on Redis 7.2.4, which has been stable for months as of 2026-07-14 |

## Per-item retrieval tokens

| category | item | est. tokens |
|---|---|---|
| fact | editor → vim | 24 |
| fact | production_db → PostgreSQL 16 | 0 |
| fact | deployment → Kubernetes | 15 |
| fact | theme → dark mode | 18 |
| fact | coffee_milk → oat milk | 26 |
| causal | deployed without running tests → production crash that took two hours to roll back (caused) | 80 |
| causal | added input validation → blocked the SQL injection attempt (prevented) | 84 |
| causal | ran the migration without a backup → data loss during the rollback (caused) | 102 |
| causal | took a nightly snapshot before the migration → the rollback restored everything (prevented) | 135 |
