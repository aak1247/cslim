# Changelog

## 0.5.0

- First downloadable Linux x86_64 release, with a checksum-verified Bash installer and a pinned sccache installer. Installing the binary does not require Python or a Rust build.
- Added `cslim migrate` for previewing, importing, and recovering old targets from registered Git worktrees without Python. Imports remain explicit and fail closed when an idle target or its references cannot be verified.
- Documented the actual cleanup behavior: 100 GiB is the default *total target soft limit*, not a per-project limit. Idle targets expire after 14 days; cache namespaces have a separate 40 GiB total soft limit and expire after 30 days.
