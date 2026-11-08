# 9router (Rust)

Rust port of [9router](https://github.com/decolua/9router): a local AI routing
gateway with one OpenAI-compatible endpoint (`/v1/*`) in front of many
upstream providers, plus a dashboard.

Status: work in progress. See [AGENTS.md](AGENTS.md) for the crate map and
porting rules.

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

Provider/model data is generated from the JS source:

```bash
node tools/dump-js-data.mjs ../9router
```

## License

MIT — see [LICENSE](LICENSE). Original work © decolua and contributors.
