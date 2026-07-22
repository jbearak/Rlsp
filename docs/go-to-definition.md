# Go-to-Definition

Go-to-definition (Cmd-click, Ctrl-click on Windows/Linux, or F12) navigates to where a symbol is defined. Raven follows `source()` chains across files and respects the cross-file dependency graph, so it works whether the definition is in the current file, a sourced sibling, or a parent script.

## What You Can Jump To

| At the cursor | Jumps to |
|---|---|
| Variable or function identifier | The most recent in-scope assignment (same file or sourced file) |
| RHS of `$` or `@` (`foo$bar`) | The member assignment or constructor-literal field — see [$ and @ Member Resolution](#-and--member-resolution) |
| Literal-string `[[` subscript (`foo[["bar"]]`) | Same as `` foo$`bar` `` — `[[` is the `$`-equivalent extractor; see [$ and @ Member Resolution](#-and--member-resolution) |
| Symbol declared via `# raven: var` / `# raven: func` | The directive line itself — see [Declared Symbols](#declared-symbols) |
| File path inside `source()` or a path directive | The referenced file, opened at line 0 |
| Static relative module spec inside `box::use(./module)` | The resolved file module or package-style `__init__.r` / `__init__.R` |
| Exported member of a box namespace alias, or an attached/renamed box binding | The original local-module definition, including through named, renamed, and wildcard re-exports; installed-package members remain non-navigable |
| Literal `.R`/`.r` source or selected/renamed local member in a static `import::` call | The exact script file or original top-level definition |
| Identifier in `.stan`, `.jags`, or `.bugs` files | The most recent definition at or before the cursor (or the first definition if the cursor precedes all of them) — see [JAGS and Stan](#jags-and-stan) for the per-language details |

## Position-Aware Resolution

Raven only navigates to definitions that are visible at the cursor's position according to R's execution order. If you cmd-click a variable before its assignment in the same file, Raven won't jump to the later assignment:

```r
x  # no jump — x isn't defined yet
x <- 1
x  # jumps to the line above
```

Inside function bodies, global definitions are hoisted (R's late-binding semantics), so a function can cmd-click a helper defined below it at file scope. Function-local variables stay strictly positional. See [Global Symbol Hoisting](cross-file.md#global-symbol-hoisting).

## Cross-File Navigation

When a symbol is defined in a sourced file, cmd-click opens that file and places the cursor on the definition:

```r
# main.R
source("utils.R")
result <- helper_function(42)  # cmd-click jumps into utils.R
```

```r
# utils.R
helper_function <- function(x) { x * 2 }  # ← jump target
```

Raven follows both `source()` calls and [forward directives](directives.md#forward-directives) (`# raven: source`, `# raven: run`, `# raven: include`). Backward directives (`# raven: sourced-by`, `# raven: run-by`, `# raven: included-by`) bring the parent file's symbols into scope, so cmd-clicking a parent-defined symbol in a child file works too.

If no definition is found in the dependency graph, Raven falls back to other open documents and the workspace index so you can still navigate to reasonable candidates during project-wide exploration.

## `$` and `@` Member Resolution

When you cmd-click the RHS of `$` or `@` (e.g. `config$host`), Raven resolves it as a member of the LHS — not as a free variable. It searches for:

- Member assignments: `config$host <- …`, `config[["host"]] <- …` (both apply to `$`); `object@slot <- …` (applies to `@`)
- Constructor literals: named arguments in `list()`, `data.frame()`, `tibble()`, `c()`, S4 `new()`, etc.

Nested members resolve against the full container path at any depth — cmd-clicking `gamma` in `alpha$beta$gamma` resolves it as a member of `alpha$beta` (descending nested constructor literals or matching `alpha$beta$gamma <- …` assignments), never as the free variable `gamma`. `$`, `@`, and `[["lit"]]` segments may be mixed in the chain.

The **string-subscript form `foo[["bar"]]`** is the `$`-equivalent extractor, so cmd-clicking the literal-string subscript navigates exactly as `` foo$`bar` `` does — handy for non-syntactic names (`df[["weird col"]]` ↔ `` df$`weird col` ``). This applies only to a **single, positional, literal** string subscript. It does **not** apply to computed or dynamic subscripts (`x[[i]]`, `x[[paste0("a", "b")]]`), numeric indices (`x[[1]]`), named or multi-argument subscripts (`x[[name = "a"]]`, `x[["a", exact = FALSE]]`), or the single-bracket form `x["bar"]` (which returns a sub-container, not the element). A cursor on any of those does not navigate as a member. (This is distinct from `foo["bar"] <- …` *assignments*, which remain resolution candidates — see above.)

If multiple candidates exist across the dependency graph, Raven tie-breaks by graph distance (closer wins). Cmd-clicking on `$` or `@` itself (the punctuation) does nothing — only identifiers and literal-string `[[` subscripts are navigable.

For the full scope rules, see [$ and @ Member Resolution](cross-file.md#-and--member-resolution) in the cross-file doc.

## Declared Symbols

Symbols declared via [`# raven: var` or `# raven: func`](directives.md#declaration-directives) are navigable. Cmd-click jumps to the directive line at column 0. If the same name is declared more than once in the providing file, Raven jumps to the **first declaration by line number**:

```r
# raven: var config   ← jump target (first declaration)
load("config.RData")
# raven: var config   ← later duplicate, not used
use(config)
```

This is the same rule the [diagnostics](diagnostics.md) and [completion](completion.md) subsystems use to identify the canonical declaration site.

## File Path Navigation

Inside `source()` strings and path-taking directives, cmd-click navigates to the referenced file. Path resolution follows the standard Raven rules:

- `source()` calls and forward directives respect `# raven: cd`.
- Backward directives (`# raven: sourced-by`, `# raven: run-by`, `# raven: included-by`) resolve relative to the file's own directory and ignore `# raven: cd`.
- Workspace-root fallback applies to AST-detected `source()` calls and forward directives (`# raven: source`, `# raven: run`, `# raven: include`), and only when no working directory is in effect.

See [Cross-File Awareness](cross-file.md#automatic-source-detection) and [Directives](directives.md#working-directory-directives) for details.

### box module paths and exports

Cmd-click on a static relative [`box::use()` module spec](modules.md#box-modules-boxuse) opens the exact resolved module. box path resolution is file-relative and case-sensitive, ignores Raven's `source()` working-directory/fallback rules, and checks `path.r`, `path.R`, `path/__init__.r`, then `path/__init__.R`.

For namespace aliases and attached/renamed bindings, navigation crosses only the exported interface. `$`, `@`, and a single positional literal-string `[[...]]` access resolve to the original local-module definition, following named, renamed, or wildcard re-export chains with cycle guards. Private names and merely transitive imports do not navigate. Installed-package box imports reuse package export metadata but remain non-navigable for the same reason as other installed package exports.

## load_all() Internals

When a file calls `devtools::load_all()` / `pkgload::load_all()`, Raven models this as attaching the package under development. Cmd-click on any symbol that `load_all()` exposes — including non-exported helpers defined in `R/` — navigates to its real definition in the package source, whether that file is currently open or not.

```r
# internal/scratch.R
devtools::load_all()
my_helper(data)   # Cmd-click → jumps to R/helpers.R where my_helper is defined
```

This also fixes `R/`→`R/` navigation in package mode: a function defined in `R/utils.R` and called from `R/main.R` is directly navigable via cmd-click without any `source()` call.

Goto into **external/installed** packages (e.g. a dependency's source after a `library()` or `:::` call) is not yet supported — installed packages ship compiled lazy-load databases rather than readable source. Use hover to see the `{package}` attribution and look up the function in its own documentation or source repository.

## Package Exports

Cmd-click on a symbol that comes from an installed package (e.g. `dplyr::mutate` after `library(dplyr)`) does **not** navigate. Package exports are tracked for completions, diagnostics, and hover, but Raven doesn't currently open installed package sources. Use hover to see the `{package}` attribution instead.

## R Markdown and Quarto

Go-to-definition works inside R code chunks of `.Rmd` and `.qmd` documents. Raven treats all R chunk bodies as one R program, so you can jump from a use in one chunk to a definition in an earlier chunk; the definition lands on the correct document line. Invoking it on prose, YAML front matter, or a non-R chunk returns no result. (Hover and find-references behave the same way.)

## JAGS and Stan

For `.stan`, `.jags`, and `.bugs` files, Raven provides best-effort go-to-definition within the current file:

- **Stan** — jumps to the most recent declaration of a variable or function at or before the cursor (or the first declaration if none precede it).
- **JAGS** — jumps to the most recent assignment at or before the cursor (or the first assignment if none precede it), or falls back to the first occurrence when the symbol is a data input or constant with no assignment in the file. Built-in keywords, distributions, and functions are excluded from the fallback.

Identifier resolution is file-local — Raven doesn't build a cross-file scope graph for these languages. File-path navigation (e.g. cmd-click on a path in `# raven: sourced-by`) still works normally. See also [Find References — JAGS and Stan](find-references.md#jags-and-stan).

## Related

- [Find References](find-references.md) — the reverse operation: list every usage of a symbol
- [Cross-File & Package Awareness](cross-file.md) — how the dependency graph and scope model work
- [Directives](directives.md) — `# raven: source`, `# raven: sourced-by`, `# raven: var`, `# raven: func`, and friends
- [Completions](completion.md) — position-aware scope used for `$` member and symbol completions
