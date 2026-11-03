/**
 * Entry point for `node --test <this directory>`.
 *
 * Node's test runner normally recurses into a directory positional, but the
 * Node build used in this workspace resolves the path as a module instead and
 * fails with MODULE_NOT_FOUND. Because the module loader resolves a directory
 * to its index, this file makes the documented acceptance command
 * (`node --test causal-memory/dsh-plugin/test/`) execute the real suite.
 *
 * It is pure re-export: no assertions live here, and on Node versions that do
 * scan directories `index.js` is not one of the runner's test-file patterns,
 * so the suite still runs exactly once.
 */

import './service.test.mjs'
