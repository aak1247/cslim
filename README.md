# cslim

`cslim` is a Cargo wrapper that keeps build targets under one managed root, reclaims idle targets, and shares compatible compiler results across projects through versioned [sccache](https://github.com/mozilla/sccache) namespaces. Each workspace still has its own Cargo target directory; compiled artifacts are never copied between workspaces.

[简体中文文档](README.zh-CN.md) · [Releases](https://github.com/aak1247/cslim/releases) · [Changelog](CHANGELOG.md) · [Configuration example](config.example.toml) · [MIT license](LICENSE)

## Requirements

- Rust/Cargo 1.91 or newer is needed to build projects, even when cslim is installed from a binary.
- The prebuilt release and Bash installer support Linux x86_64. The installer downloads and verifies sccache 0.18.0 if it is missing. Source builds support Linux/Unix.

## Install

While the repository is private, authenticate with GitHub CLI first:

```sh
gh repo clone aak1247/cslim
cd cslim
bash install.sh                 # latest release binary, checksum and sccache verified
bash install.sh --timer         # optionally enable hourly systemd user GC
bash install.sh --version v0.6.0
```

Once the repository is public, `curl -fsSL https://raw.githubusercontent.com/aak1247/cslim/main/install.sh | bash` works without GitHub CLI. To build from source instead, run `bash scripts/install.sh` after installing sccache. Installation and ordinary use do not require Python.

The installer puts `cargo-cslim`, `cslim`, and the optional `cslim-dev` wrapper in `~/.local/bin`. Add that directory to `PATH`. The default managed root is `${XDG_CACHE_HOME:-~/.cache}/cslim`; to use another disk, copy [config.example.toml](config.example.toml) to `~/.config/cslim/config.toml` and set an absolute `root` path. Only the current user can access a root created by cslim.

The timer runs garbage collection hourly while the systemd user manager is active. Without it, run `cslim watch --interval 3600` or schedule `cslim gc --apply` yourself.

## Use

```sh
cargo cslim build --release
cargo cslim test --locked
cargo cslim check
cargo cslim run -- --help
cargo +stable cslim build

cslim status --json     # managed targets, caches, size, and last use
cslim cache-stats       # sccache statistics for the current toolchain
cslim target-dir        # target path for the current workspace
cslim prune             # preview fine-grained cleanup of old artifacts and test binaries
cslim prune --apply     # reclaim obsolete build artifacts and ephemeral test runners
cslim gc                # preview; no deletion
cslim gc --apply        # reclaim eligible idle entries
cslim clean             # remove this workspace's managed target
```

`cslim` wraps Cargo build, check, test, run, bench, doc, rustc, rustdoc, clippy, and clean. Use regular Cargo for other commands. Cargo output and exit codes are preserved. Build outputs live under the managed root, so scripts that read `./target` directly need the path from `cslim target-dir`.

To invoke a project from another directory, add its absolute manifest path to your user config:

```toml
[projects]
demo = "/absolute/path/to/demo/Cargo.toml"
```

```sh
cslim project demo check --locked
cslim worktrees --project demo
cslim project demo --worktree /path/to/older-checkout build --locked
```

`cslim-dev` temporarily places a Cargo shim in one command's process tree, for example `cslim-dev cargo check --locked` or `cslim-dev pnpm run check:rust`. Build commands go through cslim; non-build Cargo commands go to Cargo. It does not modify project scripts or the global Cargo installation.

Existing `target/` directories are left alone. To inspect and explicitly import idle targets from registered Git worktrees, use `cslim migrate --report /absolute/path/to/plan.json`, review the report, then use `cslim migrate --apply --report /absolute/path/to/result.json`. See the [migration guide](README.zh-CN.md#历史工作树与旧-target) for safeguards and recovery.

## Storage and cleanup

`<root>/v1/targets` contains a separate target for each workspace path and compiler version. `<root>/v1/caches` contains sccache namespaces keyed by the storage format, full `rustc -vV` output, and sccache version. sccache decides whether individual compilations match by their inputs. Upgrading Rust or sccache creates a new namespace; old namespaces remain until reclaimed.

Defaults are 14 days of inactivity and **100 GiB total across all managed targets**, 30 days and **40 GiB total across all cache namespaces**, and 10 GiB per sccache namespace. These are not per-project target limits. For example, if two idle targets use 60 GiB and 50 GiB, the next GC can remove the least recently used whole target to get below 100 GiB. An active build can exceed the limit; cslim cannot cap the peak size of a single project, and a 120 GiB target may be reclaimed after that build finishes. Raise `target_max_gib` if you want to retain such a target.

Wrapped builds trigger GC when they finish. The optional timer runs it hourly even when no builds occur. `cslim gc` previews by default; `cslim gc --apply` deletes eligible entries. Prior to evicting an entire target, GC performs fine-grained pruning only on idle reclamation candidates (expired targets or targets considered while over the total capacity limit). Warm targets below the limit keep their dependency/feature variants and test runners, so routine GC does not force unchanged builds to recompile. Capacity pruning stops once the total is below the limit. Explicit `cslim prune` and `cslim gc --prune` still trim warm targets; Cargo may need to rebuild removed variants afterward. Furthermore, `cslim migrate` automatically utilizes Copy-on-Write (reflink via `FICLONE`) on supported filesystems (e.g., XFS, Btrfs) to avoid disk duplication, and `slim_profile = true` strips redundant debuginfo from dependency builds to reduce target sizes by ~40%. GC skips active entries and only deletes cslim-owned directories with valid metadata. It never automatically deletes ordinary project `target/` directories or Cargo's registry cache. See the [Chinese documentation](README.zh-CN.md#清理策略) for details.

## Development

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
python3 scripts/test_cslim_dev.py
python3 scripts/test_gc.py target/debug/cargo-cslim
python3 scripts/test_worktrees.py target/debug/cargo-cslim
python3 scripts/smoke.py
```

The Python scripts above are developer-only test fixtures; installing and operating cslim uses Bash and the Rust binary. The last two tests require sccache; the smoke test also requires `itoa` 1.0.18 in the local Cargo cache. Contributions are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md).
