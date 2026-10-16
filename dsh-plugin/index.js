/**
 * causal-memory — DeepSeek Harness 原生记忆插件
 *
 * A Cordis plugin that bridges the causal-memory Rust binary over MCP stdio and
 * publishes its tools on `ctx.tools` under their clean raw names (no
 * `mcp__<server>__` prefix), plus a system-prompt section that tells the model
 * when to consult the causal store.
 *
 * Zero runtime dependencies: only Node built-ins. The plugin talks JSON-RPC
 * over the child's stdio itself (newline-delimited JSON, the wire format rmcp
 * uses), so installing it needs no extra npm packages.
 *
 * Lifecycle: `apply` is async — the fiber activates only after the initial
 * MCP handshake and tool discovery settle. Everything registered is
 * effect-scoped; disposal kills the child and unregisters all tools.
 */

import { spawn } from 'node:child_process'
import { existsSync } from 'node:fs'
import { homedir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

export const name = 'causal-memory'

/** Services this plugin requires. */
export const inject = ['tools', 'systemPrompt']

const PLUGIN_DIR = dirname(fileURLToPath(import.meta.url))
const DEFAULT_DB = () => join(homedir(), '.local', 'share', 'causal-memory', 'causal.db')
const DEFAULT_TIMEOUT_MS = 60_000

/**
 * Resolve the causal-memory server binary. DSH convention (see the official
 * @deepseek-ai/dsh-mcp-client + examples/mcp-memory): never hardcode an
 * absolute build path — the machine layout differs. Resolution chain:
 *   1. config.command        (cordis overlay, highest priority)
 *   2. CAUSAL_MEMORY_BIN     (ambient env override)
 *   3. <plugin>/../target/release/causal-memory  (this repo checked out
 *      anywhere — the dev workflow: cargo build --release --bin causal-memory)
 *   4. bare `causal-memory`  (resolved from PATH — `pip install causal-memory`
 *      puts the console script there; no Rust toolchain needed)
 */
function resolveCommand(explicit) {
  if (explicit) return explicit
  const envBin = process.env.CAUSAL_MEMORY_BIN
  if (envBin) return envBin
  const suffix = process.platform === 'win32' ? '.exe' : ''
  const repoBin = join(PLUGIN_DIR, '..', 'target', 'release', `causal-memory${suffix}`)
  if (existsSync(repoBin)) return repoBin
  return `causal-memory${suffix}` // resolved from PATH by the OS
}

/**
 * Minimal newline-delimited JSON-RPC client over a child's stdio.
 * Handles requests (id) and ignores notifications. Resolves on `result`,
 * rejects on `error` or timeout.
 */
function createMcpClient(command, env, timeoutMs) {
  const child = spawn(command, [], {
    stdio: ['pipe', 'pipe', 'pipe'],
    env,
  })

  let buf = ''
  const pending = new Map()
  let seq = 0

  child.stdout.on('data', (d) => {
    buf += d.toString()
    let idx
    while ((idx = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, idx).trim()
      buf = buf.slice(idx + 1)
      if (!line) continue
      let msg
      try { msg = JSON.parse(line) } catch { continue }
      if (msg.id !== undefined && pending.has(msg.id)) {
        pending.get(msg.id)(msg)
        pending.delete(msg.id)
      }
    }
  })

  const rpc = (method, params = {}) => {
    const id = ++seq
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        pending.delete(id)
        reject(new Error(`causal-memory: timeout on ${method}`))
      }, timeoutMs)
      pending.set(id, (msg) => {
        clearTimeout(timer)
        if (msg.error) reject(new Error(msg.error.message ?? 'causal-memory rpc error'))
        else resolve(msg.result)
      })
      child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n')
    })
  }

  const notify = (method, params = {}) => {
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', method, params }) + '\n')
  }

  return {
    child,
    /** Resolve the MCP handshake and return the discovered tool list. */
    async connect() {
      await rpc('initialize', {
        protocolVersion: '2025-03-26',
        capabilities: {},
        clientInfo: { name: 'dsh-causal-memory-plugin', version: '1.0.0' },
      })
      notify('notifications/initialized')
      const list = await rpc('tools/list', {})
      return list.tools ?? []
    },
    async call(toolName, args, signal) {
      if (signal?.aborted) throw new Error(`causal-memory: ${toolName} aborted before call`)
      const result = await rpc('tools/call', { name: toolName, arguments: args ?? {} })
      const text = (result.content ?? [])
        .filter((block) => block.type === 'text' && typeof block.text === 'string')
        .map((block) => block.text)
        .join('\n')
      if (result.isError === true) throw new Error(text || `causal-memory tool ${toolName} failed`)
      return text
    },
    dispose() {
      try { child.kill() } catch { /* already gone */ }
    },
  }
}

/** Plain-text description of a tool's schema, for the discovery log only. */
function toolNames(tools) {
  return tools.map((t) => t.name).join(', ')
}

/**
 * @param ctx - plugin context (injected services `tools` + `systemPrompt`).
 * @param config - optional config: `command`, `dbPath`, `toolCallTimeoutMs`,
 *   `exclude` (array of raw tool names to skip), `failOnStartupError`.
 */
