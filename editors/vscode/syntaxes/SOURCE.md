# Vendored grammar provenance

## `r.tmLanguage.json` and `rmd.tmLanguage.json`

- Upstream: <https://github.com/REditorSupport/vscode-R-syntax>
- Synced at commit: `d5fbd7aca79df03112d2d3ee306bd4bef8a5526c` (2026-03-08)
- License: MIT (see upstream `LICENSE`)
- Upstream file names: `syntaxes/r.json`, `syntaxes/rmd.json`
- Sync method: manual `curl` from the pinned commit. No build-time fetch.

### Why we vendor these specifically

Raven typically defers TextMate grammar discovery to sibling extensions via
the priority list in `editors/vscode/src/knit/grammar-registry.ts`. The
R and R Markdown grammars are the exception, because:

1. The knit-preview pipeline tokenizes R code blocks on the workspace
   extension host (where Raven runs), not the UI renderer.
2. `vscode.r` (the VS Code builtin) and `REditorSupport.r-syntax` are
   pure declarative extensions with no `main` field, so VS Code installs
   them with `extensionKind: "ui"`.
3. In remote workspaces (Remote SSH, Dev Containers, WSL, Codespaces),
   UI-only extensions are invisible to the workspace host, and their
   grammar files live on the UI machine's filesystem — unreachable from
   the remote workspace host even if visibility were added.
4. Vendoring is the only path that makes the grammar bytes reachable
   from where the knit pipeline runs. The same vendored grammars
   federate to the editor renderer via `contributes.grammars`, so
   `.Rmd` files highlight out of the box in remote setups. Raven also maps its
   `quarto` language ID to this same vendored R Markdown grammar so `.qmd`
   highlighting remains available when the `quarto.quarto` extension is
   disabled; the grammar bytes and scope are intentionally shared.

Sibling grammars still win when installed:
`editors/vscode/src/knit/grammar-registry.ts` carries both
`R_GRAMMAR_PRIORITY` (lists `reditorsupport.r-syntax`,
`reditorsupport.r`, and `vscode.r` ahead of Raven's own contribution)
and `RMD_GRAMMAR_PRIORITY` (`reditorsupport.r-syntax`,
`reditorsupport.r`, then Raven), looked up per language via
`GRAMMAR_PRIORITY_BY_LANGUAGE`.

### Other grammars in this directory

The remaining files (`dcf.tmLanguage.json`, `jags.tmLanguage.json`,
`namespace.tmLanguage.json`, `rbuildignore.tmLanguage.json`,
`rd.tmLanguage.json`, `stan.tmLanguage.json`) cover languages Raven
contributes as first-class language IDs but for which no widely-installed
sibling extension exists. They are maintained in-tree and have no
upstream sync point.

## Sync procedure

To resync `r.tmLanguage.json` / `rmd.tmLanguage.json`:

1. Pick a commit on <https://github.com/REditorSupport/vscode-R-syntax>.
2. `curl -sL https://raw.githubusercontent.com/REditorSupport/vscode-R-syntax/<sha>/syntaxes/r.json -o editors/vscode/syntaxes/r.tmLanguage.json`
3. `curl -sL https://raw.githubusercontent.com/REditorSupport/vscode-R-syntax/<sha>/syntaxes/rmd.json -o editors/vscode/syntaxes/rmd.tmLanguage.json`
4. Update the commit hash and date above.
5. Update the matching entry in the repo-root `NOTICE` file.
6. Verify that `scopeName` (`source.r`, `text.html.markdown.rmarkdown`)
   and the rmd `embeddedLanguages` map in `editors/vscode/package.json`
   still match the new upstream `package.json`. If upstream renames a
   scope or adds an embedded language we want surfaced, update the
   manifest in lockstep. The `rmd` and `quarto` contributions share that
   scope, so both entries must carry identical `embeddedLanguages` (and
   `tokenTypes`, if added) metadata; VS Code keeps one definition per scope
   and either entry may otherwise clobber embedded-language behavior.
7. If you add or remove a vendored grammar (not merely resync the bytes of
   an existing one), keep the user-facing docs that describe syntax
   highlighting and remote-host behavior in lockstep —
   `docs/syntax-highlighting.md` and `docs/knit.md`.
