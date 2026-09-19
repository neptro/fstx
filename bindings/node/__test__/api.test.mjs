import { test } from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { createRequire } from 'node:module'

const require = createRequire(import.meta.url)
const { Transaction, transaction, recover, inspect } = require('../index.js')

function scratch() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'fstx-node-'))
}
const read = (d, p) => fs.readFileSync(path.join(d, p), 'utf8')
const exists = (d, p) => fs.existsSync(path.join(d, p))

test('commit makes all changes visible at once', () => {
  const d = scratch()
  fs.writeFileSync(path.join(d, 'old.txt'), 'old')
  fs.writeFileSync(path.join(d, 'tmp.log'), 'log')
  const tx = Transaction.begin(d)
  tx.write('config.json', '{"v":2}')
  tx.createDirAll('src/gen')
  tx.rename('old.txt', 'src/gen/new.txt')
  tx.remove('tmp.log')
  assert.equal(exists(d, 'config.json'), false, 'nothing visible before commit')
  assert.equal(exists(d, 'tmp.log'), true)
  assert.equal(tx.finished, false)
  tx.commit()
  assert.equal(tx.finished, true)
  assert.equal(read(d, 'config.json'), '{"v":2}')
  assert.equal(read(d, 'src/gen/new.txt'), 'old')
  assert.equal(exists(d, 'old.txt'), false)
  assert.equal(exists(d, 'tmp.log'), false)
})

test('read-your-writes, Buffer data and mode', () => {
  const d = scratch()
  const tx = Transaction.begin(d)
  tx.write('bin.dat', Buffer.from([0, 1, 2, 255]))
  tx.write('u8.dat', new Uint8Array([9, 8]))
  tx.write('run.sh', '#!/bin/sh\n', { mode: 0o755 })
  assert.deepEqual([...tx.read('bin.dat')], [0, 1, 2, 255])
  assert.ok(tx.exists('run.sh'))
  tx.commit()
  assert.deepEqual([...fs.readFileSync(path.join(d, 'u8.dat'))], [9, 8])
  assert.equal(fs.statSync(path.join(d, 'run.sh')).mode & 0o777, 0o755)
})

test('discard leaves the tree unchanged and releases the lock', () => {
  const d = scratch()
  fs.writeFileSync(path.join(d, 'a'), 'a')
  const tx = Transaction.begin(d)
  tx.write('a', 'changed')
  tx.removeDirAll
  tx.discard()
  assert.equal(read(d, 'a'), 'a')
  // A second transaction can start immediately (the lock was released).
  const tx2 = Transaction.begin(d)
  tx2.write('b', 'b')
  tx2.commit()
  assert.equal(read(d, 'b'), 'b')
})

test('transaction() commits on return and discards on throw', () => {
  const d = scratch()
  const out = transaction(d, (tx) => {
    tx.write('x', '1')
    return 42
  })
  assert.equal(out, 42)
  assert.equal(read(d, 'x'), '1')
  assert.throws(
    () =>
      transaction(d, (tx) => {
        tx.write('x', '2')
        tx.write('y', 'y')
        throw new Error('boom')
      }),
    /boom/,
  )
  assert.equal(read(d, 'x'), '1')
  assert.equal(exists(d, 'y'), false)
})

test('async transaction() and commitAsync()', async () => {
  const d = scratch()
  const v = await transaction(d, async (tx) => {
    await new Promise((r) => setTimeout(r, 5))
    tx.write('async.txt', 'ok')
    return 'done'
  })
  assert.equal(v, 'done')
  assert.equal(read(d, 'async.txt'), 'ok')
  await assert.rejects(
    transaction(d, async (tx) => {
      tx.write('async.txt', 'nope')
      throw new Error('async boom')
    }),
    /async boom/,
  )
  assert.equal(read(d, 'async.txt'), 'ok')
  const tx = Transaction.begin(d)
  tx.write('c.txt', 'c')
  await tx.commitAsync()
  assert.equal(read(d, 'c.txt'), 'c')
  await assert.rejects(tx.commitAsync(), (e) => e.code === 'TRANSACTION_FINISHED')
})

test('errors carry stable codes', () => {
  const d = scratch()
  fs.writeFileSync(path.join(d, 'f'), 'f')
  fs.mkdirSync(path.join(d, 'dir'))
  fs.writeFileSync(path.join(d, 'dir/x'), 'x')
  const tx = Transaction.begin(d)
  const code = (fn) => {
    try {
      fn()
    } catch (e) {
      assert.ok(e instanceof Error)
      assert.ok(!e.message.startsWith('['), e.message)
      return e.code
    }
    return 'no error'
  }
  assert.equal(code(() => tx.remove('missing')), 'NOT_FOUND')
  assert.equal(code(() => tx.rename('f', 'dir')), 'ALREADY_EXISTS')
  assert.equal(code(() => tx.write('../escape', 'x')), 'INVALID_PATH')
  assert.equal(code(() => tx.write('.fstx/x', 'x')), 'INVALID_PATH')
  assert.equal(code(() => tx.remove('dir')), 'DIRECTORY_NOT_EMPTY')
  assert.equal(code(() => tx.write('f/sub', 'x')), 'NOT_A_DIRECTORY')
  assert.equal(code(() => tx.write('dir', 'x')), 'IS_A_DIRECTORY')
  assert.equal(code(() => tx.rename('dir', 'dir/x/y')), 'INVALID_MOVE')
  tx.commit()
  assert.equal(code(() => tx.write('g', 'g')), 'TRANSACTION_FINISHED')
  assert.equal(code(() => Transaction.begin(path.join(d, 'no-such-dir'))), 'IO')
  assert.equal(exists(path.dirname(d), 'escape'), false)
})

test('recover() and inspect() shapes', () => {
  const d = scratch()
  transaction(d, (tx) => tx.write('a', 'a'))
  assert.deepEqual(inspect(d), { transactions: [], garbage: [] })
  const r = recover(d)
  assert.deepEqual(r, { rolledBack: [], completed: [], discarded: [], garbageRemoved: 0 })
})
