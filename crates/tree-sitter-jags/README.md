# tree-sitter-jags

An in-tree, clean-room Tree-sitter grammar targeting the syntax accepted by
JAGS 4.3.2. This crate is an independently testable grammar only; it does not
change Raven's runtime parser routing, diagnostics, CLI, or editor behavior.

The normative evidence is the public-command-line matrix under `oracle/`.
Tree-sitter R and Stan are implementation references, not syntax authorities.

Generate pinned parser artifacts from this directory:

```sh
npm install
npm run generate
```

The grammar intentionally claims only `.jags`. The `.bugs` suffix is shared by
multiple related languages; strict compatibility is not established here.
