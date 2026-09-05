# Memory Git Sync — 记忆版本化与跨位置同步设计

> Status: design（已评审，P0 实施中）
> Date: 2026-09-05
> 关联: [cloud-context-restore.md](cloud-context-restore.md)（会话级 commit/archive，正交）、
> [unified-memory-design.md](unified-memory-design.md)（one graph, one engine）、
> [roadmap.md](../roadmap.md)（Backup/restore tooling、multi-tenant 两条 open 项）
> 目标：**agent 用 agent_id 在任意位置拉取/同步自己的记忆上下文**，机制小而可靠。

---

## 1. 结论先行

**记忆 = repo，agent_id = 仓库名。** 给因果记忆加一层 git 式快照版本机制：
本地因果库（工作区）→ `commit` 打快照 → `push/pull` 与云端（agent_id 命名的
remote）同步 → 换机器 `clone` 即恢复全部上下文 → 任何时刻 `checkout` 回到
任意历史版本。**六个命令 + remote 管理，零 merge 算法，复用现有 export/import
的幂等性。**

```
本地（agent 跑的地方）                        Remote（agent_id 命名空间）
┌────────────────────────────┐            ┌──────────────────────────┐
│ causal.db  （工作区，可变）  │  push      │  objects/<sha256> 快照链  │
│ .cm/HEAD → refs/heads/main │ ────────▶  │  refs/heads/main         │
│        ▲                   │  pull      └──────────────────────────┘
│        │ commit 快照       │ ◀────────
└────────────────────────────┘
```

**为什么是 git 语义而不是 REST 全量同步/增量 diff：**
1. 语义人人会：commit/log/push/pull/clone/checkout 无需解释，与 agent 心智模型吻合；
2. 记忆库是 KB~MB 级 → **全量快照**，内容 hash 去重，砍掉 git 最复杂的
   pack/delta/索引部分；
3. 合并 = 现有 `import_jsonl`（**幂等**：重复导入自动 skip duplicate，
   io.rs 测试已验证）→ 同步不需要 merge 算法；
4. 每个 commit 是独立可审计的版本点 —— "这个 agent 在什么时候记住了什么"，
   与 recall_audit 互补；
5. **回退 = `checkout <commit>`**：快照是完整状态，空库重放即精确重建，
   不需要 diff/undo 日志（见 §3.6）。

---

## 2. 对象模型（最小）

### 2.1 commit 对象

一个 commit = 一份**全量记忆快照** + meta，存为一个自包含文件：

```
objects/<sha256>
├── 第 1 行: meta JSON（单行）
└── 第 2..N 行: 快照数据行 —— export_jsonl 输出**去掉 header 行**后的
    chunk/edge/meta_edge 行（格式沿用 io.rs）
```

> **为什么去掉 export header 行**：header 带 `exported_at = now()`，每跑一次
> 都变。若 hash 覆盖它，同一状态的两次 commit hash 必然不同 → "nothing to
> commit" 永远不触发、push 去重失效。数据行由 `ORDER BY id` / 排序后的 chunk
> 行组成，**内容完全确定**。`import_jsonl` 不依赖 header 行（header 只做
> format_version 检查，缺失时数据行照常导入）。

meta 字段：

```json
{
  "format_version": 1,
  "hash": "<sha256 = 快照数据行拼接的哈希；与文件名 objects/<hash> 同值，无自指>",
  "parent": "<父 commit hash> | null",
  "message": "学会：灰度发布优于周五直推",
  "agent_id": "athena-researcher",
  "created_at": 1789000000,
  "counts": { "edges": 372, "meta_edges": 41, "chunks": 5 }
}
```

**哈希定义（唯一，无自指）**：`hash = sha256(数据行按序以 \n 拼接)`。文件名
`objects/<hash>`、`meta.hash`、pull/checkout 校验三方同值。**meta 行与 export
header 行都不在哈希覆盖范围内**。commit 时先序列化数据行 → 算 hash → 组装
meta → 落盘，文件内容与文件名天然一致。

**快照范围（commit 语义的关键）**：快照 = 记忆的**全真相** —— valid 与
invalidated/superseded 边都含（`include_invalidated: true`），这样"哪条
教训被证伪过"也随 agent 同步，新机器不会重蹈覆辙，checkout 也能回到"遗忘前"
的状态。**redact 关闭**（`redact: false`）—— 内部同步必须保留原文。派生数据
（cooccurrence_edges、edge_embeddings、q_value 调优）**不随快照** ——
它们可由检索/巩固重学，丢失无损失。

