/**
 * Contract tests for the `ctx.causalMemory` programmatic seam.
 *
 * These run against the real `apply` from `../index.js` with a fake Cordis
 * context and a fake MCP client injected through the `createClient` test seam,
 * so the assertions cover the actual wiring (handshake order, effect scope,
 * option → MCP parameter mapping) rather than a copy of it.
 *
 * The fake context mirrors the two Cordis behaviours the seam depends on:
 *   - `ctx.effect(fn)` runs `fn` immediately and keeps its disposer;
 *   - `ctx.provide(name, value)` returns a disposer and rejects a second owner.
 */

import test from 'node:test'
import assert from 'node:assert/strict'

import { apply, SERVICE_NAME } from '../index.js'

/** Minimal stand-in for the tools the server advertises at handshake time. */
const TOOLS = [
  { name: 'search_memory', description: 'facts + causal', inputSchema: { type: 'object', properties: {} } },
  { name: 'search_causal', description: 'episodes', inputSchema: { type: 'object', properties: {} } },
  { name: 'record_decision', description: 'write an edge', inputSchema: { type: 'object', properties: {} } },
  { name: 'record_fact', description: 'write a fact', inputSchema: { type: 'object', properties: {} } },
]

/**
 * Build a fake ctx + fake MCP client pair.
 *
 * `options.connect` (throwing or resolved value), `options.tools` and
 * `options.call` let a test steer the handshake and the tool calls.
 */
function makeHarness(options = {}) {
  const services = new Map()
  const effects = []
  const registered = []
  const sections = []
  const logs = []
  const calls = []

  const ctx = {
    logger: {
      info: (...parts) => logs.push(['info', parts.join(' ')]),
      warn: (...parts) => logs.push(['warn', parts.join(' ')]),
      error: (...parts) => logs.push(['error', parts.join(' ')]),
      debug: (...parts) => logs.push(['debug', parts.join(' ')]),
    },
    effect(fn) {
      const cleanup = fn()
      effects.push(cleanup)
      return () => cleanup?.()
    },
    systemPrompt: {
      section(section) {
        sections.push(section)
        return () => {}
      },
    },
    tools: {
      register(definition) {
        registered.push(definition)
        return () => {}
      },
    },
    provide(serviceName, value) {
      if (services.has(serviceName)) {
        throw new Error(`service "${serviceName}" has been registered at <other-plugin>`)
      }
      services.set(serviceName, value)
      return () => {
        if (services.get(serviceName) === value) services.delete(serviceName)
      }
    },
    get(serviceName) {
      return services.get(serviceName)
    },
  }

  const client = {
    disposed: 0,
    async connect() {
      if (options.connect) return options.connect(ctx)
      return options.tools ?? TOOLS
    },
    async call(tool, args, signal) {
      calls.push({ tool, args, signal })
      if (options.call) return options.call(tool, args, signal)
      return `text:${tool}`
    },
    dispose() {
      this.disposed += 1
    },
  }

  return {
    ctx,
    client,
    services,
    effects,
    registered,
    sections,
    logs,
    calls,
    /** Tear everything down in reverse order, like Cordis unloading a fiber. */
    disposeAll() {
      for (let i = effects.length - 1; i >= 0; i -= 1) effects[i]?.()
    },
  }
}

function applyWith(harness, config = {}) {
  return apply(harness.ctx, { createClient: () => harness.client, ...config })
}

test('publishes the service only after the handshake resolved', async () => {
  let serviceAtHandshake
  let toolsAtHandshake
  const harness = makeHarness({
    connect: (ctx) => {
      // Runs mid-apply: nothing may be published or registered yet.
      serviceAtHandshake = ctx.get(SERVICE_NAME)
      toolsAtHandshake = harness.registered.length
      return TOOLS
    },
  })
  await applyWith(harness)

  assert.equal(serviceAtHandshake, undefined, 'service must not exist before tool discovery settles')
  assert.equal(toolsAtHandshake, 0, 'tools are registered after discovery too')

  const service = harness.ctx.get(SERVICE_NAME)
  assert.ok(service, 'ctx.causalMemory is published after a successful handshake')
  assert.equal(service.available, true)
  assert.equal(typeof service.search, 'function')
  assert.equal(typeof service.searchCausal, 'function')
  assert.equal(typeof service.record, 'function')
})