export async function apply(ctx, config = {}) {
  const command = resolveCommand(config.command)
  const dbPath = config.dbPath ?? process.env.CAUSAL_MEMORY_DB ?? DEFAULT_DB()
  const timeoutMs = config.toolCallTimeoutMs ?? DEFAULT_TIMEOUT_MS
  const exclude = new Set(Array.isArray(config.exclude) ? config.exclude : [])
  const failOnStartupError = config.failOnStartupError !== false

  // Extra env for the child. Needed because the server reads its LLM / embed
  // configuration from the environment (CAUSAL_MEMORY_LLM_API / _KEY / _MODEL,
  // CAUSAL_MEMORY_EMBED_*). Without this the tools that delegate to an LLM —
  // resolve_updates, reconstruct_lesson's narrative + calibration, and the
  // extraction path behind `remember` — silently degrade to their rule-based
  // fallbacks. Values in config.env win over the ambient environment; non-string
  // values are dropped rather than coerced, so `!!js` expressions that evaluate
  // to undefined cannot kill the child at spawn.
  const configEnv = config.env && typeof config.env === 'object' && !Array.isArray(config.env)
    ? config.env
    : {}
  const envFromConfig = Object.fromEntries(
    Object.entries(configEnv)
      .filter(([, value]) => typeof value === 'string' && value.length > 0),
  )
  const droppedEnvKeys = Object.keys(configEnv).filter((key) => !(key in envFromConfig))
  if (droppedEnvKeys.length > 0) {
    ctx.logger.warn(`causal-memory: ignored non-string config.env value(s): ${droppedEnvKeys.join(', ')}`)
  }
  const childEnv = { ...process.env, ...envFromConfig, CAUSAL_MEMORY_DB: dbPath }

  const client = createMcpClient(command, childEnv, timeoutMs)

  // Cleanup: kill the child and unregister tools when this plugin unloads.
  const disposers = []
  ctx.effect(() => {
    return () => {
      for (const dispose of disposers) dispose()
      client.dispose()
    }
  }, 'causal-memory.connection')

  // The system-prompt section is the model's standing knowledge of the store:
  // when to consult it, and that lessons survive compaction.
  ctx.systemPrompt.section({
    name: 'causal-memory',
    order: 300,
    text: '## Causal memory\n'
      + 'You have a persistent causal-memory store (SQLite at '
      + `${dbPath}) that records stable facts and decision → outcome causal `
      + 'edges with typed relations (caused / enabled / prevented). It lives '
      + 'outside the context window: recorded lessons survive compaction and '
      + 'restart.\n'
      + 'What is actually usable (verified 2026-10-16, 10 of 16 tools carry real value): '
      + 'search_causal / record_decision / record_fact / search_facts / search_memory / '
      + 'causal_directory / trace_cause / invalidate_decision / invalidate_pattern / '
      + 'prediction_report are live. counterfactual_query only matches DECISION text — put '
      + 'the alternative you are weighing verbatim in the decision string, or it returns '
      + 'insufficient evidence. trace_cause_chain and search_patterns return empty on this '
      + 'store (flat graph, empty pattern table); do not spend calls on them.\n'
      + 'Trigger rules — apply these deterministically, not "when it seems relevant":\n'
      + '- Starting a non-trivial task → search_causal (pass task_tag; add query text for broader recall).\n'
      + '- Weighing A against B, before choosing → counterfactual_query(decision=A, alternative=B).\n'
      + '- Before an irreversible or high-risk action (editing production config, deleting data, '
      + 'running heavy commands on shared hosts) → intervention_query.\n'
      + '- A decision has produced an outcome → record_decision, and ALWAYS pass context (a short '
      + 'description of the situation): same task_tag + context becomes a comparable branch for '
      + 'same-world counterfactual evidence. Without context the fork is never created.\n'
      + '- A new stable fact appears (user preference, config, project fact) → record_fact. '
      + 'Keep keys narrow and semantic — replace_same_key retires EVERY valid entry under that '
      + 'key, so never use it with a catch-all key like "config".\n'
      + '- Something failed → trace_cause (single-hop, works).\n'
      + '- Wrapping up a task, or every few turns → prediction_report.',
  })

  // Discover tools; activation blocks until this settles.
  let tools
  try {
    tools = await client.connect()
  } catch (error) {
    ctx.logger.error(`causal-memory: connection or tool discovery failed: ${error.message}`
      + ' — install the binary with `pip install causal-memory` (or `cargo build --release`'
      + ' in the causal-memory repo), or point config.command / CAUSAL_MEMORY_BIN at it')
    if (failOnStartupError) throw error
    return // failOnStartupError: false — activated with the prompt section but no tools
  }

  const selected = tools.filter((tool) => !exclude.has(tool.name))
  ctx.logger.info(`causal-memory: discovered ${tools.length} tools, registering ${selected.length} (${toolNames(selected)})`)

  for (const tool of selected) {
    const definition = {
      name: tool.name,
      description: tool.description ?? '',
      parameters: tool.inputSchema ?? { type: 'object', properties: {} },
      // Current dsh tool API: a mandatory canonical output contract. The
      // canonical value is MCP-shaped ({ content: [...] }); render projects
      // it to the model-facing text blocks (mirrors dsh-mcp-client).
      output: {
        schema: {
          type: 'object',
          properties: { content: { type: 'array', items: {} } },
          required: ['content'],
          additionalProperties: false,
        },
        render(_args, value) {
          const blocks = Array.isArray(value?.content) ? value.content : []
          const text = blocks
            .filter((b) => b && b.type === 'text' && typeof b.text === 'string')
            .map((b) => b.text)
            .join('\n')
          return [{ type: 'text', text: text || '(no output)' }]
        },
      },
      execute: async (args, exec) => {
        const text = await client.call(tool.name, args, exec?.signal)
        return { content: [{ type: 'text', text }] }
      },
    }
    disposers.push(ctx.tools.register(definition))
  }
}