counts 让 `log` 免开库即可显示 Δ（edges +2 / meta_edges 0）。数据行类型与
io.rs ExportStats 对齐：chunks / edges / meta_edges（export 无独立 "facts"
行类型，chunk = 事实文本，edge = 因果教训）。

### 2.2 本地状态（.cm 目录）

仓库与 DB 一一绑定：`<db 路径>.cm/`（如 `causal.db` → 旁挂 `causal.db.cm/`，
tenant 库 `causal_x.db` 各自独立仓库 —— 隔离沿用 sha1 派生，零新逻辑）。

```
causal.db.cm/
├── config.json          # {"remotes": {"origin": {"url": "file:///srv/cm/athena"}}}
├── HEAD                 # 内容: ref: refs/heads/main（git 惯例，为将来分支留位）
├── refs/heads/main      # 内容: <当前 commit hash>
├── objects/<sha256>     # commit 对象
└── backups/             # pull / checkout 自动备份的 DB（见 §3.4/§3.6）
```

remote 由 `remote add/list/remove` 管理（§3.7）；clone 后自动把源记为 origin。

DB 文件本身 = git 的"工作区 + 索引"（可变状态）；commit 把**当前全部
记忆**固化成一个不可变快照。未 commit 的新记录 = 工作区改动。

### 2.3 remote 结构（两种形态，同一布局）

```
<root>/
├── objects/<sha256>     # commit 对象（快照文件）
└── refs/heads/main      # 远端主线指针
```

| 形态 | URL | 实现 | 状态 |
|---|---|---|---|
| file | `file:///srv/cm/<agent_id>` 或裸路径 `/srv/cm/<agent_id>` | 目录 + 文件复制 | P0 |
| https | `https://cm.example.com/agents/<agent_id>` | GET/PUT 对象 + bearer auth | P1 |

file remote 让 P0 在单机即可端到端验证（两个目录互推），HTTP 是同一布局挂到
server 上的事 —— **协议不变，只换传输**。

---

## 3. 命令设计（CLI 顶层：commit/log/push/pull/clone/checkout + remote）

### 3.1 `commit [-m <msg>] [--db P]`

```
1. 打开 DB，export 全真相快照：`ExportFilters { redact: false,
   include_invalidated: true, min_confidence: 0.0, since: 0 }`
   （复用 export_jsonl —— 内部同步保留原文与证伪历史；redact/过滤只属于
   对外分享导出，见 §6）
2. 丢掉 export 输出的 header 行，只留数据行（§2.1）
3. hash = sha256(数据行按序以 \n 拼接)（meta 行、header 行不计入）
4. hash == HEAD？ → "nothing to commit, working tree clean"（exit 0）
5. 写 objects/<hash>（meta 行 + 数据行），更新 refs/heads/main、HEAD
6. 输出: commit <hash8> — <msg>  (Δ edges +2, meta_edges 0)
```

默认 msg 缺省时提示（不自动编）—— git 哲学：commit message 是人的意图。

### 3.2 `log [--oneline] [--limit N] [--db P]`

沿 parent 链回溯，免开 DB 显示：`<hash8> <date> <msg> (Δ edges +2, meta_edges 0)`。
纯读 `.cm/objects` 里的 meta 行，**不打开因果库**（毫秒级，可做大目录）。

### 3.3 `push [<remote|path>] [--db P]`  （目标缺省 origin）

```
1. 目标解析：命名 remote（查 .cm/config.json remotes，缺省 origin）或直接
   file 路径；均未提供且无 origin → "no origin configured; push <path> or
   remote add origin <path>"
2. 读本地 HEAD 与 remote ref（远端无 refs → 视为空）
3. remote 为空 → 全量推 HEAD 链；否则收集"本地有 remote 无"的 commits
   （从 HEAD 沿 parent 回溯，遇 remote ref 或 null 停）
4. 快进检查：回溯链必须覆盖 remote ref —— 不覆盖 →
   "remote has N commit(s) you don't (hash8...); pull first"（拒绝，防覆盖）
5. 逐个写 remote/objects/<hash>（先写临时文件再 rename，原子）+ 原子更新
   remote refs/heads/main
6. 输出: pushed N commit(s) → <remote url>
```

### 3.4 `pull [<remote|path>] [--db P]`  （目标缺省 origin）

