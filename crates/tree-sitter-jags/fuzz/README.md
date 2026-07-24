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

mkdir -p evidence-corpus/incremental-asan-20260724-review
cargo +nightly-2026-07-22 fuzz run --sanitizer address incremental_edits \
  evidence-corpus/incremental-asan-20260724-review seeds/incremental_edits -- \
  -max_total_time=600 -max_len=4096 -seed=424242 \
  -rss_limit_mb=2048 -timeout=5 -print_final_stats=1 -verbosity=0
```

`evidence.json` records the resulting execution totals, peak RSS, output
corpus hashes, and empty defect lists. It binds each result to the current
grammar, generated parser, fuzz target, fuzz manifest and lockfile, and
committed seed content. Check the record without running a campaign:

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
| `incremental_edits` | 3 | 101 | `9218b1c37d5928a62529c2a5abe8780bef247f440eef335d04ffe1905d00f200` |

Incremental inputs use the target wire format
`[base_index, start, delete_len, replacement...]`. Each committed seed replaces
one complete selected base with its named loop, relation, or Unicode source, so
the edit boundaries cannot split a UTF-8 code point.

## Current evidence

The parser campaign completed on 2026-07-23 and the incremental-edit campaign
completed on 2026-07-24. Both 600-second AddressSanitizer campaigns completed
successfully with no sanitizer finding, timeout, crash artifact, or parser
defect.

| Target | Executions | Average/s | New units | Peak RSS | Output corpus | Output-corpus SHA-256 |
|---|---:|---:|---:|---:|---|---|
| `parser` | 8,879,519 | 14,774 | 381 | 532 MiB | 77 files / 2,378 bytes | `e02c8aa78bbc488007a17db1df92a9ca996ea43fc5eebbd46bcb5659f64671c3` |
| `incremental_edits` | 3,102,972 | 5,163 | 498 | 572 MiB | 106 files / 9,636 bytes | `2078c3537217c2e35fb46eb6352ddc0772a809b9bebe05756a61a51741198c0e` |
