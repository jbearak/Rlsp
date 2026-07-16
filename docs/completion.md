# Completion

Raven offers context-aware completions for R symbols, package exports, function parameters, and file paths. Completions respect the cross-file dependency graph and position-aware scope.

## What's Offered

| Symbol source | Behavior |
|---|---|
| **Local definitions** | Symbols defined in the current file, above the cursor |
| **Cross-file symbols** | Symbols from sourced files, available after the `source()` call / `# raven: source` directive |
| **Package exports** | Functions and variables from loaded packages, with `{pkg}` attribution |
| **Namespace-qualified exports** | A package's exported symbols after `pkg::` (e.g. `dplyr::`) |
| **Function parameters** | Parameter names when inside a function call |
| **File paths** | `.R`/`.r` files and directories inside `source()` strings and path directives |
| **$ members** | Known members after `$` (from assignments and constructors) |

## Position-Aware Filtering

Completions respect execution order. A symbol is only offered if it would be defined at the cursor's position at runtime:

```r
# Before this line: only base symbols + earlier local definitions
library(dplyr)
# After the library() call: dplyr exports now available
source("utils.R")
# After the source() call: symbols from utils.R now available
```

Symbols defined below the cursor in the same file are not offered at the global level. Inside function bodies, global definitions are hoisted (see [Global Symbol Hoisting](cross-file.md#global-symbol-hoisting)).

## Package Completions

When a package is loaded via `library()`, its exports appear in completions with `{package}` detail:

```r
library(dplyr)
mut  # Offers: mutate {dplyr}, mutate_all {dplyr}, ...
```

Package completions are position-aware — they only appear after the `library()` call. Packages loaded in parent files (before the `source()` call to the current file) are also available. They require package awareness (`raven.packages.enabled`, on by default); base-package symbols additionally wait until the package library has finished loading.

### Namespace-Qualified Completions (`pkg::`)

After a `::` namespace qualifier, Raven completes the package's **exported** symbols:

```r
dplyr::  # Offers: mutate {dplyr}, filter {dplyr}, select {dplyr}, ...
```

Each item is attributed `{package}` and resolves to that topic's help, exactly like ordinary package completions, and covers the package's `NAMESPACE` exports. Non-syntactic exports — operators such as `%>%`, or exported names that are not valid bare identifiers — are inserted backtick-quoted (so accepting `magrittr::%>%` produces `` magrittr::`%>%` ``) and the accepted completion is valid R.

Unlike the position-aware completions above, `pkg::` does **not** require a prior `library(pkg)` call: any installed package resolves on demand by reading its `NAMESPACE` (the same way `pkg::name` works in R without attaching the package). Writing `pkg::` (or `pkg:::`) also **warms `pkg`'s metadata** into the package cache in the background — exactly as a `library(pkg)` call would — so the two refinements below become available without an explicit attach. Crucially, this warming never attaches `pkg` to bare-name scope: it only populates metadata. Two refinements arrive once a package has been loaded (in the background, or by an earlier `library()`/`pkg::` use in the session): exported **datasets** (which live in `data/`, not the `NAMESPACE`), and the complete export set for the ~6% of packages that export via `exportPattern()` — those offer their explicitly-exported names immediately.

Inside a `pkg::` expression no other completions are offered (keywords, local symbols, and other packages are irrelevant there). Internal access via `pkg:::` (non-exported symbols) warms the package's metadata but is **not** completed and is never member-validated. Like the other package completions, this requires `raven.packages.enabled`. A `pkg::member` that a *complete* export set does not contain is flagged by the [`namespace-member-not-found`](diagnostics.md#namespace-member-references-pkgmember) diagnostic.

## Function Parameter Completions

When the cursor is inside a function call, Raven offers parameter names:

```r
read.csv(  # Offers: file, header, sep, quote, ...
```

Trigger character `(` activates parameter completions automatically (configurable via `raven.completion.triggerOnOpenParen`).

## File Path Completions

Inside `source()` strings and path-taking directives, Raven completes file paths:

```r
source("utils/  # Offers: utils/helpers.R, utils/config.R, ...
```

Path completion respects `# raven: cd` and workspace-root fallback rules for forward directives (`# raven: source`, `# raven: run`, `# raven: include`) and `source()` calls. Backward directives (`# raven: sourced-by`, `# raven: run-by`, `# raven: included-by`) still resolve relative to the file's directory.

In files under `tests/testthat/` or `tests/testit/`, forward-path completion uses that test directory as its implicit working-directory anchor. For files nested below the anchor, Raven merges in entries from the file's own directory as a compatibility fallback; duplicate names appear once, with the anchor taking precedence. An explicit or inherited `# raven: cd` overrides these implicit completion bases.

## $ Member Completions

After typing `foo$`, Raven offers known members of `foo`:

```r
config <- list(host = "localhost", port = 8080)
config$  # Offers: host, port
```

Members are discovered from:
- Assignments: `foo$bar <- …`, `foo[["bar"]] <- …`
- Constructor literals: named arguments in `list()`, `data.frame()`, `tibble()`, etc.

Non-syntactic member names are shown without quoting in the completion popup,
but inserted with backticks so the resulting R code parses correctly:

```r
df <- list(`alpha beta` = 1)
df$  # Offers: alpha beta; accepting inserts df$`alpha beta`
```

Member completion follows nested access at any depth, and `$`, `@`, and
`[["lit"]]` segments are interchangeable in the chain:

```r
config <- list(db = list(host = "localhost", port = 8080))
config$db$              # Offers: host, port
config[["db"]]$         # Same — `[["db"]]` is equivalent to `$db`
```

Members reflect the value that is live at the cursor, so a whole reassignment of
an intermediate value replaces its earlier members:

```r
config$db <- list(url = "…")   # whole-value rewrite
config$db$                     # Offers: url (earlier host/port are replaced)
```

Because the container is resolved as a path, an unrelated top-level variable
that happens to share an intermediate name (a `db` elsewhere) never leaks its
members into `config$db$`.

## Cross-File Completions

Symbols from sourced files appear with their source file indicated:

```r
# main.R
source("utils.R")
help  # Offers: helper_function — "from utils.R" as the item detail
```

## R Markdown / Quarto

Inside `.Rmd` / `.qmd` documents, completions work inside R chunk bodies: local definitions, cross-file symbols, package exports, function parameters, and `$` members all resolve across chunks as if the document were a single R program. On prose, YAML front matter, or non-R chunks, completion returns nothing rather than treating the line as R.

## JAGS and Stan

For `.stan`, `.jags`, and `.bugs` files, Raven offers completions tailored to each language:

| Language | What's offered |
|---|---|
| **JAGS** | Keywords (`model`, `data`, `for`, `in`, `if`, `else`), distributions (`dnorm`, `dgamma`, …), built-in functions (`abs`, `log`, …), and file-local symbols |
| **Stan** | Types (`int`, `real`, `vector`, …), block keywords (`data`, `parameters`, `model`, …), control flow (`if`, `for`, `while`, …), built-in functions (`normal_lpdf`, `bernoulli_lpmf`, …), and file-local symbols |

File-local symbols are discovered by scanning the current file for variable declarations and assignments. R-specific reserved words are excluded from JAGS completions to avoid noise.

## Scope Rules Summary

1. Local definitions: available after their definition line
2. Cross-file symbols: available after the `source()` call / `# raven: source` directive
3. Package exports: available after the `library()` call
4. Inside function bodies: global definitions are hoisted (all visible regardless of position)
5. `rm()` / `remove()` calls: removed symbols are excluded from completions
