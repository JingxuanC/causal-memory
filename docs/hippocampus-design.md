# Hippocampus Architecture for causal-memory

> **设计目标**：把 causal-memory 从"SQL 记忆库"升级为"海马体式因果神经图"。
>
> 不是模拟生物神经元（spiking / Hodgkin-Huxley），而是提取海马体的**四个计算原理**，用工程手段实现。
>
> 核心洞察：所有现有系统（HippoRAG / SYNAPSE / CA3-Net）沿"语义边"做激活扩散。**causal-memory 沿"因果边"做激活扩散** —— 而且因果边有方向（A 导致 B ≠ B 导致 A）和类型（caused / enabled / prevented），这给扩散带来了全新的维度。

---

## 0. 设计哲学

### 三个"不做"

1. **不模拟生物硬件** —— 不做 spiking neural network / Hodgkin-Huxley / 突触可塑性方程。causal-memory 有几百到几万条因果边，不需要 860 亿神经元的并行性。
2. **不替换 SQLite** —— SQLite 做持久化（ACID / 可备份 / 跨进程），in-memory graph 做计算。两者分工明确。
3. **不重新发明激活扩散算法** —— SYNAPSE 的算法已经成熟（NeurIPS 级验证），直接适配因果图。

### 三个"必须做"

1. **有向图** —— 因果关系是严格有向的。"决策 A 导致结果 B" ≠ "结果 B 导致决策 A"。HippoRAG 用无向图（`directed=False`），丢了因果方向。这是 causal-memory 必须超越的。
2. **关系类型加权扩散** —— `caused`（正扩散）、`enabled`（弱正扩散）、`prevented`（负扩散 = 抑制）。负扩散模拟海马体的抑制性突触（GABA 能中间神经元），是**没有任何现有系统做过的**。
3. **时序约束** —— `event_time` 和 `valid_to` 限制扩散范围。只在有效时间窗口内的因果边上传播。

---

## 1. 海马体四区域 → 工程映射

### 1.0 为什么是海马体（不是新皮层）

海马体负责**快速、一次性的情景记忆**（episodic memory）—— 记住"刚才发生了什么"。新皮层负责**慢速、统计的语义记忆**（semantic memory）—— 提炼"一般规律"。

causal-memory 的 `causal_edges` 表 = 海马体（每条因果边是一次具体的"决策→结果"事件）。
causal-memory 的 `meta_causal_edges` 表 = 新皮层（跨任务的统计模式）。

本设计聚焦海马体层（`causal_edges`）的计算架构。

### 1.1 DG（Dentate Gyrus）→ Pattern Separation

**海马体做什么**：DG 用稀疏编码把相似输入"推远"。"在咖啡馆见 John" 和 "在咖啡馆见 Jane" 在 DG 里激活完全不同的神经元群 —— 防止两个相似但不同的经历互相干扰。

**工程映射**：

```
输入：一个新的决策文本 "used Redis with mutex lock"
DG 计算：
  1. 提取关键 token: ["redis", "mutex", "lock"]
  2. 生成稀疏位置码: hash(redis) ⊕ hash(mutex) ⊕ hash(lock) → 128-bit 稀疏向量
  3. 和现有记忆比较：如果位置码和某条记忆的 Hamming 距离 < 3 → 标记为"相似"（去重候选）
     如果距离 >= 3 → 新记忆

输出：一个 unique 的位置码，用于后续的去重和区分
```

**为什么需要这个**：当前 causal-memory 没有去重（同一个决策可以被 extract 多次）。DG 式的 pattern separation 让相似决策自动归组，不同决策自动分开。

**实现**：局部敏感哈希（LSH）或 SimHash。Rust 有 `lsh-rs` crate。

### 1.2 CA3 → Spreading Activation（Pattern Completion + Attractor）

**海马体做什么**：CA3 是自联想记忆（auto-associative memory）—— 从部分线索"补全"完整记忆。你听到一段旋律就想起整首歌。CA3 的 recurrent collaterals（自反馈连接）形成"吸引子" —— 相似查询收敛到同一组记忆。

**工程映射**：

