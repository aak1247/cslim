# Changelog

## 0.6.0

- Added `slim_profile` configuration (enabled by default): automatically injects `profile.dev.package."*".debug=0` and `profile.test.package."*".debug=0` via Cargo config, omitting redundant debuginfo for third-party dependencies and reducing development target sizes by ~40% out of the box while keeping full debuginfo for workspace crates.
- Added fine-grained target pruning via `cslim prune` and `cslim gc --prune`:
  - Recognizes and deletes stale intermediate build artifacts (`.rlib`, `.rmeta`, `.so`, executables) across multiple compile hashes while retaining the newest `<keep>` versions.
  - Automatically identifies and purges ephemeral integration test runner binaries (`tests/*.rs`) and their dependency maps.
  - Integrates pruning into `cslim gc` prior to whole-target eviction, reducing disk pressure without dropping hot workspace targets.
- Added Copy-on-Write (CoW) / Reflink support for Linux filesystems via `FICLONE` ioctl (`0x40049409`):
  - Achieves zero-cost clone during target migration on reflink-capable filesystems (such as XFS and Btrfs).
  - Gracefully falls back to optimized standard copy on unsupported or cross-device filesystems while strictly preserving nanosecond timestamps and POSIX modes.
  - Logs and audits reflink vs standard copy metrics in migration transaction journals.

## 0.5.0

- First downloadable Linux x86_64 release, with a checksum-verified Bash installer and a pinned sccache installer. Installing the binary does not require Python or a Rust build.
- Added `cslim migrate` for previewing, importing, and recovering old targets from registered Git worktrees without Python. Imports remain explicit and fail closed when an idle target or its references cannot be verified.
- Documented the actual cleanup behavior: 100 GiB is the default *total target soft limit*, not a per-project limit. Idle targets expire after 14 days; cache namespaces have a separate 40 GiB total soft limit and expire after 30 days.
