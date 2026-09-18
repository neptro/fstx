# fstx design

This document states what fstx guarantees, what it assumes, and why the protocol meets
the guarantee under those assumptions. Section numbers are referenced from the code.

## Guarantee

For a transaction on root `R`, after any sequence of crashes (process kills or power
losses, including crashes during recovery) followed by one completed `recover`:

* the visible tree under `R` (excluding `R/.fstx`) is **exactly** the before-state or
  **exactly** the after-state, including file identities (inode numbers of every entry
  that existed before, hard-link topology);
* it is the after-state whenever `commit()` returned `Ok`;
* `.fstx` contains no leftovers;
* if recovery cannot interpret what it finds, it returns `RecoveryRequired` and changes
  nothing.

The three proof obligations:

* **P1** every physical step has an unambiguous durable state;
* **P2** recovery never mistakes an old state for a new one;
* **P3** recovery stays correct across crashes during recovery.

## §1 Logical layer: identity-aware overlay (`overlay.rs`)

Operations never touch the tree. They update an overlay describing **which directory
entry** occupies each path:

* `placed: view path → Absent | Present(Entry)`, where an entry's source is
  `Base{origin, FileId}`, a staged blob, or a new directory;
* `detached: base path → FileId` for base entries that left their base path (moved,
  replaced, removed).

Paths without an explicit decision inherit from the nearest ancestor, so untouched
children of a moved directory stay inside it. That is what makes a directory rename O(1).

**Net-diff equality is identity, never content.** A path is unchanged only if the same
base entry is still there under an unchanged chain of ancestors. Swapping two identical
files is therefore two moves, and renaming `a→t→a` is a no-op. `write` always creates a
new inode. That is atomic-save semantics: other hard links to the old inode keep the old
content.

## §2 Token model (`plan.rs`)

A **token** is one directory entry the transaction moves, with its `FileId = (st_dev,
st_ino)` recorded in the journal *before* any tree mutation. Tokens are:

* base entries that must leave their base path;
* staged files (`staged/b<n>`, written and fsynced before the journal);
* new directories, **pre-created in `staged/d<n>`** so their identity is also known.

Every tree mutation is `renameat2(RENAME_NOREPLACE)` of one token between two of its
locations. Removal moves the token to `backup/`; installing moves it from `staged/` or
`backup/`. There is no overwrite, no copy and no hard link. Unlinks happen only inside
`.fstx` after the transaction is settled (plus one case in §4: removing a
duplicate name of the same inode).

**Two phases.** Base entries are first **detached** into `backup/k`, deepest paths
first. Tokens are then **attached** at their final paths, shallowest first. A moved entry
therefore makes two renames (`a → backup/k → b`). That costs one extra rename, and in
exchange cycles and swaps need no special cases, and during detach the tree only loses
entries while during attach it only gains them.

**Classification (P2).** A token is at location `L` iff the parent directory of `L` has
the identity recorded for `L` **and** the entry at `L` has the token's `FileId` and kind.
Existence alone is never used. Tokens stay linked until the transaction settles, so
their inode numbers cannot be reused meanwhile. Per token:

| hits | meaning |
|---|---|
| exactly one location `i` | at `i` |
| locations `i` and `i+1` (files only) | *both names*: the rename persisted without removing the old name (Weak profile); treated as "at `i`", and the extra name is removed during undo |
| none | contract violated → `RecoveryRequired` |
| an unknown `FileId` at a location whose parent matches | outside interference → `RecoveryRequired` |

A known token's `FileId` at another token's location is expected (e.g. the old file still
sitting where the new one will go).

Recovery classifies **every** token (preflight) before its first mutation.

## §3 Waves and barriers (P1)

Detach steps of equal depth form one wave; so do attach steps of equal depth. After each
wave: fsync the parent directory of every endpoint, then append `APPLY_WAVE_DONE(w)` to
`progress` and fsync it.

**Wave invariant** (checked by `plan::validate` in debug builds, in unit tests and on
every journal read):

1. each token's steps are `from = 0..n-1`, in strictly increasing waves;
2. for two steps `s ≠ t` in one wave, no **endpoint** of `s` (source or destination path)
   equals, contains, or is contained in an endpoint of `t`. For example, `rename(a→b)` and
   anything under `a/` or `b/` never share a wave;
3. symbolic execution: each source holds its token, each destination is free of tokens,
   and a parent directory that is itself a token was placed there by an *earlier* wave,
   with the identity recorded for the location;
4. every token ends at its last location, and no two tokens end at the same path.

Clause 2 compares endpoints only. Two steps may share an *ancestor*, such as `d/x` and
`d/y` both moving out of `d`. Steps only read (resolve through) their ancestors. Entry
mutations of one directory for distinct names are independent under C2, and the barrier
fsyncs `d` after both. Forbidding shared ancestors would serialize every sibling into its
own wave (one fsync barrier per file) for no gain in safety.