```
输入：一个查询 "我在 debug 一个并发问题"
CA3 计算（激活扩散）：
  1. 找种子节点：query 匹配的因果边（task_tag="concurrency" 或 text LIKE "%concurrent%"）
  2. 种子节点初始激活 = 1.0
  3. 沿因果边扩散：
     - caused 边：activation × edge_weight × decay(0.7) → 正扩散
     - enabled 边：activation × edge_weight × 0.5 × decay(0.7) → 弱正扩散
     - prevented 边：activation × edge_weight × (-0.3) × decay(0.7) → 负扩散（抑制）
  4. 每跳衰减 × 0.7
  5. 激活值 < threshold(0.1) 的节点不返回
  6. 最大扩散 5 跳

输出：按激活值排序的因果记忆列表

关键效果：
  - "并发问题" → 沿 caused 边扩散到 "mutex 导致死锁" → 沿 caused 边扩散到 "改用 channel 修复"
  - 这条因果链被"联想"到了，即使 query 里没搜 "mutex" 或 "channel"
  - prevented 边：如果 A prevented B，激活 A 时 B 被抑制（更少出现在结果里）
```

**有向 vs 无向**：
- HippoRAG 用无向图（`directed=False`）—— 往两个方向扩散
- causal-memory 用有向图 —— 只往因果方向扩散（从决策 → 结果，不反过来）
- 但 `trace_cause`（反向追溯）需要反方向扩散 —— 加一个 `reverse: bool` 参数

**关系类型加权的独创性**：

| 关系 | 扩散系数 | 生物学对应 | 效果 |
|---|---|---|---|
| `caused` | +1.0 × decay | 兴奋性突触（谷氨酸能） | 强力传播激活 |
| `enabled` | +0.5 × decay | 弱兴奋性突触 | 弱传播 |
| `prevented` | -0.3 × decay | 抑制性突触（GABA 能） | **抑制目标节点的激活** |
| `no_effect` | 0.0 | 无突触连接 | 不传播 |

**`prevented` 的负扩散是全新的** —— 没有任何现有系统做了。它的效果：如果 "决策 A prevented 结果 B"，当 agent 查 "B 类似的结果"时，"决策 A" 被抑制（因为它会阻止 B）。这对 agent 的决策非常有用 —— "上次我做了 A，它阻止了 B 发生，所以如果我想要 B，不该做 A"。

### 1.3 CA1 → Novelty Detection

**海马体做什么**：CA1 比较 CA3 的"预测"（从记忆补全的预期结果）和 EC 的"实际输入"（真实发生的事）。如果不匹配 → 标记为"新异"（novelty）→ 触发记忆形成。

**工程映射**：

```
输入：agent 做了一个决策 "用 Redis 做缓存"，实际结果 "缓存击穿"
CA1 计算：
  1. 从决策出发，用 CA3（spreading activation）预测"预期结果"
     → 预期：["降低延迟", "减少 DB 负载", "提高性能"]（从过去经验扩散）
  2. 比较预测和实际：
     实际 = "缓存击穿" 和 预期 = "降低延迟" 的语义相似度 = 0.15（很低）
  3. surprise = 1 - similarity = 0.85（高）
  4. 如果 surprise > 0.5 → 标记为 "novel" → 自动触发 record_decision

输出：NoveltyReport { surprise: 0.85, predicted: [...], actual: "缓存击穿", should_record: true }
```

**价值**：当前 causal-memory 的 `record_decision` 要么 agent 手动调，要么 extractor 批量提取。CA1 式新异性检测让系统**自动判断"这个结果出乎意料，值得记住"**。

### 1.4 SWR（Sharp-Wave Ripple）→ Offline Replay Consolidation

**海马体做什么**：睡眠时以 150-250Hz 快速重放经历序列（forward replay + reverse replay）。不是为了"回看"，是为了**重组和压缩** —— 强化重要连接（LTP），弱化不重要的（LTD），发现跨经历的模式。

**工程映射**：