```
1. 目标解析同 push；读 remote ref；remote 为空 → "nothing to pull"
2. 收集 remote 有本地无的 commits（从 remote ref 沿 parent 回溯，遇本地
   HEAD 或 null 停）；为空 → "already up to date"
3. 逐个校验拉取对象：重算数据行 hash == meta.hash == 文件名（损坏拒绝）
4. （P0 约定）import 前自动备份当前 DB（复制到 <db>.cm/backups/）。
   import 逐行写、无整体事务，中断会半合并 —— 但**幂等可重入**：
   失败重试即收敛（已导入行全部 skip）。pull 应在无活跃长驻 Memory
   实例时执行（import 走 store 直写，不触发已加载 graph 的 rebuild；
   CLI 一次性进程天然满足）
5. 从旧到新逐个 import_jsonl（数据行，无 header 也不需要）—— 幂等**只增**
   合并：重复记录 skip（本地保留），新记录插入；本地独有记录原样保留
   ⚠️ P0 语义边界：远端对已有边的**状态变更**（forget/supersede →
   valid_to）不会通过只增 import 传播 —— 见 §4 与 §7 P1 align mode
6. HEAD 快进到 remote ref（若本地 ref 落后）
7. 若本地 ref 曾领先或工作区有未 commit 记录 → 提示
   "local-only records preserved; commit to snapshot them"
8. 输出: pulled N commit(s) → 工作区 updated（edges X, meta_edges Y, chunks Z）
```

### 3.5 `clone <path|remote|agent_id> [--db P]`

```
0. 目标解析：路径或 file:// URL 直接使用；命名 remote 查 config；agent_id
   查 config remotes → 未配置时按默认约定 https://cm.example.com/agents/<id>
   （P1 registry 细化；P0 遇 https 报 "https remotes arrive in P1; use a
   file path"）
1. 打开/新建空 DB（默认路径或 --db；路径父目录自动创建）
2. pull 全量
3. 自动 remote add origin <源>（记录来源，后续 push/pull 免参）
4. 打印 bootstrap 摘要（= 云端上下文包）：
   agent <id> · edges N · meta_edges M · chunks K · 最近 3 lessons · HEAD <hash8>
```

`clone` 就是换机器场景的上下文恢复入口 —— **agent 随便跑在哪儿**：
新机器上 `causal-memory clone athena-researcher` 即可开工。

### 3.6 `checkout <commit> [--db P]`（版本回退）

**命名注**：版本回退本可叫 `restore`，但该名已被"单条边证伪回滚"
（`restore <edge_id>`，maintenance）占用 —— 用 git 原生动词 `checkout`，
语义 = `git reset --hard <commit>`：把工作区（DB）整体重置到某 commit 的
状态。

```
1. 目标解析：<hash>（64 hex 或 ≥8 位唯一前缀，限本地 .cm/objects）、HEAD、
   HEAD~N（沿 parent 链走 N 步，越界报错）
2. 读 objects/<hash>；不存在 → "commit <hash8> not found locally
   (pull first?)"
3. 校验：重算数据行 sha256 == meta.hash == 文件名，不符拒绝（损坏对象）
4. 自动备份当前 DB → <db>.cm/backups/pre-checkout-<ts>.db（与 pull 同款；
   restore 前先落备份，误回退可找回）
5. 重建：打开同目录临时 DB（<db>.checkout-<pid>.db）→ import_jsonl(数据行)
   —— 空库导入 = 精确重建：valid 边、含 valid_to 的证伪边、meta_edges、
   chunks 全部还原到该 commit 时刻的状态（§2.1 已验证：import 对空库
   无 dedup 跳过，valid_to 原样写入）
6. 关库 → rename 临时 DB 覆盖原路径（SQLite 干净关闭后安全；顺带清掉
   残留 -wal/-shm）
7. refs/heads/main → <hash>（HEAD 指向回退目标；此后 commit 从该点继续）
8. 输出: checkout <hash8> — <msg>  (edges N, meta_edges M, chunks K restored)
   ⚠️ 未 commit 的工作区记录被丢弃（备份文件里有，可手动 import 找回）
```

回退能成立的前提 = 快照自包含：commit 快照含全部有效 + 证伪记录（含
valid_to），所以"回到忘记 X 之前" = checkout 到 X 的 forget 之前的 commit，
被证伪的教训恢复为 valid 态 —— 这恰好绕开了 §9 里 pull 只增不传播状态变更
的限制（checkout 是整库重置，不是合并）。

### 3.7 `remote add <name> <path|url>` / `remote list` / `remote remove <name>`

维护 `.cm/config.json` 的 remotes 映射（git remote 语义的最小子集）。
`remote add origin /srv/cm/athena` 后 `push`/`pull` 即可免参。文件写入：
读改写 + 临时文件 rename（原子）。

---

## 4. 为什么不需要 merge 算法（冲突语义）