**Lemma 1.** Under C1–C5, the durable state after a crash during apply is: waves `< w`
fully applied, wave `w` any subset (each step atomic by C1, or both-names under Weak),
waves `> w` untouched. *Proof sketch:* wave `w+1` starts only after every directory
touched by wave `w` is fsynced (C2). Steps within a wave touch disjoint names, so any
subset of them is a well-formed state (clause 2), and no step's precondition depends on
another step in the same wave (clauses 1 and 3).

**Lemma 2.** In every state of Lemma 1, classification returns the true location of
every token. *Sketch:* by clause 2 at most one step per wave touches a location, and
earlier waves are durable. So the only entries that can carry a token's `FileId` at a
location in its path are that token itself, or its old name (both-names). Parent-identity
checks exclude look-alike paths that resolve through other directories.

Recovery also checks the Lemma-1 *shape* (all earlier waves done, all later waves
untouched) and refuses (`RecoveryRequired`) if it does not hold.

## §4 Durable state machine and recovery (`state.rs`, `recover.rs`)

```
begin:  .fstx/tx-<id>/{staged,backup}
prepare: fsync blobs, staged/, backup/, progress, tx dir, .fstx
         → journal.tmp (fsync) → rename → journal (fsync tx dir)      = PREPARED
apply:  waves with barriers                                           = APPLIED
commit: create COMMITTED, fsync tx dir                                = COMMITTED  (commit returns Ok)
settle: rename tx-<id> → gc-tx-<id>, fsync .fstx, delete
```

The journal is created only after everything it refers to is durable. The crash simulator
found a violation of this in an earlier draft: C2 lets unsynced entries of one directory
persist independently, so a journal could become durable while `staged/` did not.

Recovery decision table (`recover::decide`):

| observed | action |
|---|---|
| `gc-*` / `probe-*` | fsync `.fstx` (make the rename durable), then delete |
| COMMITTED and a rollback marker | `RecoveryRequired` |
| COMMITTED | settle (never undo) |
| ROLLED_BACK | settle |
| no journal, `backup/` empty, no ROLLING_BACK | tree never touched → settle |
| no journal, but `backup/` non-empty or ROLLING_BACK | `RecoveryRequired` |
| journal fails checksum / parse / path or plan validation | `RecoveryRequired` (the checksum detects corruption; it does not authenticate) |
| otherwise | roll back |

Rollback:

1. **Preflight**: classify all tokens and check the shape and progress (below). Nothing is
   mutated before this passes.
2. **Stabilize**: fsync the parent of every token location (where it resolves to the
   recorded identity), plus `staged/`, `backup/`, the tx dir and `.fstx`. The observed
   state, which may include page-cache changes from a killed process, is now the durable
   state.
3. Create ROLLING_BACK (O_EXCL, fsync). The decision is final: fstx never rolls forward.
4. **Undo** the waves in reverse. Each token at its step's destination is renamed back,
   and a both-names duplicate is unlinked (after verifying its identity and parent). Then
   the same barrier as apply runs, followed by `UNDO_WAVE_DONE(w)`. The barrier skips a
   parent that does not resolve to its recorded identity. That only happens when an
   ancestor token is elsewhere, which by wave order means this wave's changes in that
   directory are already durable or never happened.
5. Create ROLLED_BACK (fsync), then settle.

**Progress log** (`progress.rs`): fixed 16-byte checksummed records. It is a *lower bound
and consistency check*: a wave logged as done must be observed as done, otherwise
`RecoveryRequired`. Observation stays authoritative. The parser **resynchronizes past
corrupt bytes**. The simulator found the need: a power loss can leave a torn record,
and records appended by a later recovery must not hide behind it. Only the set of records
matters.

**Lemma 3 (P3).** Let the durable potential be
`Φ = (phase, Σ over tokens of their position index)`, ordered lexicographically, with
`phase` = 3 before ROLLING_BACK, 2 with it, 1 with ROLLED_BACK/COMMITTED, and 0 once
settled.

* After stabilize, the durable state equals the observed state. From then on recovery's
  only mutations are markers (lowering `phase`) and backward moves (lowering the sum).
  So pending, not-yet-durable operations are all Φ-decreasing, and any power-loss
  outcome has Φ ≤ the durable Φ.
* A crash before stabilize completes leaves a state recovery simply re-examines. Nothing
  was mutated, so Lemma 1 still describes it.
* Every run that gets past stabilize makes durable progress, or crashes with Φ unchanged
  and starts over from a state Lemma 2 classifies correctly. Φ is well-founded, so once
  crashes stop, recovery reaches ROLLED_BACK → settled with every token at location 0,
  which is the before-state.

The simulator tests assert "Φ(durable) never increases after the first completed
stabilize" on every explored path.

**Settling.** `gc` is a single rename (`tx-<id>` → `gc-tx-<id>`) followed by an fsync of
`.fstx`. Only then is anything deleted. Recovery also fsyncs `.fstx` before deleting a
`gc-*` it finds, because the renaming process may have died before its own fsync. The
simulator found the need for this as well: without it a power loss could keep the
deletions (including COMMITTED) and revert the rename.

