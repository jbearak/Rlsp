# Raven

Raven is a static analyzer for R that resolves what's in scope at each line — catching undefined and used-before-defined variables even in a single script, and across files by following `source()` chains — without running your code. The same scope model drives the rest of its code intelligence — completions, go-to-definition, find-references, hover — so each reflects what's reachable at your cursor, not just what exists somewhere in the project. It also diagnoses syntax errors, and can optionally check code style and formatting.

Raven is fast and runs all of this in realtime in your editor, with diagnostics updating instantly as you type. It also fills a gap at the other end of the workflow: automated checks in pull requests — running `raven check` flags undefined variables and other errors before code merges, the kind of automated review that's table stakes for other languages but has long been missing for R.

Doing this well in R is hard — part of why so little static tooling exists for the language. R leans heavily on [non-standard evaluation](docs/non-standard-evaluation.md) (NSE): a function can take its arguments as unevaluated code and decide what the symbols mean at runtime, so `col` in `dplyr::filter(df, col > 0)` is a data-frame column, not an undefined variable. A checker that didn't account for this would flag idiomatic R as errors; Raven recognizes these NSE contexts and leaves them unflagged — though code that pulls names into scope at runtime, such as `attach()`, can still draw a false positive, which you resolve by declaring those symbols with a [directive](docs/directives.md). The same analysis isn't only defensive: wherever it can determine an object's structure statically, Raven completes its fields — start typing `fruit$a` and it can suggest `apple` the moment you open the file, with no R session.

