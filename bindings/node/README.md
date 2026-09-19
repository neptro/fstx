# @neptro/fstx: atomic, crash-safe file transactions for Node.js

Change many files and directories **all at once or not at all**, even if your process is
killed or the machine loses power halfway through. Node.js bindings for the Rust library
[fstx](https://github.com/neptro/fstx).

```sh
npm install @neptro/fstx
```

```js
const { transaction } = require('@neptro/fstx')   // or: import { transaction } from '@neptro/fstx'

transaction('./my-project', (tx) => {
  tx.write('config.json', JSON.stringify({ version: 2 }))
  tx.createDirAll('src/generated')
  tx.rename('old.js', 'src/generated/new.js')
  tx.removeDirAll('cache')
}) // commits here, or changes nothing if the function throws
```

## Why

`fs.writeFileSync` and `write-file-atomic` make **one** file safe. Real programs change
several files together: installers, config tools, code generators, AI agents editing a
codebase. If they crash halfway, the folder is left half old and half new. With fstx, the
next run finds the folder exactly as it was before or exactly as committed. Never a mix.

## API

```ts
import { Transaction, transaction, recover, inspect } from '@neptro/fstx'

const tx = Transaction.begin(root)          // locks `root`, recovers earlier crashes
tx.write(path, data, { mode: 0o755 })       // data: string | Buffer | Uint8Array; mode optional
tx.read(path)                               // Buffer, sees staged changes
tx.exists(path)
tx.createDirAll(path)
tx.rename(from, to)                         // files or whole directories; fails if `to` exists
tx.remove(path)                             // file or empty directory
tx.removeDirAll(path)
tx.commit()                                 // atomic + durable
await tx.commitAsync()                      // same, off the main thread
tx.discard()                                // drop all changes, release the lock

transaction(root, (tx) => { ... })          // commit on return, discard on throw
await transaction(root, async (tx) => { ... })

recover(root)   // { rolledBack, completed, discarded, garbageRemoved }
inspect(root)   // { transactions: [{ name, action, reason, entries }], garbage }
```

- Paths are relative to `root`. `..`, absolute paths and the private `.fstx/` folder are
  rejected, and symlinks are never followed.
- Nothing changes on disk until `commit()`. Until then, `read()` shows the staged state.
- A transaction holds a lock on `root` until it is committed or discarded, so prefer
  `transaction()` or `try { ... } finally { if (!tx.finished) tx.discard() }`.
- fstx keeps a small `.fstx/` folder in `root`; it contains its own `.gitignore`.

## Errors

Errors are normal `Error`s with a stable `code`:

| code | meaning |
|---|---|
| `NOT_FOUND`, `ALREADY_EXISTS`, `NOT_A_DIRECTORY`, `IS_A_DIRECTORY`, `DIRECTORY_NOT_EMPTY` | the staged operation doesn't fit the tree |
| `INVALID_PATH` | absolute path, `..`, NUL byte, or `.fstx/` |
| `INVALID_MOVE` | moving a directory into itself |
| `UNSUPPORTED_FILE_TYPE` | operating on a symlink or special file |
| `CONFLICT` | a file was changed by another program before commit |
| `TRANSACTION_FINISHED` | using a transaction after commit/discard |
| `RECOVERY_REQUIRED` | an interrupted transaction needs attention; see `inspect(root)` |
| `COMMIT_OUTCOME_UNKNOWN` | the commit marker couldn't be confirmed; the next `recover()` decides |
| `UNSUPPORTED_FILESYSTEM`, `UNSUPPORTED_PLATFORM`, `IO` | environment problems |

## Platform

Linux only for now (x64 and arm64, glibc and musl). Node.js 20 or newer. On other operating
systems the package installs, but loading it fails with a clear error. TypeScript types
are included.

## How it works and what is tested

fstx stages changes in `.fstx/`, writes a durable journal, applies no-replace renames with
fsync barriers, and on recovery locates every file by its identity. The design and its
crash-safety arguments are in [DESIGN.md](https://github.com/neptro/fstx/blob/main/DESIGN.md).
The Rust core is tested with about 1.3 million simulated crash-recovery runs. This package
is tested with, among others, 50 real `SIGKILL`s of a Node process in mid-commit.

License: MIT OR Apache-2.0.
