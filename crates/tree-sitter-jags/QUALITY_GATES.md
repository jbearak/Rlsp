# Quality gates and evidence

The grammar targets the parse phase of JAGS 4.3.2. `model in` is the observed
syntax boundary: unknown functions and distributions pass it and fail only
after `compile`, while malformed expressions and delimiters fail during it.

## Deterministic corpora

- 115 independently authored black-box matrix probes: 50 accepted and 65
  rejected, across 21 categories.
- 358 syntax-valid cases from 295 authored templates, all accepted by JAGS and
  error-free here. The total includes all 276 unordered pairs of 24 meaningful
  syntax features; each pair has a unique recursive tree shape, and the whole
  group produces 344 distinct recursive tree-shape fingerprints.
- 50 syntax-valid semantic failures across 10 existing semantic families, all accepted
  by JAGS during parsing, rejected during compilation, and error-free here.
  They currently produce 39 distinct recursive tree-shape fingerprints.
- 75 syntax-invalid cases from 35 authored defect templates, all rejected by
  JAGS and classified as erroneous here (including the documented BOM root
  coverage boundary). They currently produce 67 distinct recursive tree-shape
  fingerprints.
- 200 mutation cases from 10 mutation categories and 20 independently authored
  contexts per category. All are rejected by JAGS and classified as erroneous
  here, with 176 distinct recursive tree-shape fingerprints.
- 25 structurally distinct incremental-edit sequences cover relation
  operators, operands, calls, subsets, loops, bounds, contextual syntax,
  special operators, non-associative operators, whole expressions, program
  blocks, CRLF/Unicode comments, and EOF comments. Every node's kind, flags,
  byte/point range, field, and children equal a fresh parse after each edit.
- 1,024 generated valid-property cases and 1,024 arbitrary-UTF-8 range/panic
  cases.

The committed `oracle-results.json` binds all 798 matrix and quality-corpus
outcomes to the source hashes, generator hash, and oracle harness hash. Normal
CI regenerates the corpus, verifies the input binding and outcomes offline,
and runs the oracle harness's timeout/hash-validation unit tests. Refreshing or
live-verifying the results additionally requires the exact wrapper and terminal
hashes in `provenance.json`; an explicit unpinned override cannot refresh the
committed manifest.

Recovery checks include exact issue kinds and byte/point ranges for six defect
families, separated-fault locality, deterministic high-fault recovery, Unicode,
CRLF, BOM, EOF, bounded tree growth, and cancellation after 256 parser progress
callbacks. Reuse of an arbitrary already-invalid tree is tested separately for
safe error classification and recursive range validity, because Tree-sitter
does not promise a canonical recovery shape for that input class.

## Performance

Release-mode local medians (five samples, Apple Silicon) are:

| Input | Median |
|---|---:|
| 1 KiB valid | 0.135 ms |
| 10 KiB valid | 1.056 ms |
| 100 KiB valid | 9.296 ms |
| 100 KiB malformed | 9.804 ms |
| 100 KiB incremental edit, early/middle/late | 2.682/2.674/2.758 ms |
| 1 MiB valid | 84.892 ms |

CI enforces 3/8/30/40/20/300 ms respectively, without an environment-specific
multiplier. Cancellation of a 65,536-relation input is separately required to
return no partial tree after exactly 256 progress callbacks and within 250 ms.

The generated `parser.c` is 150,308 bytes and the locally built release rlib is
70,464 bytes. Raven does not depend on this crate in this PR, so its runtime
binary and extension package size are unchanged.

## Fuzz targets

The separate fuzz crate retains small deterministic seed sets for arbitrary
parser input and incremental edits. The parser target recursively checks node
and parent range containment. The incremental target starts from one of eight
syntax-clean source families, applies a generated edit, and compares the fresh
and reused trees recursively, including kinds, flags, fields, and byte/point
ranges. The fuzz crate is not part of Raven's runtime dependency graph.

Publication evidence uses `nightly-2026-07-22`, rustc
`1.99.0-nightly (0e29c21d9 2026-07-21)`, cargo-fuzz 0.13.2, and
libfuzzer-sys 0.4.13 on `aarch64-apple-darwin`. Both AddressSanitizer campaigns
used seed 424242, `max_len=4096`, `timeout=5`, `rss_limit_mb=2048`,
`max_total_time=600`, `print_final_stats=1`, and `verbosity=0`:

| Target | Executions | Average/s | New units | Peak RSS | Output corpus |
|---|---:|---:|---:|---:|---|
| Arbitrary parser input | 8,879,519 | 14,774 | 381 | 532 MiB | 77 files / 2,378 bytes |
| Clean-base incremental edit | 4,338,332 | 7,218 | 451 | 615 MiB | 118 files / 9,506 bytes |

Both exited successfully with no sanitizer finding, timeout, or other defect.
The parser seed set is 4 files / 181 bytes with content-multiset SHA-256
`f36547265c8af99b20cf322950b64ee02a5e4182f0743667106061eaac268777`;
the incremental seed set is 3 files / 92 bytes with SHA-256
`7258ff0bf41dea7863cd1d4c303d9794d1b1caedb5affc30dfcf8b4821cab088`.
Exact commands, output-corpus hashes, source bindings, and the offline verifier
are in `fuzz/README.md` and `fuzz/evidence.json`.

## Known boundary

Tree-sitter core consumes a leading UTF-8 BOM before invoking a grammar. JAGS
4.3.2 rejects it. Differential tests therefore require the root node to cover
the complete input; a BOM yields a root beginning at byte 3 and is classified
as rejected. This is a documented core-normalization boundary, not an
unexplained grammar allowlist.

The grammar claims `.jags` only. `.bugs` is used by several related languages;
no OpenBUGS, WinBUGS, MultiBUGS, or NIMBLE compatibility is claimed.
