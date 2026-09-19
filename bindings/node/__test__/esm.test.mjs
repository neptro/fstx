import { test } from 'node:test'
import assert from 'node:assert/strict'
// Named ESM imports of the CommonJS entry point must work (cjs-module-lexer detection).
import { Transaction, transaction, recover, inspect } from '../index.js'

test('named ESM imports resolve', () => {
  for (const f of [Transaction, transaction, recover, inspect]) assert.equal(typeof f, 'function')
  assert.equal(typeof Transaction.begin, 'function')
})