```
触发：每次 consolidate 命令运行时（或定时触发）

SWR 循环（重复 N 次）：
  1. 随机采样一条因果链（从近期的种子决策出发，沿 caused 边走到终点）
  2. 正向回放（forward replay）：
     - 沿链的每条边：edge.weight *= 1.05（LTP：长期增强）
     - 每个节点：node.replay_count += 1（巩固强度 +1）
  3. 反向回放（reverse replay）：
     - 从终点往回走，检测模式和矛盾
     - 如果发现两条链的某段相似 → 创建 meta_causal_edge
  4. 突触衰减（LTD）：
     - 所有权重 *= 0.99（全局衰减）
     - 但 replay_count > 3 的节点衰减率减半（被巩固的记忆更抗衰减）
  5. 垃圾回收：
     - weight < 0.05 且 replay_count == 0 的边 → 标记 valid_to（遗忘）
```

**和现有 consolidate 的区别**：当前 consolidate 是 SQL 遍历（`SELECT * FROM causal_edges` 逐行处理）。SWR 式是**随机采样 + 序列回放** —— 不是遍历所有边，而是随机选几条因果链，模拟"做梦"。

---

## 2. 数据结构设计

### 2.1 In-Memory Activation Graph

```rust
/// 因果记忆节点（对应海马体的一个记忆痕迹 / engram）
pub struct MemoryNode {
    pub id: String,                    // chunk ID（和 SQLite 同步）
    pub text: String,                  // 决策或结果的文本
    pub node_type: NodeType,           // Decision / Outcome
    pub activation: f64,               // 当前激活值（0.0-1.0），每次查询后重置
    pub q_value: f64,                  // MemRL 式效用值（动态更新）
    pub replay_count: u32,             // 被 SWR 回放的次数（巩固强度）
    pub last_activated: i64,           // 上次被激活的 Unix 时间（recency）
    pub event_time: i64,               // 事件实际发生时间
    pub sparse_code: u128,             // DG 式稀疏位置码（SimHash）
    pub task_tag: Option<String>,      // 任务标签
    pub edges_out: Vec<EdgeRef>,       // 出边（因果后继）
    pub edges_in: Vec<EdgeRef>,        // 入边（因果前驱）
}

pub enum NodeType {
    Decision,
    Outcome,
}

/// 因果边（对应海马体的突触连接）
pub struct EdgeRef {
    pub target: u32,                   // 目标节点索引（在 Vec<MemoryNode> 里的位置）
    pub relation: CausalRelation,      // caused / enabled / prevented / no_effect
    pub weight: f64,                   // 突触强度（初始 = q_value, 被 LTP/LTD 动态调整）
    pub valid: bool,                   // valid_to IS NULL → true
}

pub enum CausalRelation {
    Caused,                            // 扩散系数: +1.0
    Enabled,                           // 扩散系数: +0.5
    Prevented,                         // 扩散系数: -0.3（抑制！）
    NoEffect,                          // 扩散系数: 0.0
}

/// 完整的激活图
pub struct ActivationGraph {
    nodes: Vec<MemoryNode>,            // 所有节点（Vec，O(1) 索引）
    node_index: HashMap<String, u32>,  // chunk_id → 索引（快速查找）
    
    // 扩散参数
    decay: f64,                        // 每跳衰减率（默认 0.7）
    threshold: f64,                    // 激活阈值（默认 0.1）
    max_hops: usize,                   // 最大扩散距离（默认 5）
    
    // LTP/LTD 参数
    ltp_rate: f64,                     // 长期增强率（默认 1.05）
    ltd_rate: f64,                     // 长期衰减率（默认 0.99）
    gc_threshold: f64,                 // 垃圾回收阈值（默认 0.05）
}
```

### 2.2 SQLite ↔ 内存图同步

```
启动时：
  SQLite → 加载到 → 内存图（SELECT * FROM causal_edges JOIN chunks）

运行时（写入）：
  record_decision → SQLite INSERT → 内存图 add_node + add_edge

运行时（查询）：
  search_causal → 内存图 spreading_activation → 返回结果（不碰 SQLite）

SWR 巩固后：
  内存图 weight 变化 → 批量 UPDATE SQLite（同步 LTP/LTD 结果）
  内存图 valid=false → 批量 UPDATE SQLite SET valid_to = now（遗忘）

关闭时：
  内存图全量 → 批量 UPSERT SQLite（持久化所有 weight 变化）
```