test('search maps camelCase options onto search_memory snake_case parameters', async () => {
  const harness = makeHarness()
  await applyWith(harness)
  const service = harness.ctx.get(SERVICE_NAME)

  const text = await service.search({ query: 'Redis mutex stampede', taskTag: 'goal-run' })
  assert.equal(text, 'text:search_memory')

  const { tool, args } = harness.calls.at(-1)
  assert.equal(tool, 'search_memory')
  assert.deepEqual(args, {
    query: 'Redis mutex stampede',
    task_tag: 'goal-run',
    limit: 5,
    detail_level: 'l0',
    max_tokens: 200,
  })
})

test('search honours an explicit token budget and detail level', async () => {
  const harness = makeHarness()
  await applyWith(harness)
  const service = harness.ctx.get(SERVICE_NAME)

  await service.search({ query: 'q', limit: 2, detailLevel: 'l2', maxTokens: 900 })
  assert.deepEqual(harness.calls.at(-1).args, {
    query: 'q',
    limit: 2,
    detail_level: 'l2',
    max_tokens: 900,
  })
})

test('search passes the abort signal straight through', async () => {
  const harness = makeHarness()
  await applyWith(harness)
  const controller = new AbortController()

  await harness.ctx.get(SERVICE_NAME).search({ query: 'q', signal: controller.signal })
  assert.equal(harness.calls.at(-1).signal, controller.signal)
})

test('searchCausal maps onto search_causal and allows a task_tag-only recall', async () => {
  const harness = makeHarness()
  await applyWith(harness)
  const service = harness.ctx.get(SERVICE_NAME)

  await service.searchCausal({ taskTag: 'goal-drift', maxTokens: 120 })
  assert.equal(harness.calls.at(-1).tool, 'search_causal')
  assert.deepEqual(harness.calls.at(-1).args, {
    task_tag: 'goal-drift',
    limit: 5,
    detail_level: 'l0',
    max_tokens: 120,
  })
})

test('record maps relation/task_tag/context/influenced_by in one call', async () => {
  const harness = makeHarness()
  await applyWith(harness)
  const service = harness.ctx.get(SERVICE_NAME)

  await service.record({
    decision: 'kept the cache layer read-only',
    outcome: 'acceptance command passed',
    relation: 'enabled',
    taskTag: 'goal-run',
    context: 'refactor phase, pnpm build is the acceptance gate',
    influencedBy: [12, 34],
  })

  assert.equal(harness.calls.at(-1).tool, 'record_decision')
  assert.deepEqual(harness.calls.at(-1).args, {
    decision: 'kept the cache layer read-only',
    outcome: 'acceptance command passed',
    relation: 'enabled',
    task_tag: 'goal-run',
    context: 'refactor phase, pnpm build is the acceptance gate',
    influenced_by: [12, 34],
  })
})

test('record omits optional keys instead of sending nulls', async () => {
  const harness = makeHarness()
  await applyWith(harness)
  const service = harness.ctx.get(SERVICE_NAME)

  await service.record({ decision: 'd', outcome: 'o', relation: 'caused', taskTag: 'goal-run' })
  assert.deepEqual(harness.calls.at(-1).args, {
    decision: 'd',
    outcome: 'o',
    relation: 'caused',
    task_tag: 'goal-run',
  })
})

