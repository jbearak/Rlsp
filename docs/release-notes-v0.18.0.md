# Raven v0.18.0 — selective module imports

Raven 0.18.0 adds static understanding of the [box](https://klmr.me/box/) R module system. Raven now recognises `box::use()` imports and `box::export()` / `#' @export` interface declarations, resolves local module paths, and models each import as a first-class *selective import* that is deliberately distinct from a package `library()` load and from an ordinary `source()` relationship.

It also recognizes the static first phase of the [`import`](https://rticulate.github.io/import/) package: `import::from()`, `import::here()`, and `import::into()` with package or literal `.R`/`.r` script sources, selected and renamed names, `.all`, static `.except`, literal `.directory`, and default or literal named destinations. Current-environment imports are lexical and position-sensitive; named destinations are lower-priority fallback bindings, not namespace objects. Script modules expose a partial live private top-level environment (including dotted names and nested top-level `import::here()` bindings) without adopting box export-marker rules. Wildcards de-duplicate an already selected exported identity, while a different later wildcard export targeting the same local name follows R's sequential last-write-wins behavior. Completion, hover, signatures, go-to-definition, and identity-aware references use the same selected binding without exposing unrelated package exports or private script members.

## Highlights

### `box::use()` imports

Raven parses static `box::use()` (and `box:::use()`) calls, at the top level and inside functions, and records what each one brings into scope:

- **Bare names are installed packages; local modules are explicit relative paths** beginning with `./` or `../`.
- **Namespace bindings** — `box::use(dplyr)` binds the `dplyr` namespace object (used as `dplyr$filter`); `box::use(dr = dplyr)` binds it under an explicit alias. A local module's default alias is its final path component.
- **Attach lists** — `dplyr[filter, select]` attaches members directly, `dplyr[f = filter]` attaches under a local name, and `dplyr[...]` attaches every export. An attach-only spec binds no namespace object unless you also write an explicit `alias = spec[...]`.

### Local module resolution

Local module paths resolve relative to the importing file's own directory and, matching box, intentionally ignore `# raven: cd`, the implicit testthat/testit working directory, and the workspace-root fallback. The extension is omitted in the spec; Raven tries `path.r`, `path.R`, `path/__init__.r`, then `path/__init__.R`, so a file module wins over an `__init__` package module. Resolution is case-sensitive — a path that exists only under a different case is treated as a mismatch, never silently corrected.

### Export interfaces

A module's exported surface is parsed from `box::export(...)` (the union of unquoted names; `box::export()` is an explicit empty interface) and `#' @export` tags, including tags on `box::use()` imports that re-export them. When explicit markers are present the interface is authoritative; otherwise Raven falls back to the legacy default of every non-dot-prefixed top-level name, treated as non-authoritative. Private and merely-transitively-imported names never cross the boundary.

### Language intelligence, privacy, and revalidation

Imported aliases and attached names participate in diagnostics, completion, hover, signature help, go-to-definition, and find-references in open and indexed files, in the language server and `raven check`. Namespace access through `$`, `@`, and literal `[[...]]` uses the module's export boundary; local definitions remain navigable through named, renamed, and wildcard re-exports, while installed-package members use Raven's package metadata without fabricating source locations.

Local modules are dependency-graph participants, so changing an import or exported interface revalidates importers. Their edges are deliberately non-lending: private definitions, transitive imports, and module `# raven: nse` / `# raven: func` declarations never leak as ordinary sourced scope.

Raven diagnoses missing modules, case-only path mismatches, missing packages, and selected names absent from an authoritative export set. Incomplete metadata remains conservative and never turns an unknown absence into an error.

### A reusable model for future syntaxes

The semantics above are captured in a shared *selective import* abstraction (dialect-bearing source identity, export set with a completeness marker, namespace alias, attach bindings, destination, exclusions, provenance, and interface hashing). `{box}` and `{import}` keep their own detectors, path rules, and module-export policies while reusing that scope and dependency pipeline.

## Supported static forms vs. dynamic non-goals

Support is scoped to statically analyzable forms. The following are **deliberately not** supported and fail conservatively — they neither bind names nor emit misleading diagnostics:

- Programmatic invocation (`do.call`, aliasing `box::use`, runtime-built argument lists) — only literal `box::use(...)` calls are recognised.
- Non-local module search paths such as `foo/bar`, `options(box.path = ...)`, the `R_BOX_PATH` environment variable, and remote/global modules.

Raven does not execute module code or hooks, evaluate arbitrary `options(box.path = ...)`, inspect remote/user-global modules, or guess computed imports. Marker-less modules and not-yet-complete package metadata expose known positive members but do not prove a missing member absent. See [Limitations — box module system](limitations.md#box-module-system-boxuse) for the exact static boundary.

## Install

- **VS Code:** install or update from the [VS Code Marketplace](https://marketplace.visualstudio.com/items?itemName=jbearak.raven-r).
- **Cursor, Positron, and other VS Code-based editors:** use [Open VSX](https://open-vsx.org/extension/jbearak/raven-r) or the matching platform `.vsix` from the GitHub release.
- **Other editors and CI:** download the matching `raven-<os>-<arch>.zip` and run `raven --stdio` or the desired CLI command.

## Learn more

- [Cross-file analysis — box module imports](https://github.com/jbearak/raven/blob/v0.18.0/docs/cross-file.md#box-module-imports-boxuse)
- [Limitations — box module system](https://github.com/jbearak/raven/blob/v0.18.0/docs/limitations.md#box-module-system-boxuse)

**Full changelog:** [v0.17.0...v0.18.0](https://github.com/jbearak/raven/compare/v0.17.0...v0.18.0)