---

## 3. 核心算法

### 3.1 Spreading Activation（替代 search_causal）

```rust
impl ActivationGraph {
    /// 因果激活扩散查询
    /// 
    /// 从种子节点出发，沿因果边传播激活。
    /// 不同关系类型有不同的扩散系数。
    /// prevented 边做负扩散（抑制目标节点）。
    pub fn spreading_activation(
        &mut self,
        seed_text: &str,
        task_tag: Option<&str>,
        reverse: bool,         // false = 正向(决策→结果), true = 反向(结果→决策)
    ) -> Vec<(u32, f64)> {     // (node_index, activation_value)
        
        // Phase 1: 找种子节点（DG + 语义匹配）
        let seeds = self.find_seeds(seed_text, task_tag);
        if seeds.is_empty() {
            return Vec::new();
        }
        
        // 初始化激活值
        let mut activations = vec![0.0_f64; self.nodes.len()];
        for &seed_idx in &seeds {
            activations[seed_idx] = 1.0;
            self.nodes[seed_idx].last_activated = now();
        }
        
        // Phase 2: 迭代扩散（CA3 spreading activation）
        for hop in 0..self.max_hops {
            let mut delta = vec![0.0_f64; self.nodes.len()];
            let mut any_change = false;
            
            for (i, &act) in activations.iter().enumerate() {
                if act.abs() < self.threshold {
                    continue;  // 激活值太低，不扩散
                }
                
                // 选择出边（正向）或入边（反向）
                let edges = if reverse {
                    &self.nodes[i].edges_in
                } else {
                    &self.nodes[i].edges_out
                };
                
                for edge in edges {
                    if !edge.valid {
                        continue;  // 跳过已失效的边
                    }
                    
                    // 关系类型决定扩散系数
                    let spread_coeff = match edge.relation {
                        CausalRelation::Caused   => 1.0,
                        causalRelation::Enabled  => 0.5,
                        causalRelation::Prevented => -0.3,  // 负扩散！
                        causalRelation::NoEffect => 0.0,
                    };
                    
                    let propagated = act * edge.weight * spread_coeff * self.decay;
                    
                    // 累加到 delta（正/负激活都累加）
                    delta[edge.target] += propagated;
                    if propagated.abs() >= self.threshold {
                        any_change = true;
                    }
                }
            }
            
            // 应用 delta（取 max，模拟神经元 "winner-takes-all"）
            for i in 0..activations.len() {
                if delta[i] != 0.0 {
                    activations[i] = activations[i].max(delta[i]).max(-1.0).min(1.0);
                    if activations[i].abs() >= self.threshold {
                        self.nodes[i].last_activated = now();
                    }
                }
            }
            
            if !any_change {
                break;  // 收敛
            }
        }
        
        // Phase 3: 收集结果（按激活值绝对值排序，保留符号）
        let mut results: Vec<(u32, f64)> = activations.iter()
            .enumerate()
            .filter(|(_, &a)| a.abs() >= self.threshold)
            .map(|(i, &a)| (i as u32, a))
            .collect();
        
        results.sort_by(|a, b| b.1.abs().partial_cmp(&a.1.abs()).unwrap());
        
        results
    }
}
```

### 3.2 Novelty Detection（CA1 式）

```rust
impl ActivationGraph {
    /// 新异性检测：比较"预测结果"和"实际结果"
    pub fn detect_novelty(
        &mut self,
        decision_text: &str,
        actual_outcome: &str,
    ) -> NoveltyReport {
        // 1. 从决策出发，正向扩散到"预期结果"
        let predicted = self.spreading_activation(
            decision_text, None, false  // 正向：决策 → 结果
        );
        
        // 2. 把预期结果的文本拼接
        let predicted_text: String = predicted.iter()
            .take(5)
            .filter(|(_, a)| *a > 0.0)  // 只看正激活（caused/enabled）
            .map(|(idx, _)| self.nodes[*idx].text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        
        // 3. 比较预期和实际（简单的文本相似度）
        let similarity = text_similarity(&predicted_text, actual_outcome);
        let surprise = 1.0 - similarity;
        
        // 4. 检查 prevented 边：决策是否"阻止了"和实际结果相似的东西
        let negative_predicted: Vec<_> = predicted.iter()
            .filter(|(_, a)| *a < 0.0)  // 负激活（prevented 的）
            .collect();
        
        NoveltyReport {
            surprise,
            should_record: surprise > 0.5,
            predicted_positive: predicted_text,
            predicted_negative: negative_predicted.iter()
                .map(|(idx, _)| self.nodes[**idx].text.clone())
                .collect(),
        }
    }
}
```

