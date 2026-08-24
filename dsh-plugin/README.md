# causal-memory DSH plugin

DeepSeek Harness 原生记忆插件：把 causal-memory 的 16 个工具以**干净命名**
（无 `mcp__` 前缀）挂到 DSH 的 `ctx.tools`，并注入一条系统提示词段落
（order 300），告诉模型何时查阅因果记忆库。

## 前置：causal-memory 二进制

插件按 DSH 惯例解析服务端二进制（不写死路径），二选一：

1. **本仓库开发构建**（克隆到任意位置即可）：
   ```bash
   cargo build --release --bin causal-memory
   ```
   插件会自动找到 `<仓库>/target/release/causal-memory`。
2. **PATH 全局安装**（发布二进制）：从 GitHub Releases 下载对应平台的
   `causal-memory` 放到 PATH 上（如 `~/bin` 或 `/usr/local/bin`），插件回退到
   PATH 查找裸名 `causal-memory`。

## 安装

```bash
cd <causal-memory 仓库路径>
dsh plugin --profile web add "$PWD/dsh-plugin"
```

以 `link:` 方式安装（pnpm link）——修改本目录代码即时生效，无需重装。

## 启用

在 `~/.dsh/profiles/web/cordis.patch.yml` 的 insert 列表中加入：

```yaml
- id: causal-memory-plugin
  name: causal-memory-dsh-plugin
```

重启 dsh web 后生效。

## 配置（可选，均省略时有合理默认）

| 字段 | 默认 | 说明 |
|---|---|---|
| `command` | 自动解析 | 二进制路径：`config.command` → `CAUSAL_MEMORY_BIN` → 仓库 `target/release` → PATH 裸名 |
| `dbPath` | `~/.local/share/causal-memory/causal.db` | SQLite 路径（或 `CAUSAL_MEMORY_DB`） |
| `toolCallTimeoutMs` | `60000` | 单次工具调用超时 |
| `exclude` | `[]` | 不挂载的工具名数组 |
| `failOnStartupError` | `false` | 启动连接失败时是否让插件激活失败（默认仅记日志） |

## 工具清单（16 个）

`record_decision` · `record_fact` · `remember` · `search_causal` ·
`search_facts` · `search_memory` · `search_patterns` · `causal_directory` ·
`trace_cause` · `trace_cause_chain` · `intervention_query` ·
`counterfactual_query` · `invalidate_decision` · `invalidate_pattern` ·
`resolve_updates` · `reconstruct_lesson`

（工具列表运行时从服务端动态发现，服务端升级后无需改插件。）

## 卸载

```bash
dsh plugin --profile web remove causal-memory-dsh-plugin
# 并从 ~/.dsh/profiles/web/cordis.patch.yml 删除对应 insert 行
```

## 设计说明

- 零运行时依赖：只用 Node 内置模块；JSON-RPC over stdio 直接与
  causal-memory 二进制通信（rmcp 的新行分隔 JSON 线协议）。
- `apply` 为 async：与 DSH 官方 `@deepseek-ai/dsh-mcp-client` 同款时序
  （cordis 不等待 async apply 的 promise，工具在激活后异步就位）。
- 所有注册均为 effect 作用域：卸载时注销工具并终止子进程。
- 与 MCP 桥（`@deepseek-ai/dsh-mcp-client`，工具名 `mcp__causal-memory__*`）
  二选一启用，避免双份工具。
