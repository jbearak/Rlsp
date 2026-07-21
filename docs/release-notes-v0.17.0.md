# Raven v0.17.0 — targets worker packages and Shiny application loading

Raven 0.17.0 expands static cross-file analysis for `{targets}` pipelines and Shiny applications, hardens filesystem-driven source batches, improves `renv` guidance, and resolves the JavaScript dependency alerts tracked for this release.

## Highlights

### Order-independent `{targets}` worker packages

`tar_option_set(packages = ...)` now contributes worker packages to the entire targets pipeline, including every member loaded by `tar_source()`, whether the declaration appears before or after `tar_source()`. This package set also participates in package inventory, missing-package diagnostics, cache warming, and dependent revalidation without leaking into ordinary `source()` relationships. ([#702](https://github.com/jbearak/raven/pull/702))

This is full support for the documented statically analyzable forms: a string literal, a literal character vector, or an eligible same-file, single-assignment static vector. Dynamic `packages` expressions are deliberately not evaluated or resolved. Positional `packages` arguments are also not inferred because `packages` is not `tar_option_set()`'s first formal.

### Shiny implicit application loading

Raven now models Shiny's conventional application layouts without running R. ([#703](https://github.com/jbearak/raven/pull/703))

- Legacy applications load `global.R`, immediate `R/*.[Rr]` helpers, and the separate `ui.R` and `server.R` entry environments.
- Single-file applications load immediate `R/*.[Rr]` helpers and then `app.R`; an adjacent `global.R` is not implicitly loaded.
- Helper files load in deterministic C-locale filename order.
- `R/_disable_autoload.R`, matched case-insensitively, disables helper autoloading while leaving legacy `global.R` behavior intact.
- Application-root working-directory behavior and filesystem changes are reflected across diagnostics, completion, hover, navigation, references, and `raven check`.

Support intentionally follows the current conventional layouts. Historical process-global `options(shiny.autoload.r = ...)` state is deliberately not modeled, and arbitrary filenames passed to `shinyAppFile()` are out of scope.

### Reliability and diagnostics

Sustained watcher-churn coverage now exercises repeated creation, modification, restoration, and deletion across `tar_source()`, Shiny helper loading, and bounded `list.files()` source batches. Fail-closed expansion cases—member-cap overflow, unreadable entries or matching files, and matching directories—now produce debug-level records while retaining all-or-nothing behavior. User-facing hints are deliberately deferred until they can be lifecycle-aware and avoid stale or repeated notices. ([#705](https://github.com/jbearak/raven/pull/705))

The `package-outside-active-library` diagnostic now recommends `renv::hydrate()` when a package is installed outside the active project library. ([#706](https://github.com/jbearak/raven/pull/706))

### Dependency updates

The VS Code extension lockfile now resolves `js-yaml` 4.3.0 and patched `brace-expansion` releases, resolving Dependabot alerts #59–62. The exact DOMPurify pin remains unchanged. ([#701](https://github.com/jbearak/raven/pull/701))

## Install

- **VS Code:** install or update from the [VS Code Marketplace](https://marketplace.visualstudio.com/items?itemName=jbearak.raven-r).
- **Cursor, Positron, and other VS Code-based editors:** use [Open VSX](https://open-vsx.org/extension/jbearak/raven-r) or the matching platform `.vsix` from the GitHub release.
- **Other editors and CI:** download the matching `raven-<os>-<arch>.zip` and run `raven --stdio` or the desired CLI command.

## Learn more

- [Cross-file analysis](https://github.com/jbearak/raven/blob/v0.17.0/docs/cross-file.md)
- [Diagnostics](https://github.com/jbearak/raven/blob/v0.17.0/docs/diagnostics.md)
- [CLI](https://github.com/jbearak/raven/blob/v0.17.0/docs/cli.md)

**Full changelog:** [v0.16.0...v0.17.0](https://github.com/jbearak/raven/compare/v0.16.0...v0.17.0)
