# Non-Standard Evaluation

R uses non-standard evaluation (NSE) when a function treats an argument as
unevaluated code, evaluates it in a data mask, or captures it for later use.
That means a bare name inside a call is not always an ordinary variable
reference.

Raven models many common NSE patterns so idiomatic R does not produce false
`undefined-variable` diagnostics.

```r
library(dplyr)

filter(mtcars, cyl > 4)
# `cyl` is a column in the data mask, so Raven does not flag it.
# `mtcars` is still an ordinary argument and is still checked.
```

```r
with(mtcars, mpg / cyl)
# The expression is evaluated in `mtcars`, so `mpg` and `cyl` are not flagged.
# The data argument `mtcars` is still checked.
```

```r
library(ggplot2)

ggplot(mtcars, aes(cyl, mpg)) +
  geom_point()
# `aes()` captures mappings, so `cyl` and `mpg` are not treated as missing
# variables in the surrounding script.
```

```r
select(mtcars, starts_with("c"), mpg)
# Tidyselect arguments name columns, not variables in the current R scope.
```

## What Raven Checks

Raven does not silence an entire call just because the callee uses NSE. It
resolves the callee and applies an argument policy:

- Standard-evaluation arguments are checked normally.
- Captured, data-masked, and tidy-selected arguments are suppressed.
- Local bindings can shadow package functions, matching R's call-position
  lookup (when a local function has the same name as a package function, R calls
  the local one, and Raven follows that rule).

For example, `dplyr::filter(missing_df, cyl > 4)` suppresses `cyl` as a
data-column reference but still checks the ordinary data argument
`missing_df`. A standard-evaluation call such as `paste(missing_value)` is
checked as usual.

For Shiny and `foreach()` constructs, Raven models the names those constructs
make available instead of suppressing every diagnostic inside them. Shiny
deferred expressions keep outer bindings visible while checking real references
inside the deferred body. `foreach()` iterators are available inside the loop
body, while iterator value expressions and control arguments are still checked.

## How Raven Recognizes NSE

Raven combines several sources of information:

- Built-in policies for common R functions and package families such as
  tidyverse, ggplot2, Shiny, foreach, targets, recipes, and table-building
  packages.
- Package ownership from `library()`, `pkg::fn`, imports, and meta-packages, so
  `library(tidyverse); mutate(...)` can use dplyr's NSE policy.
- Visible local function definitions. If Raven sees a helper captures a formal
  with `substitute()`, `enquo()`, `enexpr()`, `ensym()`, or a recognized tidy-eval
  forwarding pattern, it can infer the helper's argument policy.
- Cross-file scope. The same source graph that powers diagnostics, completions,
  and navigation also carries NSE declarations where needed.

## When Raven Needs a Hint

Raven cannot infer every package-specific or external NSE helper. If a function
captures arguments but Raven does not know that policy, use `# raven: nse` to
declare it.

```r
# raven: func summarize_by(data, group, value)
# raven: nse summarize_by(group, value)

summarize_by(mtcars, cyl, mpg)
# `cyl` and `mpg` are captured; `mtcars` is still checked.
```

The `# raven: func` line records the formal argument order for positional calls.
When arguments are named, Raven can match them by name:

```r
# raven: nse summarize_by(group, value)

summarize_by(data = mtcars, group = cyl, value = mpg)
```

Use `...` when a helper captures arguments passed through dots:

```r
# raven: nse select_like(...)

select_like(mtcars, cyl, mpg)
```

