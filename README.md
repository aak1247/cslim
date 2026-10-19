# cslim

`cslim` is a Cargo wrapper that keeps build targets under one managed root, reclaims idle targets, and shares compatible compiler results across projects through versioned [sccache](https://github.com/mozilla/sccache) namespaces. Each workspace still has its own Cargo target directory; compiled artifacts are never copied between workspaces.

[简体中文文档](README.zh-CN.md) · [Configuration example](config.example.toml) · [MIT license](LICENSE)

## Requirements

- Linux or Unix, Rust/Cargo 1.91 or newer, and `sccache` on `PATH`.
- The optional installer and systemd timer target Linux. The bundled sccache installer supports Linux x86_64 and verifies the pinned release archive's SHA-256.

## Install

```sh
git clone https://github.com/aak1247/cslim.git
cd cslim
python3 scripts/install-sccache.py # skip if sccache is already installed
./scripts/install.sh --timer      # omit --timer when systemd user units are unavailable
```

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

Existing `target/` directories are left alone. To inspect and explicitly import idle targets from registered Git worktrees, use `python3 scripts/migrate-targets.py --report /absolute/path/to/plan.json`; see the [migration guide](README.zh-CN.md#历史工作树与旧-target) before using `--apply`.

## Storage and cleanup

`<root>/v1/targets` contains a separate target for each workspace path and compiler version. `<root>/v1/caches` contains sccache namespaces keyed by the storage format, full `rustc -vV` output, and sccache version. sccache decides whether individual compilations match by their inputs. Upgrading Rust or sccache creates a new namespace; old namespaces remain until reclaimed.

Defaults are 14 days of inactivity and 100 GiB total for targets, 30 days and 40 GiB for cache namespaces, and 10 GiB per sccache namespace. `gc` previews by default, skips active entries, and deletes only cslim-owned entries after validating their metadata. Limits are soft during active builds. See the [Chinese documentation](README.zh-CN.md#清理策略) for locking, migration safeguards, and rollback details.

## Development

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
python3 scripts/test_cslim_dev.py
python3 scripts/test_migrate_targets.py
python3 scripts/test_worktrees.py target/debug/cargo-cslim
python3 scripts/smoke.py
```

The last two tests require sccache; the smoke test also requires `itoa` 1.0.18 in the local Cargo cache. Contributions are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md).
