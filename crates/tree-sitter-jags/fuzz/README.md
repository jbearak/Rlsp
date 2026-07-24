# JAGS parser defensive testing

This separate crate exercises Raven's generated JAGS parser. It is not a
runtime dependency. `parser` accepts arbitrary bytes and recursively checks
node and parent ranges. `incremental_edits` starts from one of eight clean JAGS
sources, applies a generated edit, and compares every fresh and reused node's
kind, flags, fields, children, and byte/point ranges.

## Pinned environment

- rustup toolchain: `nightly-2026-07-22`
- rustc: `rustc 1.99.0-nightly (0e29c21d9 2026-07-21)`
- cargo-fuzz: `0.13.2`
- sanitizer: AddressSanitizer (cargo-fuzz `--sanitizer address`)
- target: `aarch64-apple-darwin`

Install the exact tools with:

```sh
rustup toolchain install nightly-2026-07-22 --profile minimal
cargo install cargo-fuzz --version 0.13.2 --locked
```

From this directory, run the two publication gates exactly as follows. The
explicit libFuzzer controls are a fixed seed, 4,096-byte input ceiling,
five-second per-input timeout, 2,048 MiB RSS ceiling, final statistics, and a
600-second campaign duration.

```sh
mkdir -p evidence-corpus/parser-asan-20260723
cargo +nightly-2026-07-22 fuzz run --sanitizer address parser \
  evidence-corpus/parser-asan-20260723 seeds/parser -- \
  -max_total_time=600 -max_len=4096 -seed=424242 \
  -rss_limit_mb=2048 -timeout=5 -print_final_stats=1 -verbosity=0

mkdir -p evidence-corpus/incremental-asan-20260723
cargo +nightly-2026-07-22 fuzz run --sanitizer address incremental_edits \
  evidence-corpus/incremental-asan-20260723 seeds/incremental_edits -- \
  -max_total_time=600 -max_len=4096 -seed=424242 \
  -rss_limit_mb=2048 -timeout=5 -print_final_stats=1 -verbosity=0
```

`evidence.json` records the resulting execution totals, peak RSS, output
corpus hashes, and empty defect lists. It binds each result to the current
grammar, generated parser, fuzz target, fuzz lockfile, and committed seed
content. Check the record without running a campaign:

```sh
python3 verify_evidence.py
```

Output-corpus and seed-corpus hashes are content multisets: SHA-256 each file,
sort those lowercase hex digests, append one newline to each, then SHA-256 the
combined ASCII text. This is independent of corpus filenames.

The committed seed bindings are:

| Target | Seed files | Bytes | Content-multiset SHA-256 |
|---|---:|---:|---|
| `parser` | 4 | 181 | `f36547265c8af99b20cf322950b64ee02a5e4182f0743667106061eaac268777` |
| `incremental_edits` | 3 | 92 | `7258ff0bf41dea7863cd1d4c303d9794d1b1caedb5affc30dfcf8b4821cab088` |

## Current evidence

Both 600-second AddressSanitizer campaigns completed successfully on
2026-07-23 with no sanitizer finding, timeout, crash artifact, or parser
defect.

| Target | Executions | Average/s | New units | Peak RSS | Output corpus | Output-corpus SHA-256 |
|---|---:|---:|---:|---:|---|---|
| `parser` | 8,879,519 | 14,774 | 381 | 532 MiB | 77 files / 2,378 bytes | `e02c8aa78bbc488007a17db1df92a9ca996ea43fc5eebbd46bcb5659f64671c3` |
| `incremental_edits` | 4,338,332 | 7,218 | 451 | 615 MiB | 118 files / 9,506 bytes | `c3b0c621c11c550d87bd78c4586adf2d3849aa44b3da6020710c86659b583079` |
