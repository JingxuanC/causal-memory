# 产品化方案：多租户托管架构 + 收费方案

> 2026-10-02 · 状态：提案，待评审
> 上游决策：[../commercialization.md](../commercialization.md)（Apache-2.0 + Mem0 模式已定稿）
> 本文是其 L1「托管 SaaS」层的落地设计：多租户怎么搭、钱怎么收。
> 关联：[enterprise-scaling.md](enterprise-scaling.md)（实测扩容阶梯）、
> [cloud-context-restore.md](cloud-context-restore.md)（session 恢复，付费功能原料）、
> [memory-git-sync.md](memory-git-sync.md)（云同步）、[deploy-docker.md](deploy-docker.md)

---

## 0. 现状盘点：已经拥有的「半个产品」

| 资产 | 位置 | 产品化中的角色 |
|---|---|---|
| 多租户 bearer auth | `crates/causal-memory-cli/src/tenant.rs` | 数据平面认证：token→tenant，**每租户独立 SQLite**，懒加载，热 reload |
| sha256 token 桥接 | tenant.rs（`sha256:<hex>` 条目） | **已预见网站 dashboard 模式**：控制面只存 hash，明文不落盘 |
| HTTP MCP server | `causal-memory http`（:9938） | 数据平面入口，streamable HTTP |
| Docker 部署 | `docs/design/deploy-docker.md` | 数据平面交付形态 |
| git-sync 云同步 | `causal-memory git-sync` + sync server | 本地↔云端迁移工具（commercialization §2 L1 配套） |
| 扩容实测 | `docs/design/enterprise-scaling.md` | 容量规划的实测依据（本文 §3.4） |
| 17 MCP tools + skill + 4 框架插件 | 全仓 | 获客漏斗的免费层 |
| drift 报告 / 自我纠错 harness | `drift.rs`、benches/ | 托管版的差异化付费功能（§4.3） |

**缺口**：控制面（账号/项目/token 签发/计费/用量聚合）、配额与限流、per-tenant LLM 配置（BYOK）、store 池 LRU 驱逐、团队共享语义。

---

## 1. 产品形态：三层漏斗

```
L0 Community（开源 Apache-2.0，免费）     ← 获客/信任/标准层
   全功能本地引擎，用户自己跑、自己出 LLM key
        │  升级诱因：免运维、跨设备同步、不用自己搞 key
L1 Cloud（托管 SaaS，本文主角）            ← 卖"省心"
   Free → Pro → Team，按用量分档
        │  升级诱因：合规、权限、SLA、私有化
L2 Enterprise（闭源增值仓库，Open Core）   ← 卖"能力"
   SSO/审计/RBAC、air-gapped 包、官方支持
```

纪律沿用 commercialization.md：Apache 版永不阉割；L2 独占功能不进开源主干。

---

## 2. 收费方案

### 2.1 计费单元的选择

候选对比：

| 单元 | 代表 | 问题/优点 |
|---|---|---|
| 按 memory 条数 | Mem0 营销话术 | ❌ 坏单元——鼓励少记，与产品价值对立；且 97.8% junk 的前车之鉴（mem0 #4573）说明条数≠价值 |
| 按写入字节 credits | Zep | ❌ 用户算不明白，与成本（LLM token）不成比例 |
| **按操作事件：写事件 + 查事件** | Mem0 实际执行的方式 | ✅ 用户可预测、成本可映射、与我们架构天然对齐 |

**我们的计费单元**（MCP tool 调用直接映射，无需新概念）：

- **写事件**：`remember` / `record_decision` / `record_fact`（成本大头在 remember 的 LLM distill）
- **查事件**：`search_causal` / `search_memory` / `search_facts` / `trace_cause` / `intervention_query` / `counterfactual_query` 等读路径
- 管理操作（invalidate、sleep、export）不计费

### 2.2 杀手锏：BYOK 免费档

我们的 LLM 成本结构有一个 Mem0 没有的选项：**distill 用 DeepSeek 级模型就够**（bench 全链路就是 deepseek 跑的）。DeepSeek V4.1 Flash 当前 $0.15/M input（cache miss）/ $0.60/M output，一次 remember（约 2k in + 0.4k out）≈ **$0.0005**。

因此可以免费档直接给用户**自带 key（BYOK）**：用户填自己的 DeepSeek/OpenAI key 存进控制面，distill 走他的 key，我们只出存储和 CPU——免费档边际成本≈0，可以**永久免费且不限写事件条数**（只限存储量和查事件）。这是获客核武器：Mem0 免费档卡 1k 检索/月，我们卡得起的上限高一个数量级。

### 2.3 分档

