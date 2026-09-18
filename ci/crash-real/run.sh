#!/usr/bin/env bash
# Empirical crash testing on real filesystems with dm-log-writes (Linux, root).
#
# For each filesystem: record an fstx workload through a dm-log-writes target, then
# replay the log up to every logged entry after the workload started (each a state the
# storage stack could expose after a power loss), mount it, run `fstx::recover`, and
# check that all files agree on one committed generation.
#
# This validates that the *tested* kernel/filesystem/storage configurations stayed within
# the crash contract C1–C5 for *these* workloads. It is evidence, not proof (DESIGN.md §5).
#
# Requires: root, the dm-log-writes module, `replay-log` from xfstests
# (src/log-writes/replay-log), mkfs.ext4, mkfs.xfs, mkfs.btrfs.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$here/../.."
work="$(mktemp -d)"
cleanup() {
  umount "$work/mnt" 2>/dev/null || true
  dmsetup remove fstx-log 2>/dev/null || true
  [ -n "${data:-}" ] && losetup -d "$data" 2>/dev/null || true
  [ -n "${log:-}" ] && losetup -d "$log" 2>/dev/null || true
  rm -rf -- "$work"
}
trap cleanup EXIT

command -v replay-log >/dev/null || { echo "replay-log (xfstests) not found" >&2; exit 2; }
modprobe dm-log-writes

cargo build --manifest-path "$repo/Cargo.toml" --release --example crash_workload
workload="$repo/target/release/examples/crash_workload"

for fs in ext4 xfs btrfs; do
  echo "== $fs =="
  truncate -s 512M "$work/data.img"
  truncate -s 1G "$work/log.img"
  data=$(losetup -f --show "$work/data.img")
  log=$(losetup -f --show "$work/log.img")
  dmsetup create fstx-log --table "0 $(blockdev --getsz "$data") log-writes $data $log"
  case $fs in
    ext4) mkfs.ext4 -q /dev/mapper/fstx-log ;;
    *) "mkfs.$fs" -f -q /dev/mapper/fstx-log ;;
  esac
  mkdir -p "$work/mnt"
  mount /dev/mapper/fstx-log "$work/mnt"
  "$workload" init "$work/mnt/root"
  sync
  dmsetup message fstx-log 0 mark start
  "$workload" run "$work/mnt/root" 30
  umount "$work/mnt"
  dmsetup remove fstx-log

  start=$(replay-log --log "$log" --find --end-mark start)
  total=$(replay-log --log "$log" --num-entries)
  checked=0
  for ((i = start; i <= total; i++)); do
    replay-log --log "$log" --replay "$data" --limit "$i" >/dev/null
    mount "$data" "$work/mnt"
    "$workload" check "$work/mnt/root" >/dev/null
    umount "$work/mnt"
    checked=$((checked + 1))
  done
  echo "$fs: $checked crash states checked"
  losetup -d "$data" "$log"
  unset data log
done
