# 技术与算法全景

> GitHub 原生渲染 Mermaid（无需 HTML）。全部图均可编辑：改本文件后 GitHub 自动重绘。

## 1. 技术栈

```mermaid
mindmap
  root((causal-memory))
    Rust 核心
      零新依赖纪律
      手写连接池
      手写 BM25 倒排
      手写 CJK 分词器
      手写 LRU 缓存
    SQLite v11
      chunks
      causal_edges
      agent_facts
      meta_causal_edges
      cooccurrence_edges
      sessions
      bm25_index
      WAL + busy_timeout
    MCP (rmcp)
      16 个工具
      stdio
      streamable-HTTP
    axum
      HTTP 传输
      AMC 服务端
    嵌入
      fastembed ONNX
      bge-small-en-v1.5 384d
      HTTP 嵌入端点
      entity token 缓存
    LLM (DeepSeek)
      判官: 极性/因果/取代
      蒸馏 V3
      叙事重构
    PyO3
      maturin 绑定
    生态集成
      DSH Cordis 插件
      Claude Code / Cursor
      Docker (AMC)
```

## 2. 分层架构

```mermaid
flowchart TB
  subgraph Entry["入口层"]
    A1[Agent: Claude Code / Cursor / grok-build / HTTP]
    A2[MCP Server — 15 工具]
    A3[CLI: sleep / resolve-updates / ingest]
    A4[Python (PyO3) / DSH 插件 / Benchmarks]
  end
  subgraph Facade["Memory facade — 15+ ops"]
    F1[search_memory 融合检索]
    F2[search_memory_multi_pass 多遍检索]
    F3[trace / intervention / counterfactual / reconstruct]
    F4[record_decision / record_fact / remember]
  end
  subgraph Core["核心引擎"]
    S["Store SQLite v11<br/>WAL · 连接池 · 永不被压缩"]
    H["Hippocampus CausalGraph<br/>扩散激活 + 抑制 + Hebbian + Q-value"]
    R["Retrieval<br/>BM25 · 语义 · hop · RRF · 多遍"]
    C["Consolidation sleep<br/>0→4 阶段 + C7 supersession"]
  end
  subgraph X["横切智能"]
    X1[LLM 判官 + 蒸馏]
    X2[embed 共享 + LRU]
    X3[query_router / patterns / tokenizer]
  end
  A1 --> A2 & A3 & A4
  A2 --> Facade
  A3 --> Facade
  A4 --> Facade
  Facade --> S & H & R
  H --> S
  R --> S
  C --> S
  X1 --> Facade
  X2 --> R
  X3 --> Facade
```

## 3. 检索管线

```mermaid
flowchart LR
  Q[query] --> PD["query decomposition<br/>实体抽取 + 时间锚"]
  PD --> B25["base BM25(query)"]
  PD --> ENT["每实体 BM25(term) ×N"]
  B25 --> MERGE["edge-id 去重合并"]
  ENT --> MERGE
  MERGE --> TOPO{"证据拓扑判定<br/>聚合形态? 时间锚? 跨 session?"}
  TOPO -- 否 --> PLAIN["单遍结果"]
  TOPO -- 是 --> TW["时间窗加权<br/>(不硬过滤)"]
  TW --> EXP["全覆盖 session 展开<br/>(预算 80)"]
  EXP --> VERIFY{"验证回环<br/>how many / list / which"}
  VERIFY -- 是 --> LOOP["列出已找到项 → 提取新实体<br/>→ 补检索 → 有新证据才重答<br/>(≤2 轮)"]
  LOOP --> FUSE["RRF 融合"]
  VERIFY -- 否 --> FUSE
  PLAIN --> FUSE
  FUSE --> ANS["answer LLM + judge"]
```

## 4. 记忆巩固（sleep 周期）

```mermaid
flowchart LR
  subgraph Sleep["sleep consolidation"]
    direction LR
    S0["0 新颖性门控<br/>(diversity < 阈值 → 跳过)"]
    S1["1 重激活<br/>(replay 优先级排序)"]
    S15["1.5 Q-value<br/>(Bellman 强化)"]
    S17["1.7 C7 supersession<br/>(LLM 判官 退休/软标注)"]
    S2["2 泛化<br/>(合并冗余 + 模式挖掘)"]
    S3["3 降权<br/>(半衰期分层 + GC)"]
    S4["4 REM 跨域迁移<br/>(meta 边)"]
    S0 --> S1 --> S15 --> S17 --> S2 --> S3 --> S4
  end
  CMD["CLI: sleep --dry-run<br/>--immutable (VACUUM INTO)<br/>--restore (备份回滚)"] --> Sleep
```

## 5. 知识更新（C7）

```mermaid
flowchart TD
  WRITE["record_decision 再次记录<br/>同一决策文本 + 不同结果"] --> CAND["find_falsified_candidates<br/>(chunk 精确复用配对)"]
  CAND --> JUDGE{"LLM judge_supersession<br/>JSON 裁决"}
  JUDGE -- "supersedes=true" --> ACT{"SupersessionAction"}
  ACT -- "Retire" --> RET["invalidate_edge 硬失效<br/>(检索隐藏, 审计保留)"]
  ACT -- "Annotate" --> ANN["annotate_superseded 软取代<br/>(superseded_by 标注, 全检索可见)"]
  JUDGE -- "keep / judge 失败" --> KEEP["保守保留<br/>(规则回退)"]
  WRITE -. 规则快路 .-> RULE["旧负 → 新正 自动失效"]
  DISTILL["distill 路径 supersedes hint<br/>+ Cancelled/superseded 否定记忆"] --> D7["CausalEval C7 50%→100%<br/>的真来源 (三臂实验归因)"]
```

## 6. 因果能力（Pearl 阶梯）

```mermaid
flowchart TB
  R1["Rung-1 关联<br/>search_causal / search_facts / trace_cause"]
  R2["Rung-2 干预<br/>intervention_query (safe/warning/danger)"]
  R3["Rung-3 反事实<br/>counterfactual_query (经验对比, 非 SCM)"]
  R1 --> R2 --> R3
  H["Hippocampus 扩散激活<br/>caused +1.0 / enabled +0.5 /<br/>prevented −0.3 (GABA 抑制)"]
  R1 -. 驱动 .-> H
  R2 -. 驱动 .-> H
  R3 -. 驱动 .-> H
```

## 7. 评测矩阵

```mermaid
flowchart LR
  CE["CausalEval 140 题<br/>typed DAG 生成 gold<br/>C1-C7 能力测试"] --> R
  LME["LongMemEval 500 题<br/>多会话 57.9% (+25.6pp)"] --> R
  AMC["Agent Memory Challenge<br/>编程场景 #1"] --> R
  OTHER["LoCoMo / PersonaMem / Memora<br/>Tau2 / 压缩存活率"] --> R
  R["能力验证: 因果推理 / 长程记忆 /<br/>文本召回 / 压缩存活"]
```
