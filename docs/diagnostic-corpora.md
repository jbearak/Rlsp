# External diagnostic corpora

Raven has a pinned external holdout for its native Stan and JAGS diagnostics.
The holdout complements the authored fixtures and generated cases in the
repository: it asks whether Raven remains quiet on oracle-accepted complete
models and documentation snippets published by the upstream projects, without
turning those examples into parser requirements or development examples.

The corpus is **post-hoc**. It was added after the corresponding parsers and
diagnostic collectors were implemented, and it is not used to derive grammar
productions, diagnostic rules, recovery behavior, or built-in catalogs. A
failure is investigated against Raven's documented language boundary; an
upstream model is not automatically a specification that Raven must accept.

## What is committed

The repository commits only the corpus manifests and tooling. The Stan and JAGS
manifests live under `crates/raven/tests/fixtures/diagnostic_corpora/`; they pin
each upstream archive/revision, checksum, license/provenance metadata, and the
discovery rules used by the holdout. The upstream archives and extracted models
are not committed, bundled into Raven releases, or included in the VSIX.

`check` is deliberately offline. It validates the committed manifest, hashes,
path allowlists, expected materialization layout, and attribution metadata
without downloading external content:

```sh
python3 scripts/diagnostic-corpora.py check
```

CI runs this check on every integration run, including documentation-only pull
requests, so the stable required check never disappears because of a path
filter.

## Materialize and test

Full validation has an explicit network boundary:

```sh
python3 scripts/diagnostic-corpora.py materialize --all
node editors/vscode/scripts/check-stan-diagnostics-fixtures.mjs \
  --check-external --external-root target/diagnostic-corpora
python3 crates/tree-sitter-jags/oracle/jags_oracle.py \
  --verify-external --external-root target/diagnostic-corpora
cargo test -p raven --features test-support \
  --test external_model_diagnostics -- --ignored
```

`materialize --all` downloads the pinned upstream artifacts, verifies their
digests before use, safely re-extracts regular files from the verified cached
archives on every run, and materializes only candidates selected by the manifest
discovery rules. Extracted trees are never trusted as caches. The directory is
disposable build output; pass `clean --downloads` to discard the verified archive
cache as well.

The Stan oracle uses Raven's pinned development-time stanc3 compiler to classify
every discovered Stan candidate and to confirm the positive subset before the
ignored Rust runner checks Raven's native diagnostics. The JAGS verifier binds
the materialized models to committed outcomes obtained from pinned JAGS 4.3.2;
it accounts for all 60 discovered models and admits the 59 accepted by `model
in` as no-false-positive cases. The Rust runner covers both positive subsets.
These tools are test-only: Raven does not download corpora, start either oracle,
or contact the network at runtime.

For JAGS, materialization is fetch-only and validation is post-hoc. The corpus pipeline does not inspect or use JAGS implementation or parser
sources. The selected official model files are consumed automatically as opaque
holdout inputs; they were not manually inspected and were not used to author the
clean-room grammar. This is separate from the independently authored black-box
command-line probes under `crates/tree-sitter-jags/oracle/`.

## CI policy

The `External Diagnostic Corpora` integration job always performs the offline
manifest check. It performs downloads, the Stan external oracle, and the ignored
Rust runner on pushes to `main` and on pull requests that touch the corpus
controls, model parsers, diagnostic implementation, or their tests. The path
filter is intentionally conservative; unrelated pull requests retain the same
successful job name while skipping only the network-heavy portion.

Tag-triggered release builds do not use a path filter. Full materialization and
both external validations are a required preflight dependency of every platform
build, preventing a release from bypassing the holdout because the tagged commit
arrived through an unrelated-looking change.

When changing the corpus manifest, fetch tooling, source pins, or allowlist,
update the relevant attribution and provenance records in `NOTICE`, this file,
and the JAGS clean-room evidence documents. Do not commit materialized upstream
files.