**Commit outcome.** `commit` returns `Ok` only after COMMITTED is durable. If creating or
syncing COMMITTED fails, it returns `CommitOutcomeUnknown` and does *not* roll back in
process, because the marker may already be durable. The next `recover` decides from the
durable state. Any failure before that point rolls back in process through the same
routine as recovery. If that also fails, the result is `RollbackFailed`, and the state is
left for `recover`.

## §5 Crash-consistency contract

Correctness depends only on:

* **C1** `rename` with NOREPLACE is atomic: after a crash the (source, destination) pair
  is in its pre-state or post-state. The *Weak* variant also allows "both names, same
  inode" for non-directories, but never "neither".
* **C2** a directory's entry mutations are durable once `fsync(dir)` returns. Before
  that, any subset of pending mutations may persist, each atomically.
* **C3** `fsync(file)` makes data and size durable; unsynced data may be arbitrary.
* **C4** O_EXCL create + fsync(parent) gives durable, atomic existence.
* **C5** fsync errors are reported; nothing is silently lost after a successful fsync.

`SimFs` (`src/sim.rs`, feature `sim`) implements exactly this and is otherwise
adversarial:

* a pending mutation is guaranteed durable only once *every* directory it touches has
  been fsynced after it;
* a power loss keeps any subset of the other mutations, applied in order;
* unsynced file data comes back as old, new, torn or garbage.

It is a sound over-approximation: it allows every behaviour a conforming filesystem can
show, plus some none can. It also flags any re-use of a name for a different inode before
the name's removal is durable.

**Capability probing** (`caps.rs`) runs in `.fstx/probe-*` on the root's own filesystem
and checks *semantics*, never names:

* no-replace rename refuses an occupied destination;
* file identity survives rename;
* directory fsync is accepted;
* case sensitivity.

Failing a probe gives `UnsupportedFilesystem`. Only EINVAL/unsupported counts as
"missing": an I/O error during the probe is reported as an error, not a capability
verdict. Network and FUSE mounts are refused unless `Options::allow_untested_fs`.
**Probes cannot test durability.** C1–C5 remain assumptions about the kernel,
filesystem and storage stack.

**Empirical harness** (`ci/crash-real/`): records fstx workloads with `dm-log-writes`,
replays each flush-bounded crash point on ext4/xfs/btrfs and runs recovery. It
empirically checks that *the tested kernel/filesystem/storage configurations* stayed
within C1–C5 for *the exercised workloads*. It is evidence, not proof.

## §6 Confinement (`sys/linux.rs`)

The root is an owned directory fd. Each operation resolves the parent directory beneath
it with `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS |
RESOLVE_NO_XDEV)`. On kernels without `openat2` it walks component by component with
`O_NOFOLLOW | O_DIRECTORY` and an `st_dev` check. It then makes one `*at` call on that
held fd with a single-component leaf:

| operation | syscalls | why it stays confined |
|---|---|---|
| stat / classify | `fstatat(pfd, leaf, AT_SYMLINK_NOFOLLOW)` | never follows |
| read | `openat(pfd, leaf, O_RDONLY\|O_NOFOLLOW\|O_NONBLOCK)` + `fstat` must be regular | leaf symlink → ELOOP; FIFOs and devices refused |
| create file | `openat(pfd, leaf, O_CREAT\|O_EXCL\|O_NOFOLLOW)` (inside `.fstx` only) | O_EXCL never follows |
| mkdir | `mkdirat` (inside `.fstx` only) | EEXIST on any existing entry |
| rename | `renameat2(pfd1, leaf1, pfd2, leaf2, RENAME_NOREPLACE)` after `fstat(pfd)` matches the journaled parent identity, **on the same fd** | acts on entries, never follows, never overwrites |
| unlink / rmdir | `unlinkat` (tree: only both-names duplicates, identity-checked; otherwise `.fstx` only) | acts on the entry itself |
| fsync | `fsync` on an fd opened through the same resolution | — |

Symlinks and special files cannot be operated on directly (`UnsupportedFileType`). They
may live inside directories that are moved or removed as a whole. Removal never follows
them: cleanup unlinks the link itself.

**Residual risk.** An attacker with write access to the tree can *move* a directory whose
fd fstx holds. The operation then lands where the attacker could already write, so there
is no privilege gain, and the parent-identity check refuses the next step. Writers
outside fstx are outside the contract. fstx detects what it can (identity checks,
foreign entries), and its locking only coordinates fstx users.

## Platforms

v0.1 has a Linux backend. On other platforms `Transaction::begin`, `recover` and
`inspect` return `UnsupportedPlatform`. The simulator and everything above it are
platform-independent. macOS (`renameatx_np(RENAME_EXCL)`, `F_FULLFSYNC`,
`O_NOFOLLOW_ANY`) and Windows (`MoveFileExW` without `REPLACE_EXISTING`, handle-relative
opens) are planned. Each will be gated by the same probes rather than by filesystem name.
