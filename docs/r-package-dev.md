# R Package Development

Raven automatically detects R package workspaces and provides enhanced code intelligence tailored to package development workflows.

## How It Works

Raven activates **package mode** when the workspace root contains a `DESCRIPTION` file with a parseable, non-empty `Package:` field. Mere presence of `DESCRIPTION` is not sufficient — the file must carry a valid `Package:` entry. In package mode:

1. **Mutual visibility** — All `R/*.R` files see each other's top-level symbols, matching `devtools::load_all()` semantics. A function defined in `R/utils.R` is available in `R/analysis.R` without any `source()` call.

2. **Import resolution** — Symbols imported via `NAMESPACE` or roxygen annotations (`@import`, `@importFrom`) suppress undefined-variable diagnostics. Packages listed in `DESCRIPTION` `Depends:` are also treated as attached — their exports resolve unqualified, equivalent to a NAMESPACE `import(pkg)` of each — because R puts a `Depends:` package's exports on the search path when your package loads. `Imports:` keeps the stricter R semantics (loaded but not attached), so an `Imports:`-only package still requires `pkg::fn` or an explicit `@importFrom`/`importFrom(...)`.

3. **Roxygen + NAMESPACE merge** — Raven unions imports and exports parsed from the generated `NAMESPACE` file with roxygen tags (`@import`, `@importFrom`, `@export`) parsed from `R/*.R` files. Imports visible to your code are the combined set from both sources, so you get correct import resolution whether you edit `NAMESPACE` directly, rely on `devtools::document()` to regenerate it from roxygen, or are mid-edit between the two.

4. **Own NSE verbs** — The package's own exported non-standard-evaluation verbs keep their argument policy inside the package's own files (any of its `.R`/`.Rmd`/`.Rmarkdown`/`.qmd` source files — `R/`, `tests/`, vignettes, `man/` examples, `inst/`, `data-raw/`, and so on). When you develop a package named `dplyr`, a `filter(df, x > 1)` in its test suite does not flag the masked column `x`, even though no `library(dplyr)` attaches the package under development. See [Non-Standard Evaluation](non-standard-evaluation.md) for how the per-call argument policy works.

## What's Supported

### Mutual Visibility

All top-level symbols (functions, variables, constants) defined in files under `R/` are visible to every other file under `R/`. This eliminates false-positive "undefined variable" diagnostics for cross-file function calls within your package.