| 场景 | 处理 | 理由 |
|---|---|---|
| 快照重复 | import 幂等 skip（本地保留） | 记录按文本三元组去重（io.rs 已验证） |
| 同记录不同内容 | import 是**只增**：同文本边 skip，新文本插入；真正的"更新"以 supersede 链表达 | supersede 机制已有（schema v14+）；**状态传播是 P0 边界，P1 align mode 补**（见下） |
| 因果边冲突 | 不产生 —— 边是 (decision,outcome) 文本的 id，文本同则 id 同 | 内容寻址天然去重 |
| 双实例并发 push | 快进检查拒绝，报 pull first | 单写者假设：MVP 下同 agent 不同时两处写 |
| 本地独有记录 | pull 后保留在工作区，提示 commit | 与 git 未提交改动语义一致 |
| 需要回退 | `checkout <commit>` 整库重置（§3.6），不 merge | 快照自包含 → 空库重放精确重建 |
| 同文本跨机重复记录 | 两台机器各自 record 同一教训（文本同、event_time 不同）→ dedup 不判重 → 两条近重复边 | 低概率，MVP 接受（P2 去掉 dedup 的 event_time 维度或记后清理） |

记忆是 **agent 私有上下文**，无多用户协作写 —— 不需要 branch/merge/rebase。
若未来出现"共享记忆库多人写"（团队场景），那时再加 refs 语义，本设计的
objects/refs 布局已为其留位。

---

## 5. 复用与新增盘点

| 资产 | 状态 | 本设计用途 |
|---|---|---|
| `export_jsonl` / `import_jsonl` | ✅ io.rs | 快照序列化（去 header 行）+ 幂等合并 + checkout 重建 |
| `sha1(id)[:16]` tenant 派生 | ✅ 9/4 提交 | agent_id → 云端仓库隔离 |
| HTTP server + bearer 中间件 | ✅ :9938 / http_auth.rs | P1 https remote 底座 |
| recall_audit / metrics | ✅ | commit 可审计（谁在何时固化） |
| `.cm/` 状态 + objects + refs | 🆕 ~1 模块 | 新增量集中在 `commands/git.rs` |
| sha2 crate | 🆕 依赖 `sha2 = ">=0.10,<0.12"`（仓库现无 sha 实现） | 内容寻址 + 完整性校验 |

### 与 session_archives（cloud-context-restore.md v16）的关系

**正交，不重叠**：
- 本机制 = **记忆层版本**（快照全部 chunks/edges/meta_edges，粒度 = commit 时机，跨位置同步 + 版本回退）
- session_archives = **会话层深恢复**（按 session 回放原始轮次，L0/L1/L2 分层）
- 衔接点（P2）：`commit -m` 的默认 message 可由最近 commit_session 的
  L0 摘要自动生成 —— "记忆版本"带上来龙去脉。

---

## 6. 安全与隐私

- commit 快照 = 全部记忆明文（与 DB 同级敏感）。file remote 场景等同本地文件
  权限；https remote 必须 TLS + bearer（P1）。
- commit 快照**保留原文**（`redact: false`）—— 内部同步语义；敏感信息的
  安全在传输与存储层（file remote 目录权限、https TLS + bearer）。
  redact（`«redacted:…»` → `[REDACTED]`）只属于**对外分享导出**
  （export 命令默认开、`--no-redact` 关闭），**不进入 commit 快照**。
- agent_id 隔离 = 仓库隔离：`/agents/<agent_id>/` 路由 + token 映射（P1 细化）。
- checkout 自动备份落 `.cm/backups/` —— 回退本身可逆，误操作不丢数据。

---

## 7. 实施路径

**P0（~1 天，单机可验证）✅ 已完成（2026-09-05，7619d4b）**
- [x] `commands/git.rs`：commit / log / push / pull / clone / checkout /
      remote（file remote：裸路径与 file:// URL 均支持）
- [x] `.cm/` 读写（HEAD/refs/config 原子更新：写临时文件 + rename）
- [x] sha2 依赖 + 数据行 hash（meta 行、header 行不计入）
- [x] 单测：roundtrip（A commit → clone 到 B → 检索一致）、pull 两次幂等、
      快进拒绝、未提交记录保留、损坏对象拒绝、checkout 回退（含证伪态还原、
      备份生成、工作区记录丢弃）、remote add/list/remove
- [x] lib.rs dispatch + help 段

**P1（云端 + 状态对齐）✅ 已完成（2026-09-05，d0d3890/a857592/87d7fe1）**
- [x] **import align mode**：快照行带 valid_to/confidence → 本地同文本边
      UPDATE 状态而非 skip（补上 §4 的只增边界：forget/supersede 状态
      跨机传播，pull 才真正等于"状态对齐"；meta_edge.valid_from 同步携带）
