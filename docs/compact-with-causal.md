# compact_with_causal — 压缩协同工作流（CE-3）

> 定位：**context rot 时代，因果边是压缩摘要的保留选择器。**
> 压缩摘要每压一次就损一次（LoCoMo k=5 实测：text-only 65.0% → 44.5%，
> 见 [`benchmarks/locomo.md`](benchmarks/locomo.md#compaction-survival-run-20260727_174000-k--5)）；
> 而因果边存在**上下文窗口之外**，压缩碰不到它——前提是在压缩发生**之前**把它记下来。

## 工作原理

```
transcript (将被压缩的对话原文)
    │  compact-ingest（确定性抽取：相邻 turn 配对 decision→outcome）
    ▼
causal store (SQLite, 上下文窗口之外)     ← 压缩无法触及
    │
    ├── causal_pin  →  CAUSAL_MEMORY.md (L0 钉入, CE-2)
    └── trace_cause / search_causal / intervention_query (MCP 工具, 按需)
```

每次压缩发生时，被压缩掉的原文就永远消失了。`compact-ingest` 在压缩发生前
（或 Claude Code PostCompact hook 触发时——转录文件里仍保留着压缩前的完整
对话）把原文的 turn 结构回填进因果库。回填是**幂等**的（内容寻址 chunk id +
边存在性检查），hook 每次压缩后触发也不会产生重复。

## 用法

```bash
# 手动：回填一个 Claude Code 转录
causal-memory compact-ingest ~/.claude/projects/<proj>/<session>.jsonl --pin

# 或从 stdin（配合 hook）
echo "$TRANSCRIPT_PATH" | xargs causal-memory compact-ingest --pin

# 选项
#   --task-tag TAG   给边打任务标签（检索过滤用）
#   --db PATH        指定库文件（默认 $CAUSAL_MEMORY_DB 或 ~/.local/share/causal-memory/causal.db）
#   --pin            回填后顺手刷新 L0 钉入文件（CAUSAL_MEMORY.md）
#   --stdin          从标准输入读转录内容
```

输出示例：

```
✅ compact-ingest: 128 turns → 127 causal edge(s) recorded, 0 already present (skipped).
   These edges live OUTSIDE the context window — compaction cannot touch them. ...
📌 refreshed ~/.local/share/causal-memory/CAUSAL_MEMORY.md (23 lines).
```

## 接线 Claude Code PostCompact hook

Claude Code 的 `PostCompact` hook 在每次压缩完成后触发，stdin 收到一个 JSON，
其中 `transcript_path` 指向完整转录（压缩前的对话仍在其中）。在
`~/.claude/settings.json`（或项目 `.claude/settings.json`）中加：

```json
{
  "hooks": {
    "PostCompact": [
      {
        "matcher": ".*",
        "hooks": [
          {
            "type": "command",
            "command": "TRANSCRIPT=$(jq -r '.transcript_path' 2>/dev/null); [ -n \"$TRANSCRIPT\" ] && /path/to/causal-memory compact-ingest \"$TRANSCRIPT\" --pin || true"
          }
        ]
      }
    ]
  }
}
```

没有 `jq` 的话，用一个小的 wrapper 脚本：

```bash
#!/bin/sh
# Claude Code PostCompact hook: backfill the pre-compaction transcript into
# the causal store, then refresh the L0 pinned directory.
INPUT=$(cat)
TRANSCRIPT=$(printf '%s' "$INPUT" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("transcript_path",""))')
[ -n "$TRANSCRIPT" ] && /path/to/causal-memory compact-ingest "$TRANSCRIPT" --pin
exit 0   # never block compaction on a memory backfill failure
```

要点：

- **幂等**让重复触发安全——每次压缩后都跑一遍，已回填的边自动跳过。
- hook 必须**永远退出 0**：记忆回填失败不该阻塞 agent 的压缩流程。
- `--pin` 把新边同步进 `CAUSAL_MEMORY.md`，agent 下次启动的 L0 目录就是新的。

## 设计取舍（与 LoCoMo 压缩存活实验一致）

| 决策 | 选择 | 理由 |
|---|---|---|
| 抽取方式 | 相邻 turn 配对（decision = turn i, outcome = turn i+1），确定性、无 LLM | 与 bench 条件 B 同规则，离线可用、零成本、可复现；更聪明的抽取器（`extract` / `distill` 的 LLM 抽取）可在不改 ingest 契约的前提下叠加 |
| 边元数据 | relation=`caused`, confidence=0.4, discovered_by=`compact-ingest` | 与 LoCoMo 条件 B 完全相同——bench 测的正是产品跑的 |
| 幂等 | chunk id = `c{d\|o}` + SHA-256(文本)前 16 字节；边存在即跳过 | PostCompact 每次压缩都触发，重复回填不能复制数据 |
| 文本上限 | 每侧 2000 字符 | 转录里有大段工具输出；因果库要的是决策文本不是 payload |
| 只收 text block | tool_use / tool_result / thinking 不进边文本 | 工具 payload 不是 decision/outcome 散文 |

## 局限与后续

- 相邻配对是「记忆 tax 换 survival」的保守版：它保证了 turn 原文逐字存活，
  但不理解哪条 turn 是真的决策。压缩存活 bench 中 cat 3（open-domain）的
  回退（−4.2pp）正是这个原因——**真正的决策抽取器**（`extract` / `distill`
  的 rule+LLM 判定）接到这个工作流里是明确的下一步。
- 回填边默认 confidence 0.4（temporal 级），检索排序时自然沉底，
  高置信的手动记录不受影响。
