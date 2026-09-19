// Type-check only (tsc --noEmit): the public typings describe the real API.
import { Transaction, transaction, recover, inspect, type FstxError, type ErrorCode } from '../index.js'

const tx: Transaction = Transaction.begin('.', { allowUntestedFs: false })
tx.write('a.txt', 'text')
tx.write('b.bin', new Uint8Array([1]), { mode: 0o644 })
const buf: Buffer = tx.read('a.txt')
const ok: boolean = tx.exists('a.txt') && !tx.finished
tx.createDirAll('d')
tx.rename('a.txt', 'd/a.txt')
tx.remove('b.bin')
tx.removeDirAll('d')
void tx.commitAsync().catch((e: FstxError) => {
  const c: ErrorCode = e.code
  return c
})

const n: number = transaction('.', (t) => {
  t.write('x', 'y')
  return 1
})
const p: Promise<string> = transaction('.', async () => 'done')
const r = recover('.')
const rolled: string[] = r.rolledBack
const garbage: string[] = inspect('.').garbage
void [buf, ok, n, p, rolled, garbage]