- [x] https remote（server 挂 `/agents/<id>/objects|refs`，bearer 全开；
      PUT 校验 meta.hash == 对象名，客户端读时复核数据完整性）
- [x] `causal-memory cloud register/list/revoke`（token ↔ agent_id，
      admin token 管理，per-agent token 可吊销）
- [x] Docker 部署一页文档：[deploy-docker.md](deploy-docker.md)

**P2**
- [ ] Hermes provider 云模式（provider 指向 https remote 而非本地 wheel）
- [ ] **自动 commit**：provider on_session_end 触发 commit —— agent 场景
      无人敲命令，否则云端永不更新
- [ ] commit message 自动来自 session L0 摘要
- [ ] 计量（bootstrap/检索量挂钩 L1 商业化）

---

## 8. 验证清单（P0 验收）

```bash
# 双目录端到端（一台机器模拟换机）
causal-memory record "直推上线" "生产挂了" --tag deploy
causal-memory commit -m "学会：不直推"                    # → commit a1b2c3d
causal-memory remote add origin /tmp/remote-a
causal-memory push origin                                 # → pushed 1 commit(s)
# （"换机器"）
causal-memory clone /tmp/remote-a --db /tmp/new.db        # → edges 1 + 摘要
causal-memory ask "上线出问题" --db /tmp/new.db            # → 命中同一教训
causal-memory log                                          # → a1b2c3d 学会：不直推
causal-memory commit -m "空"                               # → nothing to commit
# （回退演示：新机器再记一条 → checkout 回到第一个 commit）
causal-memory record "再直推" "又挂了" --db /tmp/new.db
causal-memory commit -m "学会：还是别直推" --db /tmp/new.db # → commit e5f6a7b
causal-memory checkout a1b2c3d --db /tmp/new.db            # → edges 1 restored
causal-memory ask "再直推会怎样" --db /tmp/new.db           # → 不再命中（已回退）
```

---

## 9. 已知限制（MVP 接受，逐个有出路）

| 限制 | 影响 | 出路 |
|---|---|---|
| import 只增，状态变更不传播（R6） | pull 无法清除远端已证伪的教训 | P1 align mode（§7）；**checkout 不受此限**（整库重置） |
| dedup 含 event_time（R10） | 同文本跨机重复记录 → 近重复边 | P2 去 event_time 维度或记后清理 |
| pull 非事务（R4） | 中断半合并 | 幂等重试收敛 + 自动备份（§3.4） |
| graph 不随 import 重建（R5） | 长驻实例 pull 后查不到新数据 | 约定 pull 在无活跃实例时执行 |
| cooc/embeddings/q_value 不随快照（R9） | clone/checkout 后需重学 | 无损失（派生数据） |
| export/import 不携带 meta_edge.valid_from（R11） | import/checkout 后 meta 边 valid_from 置为 discovered_at（原值仅创建时=discovered_at，被 align 更新过才会漂移） | MVP 接受；P1 align mode 顺带携带 |

---

**修订记录**：
- 2026-09-05 review 后修订 —— R1 commit 快照 redact:false（§2.1/§3.1/§6）、
  R2 快照含证伪历史 include_invalidated:true（§2.1/§3.1）、R3 hash 精确定义
  无自指（§2.1）、R4/R5 pull 备份与可重入 + 活跃实例约定（§3.4）、R6 只增
  边界显式化 + P1 align mode（§4/§7）、R7 clone agent_id 解析（§3.5）、
  R8 provider 自动 commit（§7）、R10 dedup 边界（§4/§9）。
  验证依据：io.rs export/import 源码走读（chunk fnv1a(text) 内容寻址、
  edge dedup 含 event_time、redact 默认 true、import 逐行无整体事务）。
- 2026-09-05 开工前二轮修订 —— R11 **checkout 版本回退命令**（§1/§3.6/§4/
  §7/§8/§9；命名避开既有 restore <edge_id>）、R12 **export header 行不入快照**
  （§2.1/§3.1：exported_at 时间戳会污染内容寻址，破坏 nothing-to-commit 与
  push 去重）、R13 counts 术语对齐 io.rs（edges/meta_edges/chunks，无
  "facts" 行类型）、R14 `remote add/list/remove`（§3.7；clone 自动记 origin）、
  R15 meta_edge.valid_from 不随快照（§9）。验证依据：export_jsonl 头行
  exported_at=Utc::now()（io.rs:129）、import 空库无 dedup 跳过且 valid_to
  原样写入（io.rs:466/484）、ExportStats 字段（io.rs:107-112）、
  `restore` 命令已被 maintenance 占用（lib.rs:126-129）。
