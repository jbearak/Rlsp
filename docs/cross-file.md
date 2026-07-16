# Cross-File & Package Awareness

Raven builds a dependency graph of your R project and uses it to provide scope-aware completions, diagnostics, and navigation across file boundaries. This page explains how the system works.

## How It Works

Most R projects consist of multiple files connected by `source()` calls. Raven detects these relationships automatically:

```r
# main.R
library(dplyr)
source("utils.R")
result <- helper_function(42)  # Raven knows this comes from utils.R
```

```r
# utils.R
helper_function <- function(x) { x * 2 }
```

When you open a file in a workspace with detectable `source()` patterns, Raven:
1. Scans the workspace for `source()` calls and builds a dependency graph
2. Resolves which symbols are available at each position in each file
3. Provides completions, diagnostics, hover, and go-to-definition using the full graph

This happens automatically for standard `source()` patterns.

## Automatic source() Detection

Raven detects `source()` and `sys.source()` calls:
- Single and double quotes: `source("path.R")` or `source('path.R')`
- Named arguments: `source(file = "path.R")`
- `local = TRUE` and `chdir = TRUE` parameters
- `source(system.file("helper.R", package = "pkg"))` — the `system.file()` path is resolved statically: for the package being analyzed it maps to the source-tree `inst/` directory, and for an installed package it is found under the library paths (so a helper sourced this way contributes its definitions like any other `source()` target). Resolution tracks package lifecycle events live: installing or removing the referenced package mid-session, or renaming the workspace package in `DESCRIPTION`, re-resolves these edges without editing the file or restarting
- **Computed paths** built from statically-known parts are folded to a plain path (issue #638): `file.path("scripts", "helpers.R")` (all parts must fold; `fsep` accepted only as the default `"/"`; any other named argument bails), a `normalizePath(...)` wrapper (peeled syntactically; `winslash`/`mustWork` ignored), and a local variable assigned exactly once at the top level before use — so the common testthat idiom `repo_root <- normalizePath(file.path("..", "..")); source(file.path(repo_root, "scripts/helpers.R"))` resolves. Folding is strict all-or-nothing: any component that isn't statically known (another function call, a reassigned, replacement-modified, removed, or conditionally-assigned variable, a shadowed `file.path`/`normalizePath`, or a string literal containing escape sequences) makes Raven skip the call rather than guess. It is also a purely lexical analysis — `setwd()` calls and symlink resolution that runtime `normalizePath()` would perform are ignored, the same limitation every literal path already has. Folded paths work across dependency edges, scope, missing-file diagnostics, and cmd-click navigation (from any segment of the expression)
- Other dynamic paths (e.g. `paste0(...)`, `Sys.getenv(...)`) are skipped gracefully

`sys.source()` defaults to a non-global environment, so its symbols are treated as local and do **not** propagate to the calling file unless you pass `envir = globalenv()` (or `.GlobalEnv`).

**`local = TRUE` inheritance.** `source("child.R", local = TRUE)` evaluates the child in the environment from which `source()` is called. At the **top level** of a script that environment is the global environment, so the child sees all of the parent's bindings defined before the call — exactly like the default `local = FALSE` — and Raven resolves the parent's earlier definitions in the child accordingly. Only when the `source(local = TRUE)` call sits **inside a function body** does the child bind to that function's frame rather than the globals; in that case the child does not inherit the parent's top-level symbols through the relationship (declared symbols from `# raven: var` directives still flow). The child's *own* new definitions never leak back out to the parent's global scope under `local = TRUE`.

**Case-only filename mismatches.** On macOS and Windows, `source("helpers.r")` resolves to an on-disk `helpers.R` (and vice versa): Raven matches the resolved path to the real directory entry's capitalization, so the sourced file's symbols are found regardless of the case used in the `source()` string. On case-sensitive filesystems (typical Linux) `source("helpers.r")` would not find `helpers.R` at runtime — but rather than drop the file from the graph and bury the real problem under a flood of false `undefined-variable` warnings, Raven resolves the single case-insensitive match into the graph anyway and reports the case mismatch once, at the `source()` call. Either way Raven flags it as [`source-path-case-mismatch`](diagnostics.md#source-path-case-mismatch) (information on a case-insensitive filesystem, warning on a case-sensitive one). See [Path Resolution → Case-only mismatches](#case-only-mismatches) for the exact rules (exact match always wins; an ambiguous 2+-match stays unresolved).

Raven also provides **file path intellisense** inside `source()` strings and path-taking directives: completion for `.R`/`.r` files and directories, and cmd-click navigation to the target file.

For dynamic or conditional paths that Raven can't detect, use [directives](directives.md) to declare relationships explicitly.

### R Markdown / Quarto chunks

Inside `.Rmd` / `.Rmarkdown` / `.qmd` documents, only R chunk bodies feed cross-file analysis — prose and YAML front matter are masked out before detection. A `source()` or `library()` call written in a chunk participates exactly as it would in a `.R` file; the same text in prose is ignored. Within a single document, bindings from earlier chunks are visible in later chunks (ordered-concatenation semantics) — define `x` in chunk 1 and it resolves in chunk 3. A `.R` file may also declare `# raven: sourced-by report.Rmd`, in which case Raven reads the report's chunks to supply that file's inherited scope. `.Rmd` / `.Rmarkdown` / `.qmd` files are not added to the proactive workspace scan, so the editor sees these relationships when the Rmd is open or when a `.R` file points at it via a backward directive. See [R Code Chunks](./chunks.md#cross-file-resolution-from-chunks).

## Package Awareness

Raven recognizes `library()`, `require()`, and `loadNamespace()` calls and makes package exports available for completions, hover, and diagnostics.

> [!TIP]
> **Developing an R package?** When Raven detects a `DESCRIPTION` file at the workspace root, it switches to package mode — all `R/*.R` files become mutually visible without `source()` calls, and `@import`/`@importFrom` annotations suppress undefined-variable diagnostics. See [R Package Development](r-package-dev.md).

Workspace-root startup files are modeled separately from package mode. If your
project `.Rprofile` defines helpers, attaches packages, or sources setup files,
see [`.Rprofile` Startup Prelude](rprofile.md).

### How It Works

When you write `library(dplyr)`, Raven:
1. Detects the call and extracts the package name
2. Resolves the package's exported symbols — usually by reading its installed `NAMESPACE` file directly, with no R involved (see [When Raven calls R](#when-raven-calls-r) for the cases that need a subprocess)
3. Makes those symbols available with `{dplyr}` attribution in completions
4. Suppresses "undefined variable" warnings for package exports

### When Raven calls R

Raven's analysis is static: it parses your code and your installed packages' `NAMESPACE` files without a running R session. It does, however, launch a short-lived, non-interactive R subprocess — the `R` on your `PATH`, or [`raven.packages.rPath`](configuration.md#package-settings) — in two situations. These are Raven's own processes; they never touch your interactive R session, and when no R is found Raven falls back gracefully.

**1. To find where your packages are installed.** Raven runs `.libPaths()` to discover your library directories. Where packages live depends on your R installation, version, and project setup — including [`renv`](https://rstudio.github.io/renv/) project-local libraries, which Raven activates before reading the paths — so there's no reliable way to determine it statically. Without R, Raven falls back to the standard platform install locations plus any [`raven.packages.additionalLibraryPaths`](configuration.md#package-settings), which may miss user- or project-local libraries.

**2. To expand exports that can't be read from `NAMESPACE` text.** Most packages list their exports with explicit `export(name)` directives, which Raven reads straight from the installed `NAMESPACE` file — no R required. But a package can instead (or additionally) declare `exportPattern("<regex>")`: "export every object in my namespace whose name matches this regex." Raven can't expand that from the file alone — it would need to know every object the namespace actually defines once loaded — so for these packages it asks R via `getNamespaceExports()`. Several base R packages use `exportPattern`, as do a minority of installed CRAN packages. When R isn't available, Raven approximates their exports from the package's `INDEX` file plus any explicit `export()` entries; this covers documented functions but may miss pattern-only or dynamically generated symbols.

Run **Raven: Refresh package cache** after changing `.libPaths()` or running `renv::activate()` to re-run these queries.

### Resolving exports without R

When a package can't be found in any local library path — typically in CI, where `.libPaths()` is empty — Raven still resolves its **export names** through an ordered three-tier fallback, consulted per package. The trigger is a **missing package directory**, not a missing R: the fallback applies only when the package isn't found on disk at all. A package that *is* installed still resolves from Tier 1 even with no R (its `exportPattern` exports just degrade to the `INDEX` approximation, as above).

1. **Tier 1 — installed.** The authoritative path above: parse the installed `NAMESPACE`, expanding `exportPattern` via R when reachable (and approximating from `INDEX` when not). Version-exact to the install.
2. **Tier 2 — repo database.** A committed, repo-specific `.raven/packages.json` you generate with [`raven packages freeze`](cli.md#raven-packages-freeze). It is "frozen Tier 1": full structure (exports, `Depends`, datasets) captured through the authoritative path, version-exact to when it was generated.
3. **Tier 3 — `names.db` database.** Raven's `names.db` database, built **append-only** from a reference-R capture ∪ CRAN + Bioconductor (via [r-universe](https://r-universe.dev)), keeping the **highest version** of each package. It isn't bundled with the binary; install it with `raven packages update` for broad CRAN/Bioconductor coverage. Carries exports, `Depends`, and dataset names — no `:::` internals or signatures.

Tier 2 outranks Tier 3 because it is project-specific and built through the authoritative path; a repo that never generates a Tier 2 file still works in CI via Tier 3 alone when the database is present. Tiers 2 and 3 carry **export names, `Depends`, and datasets only** — no `:::` internal objects and no function signatures, which still require a local install (Tier 1). This fallback feeds **export resolution** only; it never changes a package's install status (see [Diagnostics](diagnostics.md#package-names-vs-install-status)). The full model, fidelity caveats, and how to generate the repo database are in [Package database](package-database.md).

### Base Packages

Base R packages are always available without explicit `library()` calls: **base**, **methods**, **utils**, **grDevices**, **graphics**, **stats**, **datasets**. Raven uses this fixed list directly — it does not query R to discover the base packages. The R subprocess is queried for *installed user packages* (via the library paths), not to determine which base packages exist — though base-package *exports* are still expanded via R, since they use `exportPattern` (see [When Raven calls R](#when-raven-calls-r)).

Lazy-loaded datasets are a related special case. Packages expose data objects — `mtcars` and `iris` from the base `datasets` package, `flights` from **nycflights13**, `diamonds` from **ggplot2** — that appear in neither `NAMESPACE` `export()` lines nor `getNamespaceExports()`. How Raven discovers them depends on whether the package uses R's LazyData mechanism:

- **LazyData packages** (those whose `DESCRIPTION` sets `LazyData: true`, identifiable by the presence of `data/Rdata.rdb`) build a single binary database of all data objects. Their `data/` file stems don't reliably list object names — a package like **survival** ships `lung` with no `data/lung.rda` file — so Raven queries the R subprocess via `data(package = "pkg")$results` to enumerate the authoritative set. Without R the static file-stem walk is used as a fallback (reduced fidelity).
- **Non-LazyData packages** store datasets as individual `.rda`/`.RData` files in `data/`. Raven walks those files and the `INDEX` file statically, with no subprocess needed.

Base-package datasets are always available (auto-attached at startup); a non-base package's datasets become available after its `library()` call, exactly like its function exports, and resolve transitively through `Depends` and meta-packages (`library(tidyverse); diamonds`).

`data()` calls bind a dataset's objects from the call onward, mirroring R. `data(api, package = "survey")` puts every object that the package's `api` data file binds — `apiclus1`, `apistrat`, … — in scope, even when a single data file ships several differently-named objects (the file stem and the object names differ). Because R loads those objects into the calling environment, a `data()` call overwrites earlier same-named bindings in that environment; later assignments can overwrite the data objects again. The bare form `data(api)` (no `package =`) searches the packages attached at-or-before the call and then the default-attached base packages, binding the objects from the first package that provides the dataset — mirroring R, where the first search-path hit wins and attached packages sit ahead of base packages. (Raven doesn't track attachment order, so when several attached packages provide the same dataset the alphabetically-first one is attributed.) The literal argument (`api`) is always bound too, so the behavior degrades gracefully when R is unavailable. Resolving the *object* names (beyond the file stem) requires a `data/` enumeration, captured when the package is loaded; `raven check` warms the packages named in `data(package = …)` for this. A package's namespace-internal `sysdata.rda` objects (e.g. `cli`'s internal `emojis` table) are never exposed this way — `library(cli)` followed by `emojis` is still a real R error and Raven flags it correctly.

> **Parent-file `data()` scope limit.** When a child file inherits its parent's scope via `# raven: sourced-by` (or auto-inferred backward dependency), `data()` alias expansion — the mapping from file stem to the individual object names — is not propagated through the backward parent-prefix walk. The literal stem bound by the `data()` call in the parent *does* flow to the child, so the most commonly used name resolves. Only the expanded aliases (e.g. `apiclus1` / `apistrat` from `data(api)`) may be missing in the child's scope view. Forward `source()` children receive full expansion. To work around this in a child file, repeat the `data()` call there, or use a [`# raven: var` directive](directives.md#declaration-directives) to declare the alias names explicitly.

### `devtools::load_all()` — Virtual Attached Package

`devtools::load_all()` / `pkgload::load_all()` (and a bare `load_all()`) is modeled as attaching the package under development. Raven makes the package's internal symbols — exported and non-exported `R/` definitions, `R/sysdata.rda` objects, names bound in `.onLoad`/`.onAttach`, and NAMESPACE imports — available from the call onward, exactly as `library()` makes installed-package exports available.

The propagation follows the same rules as `library()`:

- **Position-aware** — symbols surface only after the `load_all()` call.
- **Forward-propagating** — a `source()` child whose parent calls `load_all()` inherits the internals; a parent does not pick them up from a child's `load_all()`.
- **Transitive** — propagates through multi-hop `source()` chains.
- **Multi-parent union** — when a file has multiple parents and at least one calls `load_all()`, the internals are available in the child too.

A workspace-root `.Rprofile` that calls `load_all()` surfaces the package internals in ordinary scripts. In package mode, `R/` source files, package tests, and built-documentation directories do not receive the `.Rprofile`-route internals (they already get them through the package-mode dev context). See [`.Rprofile` Startup Prelude](rprofile.md).

A file **outside the package workspace root** that calls `load_all()` does NOT pick up the package internals — this avoids muting real diagnostics in unrelated scratch files.

Unlike `library(yourpkg)`, `load_all()` exposes internals even when they are not exported, matching `load_all()`'s own `export_all = TRUE` default. See [R Package Development](r-package-dev.md#scripts-that-call-devtoolsload_all) for more.

### Position-Aware Loading

Package exports are only available after the `library()` call:

```r
mutate(df, x = 1)  # Warning: undefined variable 'mutate'
library(dplyr)
mutate(df, y = 2)  # OK: dplyr is now loaded
```

### Function-Scoped Loading

When `library()` is called inside a function, exports are only available within that function's scope:

```r
my_analysis <- function(data) {
  library(dplyr)
  mutate(data, x = 1)  # OK: dplyr available inside function
}
mutate(df, y = 2)  # Warning: dplyr not available at global scope
```

### Meta-Package Support

Raven recognizes meta-packages that attach multiple packages:

- **tidyverse** attaches: dplyr, readr, forcats, stringr, ggplot2, tibble, lubridate, tidyr, purrr
- **tidymodels** attaches: broom, dials, dplyr, ggplot2, infer, modeldata, parsnip, purrr, recipes, rsample, tibble, tidyr, tune, workflows, workflowsets, yardstick

### Availability vs. ownership

A symbol made visible through a meta-package, an attached package, or a `Depends` chain has two distinct package answers that Raven keeps separate:

- **Availability** — *which loaded package made this visible?* This is what suppresses "undefined variable" warnings. `library(tidyverse)` makes `mutate` available because Raven aggregates the exports of tidyverse's attached members.
- **Ownership** — *which package actually contributes the symbol?* This is the **documentation / help owner** (used for hover help, the help panel, signature help, and completion detail) and the **NSE-policy owner** (used to classify data-masking arguments). For `mutate` under `library(tidyverse)`, the owner is `dplyr`.

This matters because `help("mutate", package = "tidyverse")` is empty — only `dplyr` owns the topic. So `library(tidyverse); mutate(...)` stays *available* through tidyverse but resolves hover, help, signatures, completion detail, and NSE policy against `dplyr`. A direct `library(dplyr)` and an explicit `dplyr::mutate(...)` resolve to `dplyr` as before, and a genuine package export is always owned by the package that exports it (the aggregate root wins for its own exports). When no contributing owner can be resolved, existing not-found behavior is unchanged.

### Cross-File Package Propagation

Packages loaded in parent files are available in sourced children:

```r
# main.R
library(dplyr)
source("analysis.R")  # dplyr available in analysis.R
library(ggplot2)      # NOT available in analysis.R (loaded after source)
```

Packages loaded in child files do NOT propagate back to parents (forward-only).

### Supported Call Patterns

| Pattern | Supported |
|---|---|
| `library(pkgname)` | Yes |
| `library("pkgname")` | Yes |
| `require(pkgname)` | Yes |
| `loadNamespace("pkgname")` | Yes |
| `library(pkg, character.only = TRUE)` | No (dynamic) |
| `sapply(c("a","b"), library, character.only = TRUE)` | Yes (apply family) |
| `sapply(libs, library, character.only = TRUE)` where `libs <- c("a","b")` | Yes (same-file variable) |
| `purrr::map(c("a","b"), library, character.only = TRUE)` | Yes (purrr family) |
| `sapply(paste0(...), library, character.only = TRUE)` | No (dynamic vector) |
| `tar_option_set(packages = c("a","b"))` | Yes, when the file also attaches targets (see below) |
| `targets::tar_option_set(packages = "a")` | Yes (qualified — no `library(targets)` needed) |
| `tar_option_set(TRUE, c("a"))` | No (positional `packages` is not matched) |

### Apply-Family Loads

Raven also recognizes package loads expressed through apply-family calls when
all the package names are statically determinable:

```r
libs <- c("dplyr", "tidyr")
sapply(libs, require, character.only = TRUE)
```

This works for `sapply`, `lapply`, `vapply`, `mapply`, and the purrr forms
(`map`, `walk`, `map_chr`, etc., bare or `purrr::`-qualified). The package
vector must be either an inline `c("a","b",...)` of string literals, or a
same-file variable assigned exactly once via `<-`, `=`, or `assign()` to such
a literal vector. `character.only = TRUE` must be present (without it, R
itself would not load the strings as packages). Dynamic constructions such as
`paste0(...)`, `tolower(x)`, `c(libs1, libs2)`, function-parameter origins,
or values defined in another file are silently ignored.

### targets::tar_option_set() Loads

Raven treats a [{targets}](https://docs.ropensci.org/targets/)
`tar_option_set(packages = ...)` call as a package attachment — targets
attaches the listed packages before running each target, so scripts sourced by
`_targets.R` use their exports as bare names:

```r
# _targets.R
library(targets)
tar_option_set(packages = c("dplyr", "tidyr"))
source("R/functions.R")  # dplyr and tidyr verbs resolve in functions.R
```

Details:

- **Bare vs. qualified callee.** The qualified spellings
  `targets::tar_option_set(...)` / `targets:::tar_option_set(...)` are
  recognized unconditionally. The bare spelling `tar_option_set(...)` is
  recognized only when the same file also attaches targets via
  `library(targets)` / `require(targets)` — anywhere in the file, before or
  after the call — so an unrelated user function named `tar_option_set`
  doesn't silently attach packages.
- **Named `packages =` only.** `tar_option_set`'s first formal is
  `tidy_eval`, not `packages`, so positional forms like
  `tar_option_set(TRUE, c("dplyr"))` are deliberately not matched.
- **Accepted value shapes.** A single string literal
  (`packages = "dplyr"`), an inline `c("a", "b", ...)` of string literals, or
  a same-file variable assigned exactly once to such a literal vector (the
  same rule as apply-family loads). Dynamic values, `character(0)`, and empty
  `c()` are ignored.
- **Per-literal anchoring.** `tar_option_set()` calls routinely span many
  lines, so each package's missing-package diagnostic is anchored at that
  package's own string literal — a `# nolint` on the literal's line suppresses
  it. The variable-resolved shape anchors at the call's end instead.
- **Union across calls.** Multiple `tar_option_set()` calls in one file union
  their `packages =` vectors. targets' real runtime semantics are
  last-call-wins, but raven deliberately favors false negatives here.
- **Limitation: no `tar_source()`.** Raven only follows `source()` calls and
  forward directives, so projects that load their function scripts via
  `tar_source()` don't inherit the `tar_option_set` packages in those scripts
  yet. A `# raven: source R/functions.R` directive (or a plain `source()`
  call) in `_targets.R` restores the link.

### Keeping Packages in Sync

Raven watches `.libPaths()` directories and invalidates caches when packages are installed, upgraded, or removed. If the watcher misses a change (e.g., after `renv::activate()`), run **Raven: Refresh package cache** from the command palette.

See [Configuration](configuration.md) for watcher settings (`packages.watchLibraryPaths`, `packages.watchDebounceMs`).

## NSE directive propagation

`# raven: nse` declarations (see [Non-Standard Evaluation](non-standard-evaluation.md) and [directives](directives.md#nse-declarations)) are cross-file facts, like defined symbols and `library()` loads: a declaration governs undefined-variable suppression for its named callee in every file connected to it through the resolved `source()` graph, in both directions and transitively. Declare a helper's NSE contract once — next to its `library()` call, its definition, or in a sourced setup file — and the corresponding false positives are suppressed at call sites in the connected files.

Propagation reuses the same dependency graph and path-resolution rules (`# raven: cd`, workspace-root fallback, `max_chain_depth`) as the scope and package facts above; backward directives participate as ordinary edges but gain no extra path fallback. The propagation set is the revalidation-consistent neighborhood — a file's ancestors plus the descendant subtrees of itself and its ancestors — so editing a `# raven: nse` (or a `# raven: func` whose formals feed cross-file positional matching) in any connected file revalidates the dependents that rely on it. Cross-file propagation is intentionally **coarse and file-level**: a propagated directive ignores its original line and governs the whole connected file, and it is consulted below the precise built-in NSE policy tables so it cannot coarsen a known verb. Two unconnected files never share NSE directives.

## Self-contained sourced files (`# raven: self-contained`)

By default a file's cross-file scope includes a **backward** contribution: the
bindings visible at every `source()` call that pulls the file in. For ordinary
script fragments, that matches R's dynamic behavior: the file may intentionally
use variables created by its caller.

Some sourced files are different. A shared helper or setup file may be
self-contained: it defines its own inputs, loads its own packages, and sources
its own helper chain. The header directive `# raven: self-contained`
(alias: `# raven: standalone`) opts that file out of the backward caller
contribution. Raven resolves it **in isolation** from callers, while still
letting its own definitions and `library()` loads flow forward to every caller.

This is both more precise and faster for true source hubs. In deeply nested,
high-fan-out graphs - for example, a setup file sourced by dozens of scripts
that itself sources dozens of files through a nested chain - the directive can
let Raven reuse one caller-independent scope instead of re-resolving the same
closure in many caller contexts.

See [Speeding Up Cross-File Analysis](cross-file-analysis-performance.md) for when to use
`# raven: self-contained`, and [Directives](directives.md#self-contained-sourced-files)
for the exact syntax. `# raven: nse` / `# raven: func` propagation over
`source()` edges is unaffected (it is graph-level, not scope-level).

## Position-Aware Scope

Symbols from sourced files are only available **after** the `source()` call:

```r
x <- 1
source("a.R")  # Symbols from a.R available after this line
y <- foo()     # foo() from a.R is now in scope
```

This applies to both `source()` calls and forward directives. The scope model aims to reflect runtime availability for the statically determinable cases — see [Symbol Recognition](#symbol-recognition) below for what's covered.

## Symbol Recognition

Raven recognizes these R constructs as definitions:

- `name <- expr` / `name <<- expr` / `expr -> name` / `expr ->> name`
- `name = expr` (in assignment context)
- `assign("name", expr)` (string-literal only)

For dynamically-created symbols (`eval()`, `load()`, dynamic `assign()`), use [declaration directives](directives.md#declaration-directives).

### Symbol Removal (rm/remove)

Raven tracks `rm()` and `remove()` calls to maintain accurate scope:

```r
x <- 1
rm(x)
x  # Warning: undefined variable
```

Supported: `rm(x)`, `rm(x, y)`, `rm(list = c("x", "y"))`. Dynamic patterns like `rm(list = ls())` are skipped.

## How This Feeds Into Features

The dependency graph and scope model power several features:

- **[Diagnostics](diagnostics.md)** — undefined variable warnings respect cross-file scope and loaded packages
- **[Completions](completion.md)** — symbols from sourced files and packages appear with source attribution
- **[Find References](find-references.md)** — locates occurrences by name across all open and indexed files (a flat name match, *not* dependency-graph-scoped)
- **Go-to-definition** — navigates to definitions in other files
- **Hover** — shows where a symbol is defined and which package it comes from

## Advanced

### Backward Dependency Modes

The `raven.crossFile.backwardDependencies` setting controls how Raven discovers which files source the current file.

**`"auto"` (default):** Raven scans the workspace for `source()` calls and infers backward relationships automatically. No `# raven: sourced-by` directives needed. Diagnostics are deferred until the workspace scan completes to avoid false positives.

**`"explicit"`:** Only relationships declared via `# raven: sourced-by` directives are used. Use this if auto-inference produces unwanted results (e.g., a file sourced by multiple parents with conflicting scopes).

**Per-file opt-out:** Adding an explicit `# raven: sourced-by` directive to a file disables auto-inference for that file.

See [Configuration](configuration.md) for the setting.

### Open buffers, disk state, and closing files

While a file is open, its buffer is authoritative: unsaved edits that add or remove `source()` calls or directives update the dependency graph live, and disk changes to that file are ignored until it closes.

Open-buffer authority is alias-aware for file URIs. If a buffer is opened through a case-only spelling difference (for example `child.R` for an on-disk `child.r`) or through a symlink path that resolves to a workspace file, Raven treats the corresponding graph URI as open: dependents revalidate as you type, cross-file scope reads the live buffer, package-internal symbols and package-sibling revalidation use the canonical package file, self-package NSE policy and parameter/signature package scope use the canonical workspace identity, and watched-file resyncs for the canonical spelling are skipped until close. The same authority applies to a workspace-root `.Rprofile`: an alias-opened live buffer owns the startup prelude while it is open, and alias-opened workspace scripts receive the prelude through the canonical workspace path. Diagnostics still publish to the URI spelling the editor opened. Closed-file graph/index keys remain raw path spellings rather than symlink-canonicalized paths: Raven avoids full filesystem canonicalization because it follows symlinks and can diverge from the uncanonicalized workspace-index keys. The source-path resolver may still correct case-only mismatches to the on-disk spelling so graph edges line up with the workspace index.

When you close a file without saving, Raven re-reads just that file from disk and converges the graph back to disk truth — a `source()` edge added only in the discarded buffer disappears, and an edge the buffer had removed comes back. If the file no longer exists on disk, its graph entry is removed entirely. Open files that depend on it have their diagnostics refreshed automatically. This never triggers a workspace rescan, and reopening the file immediately always wins over the disk re-read.

For file-backed R Markdown / Quarto documents whose extension does not identify them as chunk documents, Raven remembers the editor's last Rmd/Quarto language classification after close. Watched disk changes and closed-file cache fallbacks keep using chunk masking until that file's cross-file state is removed, so prose `source()` calls and directives do not start contributing graph edges just because the document is closed.

### Traversal budgets in large workspaces

Cross-file resolution walks the `source()` dependency graph under two safety budgets that bound analysis cost on pathologically dense graphs:

- **`raven.crossFile.maxTransitiveDependentsVisited`** (default `50000`) — the maximum number of files visited while building a file's dependency neighborhood.
- **`raven.crossFile.maxChainDepth`** (default `64`) — the maximum traversal depth.

The defaults are sized so realistic workspaces never reach them (the neighborhood is naturally bounded by the workspace's file count). If a workspace is large and dense enough to exhaust a budget, Raven stops following some `source()` edges, and the symbols those files define can surface as **false-positive `undefined-variable` warnings**. When that happens:

- In the editor, Raven shows a throttled warning naming the setting to raise.
- `raven check` prints a one-line note (grouped with the diagnostics on stdout for the default `text` output, or on stderr for `json`/`sarif` — see [Output streams](cli.md#output-streams)), so budget-induced drops are distinguishable from genuine undefined variables in CI.

Raise the relevant setting in `raven.toml` to analyze more of the graph. See [Configuration](configuration.md).

### Path Resolution

When Raven resolves a relative path to another file, the base directory depends on where the path came from:

- **Forward directives** (`# raven: source`, `# raven: run`, `# raven: include`) and **AST-detected `source()` calls** resolve relative to the directory of the file they appear in, and honor an in-effect [`# raven: cd`](directives.md) working directory.
- **Backward directives** (`# raven: sourced-by`, `# raven: run-by`, `# raven: included-by`) resolve relative to the file's own directory and **ignore `# raven: cd`**.
- **Workspace-root fallback** applies to AST-detected `source()` calls and forward directives (`# raven: source`, `# raven: run`, `# raven: include`), and only when no working directory (an explicit `# raven: cd` or one inherited from a parent file) is in effect: a path that doesn't resolve relative to the file's directory is then also tried relative to the workspace root. Forward directives are semantically equivalent to `source()` calls, so they resolve identically across dependency edges, scope, missing-file diagnostics, cmd-click, and path completion. The fallback never applies to backward directives.

#### Implicit testthat/testit working directory

testthat's `test_dir()`/`test_local()` (and testit's `test_dir()`) evaluate helper and test files with `tests/testthat/` (resp. `tests/testit/`) as the working directory. Raven mirrors this automatically: for a file under `<workspace root>/tests/testthat/` or `tests/testit/` (at any nesting depth), relative forward `source()` paths and forward directives anchor at that directory. This is layout-only — no `DESCRIPTION` or package mode required — and it is a **soft default**, deliberately weaker than a `# raven: cd`:

- An explicit `# raven: cd` (or one inherited from a parent file) overrides it.
- It does **not** suppress the workspace-root fallback, and it never propagates into files the test file sources (a sourced `scripts/helpers.R` still resolves its own `source()` calls from its own directory, with the fallback).
- For a file **nested below** the anchor (e.g. `tests/testthat/fixtures/helper.R`), the file's own directory is tried as a compatibility fallback when the anchor-relative resolution misses, so file-relative paths that worked before keep working.
- Backward directives are unaffected: they always resolve relative to the file's own directory.

Path completion inside a `source()` string in a testthat file offers entries from the anchor directory, matching where the path will actually resolve.

#### Case-only mismatches

For a `source()`/directive path, an **exact-case match always wins**. If the path doesn't match exactly but differs from the real on-disk filename **only by case** (`source("scripts/templates.r")` for an on-disk `templates.R`), Raven resolves it to the real file anyway — provided there is exactly one case-insensitive match — so the file enters the source graph and its symbols stay visible. This holds uniformly across dependency edges, scope, diagnostics, cmd-click, and path completion. Raven also reports it once, at the path's line, as [`source-path-case-mismatch`](diagnostics.md#source-path-case-mismatch) (information on a case-insensitive filesystem, warning on a case-sensitive one). If two on-disk files match the path case-insensitively — only possible on a case-sensitive filesystem — the path is ambiguous and stays unresolved (`unresolved-source-path`). Matching is ASCII-only.

This single-case-insensitive-match leniency applies to **both directions** — forward `source()` calls and forward directives (`# raven: source` / `run` / `include`), **and** backward directives (`# raven: sourced-by` / `run-by` / `included-by`). A wrong-cased `# raven: sourced-by parent.r` still resolves to an on-disk `Parent.R` on a case-sensitive filesystem (e.g. Linux CI), so the child keeps the parent's scope instead of producing a cascade of false `undefined-variable` warnings. The backward diagnostic carries a different message: because R never *executes* a backward directive (it is a Raven-only annotation), it does not claim R would error — it asks you to fix the directive's path casing to match the file on disk. The **workspace-root fallback** (above) remains forward-only and is independent of this case-leniency; backward directives gain the leniency but never the fallback.

If the buffer itself is opened under a different case spelling than the resolved graph URI, Raven bridges that open buffer back to the graph URI for live revalidation and content. This does not change graph keys: the dependency edge still targets the real on-disk spelling, while diagnostics for the open buffer still publish to the client-opened spelling.

### Global Symbol Hoisting

R has late-binding semantics — a function can reference another function that hasn't been defined yet at the time of the function's *definition*, as long as it exists by the time the function is *called*:

```r
main <- function() {
  helper()  # helper doesn't exist yet, but will when main() is called
}
helper <- function() { 42 }
main()  # works fine
```

Raven supports this by hoisting global definitions inside function bodies. When the cursor is inside a function body, all global definitions are visible regardless of position. Function-local variables remain strictly positional.

This is enabled by default. Disable with the LSP init option `crossFile.hoistGlobalsInFunctions: false` — this one is init-only and is not exposed in the VS Code Settings UI (see [Configuration](configuration.md)).

### $ and @ Member Resolution

When you cmd-click on `foo$bar` (or `foo@slot` for S4 objects), Raven resolves the member against `foo` — not as a free variable. It looks for:
- Member assignments: `foo$bar <- …`, `foo["bar"] <- …`, or `foo[["bar"]] <- …` (the string-subscript forms apply to `$` only); `foo@slot <- …` for S4 slots.
- Constructor-literal members: named arguments in constructors such as `list()`, `c()`, `data.frame()`, `tibble()`, `data.table()`, `environment()`, `list2env()`, and `new()`.

Scope-aware completions after `$` use the same rules: typing `foo$` offers known members of `foo`.