test('search and record are required-argument validated', async () => {
  const harness = makeHarness()
  await applyWith(harness)
  const service = harness.ctx.get(SERVICE_NAME)

  await assert.rejects(() => service.search({}), /causalMemory\.search: query must be a non-empty string/)
  await assert.rejects(() => service.search(null), /causalMemory\.search: query must be a non-empty string/)
  await assert.rejects(
    () => service.record({ decision: 'd', outcome: 'o', relation: 'caused' }),
    /causalMemory\.record: taskTag must be a non-empty string/,
  )
  await assert.rejects(
    () => service.record({ decision: 'd', outcome: 'o', relation: 'caused', taskTag: 't', influencedBy: 'nope' }),
    /influencedBy must be an array/,
  )
  await assert.rejects(
    () => service.record({ decision: 'd', outcome: 'o', relation: 'caused', taskTag: 't', influencedBy: ['x'] }),
    /influencedBy contains a non-numeric id/,
  )
  assert.equal(harness.calls.length, 0, 'no RPC is attempted for invalid arguments')
})

test('a failing MCP tool call rejects with the server error', async () => {
  const harness = makeHarness({
    call: () => {
      throw new Error('causal-memory tool search_memory failed: db locked')
    },
  })
  await applyWith(harness)
  const service = harness.ctx.get(SERVICE_NAME)

  await assert.rejects(() => service.search({ query: 'q' }), /db locked/)
  await assert.rejects(() => service.record({ decision: 'd', outcome: 'o', relation: 'caused', taskTag: 't' }), /db locked/)
})

test('unloading the plugin removes the service with the tools', async () => {
  const harness = makeHarness()
  await applyWith(harness)
  assert.ok(harness.ctx.get(SERVICE_NAME), 'published while the plugin is loaded')

  harness.disposeAll()

  assert.equal(harness.ctx.get(SERVICE_NAME), undefined, 'unload unregisters ctx.causalMemory')
  assert.equal(harness.client.disposed, 1, 'unload kills the MCP child')
})

test('failOnStartupError:false activates without tools and without a service', async () => {
  const harness = makeHarness({ connect: () => { throw new Error('spawn causal-memory ENOENT') } })
  await applyWith(harness, { failOnStartupError: false })

  assert.equal(harness.ctx.get(SERVICE_NAME), undefined, 'no handshake, no service')
  assert.equal(harness.registered.length, 0, 'no handshake, no tools')
  assert.equal(harness.sections.length, 1, 'prompt section still installed (preserved behaviour)')
  assert.ok(harness.logs.some(([level, text]) => level === 'error' && /connection or tool discovery failed/.test(text)))
})

test('the default failOnStartupError:true rejects and never publishes', async () => {
  const harness = makeHarness({ connect: () => { throw new Error('spawn causal-memory ENOENT') } })
  await assert.rejects(() => applyWith(harness), /spawn causal-memory ENOENT/)
  assert.equal(harness.ctx.get(SERVICE_NAME), undefined)
})

test('a name collision while publishing degrades instead of failing the plugin', async () => {
  const harness = makeHarness()
  // Simulate another fiber already owning the name (Cordis throws from provide).
  harness.ctx.provide(SERVICE_NAME, { available: true })

  await applyWith(harness)

  assert.ok(harness.logs.some(([level, text]) => level === 'warn' && /could not publish ctx\.causalMemory/.test(text)))
  assert.equal(harness.registered.length, TOOLS.length, 'tools still registered')
  assert.equal(harness.sections.length, 1, 'prompt section still installed')
})

test('existing behaviour is untouched: exclusions and the prompt section', async () => {
  const harness = makeHarness()
  await applyWith(harness, { exclude: ['record_fact'] })

  assert.deepEqual(harness.registered.map((tool) => tool.name), ['search_memory', 'search_causal', 'record_decision'])
  const [section] = harness.sections
  assert.equal(section.name, 'causal-memory')
  assert.equal(section.order, 300)
  assert.match(section.text, /^## Causal memory/)
  // The tool definitions keep their canonical MCP-shaped output contract.
  const searchTool = harness.registered.find((tool) => tool.name === 'search_memory')
  assert.equal(searchTool.output.schema.required[0], 'content')
  assert.deepEqual(searchTool.output.render({}, { content: [{ type: 'text', text: 'hi' }] }), [{ type: 'text', text: 'hi' }])
})