### 3.3 SWR Replay（离线巩固）

```rust
impl ActivationGraph {
    /// Sharp-Wave Ripple 式离线巩固
    /// 随机采样因果链 → 正向回放(LTP) → 反向回放(模式检测) → 全局衰减(LTD)
    pub fn swr_consolidate(&mut self, num_replays: usize) -> ConsolidationStats {
        let mut stats = ConsolidationStats::default();
        
        for _ in 0..num_replays {
            // 1. 随机选一个近期种子节点
            let seed = self.sample_recent_node();
            if seed.is_none() { break; }
            let seed = seed.unwrap();
            
            // 2. 正向回放：沿 caused 边走一条链
            let chain = self.walk_causal_chain(seed, 5);  // 最多 5 跳
            stats.chains_replayed += 1;
            
            // 3. LTP：回放链上的边权重增强
            for window in chain.windows(2) {
                let (from, to) = (window[0], window[1]);
                if let Some(edge) = self.find_edge_mut(from, to) {
                    edge.weight *= self.ltp_rate;  // × 1.05
                    stats.ltp_events += 1;
                }
            }
            
            // 4. 每个节点的 replay_count +1
            for &node_idx in &chain {
                self.nodes[node_idx].replay_count += 1;
            }
            
            // 5. 反向回放：检测跨链模式
            for &node_idx in chain.iter().rev() {
                let patterns = self.detect_pattern_at(node_idx);
                for pattern in patterns {
                    // 如果发现相似模式，创建 meta_causal_edge
                    stats.patterns_detected += 1;
                }
            }
        }
        
        // 6. LTD：全局突触衰减
        for node in &mut self.nodes {
            for edge in &mut node.edges_out {
                let protection = if node.replay_count > 3 { 0.5 } else { 1.0 };
                edge.weight *= 1.0 - (1.0 - self.ltd_rate) * protection;
            }
        }
        
        // 7. 垃圾回收：权重太低且没被回放过的边 → 遗忘
        for node in &mut self.nodes {
            for edge in &mut node.edges_out {
                if edge.weight < self.gc_threshold && node.replay_count == 0 {
                    edge.valid = false;  // 软遗忘
                    stats.forgotten += 1;
                }
            }
        }
        
        stats
    }
}
```

---

## 4. MCP 工具的变化

### 4.1 现有工具的升级

| 工具 | 当前（SQL） | 升级后（激活图） |
|---|---|---|
| `search_causal` | `SELECT ... WHERE task_tag = ?` | `graph.spreading_activation(query)` |
| `trace_cause` | `SELECT ... WHERE to_id = ?` | `graph.spreading_activation(query, reverse=true)` |
| `trace_cause_chain` | Recursive CTE | `graph.spreading_activation(query, max_hops=N)` |
| `record_decision` | `INSERT INTO causal_edges` | `INSERT` + `graph.add_node/add_edge` + `graph.detect_novelty()` |
| `consolidate` | SQL 遍历 | `graph.swr_consolidate(num_replays=50)` |
| `intervention_query` | SQL 分组统计 | `graph.spreading_activation(decision)` → 预期结果分布 |

### 4.2 新增工具

| 新工具 | 海马体对应 | 做什么 |
|---|---|---|
| `recall_associative` | CA3 pattern completion | 激活扩散查询（替代 search_causal 的联想版） |
| `detect_surprise` | CA1 novelty detection | 自动判断"这个结果出乎意料吗" → 是否值得记录 |

---

