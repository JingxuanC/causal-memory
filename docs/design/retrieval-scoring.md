# 检索打分双引擎：BM25 与向量化（Retrieval Scoring）

> 状态：已落地（2026-08-26 复盘版）。本文回答“BM25 是什么、向量化是什么、在我们系统里各自怎么实现、
> 用在哪、什么时候谁说了算”。配套阅读：query-path.md（查询链路）、write-path.md（写入链路）。

## 0. 一句话

检索层有两个打分引擎，互补而不是替代：**BM25 按词面匹配排序**（稀有词×出现密度÷文档长度，
内存计算、零成本、可解释），**向量化按语义距离排序**（神经网络把文本映射到高维空间，余弦最近邻，
改述也能命中）。两者各自产出种子，合并去重后交给图引擎做激活扩散排序。

## 1. BM25：词面打分（crates/causal-memory/src/store/retrieve/bm25.rs + bm25.rs）

### 1.1 公式（三个直觉）

```
score(d) = Σ_w IDF(w) × TF饱和(w,d) × 长度归一化(d)
```

- **IDF（词稀有度）**：log(1 + (N−df)/(df+0.5))。507 个候选文档里 “plants” 只出现在 20 个
  → 稀有 → 命中值钱；“user” 出现在 400 个 → IDF≈0 → 几乎不加分。
- **TF 饱和（边际递减）**：出现 1 次 vs 0 次天壤之别，5 次 vs 4 次差别很小（k1≈1.2 防刷词频）。
- **长度归一化（b≈0.75）**：同样命中一次，30 词的文档得分远高于 319 词的——假设是
  “短而密 = 专讲此事”。**这个假设在蒸馏概括句场景会反噬（见 §4）**。

### 1.2 我们的实现

```
写入：record_* → index_chunk() → bm25_index 表（token, chunk_id 倒排，SQLite 持久化）
查询：search_causal_bm25(task_tag, query, topk)
        → 从 bm25_index 拿候选 chunk（无索引/候选<3 或 >900 时回退全表扫描）
        → 内存 Bm25Index::build 对候选逐个打分
        → 排序截断 topk，task_tag 严格隔离（题库间不串）
```

- tokenizer：ASCII 词 + CJK 二元组，24 个停用词（a/an/the/to/and/...）。
- **索引覆盖缺口（欠账）**：bench 的裸 INSERT 路径不建索引（实测 bm25_index 每 chunk 0 行），
  全靠 recall-guard 回退全扫——正确但每查询多付一次全表扫描。ingest_question 应改走 store API。

## 2. 向量化：语义打分（crates/causal-memory/src/embed.rs，516 行）

### 2.1 原理

embedding 模型（神经网络）把文本映射成定长向量（如 384 维），训练目标=意思近的句子向量近。
检索=查询向量与所有文档向量算**余弦相似度**取 topk。“买车”和“提了辆电动车”零共同词
但语义近——这是 BM25 的盲区，向量化补上。代价：黑盒、精确词（型号/日期）可能被“糊”掉、
每文档要过一次模型。

### 2.2 我们的三后端降级链（init_embedder，优先级从高到低）

| 后端 | 条件 | 说明 |
|---|---|---|
| HTTP | 设了 CAUSAL_MEMORY_EMBED_API/KEY | OpenAI 兼容 /v1/embeddings，默认 text-embedding-3-small |
| 本地 ONNX | 编译 --features local-embed 且无 HTTP 配置 | fastembed-rs，BGE 系列，进程内 CPU 推理，零 API 费用 |
| None | 都没有 | 语义路静默跳过，100% BM25（当前 bench 就是此状态） |

本地 ONNX 细节（代码已写好，环境待配，见 §5 代做项）：模型枚举含 BGE-small/base/large-en、
BGE-small/large-zh、MiniLM、multilingual-e5-small；FASTEMBED_CACHE_DIR 目录存在才允许首次下载
（防 HF 被墙卡 150s）；ORT_DYLIB_PATH 指向系统 onnxruntime dylib（ort-load-dynamic，
绕开 macOS CLT 的 CoreML/静态链接问题）。

