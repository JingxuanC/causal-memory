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
import { homedir } from 'node:os'
import { join } from 'node:path'

export const name = 'causal-memory'

/** Services this plugin requires. */
export const inject = ['tools', 'systemPrompt']

const DEFAULT_COMMAND = '/Users/hjx/project/causal-memory/target/release/causal-memory'
const DEFAULT_DB = () => join(homedir(), '.local', 'share', 'causal-memory', 'causal.db')
const DEFAULT_TIMEOUT_MS = 60_000

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
  const command = config.command ?? process.env.CAUSAL_MEMORY_BIN ?? DEFAULT_COMMAND
  const dbPath = config.dbPath ?? process.env.CAUSAL_MEMORY_DB ?? DEFAULT_DB()
  const timeoutMs = config.toolCallTimeoutMs ?? DEFAULT_TIMEOUT_MS
  const exclude = new Set(Array.isArray(config.exclude) ? config.exclude : [])
  const failOnStartupError = config.failOnStartupError === true

  const client = createMcpClient(command, { ...process.env, CAUSAL_MEMORY_DB: dbPath }, timeoutMs)

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
      + 'restart. Use it: before a non-trivial decision, query past episodes '
      + 'with search_causal or compare alternatives with counterfactual_query; '
      + 'before acting, forecast outcomes with intervention_query; after acting '
      + 'on a decision, record the outcome with record_decision; record stable '
      + 'user or project facts with record_fact; when something fails, trace '
      + 'the cause with trace_cause or trace_cause_chain.',
  })

  // Discover tools; activation blocks until this settles.
  let tools
  try {
    tools = await client.connect()
  } catch (error) {
    ctx.logger.error(`causal-memory: connection or tool discovery failed: ${error.message}`)
    if (failOnStartupError) throw error
    return // activated with the prompt section but no tools
  }

  const selected = tools.filter((tool) => !exclude.has(tool.name))
  ctx.logger.info(`causal-memory: discovered ${tools.length} tools, registering ${selected.length} (${toolNames(selected)})`)

  for (const tool of selected) {
    const definition = {
      name: tool.name,
      description: tool.description,
      parameters: tool.inputSchema ?? {},
      execute: async (args, exec) => {
        return client.call(tool.name, args, exec?.signal)
      },
    }
    disposers.push(ctx.tools.register(definition))
  }
}