## 5. 性能分析

### 5.1 时间复杂度

| 操作 | SQL（当前） | 激活图（新） |
|---|---|---|
| search_causal | O(n)（全表扫描或索引扫描） | O(k × d × h)，k=种子数, d=平均度数, h=跳数 |
| trace_cause_chain | O(n^h)（CTE 递归） | O(d^h)（图扩散，同样指数但常数更小） |
| record_decision | O(1)（INSERT） | O(1)（Vec push） |
| consolidate | O(n)（全表遍历） | O(r × d × h)，r=回放次数 |

**关键**：对于 k=10 个种子、d=3 平均度数、h=5 跳：O(10 × 3 × 5) = O(150) —— 比 SQL 全表扫描 O(n) 快很多（当 n > 150 时）。

### 5.2 空间复杂度

- 每个节点：~200 bytes（id + text + activation + q_value + replay_count + sparse_code + edges）
- 每条边：~40 bytes（target + relation + weight + valid）
- 1000 个节点 + 2000 条边 ≈ 280 KB（轻松放内存）
- 10000 个节点 + 20000 条边 ≈ 2.8 MB（仍然轻松）

---

## 6. 实施计划

### Phase 1：基础设施（1-2 天）

- [ ] `ActivationGraph` 数据结构（`MemoryNode` + `EdgeRef` + `CausalRelation`）
- [ ] SQLite → 内存图加载（`load_from_store`）
- [ ] 内存图 → SQLite 持久化（`flush_to_store`）
- [ ] 单元测试：建图 / 加节点 / 加边 / 查找

### Phase 2：激活扩散（1 天）—— 核心差异化

- [ ] `spreading_activation` 实现（含 prevented 负扩散）
- [ ] 集成到 `search_causal`（替换 SQL SELECT）
- [ ] 集成到 `trace_cause`（reverse=true 模式）
- [ ] Benchmark：对比 SQL 查询 vs 激活扩散的召回率和精度

### Phase 3：新异性检测（1 天）

- [ ] `detect_novelty` 实现
- [ ] 集成到 `record_decision`（自动判断是否值得记录）
- [ ] 测试：已知因果图 + 新决策 → surprise 分数

### Phase 4：SWR 巩固（2 天）

- [ ] `swr_consolidate` 实现（LTP + LTD + 模式检测 + GC）
- [ ] 集成到 `consolidate` 命令
- [ ] Benchmark：对比 SWR 巩固前后的 LoCoMo 分数

### Phase 5：DG pattern separation（半天）

- [ ] SimHash 稀疏位置码
- [ ] 集成到 `record_decision`（去重）
- [ ] 测试：相似决策的去重率

---

## 7. 和现有系统的最终对比

| 维度 | HippoRAG | SYNAPSE | Anda Brain | **causal-memory (本设计)** |
|---|---|---|---|---|
| 图类型 | 无向 | 有向 | 有向 | **有向** |
| 边类型 | 实体关系 | 语义 + 上下文 | 概念 + 命题 | **因果关系**（caused/enabled/prevented） |
| 扩散算法 | Personalized PageRank | Spreading Activation | LLM 编排 | **Spreading Activation + 关系类型加权** |
| 负扩散 | ❌ | ❌ | ❌ | **✅ prevented 边做抑制性扩散** |
| 巩固 | ❌ | ❌ | LLM agent | **SWR 式 LTP/LTD + replay** |
| 新异性检测 | ❌ | ❌ | ❌ | **✅ CA1 式预测 vs 实际** |
| Pattern separation | PageRank 间接 | embedding 间接 | ❌ | **✅ SimHash 稀疏编码** |
| 语言 | Python | Python | Rust | **Rust** |
| 存储 | Qdrant + KG | 向量 + 图 | KIP DB | **SQLite + In-memory** |

**causal-memory 的三个独创点**（没有任何现有系统做过）：

1. **因果关系类型的加权扩散**（caused=+1.0, prevented=-0.3）
2. **CA1 式新异性检测**（自动判断"什么值得记住"）
3. **因果边上的 SWR 式巩固**（不是语义边的重放，是因果链的重放）
