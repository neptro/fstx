// Commits transactions forever; each rewrites FILES files to one generation number.
import { createRequire } from 'node:module'
import fs from 'node:fs'
import path from 'node:path'
const require = createRequire(import.meta.url)
const { Transaction } = require('../index.js')

const [root, files] = [process.argv[2], Number(process.argv[3])]
let gen = Number(fs.readFileSync(path.join(root, 'f0'), 'utf8').slice(0, 12))
for (;;) {
  gen += 1
  const tx = Transaction.begin(root)
  const body = String(gen).padStart(12, '0').repeat(50)
  for (let i = 0; i < files; i++) tx.write(`f${i}`, body)
  tx.commit()
}
