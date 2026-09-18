# fstx: apply file changes atomically from the command line

`fstx` applies a list of file changes to a directory **all at once or not at all**, even if
it is killed or the power fails halfway. It is the command-line companion to the
[`fstx`](https://crates.io/crates/fstx) Rust library.

```sh
cargo install fstx-cli     # installs the `fstx` command (Linux)
```

## Commands

| Command | What it does |
|---|---|
| `fstx apply [FILE\|-]` | Apply a JSON change set (from a file, or stdin with `-`) |
| `fstx sync SRC` | Copy every file under `SRC` into the root, e.g. dotfiles into `~` |
| `fstx recover` | Finish or roll back anything interrupted by a crash |
| `fstx inspect` | Show interrupted transactions (read-only) |

Common options: `-C, --root DIR` (default `.`), `--json` (machine-readable output), and
`--dry-run` for `apply` and `sync` (check everything, change nothing).

## Change sets

```json
{
  "expect": {
    "app.toml":  { "sha256": "9f86d081884c7d65..." },
    "new.txt":   { "exists": false }
  },
  "ops": [
    { "op": "write",      "path": "config/app.toml", "content": "version = 2\n" },
    { "op": "write",      "path": "logo.png", "content_base64": "iVBORw0KGgo..." },
    { "op": "write",      "path": "run.sh", "from_file": "payload/run.sh", "mode": "755" },
    { "op": "replace",    "path": "src/main.rs", "find": "old()", "replace": "new()", "count": 1 },
    { "op": "mkdir",      "path": "config" },
    { "op": "rename",     "from": "old.txt", "to": "archive/old.txt" },
    { "op": "remove",     "path": "tmp.log" },
    { "op": "remove_all", "path": "cache" }
  ]
}
```

- Ops run **in order** inside one transaction, and later ops see the effect of earlier ones.
  If any op fails, **nothing** is changed.
- `write` needs exactly one of `content` (text), `content_base64` or `from_file` (a path
  relative to the change-set file). `mode` is optional octal permission bits. By default a
  replaced file keeps its permissions and a new file gets the usual default.
- `replace` edits a UTF-8 file. `find` must occur **exactly** `count` times (default 1),
  otherwise the change set is refused. This is the usual "search/replace block" edit format
  of coding agents.
- `mkdir` creates missing parents. `rename` fails if the destination exists. `remove`
  deletes a file or an empty directory. `remove_all` deletes a directory tree.
- `expect` preconditions are checked against the tree **before** any op runs: `exists`
  (true or false) and/or `sha256` (hex).
- Unknown fields and unknown ops are rejected, so a typo can't be silently ignored.

## Using it from an AI coding agent

Have the agent emit one change set per edit. Include the SHA-256 of every file it read, so
the edit only lands if those files are unchanged:

```sh
fstx apply --json -C /path/to/repo - < agent-output.json
# or stream it:  my-agent | fstx apply --json -C /path/to/repo -
```

Output on success:

```json
{"ok":true,"dry_run":false,"root":"/path/to/repo","changes":[{"op":"replace","path":"src/main.rs"}],"skipped":[]}
```

Output on failure (exit code 1), with the index of the op that failed:

```json
{"ok":false,"kind":"refused","op":0,"error":"replace in src/main.rs: found 0 occurrence(s) of the text, expected 1"}
```

## Updating dotfiles

```sh
fstx sync ~/dotfiles -C ~ --dry-run    # preview
fstx sync ~/dotfiles -C ~              # apply
```

Every file and directory under `~/dotfiles` is copied to the same path under `~` in one
transaction, keeping permission bits (scripts stay executable). `.git` is skipped (use
`--exclude NAME` to skip more), symlinks in the source are skipped and reported, and files
in `~` that are not in the source are left alone.

## Exit codes

| Code | Meaning | Tree changed? |
|---|---|---|
| 0 | success | yes (no for `--dry-run`) |
| 1 | refused: a precondition, missing path, collision, etc. | no |
| 2 | invalid input: bad JSON, unknown field or op, bad `mode` | no |
| 3 | I/O or recovery problem; run `fstx inspect` | see `fstx recover` |

## Notes

- Linux only, like the library (v0.1).
- `--dry-run` stages everything in the root's private `.fstx/` directory and then discards
  it. The visible tree is not touched.
- Changes are applied relative to `--root`; paths may not contain `..` or be absolute.

License: MIT OR Apache-2.0.
