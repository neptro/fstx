# fstx: atomic, crash-safe file transactions for Rust

[![CI](https://github.com/neptro/fstx/actions/workflows/ci.yml/badge.svg)](https://github.com/neptro/fstx/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
![Platform: Linux](https://img.shields.io/badge/platform-Linux-lightgrey.svg)

**fstx** is a Rust library that changes many files and directories **as one atomic
transaction**. Either every change happens or none of them does, even if the process is
killed or the machine loses power in the middle.

```rust
let mut tx = fstx::Transaction::begin("./my-project")?;
tx.write("config.toml", "version = 2\n")?;
tx.rename("old.rs", "src/new.rs")?;
tx.remove_dir_all("build-cache")?;
tx.commit()?; // all changes appear at once, or none do
```

## Why

A filesystem makes exactly one kind of change atomic: renaming a single entry. Real
programs change many files at once:

- installers and updaters replacing a set of files,
- config managers rewriting several related configs,
- package managers and build tools,
- code generators, and AI coding agents editing many source files.

When such a program crashes halfway, the directory is left **half old, half new**, which is
often worse than either version. Existing crates such as `tempfile` and `atomicwrites`
make *one* file atomic. **fstx makes a whole set of changes atomic**, with rollback and
crash recovery.

## Features

- **All-or-nothing commits** across any number of files and directories.
- **Crash recovery**: after a crash, even one during recovery, the next start leaves the
  tree exactly as it was before or exactly as committed. Never a mix.
- **Durable**: `commit()` returns `Ok` only once the changes are safely on disk.
- **Read-your-writes**: `tx.read()` sees staged changes before commit.
- **O(1) directory moves** that keep file identity (inodes) and hard links intact.
- **Symlink-safe**: never follows symlinks and never writes outside the root directory
  (`openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`).
- **Refuses rather than guesses**: if recovery finds a state it can't interpret, it touches
  nothing and returns `RecoveryRequired`. `fstx::inspect()` shows why.
- **Non-UTF-8 file names** are supported.

## Install

```toml
[dependencies]
fstx = { git = "https://github.com/neptro/fstx" }
```

Requires Rust 1.89 or newer. Linux only for now (see [Status](#status)).

## Usage

```rust
fn update() -> fstx::Result<()> {
    // Opens a transaction on a directory. Any transaction interrupted by an
    // earlier crash is recovered first.
    let mut tx = fstx::Transaction::begin("./my-project")?;

    tx.write("config.toml", "version = 2\n")?;        // create or replace a file
    tx.create_dir_all("src/generated")?;               // create directories
    tx.rename("old.rs", "src/generated/new.rs")?;     // move files or whole directories
    tx.remove("temp.log")?;                            // delete a file or empty directory
    tx.remove_dir_all("cache")?;                       // delete a directory tree

    // Nothing on disk has changed yet; reads see the staged state.
    assert_eq!(tx.read("config.toml")?, b"version = 2\n");

    tx.commit()?; // atomic and durable
    Ok(())
}
```

Dropping a transaction without calling `commit()` discards it. Other entry points:

| Function | What it does |
|---|---|
| `fstx::recover(root)` | Finishes recovery of interrupted transactions (`begin` does this too) |
| `fstx::inspect(root)` | Read-only report of pending transactions and what recovery would do |
| `Transaction::begin_with(root, &Options)` | Begin with options, e.g. allowing network filesystems |

Try the examples:

```sh
cargo run --example basic -- /tmp/demo              # apply some changes atomically
cargo run --example crash_demo --features sim       # watch a crash get rolled back
```

## How it works

1. Every change is **staged** in a private `.fstx/` directory inside the root. Nothing
   visible changes yet.
2. On `commit`, fstx writes a **journal** listing every entry it will move, identified by
   inode number, and makes it durable.
3. It then applies the changes as **no-replace renames**, in waves separated by `fsync`
   barriers, and finally writes a durable `COMMITTED` marker.
4. After a crash, **recovery** finds each entry by its identity, never by name alone, and
   undoes any unfinished transaction in reverse, using the same barrier discipline.

The full design, the exact crash-consistency assumptions and the correctness arguments
are in **[DESIGN.md](DESIGN.md)**.

## Testing

Crash safety is only as good as its tests. fstx is tested with:

- **Exhaustive crash simulation** (`tests/crash_sim.rs`): an in-memory filesystem that
  models real crash behaviour, including reordered and lost unsynced writes. Every commit
  is crashed at *every* system call, under every allowed power-loss outcome, and then
  recovery is crashed at every system call too. About 1.3 million recovery runs, each
  checked for "exactly before or exactly after". A slower bounded check (`--ignored`)
  also covers a crash during the recovery of a recovery.
- **Real process kills** (`tests/sigkill.rs`): 200 `SIGKILL`s mid-commit on a real disk.
- **Model-based testing** (`tests/model.rs`): random operation sequences compared against
  a reference model that tracks file identities.
- **Security tests**: symlink escapes, and a thread racing to swap a directory for a
  symlink during commits.
- **Parser robustness**: property tests and cargo-fuzz targets for the on-disk formats.

```sh
cargo test --all-features                  # about 5 minutes
cargo test --all-features -- --ignored     # plus the slow bounded model check
```

The simulator found four real bugs during development, all fixed and documented in
[DESIGN.md](DESIGN.md).

## Status

**v0.1, Linux.** Tested on btrfs and tmpfs. ext4 and xfs are expected to work, since fstx
checks the required filesystem features at startup and refuses to run without them. On
macOS and Windows the crate builds, but `begin` returns `UnsupportedPlatform`. Backends for
both are planned.

**Not supported yet:** operating on symlinks directly, extended attributes, ACLs and
ownership, roots spanning several filesystems, async, a command-line tool, and isolation
for concurrent readers. fstx coordinates *fstx* users with a lock; other programs writing
to the same directory at the same time are detected where possible but not prevented.

## FAQ

**Is this a database?** No. It works on ordinary files and directories that other programs
can read normally. The only extra is a small `.fstx/` directory used while a transaction
is running.

**What happens if my program crashes during `commit()`?** The next `Transaction::begin` or
`fstx::recover` on that directory rolls the transaction back, or completes the cleanup if
it had already committed.

**How is this different from `tempfile::persist` or `atomicwrites`?** Those make a single
file replacement atomic. fstx makes an arbitrary set of writes, renames and deletions
atomic together, and recovers from crashes.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at
your option.