"Under `R/`" means what R loads: the files directly in `R/` plus the two OS-specific subdirectories `R/unix/` and `R/windows/`, and only files whose name starts with an ASCII letter or digit (R's `tools::list_files_with_type(.., "code")` filter, so `R/_helper.R` and `R/.hidden.R` are not package code). R never recurses into `R/` (`R CMD INSTALL`, `devtools::load_all()`, and `pkgload` all use the non-recursive `tools::list_files_with_type()` listing), so a script kept in another subdirectory such as `R/scripts/run.R` is not package source. Raven treats it as an ordinary script: it neither contributes symbols to the namespace nor sees package internals unqualified, and it gets the usual cross-file behavior (`source()` detection, directives) instead.

```r
# R/helpers.R
validate_input <- function(x) { ... }

# R/main.R
run_analysis <- function(data) {
  validate_input(data)  # No diagnostic — Raven knows this is in R/helpers.R
}
```

Files outside `R/` (e.g., `tests/`, `inst/`, `vignettes/`) are not included in mutual visibility — but they get one-way read access to `R/` symbols (see below).

### Internal data (`R/sysdata.rda`)

Objects stored in `R/sysdata.rda` are namespace-internal and available to your package's own code at runtime, so Raven treats them as in scope for files under `R/` (and everywhere else package symbols are visible, like testthat tests). The names are discovered by scanning `data-raw/` for the generating `usethis::use_data(..., internal = TRUE)` / `save(..., file = "R/sysdata.rda")` call; if no generating script exists (the `.rda` is committed directly), Raven loads the file via R to enumerate its objects. Both the editor and `raven check` apply this. Sysdata objects are *not* exported, so a script outside the package that does `library(yourpkg)` and references one still gets a diagnostic — matching R.

### Tests directory awareness

Files under `tests/testthat/` get one-way read access to package-internal
symbols (`R/*.R`) and to symbols imported via NAMESPACE/roxygen. Tests can
call internal package functions without "undefined variable" diagnostics.
Symbols defined in test files are not visible from `R/*.R`.

The same one-way access extends to plain top-level `tests/*.R` scripts (the
old-style files `R CMD check` runs directly, e.g. `tests/Simple.R`): because
the package is loaded when its tests run, those scripts see all `R/` top-level
symbols and NAMESPACE imports. Unlike `tests/testthat/helper-*.R`, plain test
scripts do **not** see each other's definitions — `R CMD check` runs each in a
separate process — and their own definitions never leak into `R/`.

```r
# R/helpers.R
process_data <- function(df) { ... }

# tests/testthat/test-helpers.R
test_that("process_data works", {
  result <- process_data(mtcars)  # No diagnostic — helper visible from tests
  expect_equal(nrow(result), nrow(mtcars))
})
```

In contrast, a function defined in `tests/testthat/test-helpers.R` is **not**
visible to `R/helpers.R` — symbols in `R/` are visible from `tests/testthat/`,
but not the other way around.

#### Implicit `library(testthat)` under `tests/testthat/`

Raven treats `testthat` as if it were attached (via `library(testthat)`) when
all of the following hold:

- the workspace is in package mode (DESCRIPTION with a valid `Package:` field), and
- the DESCRIPTION declares `testthat` in `Suggests:`, `Imports:`, or `Depends:`, and
- the queried file is under `tests/testthat/`.

This matches `testthat::test_check`'s loader, which attaches testthat before
sourcing each test file. Test files therefore do not need (and conventionally
do not include) an explicit `library(testthat)` — calling `test_that(...)`,
`expect_equal(...)`, etc. produces no "undefined variable" diagnostic. Outside
`tests/testthat/`, the same calls remain flagged: implicit attachment is scoped
to the testthat directory.

If the DESCRIPTION does not declare testthat, no implicit attachment happens —
the diagnostic stays as "undefined variable" until the user either adds
`Suggests: testthat` (the conventional fix) or adds an explicit `library(testthat)`.

#### Helper and setup files (`tests/testthat/helper*.R`, `setup*.R`)

Before any test runs, `testthat::source_test_helpers` sources files matching
`^helper.*\.[Rr]$` in `sort()` order, and then `source_test_setup` sources
files matching `^setup.*\.[Rr]$` the same way. Raven mirrors this:

- Top-level definitions in any `tests/testthat/helper*.R` or `setup*.R` file
  are visible from `test-*.R` (and other non-helper/setup test files),
  because by the time a test runs all helper and setup files have been
  sourced. For example, a setup file defining `CLEAN <- SETUP <- FALSE` makes
  both `CLEAN` and `SETUP` visible to every test file.
- Between helper/setup files, visibility follows sourcing order: a file sees
  earlier-sourced peers but not later ones. Helpers are sourced before setup
  files, and each group in `sort()` order — so `helper-b.R` sees
  `helper-a.R`'s top-level defs but not `helper-c.R`'s, and every setup file
  sees all helpers.
- Helper/setup files are matched by filename only at the top level of
  `tests/testthat/`; files in subdirectories (e.g.
  `tests/testthat/sub/helper-x.R`) are not auto-sourced by testthat and
  are NOT treated as helpers here either.
- Helper/setup defs never propagate into `R/` (the one-way visibility into
  `R/` stays asymmetric).
- A helper's own `source()` calls are followed (issue #638): top-level
  definitions and `library()` attaches in files the preamble transitively
  sources — including via a computed path like
  `source(file.path(repo_root, "scripts/helpers.R"))` after
  `repo_root <- normalizePath(file.path("..", ".."))` — become visible to
  sibling test files the same way the helper's own defs do. Relative paths
  anchor at `tests/testthat/` (the [implicit testthat working
  directory](cross-file.md#implicit-testthattestit-working-directory)), and
  the scan refreshes from authoritative open buffers as the helper or a
  sourced file changes, then returns to disk state when the file closes.

Note the gating difference: the helper-visibility machinery on this page
requires package mode (a `DESCRIPTION` with a `Package:` field), while the
implicit testthat working directory and computed-path folding in
[cross-file.md](cross-file.md) are layout-only and apply to any workspace with
a `tests/testthat/` directory.

A preamble file's top-level `library()` / `require()` calls **attach** their
packages for sibling test files too, mirroring the same sourcing semantics. A
`tests/testthat/helper-lib.R` containing `library(tidyr)` makes tidyr's exports
(`pivot_wider`, `tibble`, …) usable by bare name in every `test-*.R` file —
without each test repeating the `library()` call — exactly as if testthat had
attached tidyr before the test ran. `require(pkg, quietly = TRUE)` counts as an
attach too. Neither `loadNamespace()` nor `requireNamespace()` attaches (they
only enable qualified `pkg::fn` access), a `library()` call nested inside a
function body does not attach until that function runs, and a call captured by a
quoting wrapper (`quote()`, `bquote()`, `rlang::expr()`, …) is never evaluated —
so none of these propagate.
These attaches follow the same visibility rules as the defs above: source-order
between preamble files, visible to test files **in the same directory**
(`tests/testthat/` preambles never reach `tests/testit/` siblings, which don't
source them), and **never** propagated into `R/`.

```r
# tests/testthat/helper-lib.R
library(tidyr)

# tests/testthat/test-a.R
test_that("reshaping works", {
  wide <- pivot_wider(long)  # No diagnostic — tidyr attached by the helper
})
```

```r
# tests/testthat/helper-fixtures.R
demo_input <- c(1, 2, 3)

# tests/testthat/test-foo.R
test_that("works on demo_input", {
  expect_equal(length(demo_input), 3)  # No diagnostic — visible from helper
})
```

Teardown files (`teardown*.R`) run only after all tests have finished, so
their top-level bindings are never visible to test code and are not injected.

### Dev-context directories (`demo/`, `data-raw/`, `vignettes/`, `man/`)

Files under these directories get the same one-way read access to package
symbols as test files: they see all `R/*.R` top-level symbols and NAMESPACE
imports, so calling your own package functions from a vignette, demo script,
data-preparation script, or man-page Rmd helper produces no "undefined
variable" diagnostic.

Their own definitions never leak back into `R/`, and they don't see each
other — a function defined in `data-raw/prepare.R` is not visible from
`vignettes/intro.Rmd`, and vice versa.

```r
# R/analysis.R
run_model <- function(data) { ... }

# vignettes/tutorial.Rmd (or vignettes/tutorial.R)
result <- run_model(example_data)  # No diagnostic — visible from R/

# demo/walkthrough.R
output <- run_model(sample_input)  # No diagnostic
```

Symbols that are NOT exported or defined in `R/` still flag as undefined
in these directories — the one-way visibility is limited to what the package
actually provides.

**`inst/` and `revdep/` are not dev-context.** Plain `inst/` scripts (shiny
apps, rmarkdown template skeletons, example scripts) and reverse-dependency
checks are not run with the package implicitly loaded, so they rely on an
explicit `library(yourpkg)` or a [directive](directives.md) just like any other
script — a bare reference to a package function there *is* flagged. The one
exception is installed test suites: R files under `inst/tinytest/` and
`inst/unitTests/` are treated as **test files** (one-way package R/ visibility),
since those suites run with the package loaded.

### Scripts that call `devtools::load_all()`

A script **anywhere** in the package source tree (including non-standard
locations like `inst/`, `tools/`, `debug/`, or `internal/`) that calls
`devtools::load_all()` / `pkgload::load_all()` — or a bare `load_all()` — is
modeled as attaching the package under development. Raven then makes the
package's own symbols visible throughout the file: internal and exported `R/`
definitions, `R/sysdata.rda` objects, names bound in `.onLoad`/`.onAttach`, and
NAMESPACE imports. This matches what `load_all()` does at runtime, so the
exploratory and maintenance scripts package authors keep in these directories
don't draw false positives for their own package's functions.

This is deliberately broader than `library(yourpkg)`: `load_all()` defaults to
`export_all = TRUE`, so a helper saved under `R/` is available after
`load_all()` even if it is not exported in `NAMESPACE`. Exports still matter for
installed-package use, `library(yourpkg)`, and `R CMD check`.

```r
# internal/scratch.R
devtools::load_all()
result <- my_internal_helper(data)  # No diagnostic — load_all() attached the package
typo_helper()                       # Still flagged — not a package symbol
```

The injection is gated on the call, not the path: the same file *without* a
`load_all()` call (and outside the dev-context directories above) sees only the
normal global/`library()` scope.

#### Transitive propagation through `source()` chains

The `load_all()` injection propagates exactly like `library()` — forward through
`source()` chains, position-aware, and across multiple parents:

```r
# internal/setup.R
devtools::load_all()
source("run.R")      # run.R inherits the package internals

# internal/run.R  (no load_all() here)
result <- my_internal_helper(data)  # No diagnostic — inherited from setup.R
```

A file in the chain that is reached **before** the `load_all()` call (or only
through parents that never call `load_all()`) does not inherit the internals.

#### R/ source changes trigger diagnostics refresh

When you add, delete, or edit a file under `R/`, Raven automatically re-publishes
diagnostics for:

- the file that called `load_all()`,
- every file it `source()`s (forward chain), and
- every file that `source()`s the `load_all()` caller (backward chain).

This keeps the "is this function defined in the package?" check in sync as you
work on the package source, without restarting the editor.

#### Go-to-definition for load_all() internals

Cmd-click (or F12) on a symbol made available by `load_all()` navigates to its
real definition in the package's `R/` source — whether the target file is open or
not. This works for the same files that benefit from `R/`→`R/` mutual-visibility
navigation in package mode.

Goto into external/installed packages (e.g. a dependency's unexported function
referenced after `load_all()` via `:::`) is not yet supported; see
[Go-to-Definition — Package Exports](go-to-definition.md#package-exports).

## Ordinary scripts in package workspaces

Directories such as `scripts/`, `analysis/`, `tools/`, `debug/`, and plain
`inst/` are ordinary scripts from package mode's point of view. They do not
receive package `R/` symbols just because they sit in a package workspace. A
bare call to `my_internal_helper()` in `scripts/foo.R` is therefore flagged
unless that script's runtime path actually makes the name available.

Use the same mechanisms R would use:

| Loading mechanism | What becomes visible |
|---|---|
| `library(yourpkg)` | Only exported symbols (`@export` / `NAMESPACE`). Requires the package installed. Internals need `yourpkg:::name`. |
| `devtools::load_all()` / `pkgload::load_all()` | Exported and internal `R/` symbols. `load_all()`'s `export_all = TRUE` default copies all package objects into scope. |
| `source("R/...")` | All top-level definitions in the sourced file. There is no export concept; `source()` has nothing to do with packages. |
| `# raven: source R/...` | Raven-only static-analysis hint equivalent to a `source()` edge, including transitive `source()` chains. |
| Workspace-root `.Rprofile` | Startup definitions, attached packages, and literal `source()` chains, when that prelude applies. See [`.Rprofile` Startup Prelude](rprofile.md). |

### `load_all()` vs. `library()`

`devtools::load_all()` makes internal functions usable by bare name, while
`library(yourpkg)` exposes only exports. Code that works after `load_all()` can
therefore still fail under `library()` or `R CMD check`.

Adding `@export` alone does not make a function visible to an ordinary
`scripts/` file. The file still needs `library(yourpkg)` for exports, or
`devtools::load_all()` / `pkgload::load_all()`, `source()`, a
`# raven: source` directive, or a workspace-root `.Rprofile` that loads the
helpers at startup.

### Build commands

When the workspace is detected as an R package (DESCRIPTION with a non-empty
`Package:` field, or `raven.packages.packageMode` set to `enabled`) **and**
Raven's R console is active (see [Coexistence](./coexistence.md)), Raven
contributes six Command Palette entries that wrap the standard
`devtools` / `testthat` / `roxygen2` workflows. Names mirror RStudio's
**Build** menu so existing muscle memory carries over. The Command Palette
and editor-title submenu entries are gated on
`raven.rConsoleEnabled && raven.isRPackage`; if `raven.rConsole.activation`
is on the default `"auto"` and REditorSupport's R extension is enabled (or
you're running Positron), the build commands stay hidden and you should
use REditorSupport's or Positron's package-development workflow instead.

| Palette title | Runs in | R call |
|---|---|---|
| `Raven Build: Load All` | active R terminal | `devtools::load_all("<workspace>")` |
| `Raven Build: Document` | active R terminal | `devtools::document("<workspace>")` |
| `Raven Build: Install and Restart` | active R terminal | `devtools::install("<workspace>")` followed by `quit(save = "no")` |
| `Raven Build: Test Package` | `R: Package Tasks` terminal | `devtools::test("<workspace>")` |
| `Raven Build: Check Package` | `R: Package Tasks` terminal | `devtools::check("<workspace>")` |
| `Raven Build: Build Source Package` | `R: Package Tasks` terminal | `devtools::build("<workspace>")` |

Each command passes the first workspace folder's absolute path explicitly,
so a stray `setwd()` in the R session — or a terminal launched from a
subdirectory — can't redirect the build at the wrong project.

The six commands also appear as a single `$(package)` submenu in the
editor title bar when an R, R Markdown, or Quarto file is open in a package workspace.

#### Terminal routing

The three session-mutating commands (`Load All`, `Document`,
`Install and Restart`) run in the same R terminal that Send-to-R uses,
so their side effects land where you'd expect.

The three long-running commands (`Test Package`, `Check Package`,
`Build Source Package`) run in a dedicated `R: Package Tasks` terminal.
This avoids tying up the interactive prompt for the 20–60s+ these
commands can take, and keeps a clean separation between exploratory
work and batch-style package checks. The tasks terminal is reused
across invocations — Raven doesn't pay R-startup cost on every
`devtools::test()`. Both terminals respect `raven.rTerminal.program`,
so a configured `radian` or `arf` carries over.

#### Install and Restart semantics

`Install and Restart` chains `devtools::install()` with
`quit(save = "no")` so the R process exits after install completes.
When the terminal closes, Raven recreates it in the same pane. The next
Send-to-R or Build command runs in a fresh R session that picks up the
newly installed version of the package — which is the whole point of
the command.

If the install fails, the wrapper surfaces the error via `message()`
before R exits; the failure output stays visible in the closed-terminal
scrollback so you can read it before dismissing.

### testthat problem matcher

When you run `devtools::test()` or `testthat::test_dir()`, testthat's
default progress reporter prints failure headers like:

```text
Failure ('test-helpers.R:12:3'): process_data handles NAs
Expected 1 to equal 2.
Differences:
1/1 mismatches
[1] 1 - 2 == -1
```

Raven contributes a `$testthat` problem matcher that parses those headers
and surfaces each failing test in VS Code's Problems panel, with a
clickable file:line link that jumps to the failing assertion.

To wire it up, add a task to `.vscode/tasks.json` (or run it ad hoc via
**Terminal → Run Task…**):

```json
{
  "version": "2.0.0",
  "tasks": [
    {
      "label": "R: Test package",
      "type": "shell",
      "command": "Rscript",
      "args": ["-e", "devtools::test()"],
      "problemMatcher": "$testthat",
      "group": "test"
    }
  ]
}
```

The matcher recognises `Failure (…)` / `Error (…)` headers from the
default `ProgressReporter`, the `── Failure (…) ──` form from the
`CompactProgressReporter`, and the all-caps `FAILURE: …` / `ERROR: …`
shape that testthat's `LlmReporter` emits when running under an AI
coding agent (`CLAUDECODE` / `AGENT` / `GEMINI_CLI` / `CURSOR_AGENT`).
Paths resolve relative to `${workspaceFolder}/tests/testthat`, matching
the directory testthat sets as the working directory while a test
runs. The Problems-panel entry's message is the test name (when the
reporter emits one); the full expected/actual output stays in the
terminal where you can read it alongside any other context the test
printed.

### Roxygen Namespace Tags

When roxygen is detected, Raven parses these tags from source:

| Tag | Effect |
|-----|--------|
| `@export` | Marks the next definition as an exported symbol |
| `@import pkg` | All exports of `pkg` are available without qualification |
| `@importFrom pkg sym1 sym2` | Only `sym1`, `sym2` from `pkg` are available |

```r
#' @importFrom dplyr mutate filter
#' @export
transform_data <- function(df) {
  df |> filter(x > 0) |> mutate(y = x * 2)
  # No diagnostics for mutate or filter
}
```

### NAMESPACE + roxygen merge

Raven always parses the generated `NAMESPACE` file (when present) and
unions its entries with roxygen tags extracted from `R/*.R`:

- `import(pkg)` — all exports of `pkg` are available
- `importFrom(pkg, sym1, sym2)` — specific symbols are available
- `export(sym)` — informational (mutual visibility makes all symbols available internally regardless)

Roxygen `@import`, `@importFrom`, and `@export` in any `R/*.R` file
contribute to the same merged model; duplicate entries across NAMESPACE
and roxygen are deduped. This means roxygen-annotated imports are
visible to diagnostics and completions even before you run
`devtools::document()` to regenerate `NAMESPACE`, and NAMESPACE-only
imports remain visible if some `R/*.R` files don't carry roxygen tags.

`DESCRIPTION` `Depends:` packages are folded into this same set as
whole-package imports: each `Depends:` entry is treated exactly like a
NAMESPACE `import(pkg)`, because R attaches `Depends:` packages onto the
search path when your package loads, making their exports available
unqualified. Version constraints (`pkg (>= 1.0)`) and the special `R`
entry are ignored. `Imports:` is deliberately *not* folded in — an
`Imports:`-only package is loaded but not attached, so it still requires
`pkg::fn` or an explicit `importFrom`/`@importFrom`, matching R.

A meta-package in `Depends:` also expands to its members for
non-standard-evaluation. `Depends: tidyverse` contributes dplyr, tidyr,
ggplot2, … to the set of packages whose NSE argument policies are in
play, so a data-masking verb like `filter(x > 5)` in your `R/` code does
not flag the masked column `x`. This expansion is built in and does not
depend on the member packages being installed, so it holds in CI without
R. This applies to `Depends:` (and `library()`/`require()` attaches)
because those *attach* the meta-package — putting its members on the
search path. A NAMESPACE `import(tidyverse)` / `@import tidyverse` does
**not** get this expansion: an `import()` is a selective namespace import,
not an attach, so it does not bring the members' exports into scope (a
bare member verb there is still resolved only if the member is genuinely
re-exported and known to Raven's package database).

### data.table `[` detection in package mode

When your package depends on data.table — via DESCRIPTION `Imports:` /
`Depends:`, a NAMESPACE `import(data.table)` / `importFrom(data.table, ...)`,
or the equivalent roxygen `@import` / `@importFrom` — Raven treats data.table as
"detectably in play." Undefined-variable checking of `[` index expressions then
suppresses indices on **unresolved** objects (such as a function parameter
`dt`), so an idiomatic helper like `f <- function(dt) dt[, mean(value), by = grp]`
does not flag the column names `value` / `grp`. Objects you construct locally
with `data.frame()` / `tibble()` / `read.csv()` are still treated as
non-data.table, so `df[undefined_var, ]` is flagged (unless the call carries a
`:=` argument, which always marks the `[` call as NSE; see
[Diagnostics](diagnostics.md#call-arguments-and-bracket-indices)). `[[` is
always checked.
A statement-level by-reference converter updates that classification from the
call onward: `setDT(x)` makes `x` a data.table, `setDF(x)` makes it a plain
data.frame, and `setattr(x, "class", ...)` sets the class explicitly.
See [Non-Standard Evaluation](non-standard-evaluation.md) and
[Diagnostics](diagnostics.md#call-arguments-and-bracket-indices) for the full
rules and the `undefinedVariableInBracketIndices` opt-out.

### Live Updates

Raven watches for changes to `DESCRIPTION` and `NAMESPACE` files. After running `devtools::document()` or editing these files directly, diagnostics update automatically without restarting the editor.

## Generating a package database for CI

`raven check` can give you package-aware diagnostics in CI without installing anything — symbols from your dependencies resolve against Raven's `names.db` database when it is present, so they don't show as undefined variables. That database isn't bundled with the binary; run `raven packages update` during CI image setup or cache warmup for broad CRAN/Bioconductor coverage. Raw Cargo/source installs still have embedded R base-package coverage.

Generate and commit `.raven/packages.json` (Tier 2 of the three-tier [package-resolution fallback](package-database.md)) when CI needs reproducible, project-specific package metadata pinned to what your project actually installed. That is distinct from `raven packages update`, which restores broad Tier 3 (`names.db`) coverage from the moving `names-db` Release and is not version-pinned by the project.

Tier 2 also improves diagnostic accuracy in two common cases:

1. you depend on packages whose exports **aren't present** in Raven's Tier 3 database (GitHub-only, internal, or not-yet-indexed packages), or
2. you pin package versions whose exports **differ** from the versions Raven captured, in ways that could change your diagnostics (see the [drift caveat](package-database.md#fidelity-caveats)).

To generate the file:

```bash
raven packages freeze
```

This writes `.raven/packages.json` — a frozen snapshot of your installed packages' (Tier 1's) export names, `Depends`, and datasets — which `raven check` then prefers over Tier 3 when no R is present. Run it on a machine that has R and the project's dependencies installed; the file is generated, not hand-edited, committed for reproducible CI, and meant to be reviewed in PRs (a `git diff` shows "package X gained export Y").

Generation uses a **renv-first** library order: the renv project library first, system libraries only for packages renv doesn't cover. If your project uses [`renv`](https://rstudio.github.io/renv/), run `freeze` after `renv::restore()` for the best coverage — `renv.lock` acts as a *set selector* (which packages to include), while the exports are read from whatever is actually installed locally. Regeneration is a no-op when nothing changed, so re-running it produces no diff unless your dependencies' exports actually moved.

See [Package database](package-database.md), [`raven packages freeze`](cli.md#raven-packages-freeze), and [`raven packages update`](cli.md#raven-packages-update) for the full options and the three-tier resolution model.

## Configuration

| Setting | Default | Description |
|---------|---------|-------------|
| `raven.packages.packageMode` | `"auto"` | Controls package mode activation |
| `raven.packages.rprofilePrelude` | `true` | Uses the workspace-root `.Rprofile` startup prelude. See [`.Rprofile` Startup Prelude](rprofile.md). |

Values for `packageMode`:

- **`auto`** (default) — Enable package mode when a `DESCRIPTION` file with a parseable, non-empty `Package:` field is found at the workspace root.
- **`enabled`** — Always enable package mode, even without a `DESCRIPTION` file. Useful for non-standard package layouts.
- **`disabled`** — Never enable package mode, even if `DESCRIPTION` exists. Use this if you prefer script-mode behavior in a package workspace.

## Comparison with Script Mode

| Feature | Script Mode | Package Mode |
|---------|-------------|--------------|
| Cross-file visibility | Via `source()` chains and directives | All `R/*.R` files mutually visible |
| Package imports | Via `library()` calls | Via NAMESPACE/roxygen `@import`/`@importFrom` |
| Diagnostics | Position-aware (after `source()`) | All package symbols available everywhere |
| Detection | Default for non-package workspaces | Automatic when `DESCRIPTION` has a valid `Package:` field |

## Behavior: Non-Package NAMESPACE Files

### NAMESPACE without DESCRIPTION no longer suppresses diagnostics

Package mode activates when the workspace root contains a `DESCRIPTION` file
with a valid `Package:` field. `NAMESPACE` presence is optional and does not
affect activation — it is used (when present) to resolve imported symbols,
but its absence does not disable package mode.

Prior to this version, a workspace containing a `NAMESPACE` file but no
`DESCRIPTION` would still have its `import()` and `importFrom()` directives
parsed and used to suppress undefined-variable diagnostics. That behavior was
removed: non-package workspaces (no `DESCRIPTION` with a `Package:` field)
run as script mode regardless of `NAMESPACE` presence.

If you need package-mode behavior in a workspace without `DESCRIPTION`, set
`"raven.packages.packageMode": "enabled"` to force package mode.

## Known Limitations

- **`Collate:` ordering is not respected** — All `R/*.R` files are treated as fully mutually visible regardless of collation order. In practice this rarely matters since R's namespace mechanism doesn't enforce load order for symbol visibility.
- **S4/R5 method dispatch** — Raven doesn't trace `setGeneric`/`setMethod` relationships for method resolution.
- **Conditional exports** — Symbols exported conditionally (e.g., inside `if` blocks) are always treated as available.
- **`useDynLib`** — C/Fortran symbols loaded via `useDynLib` in NAMESPACE are not recognized.

## Troubleshooting

**Imports seem stale after editing roxygen tags:**
Run `devtools::document()` to regenerate the NAMESPACE file, or save the file — Raven re-parses roxygen tags from source on each file change.

**False positives persist after adding `@importFrom`:**
Ensure the imported package's export names are available to Raven: install the package locally, capture it in `.raven/packages.json` with `raven packages freeze`, or rely on `names.db` coverage (run `raven packages update` to download it). Export resolution is separate from install status. If `--report-uninstalled` or editor missing-package diagnostics are enabled, those still report local install status and require the package to exist on disk.

If the function is loaded at runtime by a workspace-root `.Rprofile` or a bootstrap `source()`, see [`.Rprofile` Startup Prelude](rprofile.md) and [Ordinary scripts in package workspaces](#ordinary-scripts-in-package-workspaces). Raven models `.Rprofile` automatically, and a `# raven: source` directive covers other conventions.

**Package mode not activating:**
Check that `DESCRIPTION` is at the workspace root (the first workspace folder) and contains a `Package:` field. You can also force it with `"raven.packages.packageMode": "enabled"`.
