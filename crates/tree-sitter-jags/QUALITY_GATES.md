# Quality gates and evidence

The grammar targets the parse phase of JAGS 4.3.2. `model in` is the observed
syntax boundary: unknown functions and distributions pass it and fail only
after `compile`, while malformed expressions and delimiters fail during it.

## Deterministic corpora

- 78 hand-authored black-box matrix probes: 34 accepted, 44 rejected.
- 320 generated valid models, all accepted by JAGS and error-free here.
- 64 syntax-valid semantic failures, all accepted by JAGS during parsing,
  rejected during compilation, and error-free here.
- 96 curated syntax-invalid cases across 12 defect families, all rejected by
  JAGS and detected here with an `ERROR`/`MISSING` node intersecting the
  recorded defect line.
- 32 incremental sequences, including CRLF and BMP/astral comments; every
  incremental tree equals a fresh parse through valid, incomplete, and
  restored states.
- 1,024 generated valid-property cases and 1,024 arbitrary-UTF-8 range/panic
  cases.

Ten mutation categories have 20 real-JAGS-rejected cases each. Detection is
20/20 (100%) in every category; there are no escaped cases or allowlist.

## Performance

Release-mode local medians (three samples, Apple Silicon) are:

| Input | Median |
|---|---:|
| 1 KiB valid | 0.057 ms |
| 10 KiB valid | 0.485 ms |
| 100 KiB valid | 3.799 ms |
| 100 KiB malformed | 4.390 ms |
| 100 KiB one-byte incremental edit | 2.095 ms |
| 1 MiB valid | 34.828 ms |

CI enforces 5/25/250/250/50/2000 ms respectively, with a threefold allowance
for shared CI hardware. Cancellation of a 100,000-relation input is separately
required to return no partial tree within 100 ms.

The generated `parser.c` is 96,457 bytes and the locally built release rlib is
63,520 bytes. Raven does not depend on this crate in this PR, so its runtime
binary and extension package size are unchanged.

## Fuzzing

Two 601-second cargo-fuzz campaigns completed without crashes, timeouts, or
artifacts:

- Arbitrary parser input: 69,940,031 executions, 116,372/s average, 29 MiB
  peak RSS.
- Arbitrary incremental edits with fresh-tree equivalence: 62,758,643
  executions, 104,423/s average, 28 MiB peak RSS.

This host lacked a nightly sanitizer runtime, so the campaigns used cargo-fuzz
0.13.2 coverage instrumentation with `--sanitizer none` and
`RUSTC_BOOTSTRAP=1`. The fuzz targets remain ready for sanitizer-enabled runs
on a nightly-equipped host.

## Known boundary

Tree-sitter core consumes a leading UTF-8 BOM before invoking a grammar. JAGS
4.3.2 rejects it. Differential tests therefore require the root node to cover
the complete input; a BOM yields a root beginning at byte 3 and is classified
as rejected. This is a documented core-normalization boundary, not an
unexplained grammar allowlist.

The grammar claims `.jags` only. `.bugs` is used by several related languages;
no OpenBUGS, WinBUGS, MultiBUGS, or NIMBLE compatibility is claimed.
