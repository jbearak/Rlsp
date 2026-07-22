# Module & import systems

Raven understands two R module/import systems whose semantics ordinary
`library()` and `source()` cannot express: the [`box`](https://klmr.me/box/)
package and the [`import`](https://rticulate.github.io/import/) package. Both are
modelled as **selective imports** — Raven brings in only the names an import
actually requests, under the names it requests. It never merges a whole file's
definitions the way `source()` does, and never dumps a package's entire export
set in as bare names the way `library()` does.

Everything here is **static**: Raven does not run your code to discover what a
module exports or where it lives. Forms it cannot read statically are left inert
— they neither bind names nor produce misleading diagnostics. For the exact
boundaries see [Limitations](limitations.md); for the diagnostic codes see
[Diagnostics](diagnostics.md).

## box modules (`box::use()`)

A box import binds a **namespace object** (used as `alias$member`) and/or an
explicitly chosen subset of a module's exports:

```r
box::use(
  dplyr,                 # namespace object:        dplyr$filter(...)
  dr = dplyr,            # ... under an alias:       dr$filter(...)
  dplyr[filter, select], # attach members directly (no namespace object)
  dplyr[f = filter],     # attach under a local name: f()
  dplyr[...],            # attach every export
  ./helpers,             # local module: helpers.r or helpers/__init__.r
  ../lib/util[foo],      # attach `foo` from a module one directory up
)
```

- A **bare name** is an installed package; a **local module** must begin with
  `./` or `../`. Both top-level and function-scoped calls are recognised, as is
  the `box:::use()` spelling.
- The default namespace name is the final path/package component (`../lib/util`
  binds `util`); write `alias = spec` to override it.
- An **attach-only** spec (`pkg[a, b]`, `pkg[...]`) binds no namespace object
  unless you also alias it (`alias = pkg[...]`).

**Local modules** resolve relative to the importing file's own directory — box
does not use Raven's `source()` working-directory or workspace-root fallback
rules. The extension is omitted in the spec; Raven tries `path.r`, `path.R`,
`path/__init__.r`, then `path/__init__.R` (so a file module wins over a package
module), and resolution is case-sensitive.

**Exports** come from `box::export()` and `#' @export` tags. When either is
present the interface is authoritative, so a missing member (`module$typo`) is
diagnosable. A module with no export markers falls back to "every top-level name
that does not begin with a dot," treated as *non-authoritative* — absence is not
concluded, because a dynamic binding might supply it. Private names, and names
only transitively imported, never cross the boundary.

Imported namespace aliases and attached members work everywhere Raven's
intelligence does — diagnostics, completion, hover, signatures, go-to-definition,
and find-references — in open and workspace-indexed files, in the language server
and `raven check`. Editing a local module's exports revalidates its importers,
but a box edge never lends ordinary `source()` scope or `# raven: nse` /
`# raven: func` declarations. The `/` inside a box spec stays exempt from the
`infix_spaces` lint; ordinary `/` expressions are unaffected.

## import package (`import::from()`)

The import package selects names from an installed package or a local `.R`/`.r`
script module:

```r
import::from(dplyr, filter, select)     # selected names
import::from(dplyr, keep = filter)       # renamed
import::from(dplyr, .except = "lag")     # every export except some
import::here(clean, .from = "utils.R")   # into the current environment
```

- A source is a package name or a **literal** `.R`/`.r` path (the extension is
  part of the path — no box-style candidate guessing). A literal `.directory`
  is honoured.
- `.all = TRUE` attaches the known export set; a non-empty static `.except`
  implies `.all`. When several `.all` bindings target the same local name, R's
  sequential last-write-wins order applies.
- `here()` binds into the current (lexical) environment at the call position —
  inside a function, its effect stays in that function. `from()` and `into()`
  bind into a lower-priority search-path destination: these are fallback bindings
  *below* your lexical names, not namespace objects, so they enable no `$` access.

A local script module's candidate exports are its private top-level environment
(including dotted names and top-level `import::here()` bindings). box export
markers do not govern import modules. Editing a local module revalidates its
importers without lending ordinary scope.

## What is not covered

Both systems deliberately stop at what is statically knowable. Programmatic
invocation, computed or dynamic sources and paths, remote/pins modules, runtime
`options(box.path = ...)`, `.character_only` package vectors, and side-effecting
load/unload hooks are left inert rather than guessed. For the exact per-system
lists, see [Limitations — box module system](limitations.md#box-module-system-boxuse)
and [Limitations — import package](limitations.md#import-package); for the
`box-*` and `import-*` diagnostic codes, see [Diagnostics](diagnostics.md).
