# JAGS black-box syntax oracle

This directory records independently authored probes of the public JAGS 4.3.2
command-line interface. The grammar is derived from these observed inputs and
outputs, not from JAGS source code or prose documentation.

`model in "PATH"` is the syntax boundary. The controls establish that unknown
functions and distributions pass that command and fail only after a subsequent
`compile`, while malformed delimiters and expressions fail during `model in`.
The oracle therefore treats the parse-error heading as syntax rejection and
does not interpret compilation errors as syntax errors.

Regenerate the deterministic quality corpus and verify all committed evidence
without starting JAGS:

```sh
python3 crates/tree-sitter-jags/oracle/generate_quality_corpus.py --check
python3 crates/tree-sitter-jags/oracle/jags_oracle.py --verify-results
cd crates/tree-sitter-jags/oracle
python3 -m unittest -v test_jags_oracle.py
```

`oracle-results.json` records 798 outcomes: 115 matrix probes and 683 quality
cases. It binds the exact syntax matrix, generated corpus, corpus generator,
oracle harness, and canonical source set by SHA-256. CI performs the offline
checks above, so an authored source, generator, harness, outcome, or binding
change cannot silently inherit old JAGS evidence.

On the pinned macOS/Homebrew installation, compare every committed result with
a fresh public-CLI observation or deliberately refresh the manifest with:

```sh
python3 crates/tree-sitter-jags/oracle/jags_oracle.py --verify-results-live
python3 crates/tree-sitter-jags/oracle/jags_oracle.py --refresh-results
```

Each probe has a five-second default timeout. Before any probe runs, the
harness checks both `/opt/homebrew/bin/jags` and the actual terminal executable
against the hashes in `provenance.json`. `--allow-unpinned-oracle` supports an
independently validated platform build for exploratory verification, but the
harness refuses to refresh committed results under that override.

The provenance manifest pins the official release URL and SHA-256 reported by Homebrew.
The release archive was downloaded only to verify its digest; its contents were
not inspected. The installed terminal executable digest is separate from the
shell-wrapper digest.

The matrix targets JAGS 4.3.2 only. It is not a compatibility claim for
OpenBUGS, WinBUGS, MultiBUGS, NIMBLE, or other BUGS-family implementations.