Beyond code intelligence, Raven's VS Code extension can bring the rest of an R workflow into the editor — an [R console](docs/r-console.md) with [plot](docs/plot-viewer.md) and [data](docs/data-viewer.md) viewers — but it's built to add to your setup rather than change it: those R-session features defer by default to REditorSupport or Positron, and the language server runs alongside or instead of [r-language-server](https://github.com/REditorSupport/languageserver) ([details below](#how-raven-differs)).

Raven is fully open source ([GPL-3.0](LICENSE)) and editor-agnostic: it speaks the Language Server Protocol, so it runs in VS Code, Neovim, Zed, or any LSP client — including over VS Code Remote-SSH, so you can develop on a remote server with more compute than your laptop.

> **Status:** Raven is under active development. It works well for day-to-day use but hasn't been widely announced yet. Bug reports and feedback are welcome!

> **Quick Start:** Install from the [VS Code Marketplace](https://marketplace.visualstudio.com/items?itemName=jbearak.raven-r) or [OpenVSX](https://open-vsx.org/extension/jbearak/raven-r), or download from the [releases page](https://github.com/jbearak/raven/releases). See [Installation](#installation) for details.

Raven's sibling project [Sight](https://github.com/jbearak/sight) implements a language server for Stata. Together they bring cross-file navigation, error detection, and code intelligence to two languages widely used in social science research.

## Features

### Code intelligence

- **[Completions](docs/completion.md)** — Symbols, packages, and function parameters — across files
- **[Go-to-definition](docs/go-to-definition.md)** — Jump to definitions across file boundaries
- **[Find references](docs/find-references.md)** — Locate all usages of a symbol across your project
- **[Hover](docs/hover.md)** — Symbol info including source file and package origin
- **[Diagnostics](docs/diagnostics.md)** — Undefined variable detection that understands sourced files and loaded packages
- **[Non-standard evaluation](docs/non-standard-evaluation.md)** — NSE-aware diagnostics for data-masking, tidyselect, ggplot mappings, Shiny, foreach, and custom helpers
- **[Linting](docs/linting.md)** — Opt-in style/lint rules (line length, trailing whitespace, trailing blank lines, tabs, assignment operator, object names, infix spaces, commented code, indentation)
- **[Document outline](docs/document-outline.md)** — Hierarchical view with sections, classes, and nested functions
- **Workspace symbols** — Project-wide symbol search (Cmd/Ctrl+T)
- **File path intellisense** — Completions and cmd-click inside `source()` paths
- **[Smart indentation](docs/indentation.md)** — Context-aware auto-indent with RStudio-style alignment
- **[Cross-file awareness](docs/cross-file.md)** — Follows `source()` chains to resolve scope across files
- **[Directives](docs/directives.md)** — Declare relationships and symbols the analyzer can't infer
- **[Syntax highlighting](docs/syntax-highlighting.md)** — R function names via LSP semantic tokens, plus JAGS and Stan syntax highlighting

Raven also provides lightweight support for **JAGS** (`.jags`, `.bugs`) and **Stan** (`.stan`) files: [syntax highlighting](docs/syntax-highlighting.md#jags-and-stan), [completions](docs/completion.md#jags-and-stan) (keywords, distributions, file-local symbols), [go-to-definition](docs/go-to-definition.md#jags-and-stan), [find references](docs/find-references.md#jags-and-stan), and [document outline with model structure navigation](docs/document-outline.md#jags-and-stan-model-structure).

### R session integration

- **[R console](docs/r-console.md)** — Interactive R console with statement detection and a temp-file fallback for large blocks; supports R, arf, and radian
- **[Code chunks](docs/chunks.md)** — R Markdown / Quarto chunk detection with Run Chunk / Run Above / Run All commands, CodeLens buttons, navigation, and background highlighting; full R language intelligence (diagnostics, completion, hover, navigation) inside chunk bodies; `# %%` cell support in `.R` files
- **[Knit Preview + Export](docs/knit.md)** — `Raven: Knit Preview` renders R Markdown to an HTML preview without requiring Pandoc; companion `Export to HTML / PDF / Word` commands save the result next to the `.Rmd` via Pandoc
- **[Plot viewer](docs/plot-viewer.md)** — Plots render in a VS Code panel via [httpgd](https://nx10.dev/httpgd/), with history navigation, save (PNG/SVG/PDF), and theme-aware background
- **[Data viewer](docs/data-viewer.md)** — `View(df)` opens a virtualized grid backed by Apache Arrow; viewport-based rendering keeps scrolling responsive on multi-million-row frames
- **[Help viewer](docs/help-viewer.md)** — Scope-aware R help: hovering shows the function in scope at the cursor instead of falling through to a multi-package list when scope can't be inferred

## How Raven Differs

Raven takes a static-analysis approach rather than attaching to a live R session, so it can start answering questions the moment a file is opened, without running user code. That has four practical consequences:

- **Available immediately, even for code you haven't run** — answers the moment you open a file, including code that errors halfway, is missing a dependency, or that you're only reading (onboarding to a repo, reviewing a pull request). A session-based tool can offer little until the code runs cleanly.
- **Reflects what your code says, not what your session remembers** — a tool tied to a live session sees whatever is in `globalenv()` right now, possibly stale. Comment out `library(dplyr)` while it's still attached in your session and a session-based tool keeps completing `dplyr` functions; Raven reads the file and knows it isn't loaded there.
- **Read-only and side-effect-free** — computing scope never runs your code, so nothing it does (writing files, hitting a database, a long job) can be triggered. This is also what makes Raven safe to run behind an agentic/AI tool.
- **Runs in CI and other headless environments** — scope resolution needs no live R session, so Raven's diagnostics and lints run in a CI pipeline or any headless context. Use [`raven check`](docs/cli.md#raven-check) for the full diagnostic set (cross-file, undefined-variable, package) and [`raven lint`](docs/cli.md#raven-lint) for style-only gating.

See [Why Raven exists](docs/comparison.md#why-raven-exists) for the origin and rationale.

**Built to coexist, not replace.** Raven layers onto your existing R setup two different ways, and neither one takes anything over.

*The language server is additive.* Its reason for being is static, cross-file analysis, and it runs happily next to [r-language-server](https://github.com/REditorSupport/languageserver): install Raven and you get its diagnostics, completions, and navigation on top of whatever you already run. If you'd rather not have two providers offering completions, set `r.lsp.enabled` to `false` to turn REditorSupport's language server off and let Raven handle code intelligence alone. If you want to keep REditorSupport's language server but not its [`lintr`](https://lintr.r-lib.org/) style diagnostics, set `r.lsp.diagnostics` to `false` — you'll keep Raven's syntax and scope diagnostics without style linting. And if you want some of that style linting back, Raven ships its own opt-in lints (`raven.linting.enabled`), configurable through a `.lintr` file for backward compatibility, a `raven.toml`, or VS Code's settings UI — a point-and-click alternative to writing `.lintr` syntax by hand. Raven also folds in [RStudio-style auto-indentation](docs/indentation.md) as you type; it never reformats whole files, so there's nothing for a dedicated formatter like [Air](https://github.com/posit-dev/air) (Posit) to collide with. The community has dedicated tools for these jobs — Air for formatting, [lintr](https://lintr.r-lib.org/) or [Jarl](https://github.com/etiennebacher/jarl) for linting — and Raven is built to run alongside them.

*The R console and viewers defer to whatever you already run.* Raven's [R console](docs/r-console.md), its [plot](docs/plot-viewer.md) and [data](docs/data-viewer.md) viewers, and its [Knit Preview](docs/knit.md) overlap with what REditorSupport, RStudio, and Positron already give you — and if you like their live-session, workspace-watcher style of working, Raven isn't trying to replace it. By default these features defer: under `raven.rConsole.activation`'s `auto` setting they step aside whenever REditorSupport is enabled or you're running in Positron, so installing Raven never displaces the R integration you already have. They exist for two reasons. First, having built a language server that deliberately needs no live R session, I didn't want to then be forced to pair it with a tool that injects itself into one — so Raven gives you a complete R workflow you *can* run on its own. Second, I had particular preferences these viewers are built around: a data viewer that stays responsive on millions of rows, reopens after a VS Code restart, remembers sorts and filters across repeated `View()` calls, and shows value labels from imported Stata / SPSS data, all without waiting on a live R session (the frame streams out to Arrow on disk); help that lists only the function actually in scope (hover `print()` and you get `base::print`, not a list of every package that happens to define a `print`); and a fast `.Rmd` preview that themes prose, syntax highlighting, and plots to match your editor. Turn them on everywhere with `raven.rConsole.activation: "enabled"`. (Raven's scope-aware [help viewer](docs/help-viewer.md) remains available regardless -- it only opens when you click the link in one of Raven's hovers, so it doesn't get in the way of anything else.)

For a detailed comparison with RStudio, Positron (Ark), and REditorSupport — covering both language intelligence and R session integration — see [docs/comparison.md](docs/comparison.md).

## Installation

**VS Code:** Install from the [VS Code Marketplace](https://marketplace.visualstudio.com/items?itemName=jbearak.raven-r) 

**Cursor, Positron, and other VS Code-based editors:** Install from [OpenVSX](https://open-vsx.org/extension/jbearak/raven-r) or download the .vsix file from the [releases page](https://github.com/jbearak/raven/releases) and install manually.

**Other editors:** Download a pre-built binary from the [releases page](https://github.com/jbearak/raven/releases), then run `raven --stdio` and connect via your editor's LSP client. See [Editor Integrations](docs/editor-integrations.md) for Zed, Neovim, and AI agent configurations.

**GitHub Actions:** Use `jbearak/setup-raven@v1` to install the pre-built CLI, then run `raven check` with the flags you want. See [CLI](docs/cli.md#github-actions-example).

**Bitbucket Pipelines and Ubuntu CI:** Install Raven from the signed apt repository in an Ubuntu image, then run `raven check`. See [CLI](docs/cli.md#bitbucket-pipelines-example).

**Build from source:** Install with `cargo install --git https://github.com/jbearak/raven raven`, or build from a local checkout. See [Development Notes](docs/development.md).

## Documentation

Each feature above links to its own page. Beyond those:

- [Editor Integrations](docs/editor-integrations.md) — VS Code, Zed, Neovim, AI agents
- [Configuration](docs/configuration.md) — All settings and options ([alphabetical reference](docs/settings-reference.md))
- [CLI](docs/cli.md) — `raven check` (full diagnostics) and `raven lint` (style) for CI and command-line usage
- [R Package Development](docs/r-package-dev.md) — Package mode, visibility rules, and build commands
- [`.Rprofile` Startup Prelude](docs/rprofile.md) — How Raven models project startup files for script scope
- [Non-Standard Evaluation](docs/non-standard-evaluation.md) — How Raven handles NSE, when to add `# raven: nse`, and when to use other escape hatches
- [Speeding Up Cross-File Analysis](docs/cross-file-analysis-performance.md) — When `# raven: self-contained` improves analysis of high-fan-out source graphs
- [Coexistence](docs/coexistence.md) — Running alongside REditorSupport (vscode-R) and Positron
- [Comparison](docs/comparison.md) — How Raven compares to other R tools
- [Chunk Keybinding Comparison](docs/keybinding-comparison.md) — Chunk shortcuts across Raven, Quarto, RStudio, and REditorSupport
- [Limitations](docs/limitations.md) — Features not yet implemented

## Development

See [Development Notes](docs/development.md) for build/test, profiling, and internal architecture.

## Provenance

Raven includes code derived from [Ark](https://github.com/posit-dev/ark) (MIT License, Posit Software, PBC) — initial LSP wiring and tree-sitter scaffolding — and from Raven's sister project [Sight](https://github.com/jbearak/sight) (also GPL-3.0) — the cross-file awareness system (directives + position-aware scope model). The bundled R and R Markdown TextMate grammars come from [vscode-R-syntax](https://github.com/REditorSupport/vscode-R-syntax) (MIT).

Raven's downloadable package-symbol database is built from CRAN/Bioconductor metadata published by [r-universe](https://r-universe.dev), maintained by [rOpenSci](https://ropensci.org/r-universe/). See [Package database](docs/package-database.md#data-source-and-acknowledgement).

## License

[GPL-3.0](LICENSE). See [NOTICE](NOTICE) for third-party attributions.
