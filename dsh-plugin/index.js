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
 *
 * Programmatic seam: after a successful handshake the same client is published
 * as the Cordis service `ctx.causalMemory` (`search` / `searchCausal` /
 * `record`), so other plugins can recall and record without going through the
 * model-facing tools. It rides the plugin's own fiber (`ctx.provide`), so it
 * disappears together with the tools on unload.
 */

import { spawn } from 'node:child_process'
import { existsSync } from 'node:fs'
import { homedir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

export const name = 'causal-memory'

/** Services this plugin requires. */
export const inject = ['tools', 'systemPrompt']

/** The Cordis service this plugin publishes for programmatic recall/record. */
export const SERVICE_NAME = 'causalMemory'

const PLUGIN_DIR = dirname(fileURLToPath(import.meta.url))
const DEFAULT_DB = () => join(homedir(), '.local', 'share', 'causal-memory', 'causal.db')
const DEFAULT_TIMEOUT_MS = 60_000

/**
 * Whether the server has an embedding endpoint to build the semantic leg of
 * retrieval. Read at apply time so the prompt section describes the retrieval
 * the store actually has, instead of asserting a snapshot of which tools are
 * "usable" — a claim that goes stale as the graph grows.
 */
const embedLive = Boolean(
  process.env.CAUSAL_MEMORY_EMBED_API || process.env.CAUSAL_MEMORY_LLM_API,
)

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

/*
 * Service defaults. The MCP tools default to limit=10 / detail_level=l2 /
 * max_tokens=0 (unlimited); a programmatic caller is almost always a plugin
 * injecting recall into a prompt, so the seam defaults to the tight budget
 * (5 pointer-level hits, ≤200 tokens) and lets the caller widen it.
 */
const SERVICE_LIMIT = 5
const SERVICE_DETAIL_LEVEL = 'l0'
const SERVICE_MAX_TOKENS = 200

function requireText(value, method, field) {
  if (typeof value !== 'string' || value.trim() === '') {
    throw new TypeError(`causalMemory.${method}: ${field} must be a non-empty string`)
  }
  return value
}

/**
 * Map the seam's camelCase options onto `search_memory` / `search_causal`'s
 * real MCP inputSchema keys — the server declares them snake_case
 * (`task_tag`, `detail_level`, `max_tokens`), and passing camelCase silently
 * falls back to defaults instead of erroring, which would look like "no
 * memories" rather than "wrong parameter".
 */
function searchArgs(options, method, { queryRequired }) {
  const args = {
    limit: options.limit ?? SERVICE_LIMIT,
    detail_level: options.detailLevel ?? SERVICE_DETAIL_LEVEL,
    max_tokens: options.maxTokens ?? SERVICE_MAX_TOKENS,
  }
  if (options.query !== undefined && options.query !== null) {
    args.query = queryRequired
      ? requireText(options.query, method, 'query')
      : String(options.query)
  } else if (queryRequired) {
    requireText(options.query, method, 'query') // throws with the canonical message
  }
  if (options.taskTag !== undefined && options.taskTag !== null) {
    args.task_tag = String(options.taskTag)
  }
  return args
}

/** Map the seam's `record` options onto `record_decision`'s inputSchema keys. */
function recordArgs(options) {
  const args = {
    decision: requireText(options.decision, 'record', 'decision'),
    outcome: requireText(options.outcome, 'record', 'outcome'),
    relation: requireText(options.relation, 'record', 'relation'),
    task_tag: requireText(options.taskTag, 'record', 'taskTag'),
  }
  if (options.context !== undefined && options.context !== null) {
    args.context = String(options.context)
  }
  if (options.influencedBy !== undefined && options.influencedBy !== null) {
    if (!Array.isArray(options.influencedBy)) {
      throw new TypeError('causalMemory.record: influencedBy must be an array of edge ids')
    }
    args.influenced_by = options.influencedBy.map((id) => {
      const edgeId = Number(id)
      if (!Number.isFinite(edgeId)) {
        throw new TypeError(`causalMemory.record: influencedBy contains a non-numeric id: ${String(id)}`)
      }
      return edgeId
    })
  }
  return args
}

/**
 * The `ctx.causalMemory` value: a thin, typed wrapper over the live MCP client.
 *
 * Deliberately not a cache and not a second retrieval implementation — every
 * method delegates to `client.call`, so the seam inherits exactly the tools'
 * ranking, token budgets and error semantics (an `isError` tool result rejects
 * here too, instead of looking like an empty result).
 *
 *   search({ query, taskTag?, limit?, detailLevel?, maxTokens?, signal? })
 *     → Promise<string>   (search_memory: facts + causal, RRF-fused)
 *   searchCausal({ query?, taskTag?, limit?, detailLevel?, maxTokens?, signal? })
 *     → Promise<string>   (search_causal: episodes only)
 *   record({ decision, outcome, relation, taskTag, context?, influencedBy?, signal? })
 *     → Promise<string>   (record_decision)
 *
 * `available: true` means "this session has a live causal-memory connection";
 * a dead child is never advertised under this name.
 */
function createCausalMemoryService(call) {
  return {
    available: true,
    async search(options) {
      const opts = options ?? {}
      return call('search_memory', searchArgs(opts, 'search', { queryRequired: true }), opts.signal)
    },
    async searchCausal(options) {
      const opts = options ?? {}
      return call('search_causal', searchArgs(opts, 'searchCausal', { queryRequired: false }), opts.signal)
    },
    async record(options) {
      const opts = options ?? {}
      return call('record_decision', recordArgs(opts), opts.signal)
    },
  }
}

/**
 * @param ctx - plugin context (injected services `tools` + `systemPrompt`).
 * @param config - optional config: `command`, `dbPath`, `toolCallTimeoutMs`,
 *   `exclude` (array of raw tool names to skip), `failOnStartupError`,
 *   `createClient` (tests only: replaces `createMcpClient`, never set in YAML).
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

  // Test seam: `createClient` lets a unit test run `apply` against a fake
  // child. It has no YAML representation, so production callers never set it.
  const makeClient = typeof config.createClient === 'function' ? config.createClient : createMcpClient
  const client = makeClient(command, childEnv, timeoutMs)

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
      + (exclude.has('remember')
        ? 'All tools are live except `remember`, which this deployment excludes because the '
          + 'name is already taken in the host tool registry — record facts with `record_fact` '
          + 'instead.\n'
        : 'All tools are live.\n')
      + 'Retrieval and its failure modes — read this before concluding a tool is broken:\n'
      + (embedLive
        ? '- Retrieval is BM25 fused with semantic cosine over embeddings.\n'
        : '- Retrieval is BM25 keyword matching only (no embedding endpoint configured), so '
          + 'QUOTE THE ORIGINAL WORDING: reuse the exact words a lesson was recorded with, '
          + 'especially the outcome polarity (成功 / 失败 / 无效 / no_effect). An abstract '
          + 'paraphrase can miss an edge that exists — that is a phrasing mismatch, not a '
          + 'missing memory. Configure CAUSAL_MEMORY_EMBED_* to add the semantic leg.\n')
      + '- `trace_cause` is single-hop and needs lexical overlap with the recorded OUTCOME '
      + 'text; if it returns nothing, retry with the original wording before giving up.\n'
      + '- `trace_cause_chain` returns empty until the graph holds multi-hop chains (it needs '
      + '>1 hop). An empty result means "not enough depth yet", never "broken".\n'
      + '- `search_patterns` returns empty until pattern mining has produced meta-edges. An '
      + 'empty result means "not mined yet", never "broken". Neither tool is worth skipping '
      + 'permanently: the store grows.\n'
      + '- `counterfactual_query` matches DECISION text — put the alternative you are weighing '
      + 'verbatim in the decision string, or it returns insufficient evidence.\n'
      + '- A prediction from `counterfactual_query` resolves only when a later `record_decision` '
      + 'under the SAME task_tag carries matching text; comparing two already-recorded options '
      + 'leaves it pending by design. An empty report is not a broken ledger.\n'
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
      + '- Something failed → trace_cause (single-hop). Retry it with the recorded outcome '
      + 'wording if the first attempt comes back empty.\n'
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

  // Programmatic seam: publish `ctx.causalMemory` for other plugins (goal-guard
  // recalls on compaction completion). Three deliberate properties:
  //   - only after the handshake above succeeded, so a dead child is never
  //     advertised (failOnStartupError:false returned long before this line);
  //   - inside `ctx.effect`, i.e. the same fiber scope as the tools, so unload
  //     unregisters the service together with them (ctx.provide is itself
  //     fiber-scoped; the explicit effect keeps the owner unambiguous);
  //   - failure to publish is logged, never thrown — a duplicate name must not
  //     take the whole plugin (and therefore the session) down.
  try {
    const service = createCausalMemoryService((tool, args, signal) => client.call(tool, args, signal))
    ctx.effect(() => ctx.provide(SERVICE_NAME, service), `ctx.provide(${JSON.stringify(SERVICE_NAME)})`)
  } catch (error) {
    ctx.logger.warn(`causal-memory: could not publish ctx.${SERVICE_NAME}: ${error.message}`)
  }
}
