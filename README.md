# 9router (Rust)

[![CI](https://github.com/decolua/9router/actions/workflows/ci.yml/badge.svg)](https://github.com/decolua/9router/actions/workflows/ci.yml)

Rust port of [9router](https://github.com/decolua/9router): a local AI routing
gateway with one OpenAI-compatible endpoint (`/v1/*`) in front of many
upstream providers, plus a dashboard.

The port covers the full HTTP surface of the JS build: the `/v1`/`/v1beta`
gateway, the `/api` management API, the OAuth credential-import routes, the
dashboard, the CLI (`9router connect|show|update|xai video`, tray/menus) and
the MITM transport. See [AGENTS.md](AGENTS.md) for the crate map and porting
rules.

It reads the same state as the JS build (`~/.9router`, or `$DATA_DIR`),
including `db/data.sqlite`, so an existing install keeps its providers, keys
and combos.

## Build

```bash
cargo build --release
./target/release/9router --port 20128
```

Requires Rust 1.85+. If your checkout is on a filesystem that can't execute
binaries (e.g. some NTFS mounts), point `CARGO_TARGET_DIR` (or a `target`
symlink) at one that can.

Provider/model data is generated from the JS source and committed under
`crates/nr-data/data/`; regenerate after merging upstream:

```bash
node tools/dump-js-data.mjs ../9router
```

## Test

```bash
cargo test --workspace --no-fail-fast
```

Workspace state: all tests green (1700+). The optional read-only probe against
a real JS-written database:

```bash
NR_REAL_DB=$HOME/.9router/db/data.sqlite \
  cargo test -p nr-db --test js_compat -- --ignored real_database
```

## CI

`.github/workflows/ci.yml` runs `cargo check` + `cargo build` + the full test
suite on push/PR. `.github/workflows/release.yml` builds release binaries for
Linux (x86_64/aarch64), macOS (both arches) and Windows on a `v*` tag.

Tray support is an opt-in feature (`cargo build --features tray`) that needs GTK
dev libraries on Linux and is not built by default.

## License

MIT — see [LICENSE](LICENSE). Original work © decolua and contributors.