| | **Cloud Free** | **Cloud Pro** | **Cloud Team** | **Enterprise** |
|---|---|---|---|---|
| 价格 | $0 | **$19/mo** | **$79/mo（5 席）** | 定制（年费） |
| LLM | BYOK（自带 key） | 平台出 key 含内 | 平台出 key 含内 | 平台/BYOK/私有模型 |
| 写事件/月 | 不限（BYOK） | 含 50k，超出 $2/10k | 含 200k | 不限 |
| 查事件/月 | 5k | 50k，超出 $1/10k | 250k | 不限 |
| 项目/tenant 数 | 1 | 10 | 不限 | 不限 |
| 存储 | 100 MB | 2 GB | 10 GB | 不限 |
| 数据保留 | 活跃即保留 | 同左 | 同左 | 自定 |
| git-sync 云备份 | ✅ | ✅ | ✅ | ✅ |
| session commit/restore（cloud-context-restore） | ❌ | ✅ | ✅ | ✅ |
| drift 漂移报告（定时邮件/webhook） | ❌ | ✅ 周报 | ✅ 日报+告警 | ✅ 定制 |
| 团队共享记忆（export/import 托管化） | ❌ | ❌ | ✅ | ✅ |
| SLA / SSO / 审计日志 | ❌ | ❌ | ❌ | ✅（L2） |
| 支持 | 社区 | email | 优先 | 专属 |

价格锚点逻辑：

- **$19 入门与 Mem0 Starter 同价**——同价不同质：Mem0 $19 档 1k 检索被我们免费档 5k 打，Pro 档我们给 50k 检索对齐其 $249 档的 50k，靠的是成本结构（DeepSeek vs 他们的 GPT 级 ingestion）
- **$79 Team 卡 Zep Flex（$125）之下**，且 Zep 的 credit 模型用户算不清，我们的事件模型一句话说清
- 年付 -20%；学生/开源维护者 Free 升 Pro（人工审批，换案例授权）

### 2.4 单位经济学（Pro 档核算，假设 2026-10 DeepSeek 牌价）

| 项 | 假设 | 月成本 |
|---|---|---|
| LLM distill | 重度用户 2k 写事件/月（≈每天 60 次 remember，远超典型值）× $0.0005 | ~$1 |
| 查事件 | 10k 次 × 纯 CPU（无 LLM），分摊 | <$0.5 |
| 存储+备份 | 2 GB SQLite + git-sync 对象存储 | ~$0.1 |
| 服务器分摊 | 单 8C16G 节点估撑 500 活跃租户（§3.4），$100/月 | $0.2 |
| **合计** | | **< $2 / 重度 Pro 用户** |

毛利率 >90% 的安全边际：即使配额打满 50k 写事件（$25 LLM 成本）仍有超出计费兜底。**结论：配额是容量帽不是预期用量，定价风险低。**

### 2.5 竞品对标（2026-09 核实的公开牌价）

| | 免费档 | 入门付费 | 中档 | 计费单元 |
|---|---|---|---|---|
| **causal-memory Cloud** | BYOK 不限写 + 5k 查 | $19（50k 写/50k 查） | $79 Team | 写/查事件 |
| Mem0 | 10k add / 1k 检索 | $19（50k add / 5k 检索） | $249 Pro | add/retrieval 请求 |
| Zep | 10k credits | $125 Flex（50k credits） | $375 | 摄入字节 credits |
| Letta | 自托管免费 | ~$20 Pro | — | per agent |

差异化叙事一句话：**"同价更懂因果"**——CausalEval 81% vs mem0 65%、压缩生存 +20.8pp、重复犯错率 67%→33%，外加竞品都没有的 drift 自我偏差审计。

---

## 3. 多租户架构

### 3.1 总体：控制面 / 数据面分离

```
┌──────────────── 控制面（新，Web 服务）────────────────┐
│  账号/项目   token 签发（只存 sha256）   用量聚合      │
│  Stripe 计费   BYOK key 保管（加密）    配额下发       │
└──────────┬───────────────────────────────────────────┘
           │ 写 cloud.json（sha256:token → tenant+配额）
           ▼
┌──────────────── 数据面（现有 causal-memory http 演进）──┐
│  tenant.rs 认证（已实现）→ per-tenant SQLite（已实现）  │
│  + 配额中间件（新）  + 计量中间件（新）                 │
│  + store 池 LRU（新）  + per-tenant LLM 配置（新）      │
└────────────────────────────────────────────────────────┘
```

**关键设计：控制面只写 tenant.rs 已支持的 tokens 目录格式**（`cloud.json`，sha256 条目）——数据面零协议改动，这是 tenant.rs 设计时就埋好的桥。配额和 BYOK key 以 sidecar 文件（`quotas.json`、`llm_keys.json`，同目录同热 reload 机制）下发。

### 3.2 隔离模型：一租户一 SQLite（维持现状，不引入 PG）

- **隔离强度**：文件级隔离 = 最强租户边界，天然防"记忆串号"（mem0 #2062 类事故在我们架构里结构上不可能）
- **合规红利**：删租户=删文件（GDPR 遗忘权一行 `rm`）；导出租户=copy 文件；单租户备份/回滚互不影响
- **迁移路径**：本地→云端 = git-sync push；云端→本地（防锁定）= git-sync pull。**用户永远能带着自己的图离开**——这是信任型品类的核心卖点，也是免费档不阉割的底气

### 3.3 计量与配额（数据面中间件）

