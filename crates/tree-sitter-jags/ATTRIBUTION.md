# Attribution and clean-room boundary

The JAGS grammar is an independently authored implementation based on the
black-box observations under `oracle/`. No JAGS source, parser file, or manual
prose was inspected or copied. JAGS is not linked into this crate.

Raven also has an optional, checksum-pinned official-model holdout documented in
`../../docs/diagnostic-corpora.md`. It was added post-hoc and is fetch-only; the
selected model files were not manually inspected or used to derive grammar
productions. The upstream files are neither
committed nor distributed with Raven. This quality check does not weaken or
replace the clean-room boundary above.

Implementation structure and small Tree-sitter idioms were informed by these
MIT-licensed references already pinned by Raven:

- `tree-sitter-r`, revision
  `8ac99ed1e7ad319737fc11dde20c07d1e1942383`, copyright 2025
  tree-sitter-r authors.
- `tree-sitter-stan`, revision
  `86544507c3600d5c4719d98ada477123fee81983`, copyright 2023 Brian
  Ward.

The R reference informed compatible expression, call, subset, identifier, and
operator-rule structure. The Stan reference informed ordered program blocks,
named block nodes, C-style block-comment tokenization, and Rust build layout.
JAGS-specific program order, non-empty blocks, relations, bounds, separators,
tokens, and rejected constructs come from the black-box matrix.

Both reference licenses permit use and modification when their notices are
preserved. Their complete MIT notices are recorded in the repository `NOTICE`.
