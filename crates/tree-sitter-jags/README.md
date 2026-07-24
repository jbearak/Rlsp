# tree-sitter-jags

An in-tree, clean-room Tree-sitter grammar targeting the syntax accepted by
JAGS 4.3.2. The crate is independently testable and supplies Raven's native
JAGS parse trees and strict JAGS-dialect syntax diagnostics for standalone
`.jags`, `.bugs`, and `.bug` programs.

The normative evidence is the public-command-line matrix under `oracle/`.
Tree-sitter R and Stan are implementation references, not syntax authorities.

Install the pinned CLI, verify generated artifacts, and run the exact-shape
Tree-sitter corpus from this directory:

```sh
npm ci
npm run check:generated
npm run check:oracle
npm run check:evidence
npm test
```

Run the Rust correctness, live-oracle, and release performance gates from the
repository root:

```sh
cargo test -p tree-sitter-jags
cargo test -p tree-sitter-jags --test quality_gates \
  all_committed_outcomes_match_live_jags -- --ignored
cargo test --release -p tree-sitter-jags --test performance -- --ignored
```

The normal oracle check is offline: it verifies the deterministic corpus and
the committed 806-result manifest's input and tool hashes. The live check
requires the exact JAGS 4.3.2 wrapper and terminal hashes recorded in
`oracle/provenance.json` and uses only the public command-line interface.

Raven applies strict diagnostics to `.jags`, `.bugs`, and `.bug`,
case-insensitively. These suffixes select JAGS 4.3.2 syntax and do not claim
general OpenBUGS, WinBUGS, MultiBUGS, or NIMBLE compatibility.

See `QUALITY_GATES.md` for quantitative evidence and limitations and
`PRODUCTION_MAPPING.md` for the clean-room rule mapping.