### 2.3 存储与检索

```
写入：record_decision → embed_shared(decision+outcome) → put_embedding → edge_embeddings 表（blob）
      record_fact     → embed_shared(key+value)     → put_fact_embedding
查询：search_causal_semantic(query_vec, task_tag, limit)
        SQL join edge_embeddings → Rust 逐条 cosine_similarity → 排序截断
```

**余弦暴力扫描，无 ANN 索引**——单题 507 边毫秒级；全库 12 万边=12 万次余弦，是规模欠账
（量级上来要上 sqlite-vec 或 hnsw）。工程细节：embed_shared 全局单例 + LRU 缓存（lru crate），
8s 超时（record 路径同步调用不能卡 MCP），entity_boosted 变体=余弦打底+查询实体共享加权
（补专有名词被语义糊掉的短板）。

## 3. 谁在哪儿用（调用点全清单）

| 调用点 | BM25 | 向量化 |
|---|---|---|
| unified 引擎播种（unified.rs:78）| ✓ bm25_seed_ids（双命名空间）| ✓ facts+causal 语义种子 |
| dual-pool RRF fallback（ops.rs:675）| ✓ 语义空时降级承接 | ✓ 优先层 |
| search_facts（ops.rs:470）| ✓ 降级承接 | ✓ 短路优先 |
| intervention_query（ops.rs:1282）| ✓ 第三兜底 | ✓ 唯一种子源（无语义直接 return）|
| 写入副作用（ops.rs:85/429）| — | ✓ 顺手向量化入库 |
| bench（longmemeval）| ✓ 唯一检索路 | ✗ 未配置 embedder |

## 4. 已知问题：BM25 长度归一化 vs 蒸馏概括句（2026-08-26 实测）

distill 模式下检索被蒸馏 episode 污染，实测根因链：

```
蒸馏 episode：30 tokens 概括句（“User got a snake plant last month”）
原始 turn：  176-394 tokens 对话（含 bullet-list 长回复）
→ 同命中 plants/last/month 时，长度归一化给概括句 ~10.25 分 vs 原始 turn 9.13
→ topk=10 被 episode 占满，原始证据被挤出 → evidence_hit 从 85% 掉到 21%
→ 且概括句措辞天然对齐问题句式（“acquire...last month” vs “got...last month”）
```

三个放大因素：① BM25 长度归一化偏爱短密文档；② 蒸馏产出问题式措辞；③ 同一事件两个版本
（原始+概括）在同一池抢名额而 evidence 判定只认原始 id。**修复方向：分层配额（原始 turn 与
episode 分池，如 7:3）+ evidence 判定把 episode 覆盖 answer session 记为 hit。**

## 5. 代做项（TODO，2026-08-26 记录）

1. **本地 ONNX embedding 环境搭建**（代码就绪，环境未配）：
   - [ ] brew 装 onnxruntime（当前 brew API 网络挂）或 pip 装后借 dylib，设 ORT_DYLIB_PATH
   - [ ] FASTEMBED_CACHE_DIR 目录预创建（防呆门），HF_ENDPOINT=https://hf-mirror.com 走镜像
   - [ ] cargo build --features local-embed，验证 init_embedder() 走 Local 分支
   - [ ] bench 配语义路后重跑 56 题消融：排序污染在双路检索下还剩多少
2. **distill 检索分层配额**（§4 根因的修复）：episode 不与原始 turn 抢 topk，分池或仅作补充；
   evidence_hit 协议改为 episode 覆盖 answer session 也算命中
3. **bm25_index 覆盖缺口**：bench ingest_question 裸 INSERT 不建索引，改走 store API（record_*）
   或补 index_chunk 调用，消掉每查询的全表扫描回退
4. **余弦暴力扫描规模化**：向量量级 ≥10 万时上 sqlite-vec / HNSW ANN 索引
5. **distill 77 题补蒸馏**（余额中断处续跑，命令在会话记录里）+ 全量 133 distill 基线复测，
   排除 56 题子集偏差后再下“distill 是否真不如 raw”的结论