`# raven: nse` is position-aware in the file where it appears: it applies to
calls after the directive line, and the most recent declaration wins. Across a
connected `source()` graph, it propagates as a coarse file-level fact so you can
declare a helper's policy once in a setup file and use it in connected files. See
[NSE declarations](directives.md#nse-declarations) for the full syntax and
[NSE directive propagation](cross-file.md#nse-directive-propagation) for the
cross-file rules.

## Declaring NSE From a Package

`# raven: nse` describes a helper from the *calling* side, which suits a helper
defined in your own project. A package author can instead ship the policy with
the package, so every user of the package gets it without writing directives.

Put the declarations in `inst/raven/nse.toml`. `R CMD INSTALL` copies `inst/` to
the package root, so Raven reads the installed file at
`<libpath>/<pkg>/raven/nse.toml` alongside `NAMESPACE`.

```toml
schema = 1

[[function]]
name = "gen"
formals = ["data", "variable", "values", "where"]
captured = ["variable", "values", "where"]
```

With that installed, `gen(survey, adjusted, income + 5)` suppresses `adjusted`
and `income` while still checking `survey`.

Each `[[function]]` entry takes:

| Key | Meaning |
| --- | --- |
| `name` | The exported function. A name the package does not export is ignored. |
| `policy` | `"per-formal"` (the default), `"whole-call"`, or `"named-arguments"` — the three policy kinds Raven's built-in table uses. |
| `formals` | The function's formal arguments **in declaration order**. Raven matches positional arguments against this list, so a stale order shifts which argument is suppressed. |
| `captured` | The formals whose argument is captured, data-masked, or tidy-selected. Every name must appear in `formals`. |
| `captured_dots` | `true` when arguments absorbed by `...` are captured too. |

`formals` and `captured` apply to `policy = "per-formal"` only; the other two
kinds need just a `name`.

Three rules keep a declaration file from making diagnostics worse:

- **Built-ins win.** A declaration is consulted only after Raven's own table
  misses, so an installed package cannot silence a diagnostic Raven models
  itself.
- **Exports only.** Declarations are intersected with the package's export set,
  so an entry for an internal helper is dropped rather than applied to an
  unrelated call.
- **Failure is silent and safe.** A missing, malformed, or newer-schema file
  yields no policies — the false positives you had before it, never fewer
  checks.

Because Raven reads the file from the installed package directory, a declaration
takes effect once the package is installed and Raven has loaded it. Policies are
not yet carried in `.raven/packages.json`, so a CI run that relies on
[`raven packages freeze`](package-database.md) rather than an installed library
does not see them.

## Choosing the Right Escape Hatch

Use `# raven: nse` when the false positive comes from a function argument being
captured, data-masked, tidy-selected, or otherwise evaluated non-standardly. It
describes the callee's argument policy and keeps Raven checking the rest of the
call.

Use `# raven: var` when the name is created dynamically outside syntax Raven can
see, such as by `eval()`, a dynamic `assign()`, a runtime setup step, or external
data loading.

Use `# raven: ignore` or `# raven: ignore-next` when neither of those describes
the problem, or when you want a one-line suppression for code that is
intentionally too dynamic to model.

If a workspace is highly dynamic and targeted escape hatches create too much
noise, you can also disable undefined-variable checking inside call arguments
with `raven.diagnostics.undefinedVariableInCallArguments`, or inside `[` / `[[`
indices with `raven.diagnostics.undefinedVariableInBracketIndices`. These are
broad opt-outs: they reduce false positives by giving up real
undefined-variable checks in those positions.

## Limitations

Raven's built-in NSE policy table covers common, slow-moving surfaces, but it is
not exhaustive. An uncatalogued package helper can still produce a false
positive until you add `# raven: nse` or Raven learns that helper's policy.

Some code is too dynamic for static analysis to classify precisely. If a
function value comes from runtime code, Raven may not know how that function
evaluates its arguments. For example, after `fn <- get_function()`, Raven can
see `fn(typo)`, but not whether `fn` treats `typo` as an ordinary variable or as
captured code. In those cases Raven may suppress argument diagnostics rather
than guess. Runtime name creation, `attach()`, dynamically constructed package
loads, and computed data or column names may still need declarations or local
suppressions.

For `[` calls, Raven checks indices for ordinary objects but treats data.table
contexts specially. In data.table-heavy projects, an unresolved object can be
treated as data.table-like when data.table is detectably in play; this avoids
many false positives but can miss a real undefined index in ambiguous code. A
`[` call with a `:=` argument (`d[, col := value, by = grp]`, in either the
infix or the functional `` `:=`(...) `` spelling, and only when the `:=` is the
argument itself rather than nested inside another expression) is treated as
NSE unconditionally, whatever package is in play and whatever the indexed
object is, because no base `[` method evaluates `:=`; packages that adopt
data.table's bracket notation for their own containers get the same treatment
without a declaration.

## Toward NSE Metadata

R package metadata exposes exported symbols, including function names, but not a
machine-readable table of function signatures or argument-evaluation policy.
[`inst/raven/nse.toml`](#declaring-nse-from-a-package) is Raven's version of that
table: lightweight metadata for exported function signatures — function names,
formal argument names, and which arguments are evaluated non-standardly — that a
package can author and ship today. It is Raven-specific by necessity, not by
preference; the shape is deliberately simple enough to be worth proposing as a
convention other R tooling could read.

That only covers packages whose authors opt in. Raven could also experiment with
generating the same metadata itself from package source,
including across public package repositories such as CRAN and Bioconductor. That
kind of corpus-derived table could improve Raven's own diagnostics; it could
also serve as a starting point for discussion about including such data in the R
packaging ecosystem.
