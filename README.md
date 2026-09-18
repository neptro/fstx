# fstx

**Atomic, crash-safe transactions for directory trees.**

Databases have transactions. Filesystems give you one atomic operation: renaming a
single entry. When an installer, a config manager, a package tool or an AI coding agent
changes 20 files and the process dies (or the power goes out) halfway, the tree is left
half old and half new.

fstx lets you stage any number of changes and commit them all at once:

```rust
let mut tx = fstx::Transaction::begin("./project")?;
tx.write("config.toml", "version = 2\n")?;
tx.create_dir_all("src/gen")?;
tx.rename("old.rs", "src/gen/new.rs")?;    // directories move in O(1)
tx.remove_dir_all("build-cache")?;
assert_eq!(tx.read("config.toml")?, b"version = 2\n");   // read-your-writes
tx.commit()?;                               // Ok ⇒ durable
```

Whatever crashes, and however often (including during recovery), the next
`Transaction::begin` or `fstx::recover(root)` leaves the tree **exactly as it was before**
or **exactly as committed**. Commits that returned `Ok` always survive. `fstx::inspect(root)`
reports what recovery would do without changing anything.

## Guarantees and assumptions

| | |
|---|---|
| Atomicity | before-state or after-state, including file identities and hard links |
| Durability | `commit()` returns `Ok` only after the commit is on disk |
| Recovery | crash-safe itself; refuses (`RecoveryRequired`) rather than guessing |
| Confinement | never follows a symlink, never leaves the root (`openat2` + `RESOLVE_BENEATH`/`NO_SYMLINKS`) |
| Assumes | a local filesystem with atomic no-replace rename and honest fsync (checked by probes where possible; durability itself cannot be probed) |
| Coordinates | fstx users through a lock file; writers outside fstx are detected where possible, not prevented |

How and why it works, with proof sketches: [DESIGN.md](DESIGN.md).

## Status

v0.1.

* **Platform**: Linux (tested on btrfs and tmpfs; ext4/xfs expected, since the probes check
  the required semantics at runtime). On other platforms `begin` returns
  `UnsupportedPlatform`.
* **Out of scope**: symlink operations, xattrs/ACLs/ownership, roots spanning
  filesystems, async API, CLI, isolation for concurrent readers.

## Testing

```
cargo test --all-features                   # everything below except the slow bounded check
cargo test --all-features -- --ignored      # + bounded nested-crash model check (slow)
cargo run --example crash_demo --features sim
```

* `tests/crash_sim.rs` crashes each commit at **every** syscall on a simulated filesystem
  that implements exactly the crash contract and is adversarial otherwise. For each crash
  it takes every allowed power-loss outcome, runs recovery, and crashes recovery at every
  syscall too. That is about 1.3 million recovery runs, checked for "exactly before or
  exactly after".
* `tests/model.rs` checks fstx against an identity-tracking reference model on random
  operation sequences (proptest).
* `tests/recovery_decisions.rs` covers each row of the recovery decision table, the
  capability probes, and `inspect` never mutating.
* `tests/sigkill.rs` SIGKILLs real processes mid-commit 200 times; after recovery, all
  files must agree on one generation.
* `tests/path_security.rs` and `tests/symlink_race.rs` cover symlink components and
  targets, and a thread swapping a directory with a symlink during commits.
* `fuzz/` holds cargo-fuzz targets for the journal and progress parsers (nightly). The
  same properties also run as proptests.
* `ci/crash-real/` is a `dm-log-writes` replay harness for real filesystems (root, Linux).

## License

MIT or Apache-2.0, at your option.
