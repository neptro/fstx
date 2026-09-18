# Changelog

## 0.1.1

- Added `Transaction::write_with_mode` to set a file's permission bits exactly (for
  example `0o755` for scripts). Found missing while building the `fstx` command-line
  tool's dotfiles sync.
- New companion crate `fstx-cli`, which provides the `fstx` command: `apply` (JSON change
  sets, including AI-agent patches with SHA-256 preconditions), `sync` (dotfiles),
  `recover` and `inspect`.

## 0.1.0

- First release: atomic, crash-safe multi-file transactions on Linux.
