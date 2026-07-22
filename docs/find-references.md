# Find References

Raven locates all occurrences of a symbol — definitions and usages alike — across your project, not just the current file.

## How It Works

When you invoke Find References (Shift+F12 / right-click → Find All References), Raven:
1. Identifies the name of the identifier at the cursor.
2. Searches the current file, all other open documents, and every workspace-indexed file for identifier nodes with that same name.
3. Returns every match — both definition sites (assignments, function parameters) and usage sites.

Find References is ordinarily a **name-based** search: it matches on the identifier text, with no dependency-graph or scope filtering. Static members imported through `box::use()` or supported `import::` calls are the exception: Raven follows their local definition or installed-package export identity so unrelated same-named members are excluded. See [Scope and pooling](#scope-and-pooling) and [selective-import member identity](#selective-import-member-identity).

## Cross-File Scoping

Because the search spans all open and indexed files, a definition in one file and its usages in another are returned together:

```r
# main.R
source("utils.R")
result <- helper_function(42)  # ← reference
```

```r
# utils.R
helper_function <- function(x) { x * 2 }  # ← definition
```

Invoking Find References on `helper_function` in either file returns both locations — whether or not a `source()` path connects them.

## Scope and pooling

Unlike completions and diagnostics, Find References does **not** consult the `source()` dependency graph or position-aware scope. Every identifier in the workspace whose name matches is returned, which means:

- Definitions (left-hand sides of assignments, function parameters) are listed alongside usages.
- Same-named symbols in files that are *not* connected by any `source()` path are pooled together rather than treated as distinct symbols.
- Because the search keys on the member *name* and not the accessor operator, the `` x$`name` `` and `x@name` forms, the `x[["name"]]` literal-string subscript form, and a `name =` constructor argument all pool together — cmd-clicking any one returns the others. The `[[` form participates only for a single, positional, literal string subscript (the same rule [Go-to-Definition](go-to-definition.md) uses); `x[[i]]`, `x[[1]]`, `x["name"]`, and computed/named/multi-argument subscripts are not matched, and a plain string literal that is not a `[[` subscript is never treated as a reference.

If you need a result scoped to one ordinary symbol's definition, use [Go-to-Definition](go-to-definition.md), which *is* scope- and dependency-aware.

### Selective-import member identity

For a resolved local-module member imported through static [`box::use()`](cross-file.md#box-module-imports-boxuse) or a supported [`import::` call](cross-file.md#import-package-selective-imports), Raven starts from the original definition and keeps only occurrences that resolve to that exact identity. The result can include namespace access through `$`, `@`, or literal `[["name"]]` where the syntax supports it, named/wildcard attachments, renamed local bindings, re-export chains, and the underlying definition in open or workspace-indexed files.

Selected installed-package members have no navigable source definition, so Raven instead preserves the exact `(package, exported name)` identity. Renamed declaration tokens and uses remain linked to the original export, while unrelated local bindings or imports from another package with the same spelling are excluded. Ordinary non-selective structural references retain the broad pooling described above.

> Find References works inside R code chunks of R Markdown / Quarto (`.Rmd` / `.Rmarkdown` / `.qmd`) documents — all R chunk bodies are pooled as one R program, so references span chunks. Invoking it on prose, YAML, or a non-R chunk returns no results.

## Workspace Symbols (Cmd/Ctrl+T)

For project-wide symbol search by name, use **Cmd/Ctrl+T**. This searches all indexed symbols across the workspace by name, regardless of dependency relationships.

The maximum number of results is configurable via `raven.symbols.workspaceMaxResults` (default: 1000).

## JAGS and Stan

For `.stan`, `.jags`, and `.bugs` files, Find References returns all occurrences of the identifier across every open or indexed file of the same language. As with R, this is a flat name match — there is no dependency graph; results are collected by name across all Stan (or JAGS) files in the workspace.

## Go-to-Definition

Go-to-definition is the reverse of find references — it navigates to a symbol's definition rather than listing its usages. See [Go-to-Definition](go-to-definition.md).