- **计量**：每请求记录 `(tenant, tool, ts, tokens_llm)` → 本地 append-only `usage` 表，每小时聚合上报控制面（失败本地积压重放，宁可重复不可丢——控制面按 (tenant, hour, tool) 幂等去重）
- **配额**：三道闸门，全部 fail-closed：
  1. 速率：token bucket per tenant（查事件 10 rps 默认，防单租户打爆共享 CPU）
  2. 月度事件配额：超档 → 写事件 429 + 升级提示（**读事件永不硬断**，超限降速——记忆不可读比不可写更伤信任）
  3. 存储配额：DB 文件超档 → 拒绝写事件，引导导出/升级

### 3.4 扩容阶梯（锚定 enterprise-scaling.md 实测）

实测瓶颈是**驻留图重建 + RAM**，不是存储。多租户恰好规避了单库巨大化（单用户库通常 <50MB）：

| 阶段 | 活跃租户 | 动作 |
|---|---|---|
| S0 单节点 | <200 | 现状即可：tenant.rs 懒加载 + 已调优的 T0 rebuild cadence（4e9d723） |
| S1 单节点调优 | 200-1k | **store 池 LRU 驱逐**（新）：活跃 tenant 驻留图上限 N 个，冷租户关图释放 RAM（每个 50k node 图 ~0.12 GB）；`maybe_rebuild_graph` 阈值按租户规模分档 |
| S2 水平分片 | 1k-10k | 按 tenant id 一致性哈希分片到多节点——每租户 DB 只活在一个节点，**sticky 路由无状态化**，无需分布式存储 |
| S3 巨型租户 | 单租户 >1M nodes | 才轮到 enterprise-scaling.md 的 PG 后端选项；此前一律 SQLite |

红线（实测值）：单租户 300-500k edges 时全图重建 >30s——S1 的 LRU + 分档 rebuild 阈值必须在此之前上线。

### 3.5 BYOK 与 per-tenant LLM 配置

- 控制面保管用户 key（KMS 加密 envelope，落库只存密文），下发数据面 `llm_keys.json`（内存驻留，同 mtime 热 reload；可选只下发数据面临时token、数据面回控制面代理 LLM 调用——零明文到数据面，Pro 以上默认后者）
- 数据面 distiller 按 tenant 解析 LLM 配置：BYOK 租户走用户 key，托管档走平台 key 池
- **平台 key 池本身是成本中心**：免费档绝不给平台 key（这就是 BYOK 免费档可持续的原因）

---

## 4. 阶段路线图

| 阶段 | 内容 | 验收标准 | 依赖 |
|---|---|---|---|
| **P0 托管 alpha**（1-2 周） | 单节点 Docker 部署 + 手工签发 token + git-sync 迁移指南，邀请 10 个 dogfood 用户 | 真实跨设备同步跑通；usage 表落数据 | 现有资产即可 |
| **P1 控制面 + 计费**（4-6 周） | 网站 dashboard（注册/项目/token/BYOK key 录入）、Stripe 订阅、计量上报 + 配额三闸门、store 池 LRU | Free/Pro 两档自助开通付费全流程 | P0 的 usage 数据校准配额 |
| **P2 Team 档 + 差异化功能**（4 周） | 团队共享记忆（export/import 托管化 + 权限）、drift 定时报告、session commit/restore（cloud-context-restore.md 实施） | Team 档首单 | P1 |
| **P3 Enterprise（L2 闭源仓）** | SSO/审计/RBAC、air-gapped 包 | 首个年费合同 | 有真实询单再动 |

P0 的关键动作其实是**用真实用量校准 §2.4 的成本假设**——配额数字（5k/50k）先按对标拍脑袋，alpha 数据回来再调。

## 5. 风险与对策

| 风险 | 对策 |
|---|---|
| BYOK 免费档被薅（无限存储/查询） | 存储 100MB + 查 5k/月硬顶；免费档不开 session restore；滥用模式（脚本化批量导入）速率闸拦截 |
| 平台 key 成本失控 | 免费档零平台 key；Pro 配额即成本帽；distill 默认走 Flash 档模型 + 峰谷调度（DeepSeek 谷时半价） |
| 控制面是新代码，质量拖累引擎口碑 | 控制面独立仓库独立发布；数据面（开源引擎）无控制面也可完整运行——引擎口碑不受网站 bug 影响 |
| Mem0 跟进 BYOK/降价 | 不跟价格战；差异化押注因果质量 + drift 独家能力，benchmark 每 release 刷 |
| 租户数据安全事件 | 文件级隔离 + 控制面只存 hash + BYOK 信封加密；渗透测试进 P3  checklist |

## 6. 开放问题

1. 控制面技术栈：独立 Web 服务（轻量，如 Rust axum / Next.js + Supabase）还是买现成的（Clerk + Stripe Billing 组合）？倾向后者——solo 精力应留给引擎
2. 国内 vs 海外：DeepSeek 成本优势在国内，支付（Stripe vs 微信支付）和合规（个保法：记忆=个人数据，drift 报告里的删除权要产品化）是两套打法；建议先海外（Stripe + 英文站），国内走企业定制
3. 免费档 5k 查/月是否太慷慨？等 P0 alpha 数据校准
4. AGPL 重估触发点沿用 commercialization.md §1：月营收可预期时重新评估
