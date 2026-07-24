# Raven

Raven is a language server for R, Stan, and JAGS. Its defining idea: **what's in scope depends on where your cursor is.** Raven traces `source()` chains and resolves scope at your position, so completions, diagnostics, and navigation reflect what's actually defined when each line runs — across files, and within a single script (a variable defined on line 50 isn't in scope on line 10).

Because scope is resolved by position, Raven can flag genuinely undefined variables — and, parsing as you type, it catches parse errors (unclosed brackets, an `else` stranded from its `}`) and likely-bug patterns like mixed logical operators (`a & b | c`).

[vscode-R](https://marketplace.visualstudio.com/items?itemName=REditorSupport.r) (REditorSupport's R extension) is the established R extension for VS Code; Raven's language server runs alongside it, contributing cross-file, scope-aware code intelligence (plus configurable syntax-aware indentation) on top of what you already have.

vscode-R's language intelligence comes from [r-language-server](https://github.com/REditorSupport/languageserver), an R package that runs inside an R session and indexes the documents you have open (and, in an R package, its `R/` directory). Raven is written in Rust to be fast, and needs no R session: it indexes your whole workspace and follows `source()` chains, so completions and navigation reach symbols in files you haven't opened — jump straight to a variable's definition in another file.

Raven is designed to complement, not replace, your existing tools. But it *can* run without vscode-R installed, if you want: then Raven uses its own R console, with data and plot viewers. Otherwise — if vscode-R is installed, or you're running inside Positron — those features stay off by default; they simply don't appear, so Raven leaves your existing setup untouched.

## Features

### Code intelligence

- **[Completions](https://github.com/jbearak/raven/blob/main/docs/completion.md)** — Symbols, packages, and function parameters — across files
- **[Go-to-definition](https://github.com/jbearak/raven/blob/main/docs/go-to-definition.md)** — Jump to definitions across file boundaries
- **[Find references](https://github.com/jbearak/raven/blob/main/docs/find-references.md)** — Locate all usages of a symbol across your project
- **[Hover](https://github.com/jbearak/raven/blob/main/docs/hover.md)** — Symbol info including source file and package origin
- **[Diagnostics](https://github.com/jbearak/raven/blob/main/docs/diagnostics.md)** — Undefined variable detection that understands sourced files and loaded packages, plus opt-in [style/lint rules](https://github.com/jbearak/raven/blob/main/docs/linting.md)
- **[Document outline](https://github.com/jbearak/raven/blob/main/docs/document-outline.md)** — Hierarchical view with sections, classes, and nested functions
- **Workspace symbols** — Project-wide symbol search (Cmd/Ctrl+T)
- **File path intellisense** — Completions and cmd-click inside `source()` paths
- **[Smart indentation](https://github.com/jbearak/raven/blob/main/docs/indentation.md)** — Context-aware auto-indent with configurable argument and chain styles
- **[Cross-file awareness](https://github.com/jbearak/raven/blob/main/docs/cross-file.md)** — Follows `source()` chains to resolve scope across files
- **[Directives](https://github.com/jbearak/raven/blob/main/docs/directives.md)** — Declare relationships and symbols the analyzer can't infer
- **[Syntax highlighting](https://github.com/jbearak/raven/blob/main/docs/syntax-highlighting.md)** — R function names via LSP semantic tokens, plus JAGS and Stan syntax highlighting

Raven also provides lightweight support for **JAGS** (`.jags`, `.bugs`, `.bug`) and **Stan** (`.stan`) files: syntax highlighting, completions (keywords, distributions, file-local symbols), strict syntax diagnostics, go-to-definition, find references, and document outline with model structure navigation. All three JAGS extensions are matched case-insensitively and validated as strict JAGS, not as general OpenBUGS, WinBUGS, MultiBUGS, or NIMBLE input.

### R session integration

- **[R console](https://github.com/jbearak/raven/blob/main/docs/r-console.md)** — Interactive R console with statement detection and a temp-file fallback for large blocks; supports R, arf, and radian
- **[Code chunks](https://github.com/jbearak/raven/blob/main/docs/chunks.md)** — R Markdown / Quarto chunk detection with Run Chunk / Run Above / Run All commands, CodeLens buttons, navigation, and background highlighting; `# %%` cell support in `.R` files
- **[Knit Preview + Export](https://github.com/jbearak/raven/blob/main/docs/knit.md)** — `Raven: Knit Preview` renders R Markdown to an HTML preview without requiring Pandoc; companion `Export to HTML / PDF / Word` commands save the result next to the `.Rmd` via Pandoc
- **[Plot viewer](https://github.com/jbearak/raven/blob/main/docs/plot-viewer.md)** — Plots render in a VS Code panel via [httpgd](https://nx10.dev/httpgd/), with history navigation, save (PNG/SVG/PDF), and theme-aware background
- **[Data viewer](https://github.com/jbearak/raven/blob/main/docs/data-viewer.md)** — `View(df)` opens a virtualized grid backed by Apache Arrow; viewport-based rendering keeps scrolling responsive on multi-million-row frames
- **[Help viewer](https://github.com/jbearak/raven/blob/main/docs/help-viewer.md)** — Scope-aware R help: hovering shows the function in scope at the cursor instead of falling through to a multi-package list when scope can't be inferred

## Settings

All settings live under the `raven.*` prefix. See the [full configuration reference](https://github.com/jbearak/raven/blob/main/docs/configuration.md) for the complete list.

## Coexistence with vscode-R and Positron

Raven's R-console features (R console, plot viewer, data viewer) and [vscode-R](https://github.com/REditorSupport/vscode-R) cover overlapping ground. By default `raven.rConsole.activation` is `"auto"`, which leaves Raven's R-console features off when vscode-R is enabled or you're running inside Positron. Raven's help viewer and language server activate either way.

Raven ships its own [opt-in style linter](https://github.com/jbearak/raven/blob/main/docs/linting.md) — a subset of `lintr`'s rules re-implemented natively, with no R session or `lintr` install required. For `lintr` rules [outside that subset](https://github.com/jbearak/raven/blob/main/docs/linting.md#gaps-vs-lintr), vscode-R's `lintr` diagnostics run from its own language server.

Two vscode-R settings (both default to `true`) let you trim that overlap:

- **`r.lsp.diagnostics`** — set to `false` to silence `lintr` while keeping vscode-R's session-based completions.
- **`r.lsp.enabled`** — set to `false` to shut vscode-R's language server down entirely. Use this when you only want vscode-R for its R-session features (console, viewers) and are happy to let Raven handle all code intelligence.

```json
"r.lsp.diagnostics": false,   // keep vscode-R's LSP, drop only its lintr diagnostics
"r.lsp.enabled": false        // or: disable vscode-R's LSP entirely
```

For a deeper comparison see [docs/comparison.md](https://github.com/jbearak/raven/blob/main/docs/comparison.md).

## More Information

See the [main repository](https://github.com/jbearak/raven) for full documentation, including running the same analysis in CI with [`raven check`](https://github.com/jbearak/raven/blob/main/docs/cli.md), cross-file directives, editor integrations, and comparison with other R tools.

If you work with Stata, see Raven's sibling project, [Sight](https://github.com/jbearak/sight), a Stata language server with the same cross-file awareness model.

## License

[GPL-3.0](https://github.com/jbearak/raven/blob/main/LICENSE)
