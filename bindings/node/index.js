'use strict'
// Public entry point. Wraps the native binding (binding.js) to:
//  - turn "[CODE] message" errors into errors with `err.code`
//  - add Transaction#commitAsync() and the transaction() helper

const native = require('./binding.js')

const TAG = /^\[([A-Z_]+)\] /

function withCode(err) {
  if (err instanceof Error) {
    const m = TAG.exec(err.message)
    if (m) {
      err.code = m[1]
      err.message = err.message.slice(m[0].length)
    }
  }
  return err
}

function wrap(fn) {
  return function (...args) {
    try {
      return fn.apply(this, args)
    } catch (err) {
      throw withCode(err)
    }
  }
}

const Transaction = native.Transaction
for (const name of Object.getOwnPropertyNames(Transaction.prototype)) {
  const d = Object.getOwnPropertyDescriptor(Transaction.prototype, name)
  if (name !== 'constructor' && typeof d.value === 'function') {
    Object.defineProperty(Transaction.prototype, name, { ...d, value: wrap(d.value) })
  }
}
Object.defineProperty(Transaction, 'begin', {
  value: wrap(Transaction._begin),
  writable: true,
  configurable: true,
})

/** Commits on a worker thread so fsyncs don't block the event loop. */
Transaction.prototype.commitAsync = async function commitAsync() {
  const out = await this._commitInWorker()
  if (!out.ok) {
    const err = new Error(out.message)
    err.code = out.code
    throw err
  }
}

/**
 * Runs `fn(tx)`; commits if it returns (or its promise resolves), discards if it throws.
 */
function transaction(root, fn, options) {
  const tx = Transaction.begin(root, options)
  let result
  try {
    result = fn(tx)
  } catch (err) {
    tx.discard()
    throw err
  }
  if (result && typeof result.then === 'function') {
    return result.then(
      async (value) => {
        await tx.commitAsync()
        return value
      },
      (err) => {
        tx.discard()
        throw err
      },
    )
  }
  tx.commit()
  return result
}

const recover = wrap(native.recover)
const inspect = wrap(native.inspect)

// Plain shorthand properties so Node can detect named exports for `import { ... }`.
module.exports = { Transaction, transaction, recover, inspect }
