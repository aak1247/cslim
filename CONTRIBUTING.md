# Contributing

Issues and pull requests are welcome. Please describe the affected Cargo command, toolchain and sccache versions, host platform, expected behavior, and observed behavior. Do not include private project paths, credentials, or build logs containing secrets.

For changes to target layout, locking, migration, or garbage collection, explain how existing managed data stays safe. Run `cargo fmt --check`, `cargo test --locked`, `cargo clippy --locked --all-targets -- -D warnings`, and the relevant Python tests from the [README](README.md#development). Integration tests need `sccache` on `PATH`.

Please keep PRs focused and update the English and Chinese documentation when behavior changes.
