# PersonaMem benchmark — long-horizon memory-system evaluation

> Tencent-style evaluation methodology: **continuous long sessions + dual metrics**
> (accuracy AND token cost), following TencentDB Agent Memory's benchmark design
> (https://github.com/TencentCloud/TencentDB-Agent-Memory) — long-horizon sessions
> are evaluated over accumulated context pressure, not isolated QA turns.

## What it measures

Each sample is a user persona with a full multi-session conversation history
(32k / 128k tokens). Questions are multiple-choice over 7 profile types
(recall facts, preference-aligned recommendations, preference evolution,
new-scenario generalization, new ideas, reasons behind updates).

Two conditions per question:

| condition | setup | token cost |
|---|---|---|
| baseline | agent reads the full (truncated) context and answers | ~context length |
| memory | causal-memory distills the persona profile once (record_fact), then answers by retrieving relevant facts (search_facts) | distill + retrieval, ~1/11 of baseline |

The agent is the **Claude Code CLI** (headless, `claude -p`) with the
**causal-memory MCP server** attached (see `~/.claude.json` ->
`mcpServers.causal-memory`). The eval DB is isolated to a temp file.

## Quick start

```bash
# 1. data
./fetch_data.sh                      # downloads questions_*.csv + shared_contexts_*.jsonl (hf-mirror)

# 2. claude must be authenticated + causal-memory MCP configured
claude -p "hi"                      # sanity check

# 3. run (32k is fast; 128k exercises long-context pressure)
python3 harness.py --version 32k  --personas 3 --per-persona 5 --mode both
python3 harness.py --version 128k --personas 3 --per-persona 5 --mode both
```

Results land in `results/run_<version>_<ts>.json` (accuracy, est. tokens, wall time,
per-type breakdown, per-item detail).

## Protocol notes

- **Persona-level distillation**: each persona's full context is distilled ONCE into
  profile facts (shared across that persona's questions) - mirrors cross-session
  profile accumulation. `record_fact` persists into the eval DB.
- **Token accounting**: `rough_tokens` is a deterministic CJK-aware yardstick
  (relative numbers matter; not a real tokenizer). Baseline tokens = full prompt;
  memory tokens = distill prompt + each answer prompt.
- **GLM context limit**: baseline context is capped (per-message 500 chars) so the
  underlying model can process it - the 128k full-text case times out otherwise.
  The memory condition reads the full persona during distillation (up to 30k chars
  per call; extend for larger coverage).
- The eval MCP DB is isolated via `CAUSAL_MEMORY_DB=/tmp/personamem_eval2/causal.db`
  (the MCP config in `~/.claude.json` must point there during the run).

## Reference results (2026-08-19, GLM-5.2 via Claude CLI, causal-memory MCP)

### 32k (15 questions, 3 personas)

| condition | accuracy | est tokens/question |
|---|---|---|
| baseline | 10/15 = 66.7% | ~88k |
| memory | 6/15 = 40.0% | ~8.1k |

32k fits in context for both conditions; memory underperforms because the
distillation truncated the context (first 30k chars) - see `results/` for detail.

### 128k (15 questions, 3 personas)

| condition | accuracy | est tokens/question | wall time/question |
|---|---|---|---|
| baseline | 11/15 = **73.3%** | **87,880** | 41s |
| memory | 8/15 = **53.3%** | **8,114** | 64s |

At 128k the memory condition uses **~1/11 the tokens** of baseline while reaching
~73% of its accuracy, and answers some questions baseline misses (e.g.
`suggest_new_ideas` 1/3 vs 0/3). The recall gap is the known distillation
coverage limit (only the first 30k chars distilled) - extending distillation
coverage is the next lever.

## Future work

- Full-context persona distillation (chunked) to close the recall gap
- Report real LLM usage tokens instead of the yardstick
- Add a `compact` condition (context-compressed baseline) to mirror Tencent's
  OpenClaw-default comparison
