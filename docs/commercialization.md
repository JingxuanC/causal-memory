# 商业化决策记录（Apache-2.0 路线）

> 决策日期：2026-09-04
> 状态：已定稿（owner 拍板）
> 范围：协议策略 + 变现架构 + 风险对策
> 一句话定位：**让 causal-memory 成为 agent 记忆的标准层（类 SQLite 之于存储），
> 商业化在开源核心之上分层变现，不靠协议卡人。**

---

## 1. 协议决策：保持 Apache-2.0

2026-09-04 完整评估过四个候选，最终维持 Apache-2.0：

| 候选 | 结论 | 否决/采纳理由 |
|---|---|---|
| 完全闭源 | ❌ 否决 | 记忆是信任型品类，闭源=黑盒=企业一票否决；所有生态渠道（Hermes 内置 provider、Claude Code marketplace、DSH、MCP）只对开源开放；已开源到 v0.9.2 再闭源=社区信任永久清零 |
| AGPL-3.0 | ⚪ 暂缓 | 能防别人 fork 做闭源 SaaS，但 solo 阶段（67★）真实被抄风险 < 生态摩擦成本；**作为"上托管收费时"的升级选项保留**（触点清单见 §6） |
| BSL 1.1 | ❌ 否决 | 非 OSI 认证，吓跑集成方，与"做标准层"目标冲突 |
| **Apache-2.0** | ✅ **采纳** | 生态最顺、企业/框架集成零顾虑；商业化走 Mem0 模式（开源获客，上层变现）——Mem0 证明 Apache 下可以商业化成头部 |

**重估触发条件**（满足其一再切 AGPL / 加商业授权）：
- 开始卖托管服务且月营收可预期（届时 AGPL 保护的是真实收入流）
- 出现直接 fork 抄 SaaS 的竞品并造成实质分流
- 有大厂提出"闭源内置"需求 → 直接走商业授权/双许可，不用切全仓

---

## 2. 变现架构：开源核心 + 分层增值（Mem0 模式）

```
L0 开源核心 (Apache-2.0, 免费)          ← 获客/信任/渠道
   Rust 引擎 + pyo3 wheel + MCP server + 各框架插件
        │
L1 托管 SaaS（将来，云上跑开源版）        ← 卖"免运维"
   个人免费档 → 企业档(多用户/权限/SLA)
   配套：本地→云端数据迁移工具
        │
L2 企业增值（独立闭源仓库, Open Core）    ← 卖"能力"
   图谱可视化控制台(已有 graph html 资产)、多租户管理、
   RBAC/审计、离线 air-gapped 部署包、分布式、官方支持
        │
L3 OEM/集成授权                           ← 卖"嵌入"
   已验证路径: Hermes 内置 memory provider
   复制到 Codex/Cursor/OpenClaw/其他 agent 框架，
   签集成协议或 support 合同
```

**关键纪律**：
- Apache 版**永不阉割**（无功能门/激活码）——那是 OpenViking 开源版的原则，也是 Mem0 信任的来源
- 新独占功能进 L2 闭源仓库，不进开源主干（Apache 一旦发布不可收回）
- L2 仓库不开源 ≠ 项目不开源：开源是获客层，闭源是变现层，两层不混

---

## 3. Apache 下的护城河（不靠协议，靠什么）

| 护城河 | 现状 | 打法 |
|---|---|---|
| 品类定义权 | ✅ causal 记忆 + prevented 负扩散独家（HeLa-Mem 只做了 excitatory 侧） | paper + benchmark 持续卡位，让"因果记忆=causal-memory" |
| Benchmark 弹药 | CausalEval 81% vs mem0 65%；压缩生存 +20.8pp（独家维度）；重复犯错率 67%→33% | 每次 release 刷分 + 发对比，竞品无法用"无因果"回应 |
| 生态位深度 | Hermes 内置 provider（已验证） | 每嵌入一个框架，切换成本+1，fork 抄的人要重铺渠道 |
| 迭代速度 | solo 但 300+ commits 的高产 | 保持每周 release；速度是 Apache 下对抗资源型竞品唯一武器 |

---

## 4. 风险与对策

| 风险 | 概率 | 对策 |
|---|---|---|
| 有人 fork 后开闭源 SaaS 打价格战 | 中（品类热起来后） | 速度+品牌先行；SaaS 差异化在因果质量而非价格；真发生且造成流失 → 触发 §1 重估 |
| 大厂把机制抄进自家闭源记忆 | 高（算法可读论文重实现） | 接受——论文已公开；用 Rust 工程深度 + benchmark 拉开执行差距 |
| 社区分叉 | 低 | 保持 Apache 即无分叉动机；贡献者少，无治理风险 |
| solo 断更风险 | 中 | 文档/测试完备（368 tests）降低 bus factor；这也是开源的意义——代码不属于一个人 |

---

## 5. 短期动作清单（0 成本，按优先级）

- [ ] L0 就绪度：`:9938` HTTP server 补 auth + 多租户 + Docker Hub 发布（已有 Dockerfile/AMC 雏形）
- [ ] 一页 landing + Live Demo（benchmark 对比表 + 30s danger 演示已有素材）
- [ ] 渠道复制：Codex 插件已存在 → 提 Cursor / OpenClaw / LangChain 集成 PR
- [ ] docs/roadmap 补"商业化"章节指针（本文件）
- [ ] PyPI wheel 发布自动化（`causal-memory` 包已是 pip 安装形态）

---

## 6. 附录：Apache → AGPL 切换触点清单（备用，已验证 10 分钟可完成）

若将来触发 §1 重估，切换只需改以下文件（2026-09-04 实测，改完 `git restore` 无痕回滚）：

1. `LICENSE` — 全文替换 AGPL-3.0 官方文本（661 行，gnu.org 拉取）
2. `Cargo.toml` — `license = "AGPL-3.0-only"`（子 crate 走 workspace 继承）
3. `crates/causal-memory-py/pyproject.toml` — license 字段 + PyPI classifier
4. `hermes-plugin/pyproject.toml` — license 字段
5. `dsh-plugin/package.json` — npm license 字段
6. `README.md` / `README.zh-CN.md` — badge + License 段（写明"自 vX.Y 起生效，更早版本保持 Apache-2.0"）
7. `PROFILE.md`、`crates/causal-memory-py/README.md`、`docs/paper/paper-full-draft.md`(×2) — 引用
8. `CHANGELOG.md` — 生效版本条目

注意：外部贡献（Hanmiao Li 的 #17，1117 行核心 bugfix）Apache-2.0 与 AGPL-3.0 兼容，可并入，但按惯例需在 PR 留言通知。
