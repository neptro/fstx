import { test } from 'node:test'
import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { createRequire } from 'node:module'
import { fileURLToPath } from 'node:url'

const require = createRequire(import.meta.url)
const { transaction, recover } = require('../index.js')
const here = path.dirname(fileURLToPath(import.meta.url))
const FILES = 200

function generations(root) {
  const gens = new Set()
  for (let i = 0; i < FILES; i++) {
    const s = fs.readFileSync(path.join(root, `f${i}`), 'utf8')
    assert.equal(s.length, 600, `f${i} is torn`)
    gens.add(s.slice(0, 12))
  }
  return gens
}

test('SIGKILL mid-commit: all files always agree after recover()', { timeout: 300_000 }, async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'fstx-node-crash-'))
  transaction(root, (tx) => {
    for (let i = 0; i < FILES; i++) tx.write(`f${i}`, '0'.repeat(600))
  })
  let seed = 0x2545f491
  let advanced = 0
  let last = 0
  for (let round = 0; round < 50; round++) {
    // Drop NODE_TEST_CONTEXT so the child runs as a plain script, not a test-runner child.
    const { NODE_TEST_CONTEXT, ...env } = process.env
    const child = spawn(process.execPath, [path.join(here, 'crash-child.mjs'), root, String(FILES)], { stdio: 'ignore', env })
    seed = (Math.imul(seed, 1103515245) + 12345) >>> 0
    await new Promise((r) => setTimeout(r, 50 + (seed % 400)))
    child.kill('SIGKILL')
    await new Promise((r) => child.once('exit', r))
    recover(root)
    const gens = generations(root)
    assert.equal(gens.size, 1, `round ${round}: files disagree: ${[...gens].join(', ')}`)
    const g = Number([...gens][0])
    assert.ok(g >= last, `generation went backwards ${last} -> ${g}`)
    if (g > last) advanced++
    last = g
  }
  assert.ok(advanced > 0, 'no commit ever completed; widen the kill window')
  console.log(`50 kills, generation advanced ${advanced} times (now ${last})`)
})
