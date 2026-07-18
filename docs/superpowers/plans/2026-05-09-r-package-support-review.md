# Code Review: R Package Workspace Support

## Summary

This PR adds native R package development support to Raven. When the workspace root contains a `DESCRIPTION` file, Raven activates "package mode":

- All `R/*.R` files are treated as mutually visible (flat namespace, like `devtools::load_all()`)
- Roxygen2 `@export`/`@import`/`@importFrom` annotations are parsed from source
- NAMESPACE `import()`/`importFrom()` directives suppress undefined-variable diagnostics
- A `raven.packages.packageMode` setting (auto/enabled/disabled) controls activation

## Files to Review

### New files
- `crates/raven/src/package_namespace.rs` — Package detection, `PackageNamespaceModel`, NAMESPACE parsing, roxygen aggregation
- `docs/r-package-dev.md` — User-facing documentation

### Modified (core logic)
- `crates/raven/src/roxygen.rs` — New `RoxygenNamespace` struct and `extract_roxygen_namespace_tags()` function (appended at end)
- `crates/raven/src/handlers.rs` — `DiagnosticsSnapshot` gains `package_internal_symbols` and `package_full_imports` fields; diagnostic suppression check; package-mode completions block
- `crates/raven/src/state.rs` — `WorldState` gains `package_workspace`/`package_namespace_model`; `scan_workspace` detects packages; `apply_workspace_index` respects `PackageMode`
- `crates/raven/src/backend.rs` — DESCRIPTION/NAMESPACE change handling in `did_change_watched_files`; `packageMode` setting parsing
- `crates/raven/src/cross_file/config.rs` — `PackageMode` enum, `package_mode` field on `CrossFileConfig`
- `crates/raven/src/cross_file/workspace_index.rs` — New `entries()` method

### Modified (wiring/docs)
- `crates/raven/src/lib.rs`, `crates/raven/src/main.rs` — Module declaration
- `crates/raven/src/state_tests.rs` — Updated destructuring for 6-tuple `WorkspaceScanResult`
- `editors/vscode/package.json` — Setting schema
- `editors/vscode/src/initializationOptions.ts` — Setting forwarding
- `editors/vscode/src/test/settings.test.ts` — Test mapping
- `docs/cross-file.md`, `docs/configuration.md`, `README.md`, `AGENTS.md` — Links

## Review Focus Areas

1. **Locking discipline** — `collect_package_internal_symbols` is called inside `DiagnosticsSnapshot::build` which holds the `WorldState` read lock. It calls `cross_file_workspace_index.entries()` which acquires its own `RwLock<LruCache>` read lock. Verify no deadlock risk with existing lock ordering.

2. **Performance of mutual visibility** — `collect_package_internal_symbols` iterates all workspace index entries on every snapshot build. For large packages (hundreds of R files), is this acceptable? Should results be cached?

3. **Completions duplication** — The package-mode completions block iterates `workspace_index` entries. Could this produce duplicates with symbols already in scope via the dependency graph (e.g., if a file also has `source()` chains)?

4. **NAMESPACE multiline parsing** — `normalize_multiline` in `package_namespace.rs` joins lines when the previous line has unbalanced parens. Verify this handles edge cases (comments inside multiline directives, nested parens).

5. **Roxygen `@export` extraction** — `extract_definition_name` uses heuristics to find the symbol name after a roxygen block. Review edge cases: `setGeneric(...)`, `setMethod(...)`, backtick-quoted names, `%>%` operators.

6. **File watching race** — The DESCRIPTION/NAMESPACE rebuild in `did_change_watched_files` does synchronous filesystem reads under the write lock. For very large packages with many R files, this could block. Should it be async?

7. **`PackageMode::Enabled` without DESCRIPTION** — When forced enabled, the `PackageWorkspace` gets `name: "unknown"` and `roxygen_managed: false`. Verify downstream code handles this gracefully.

## Design Decisions

- **Scope injection vs. diagnostic suppression** — Package-internal symbols are NOT injected into the scope resolution engine. They're checked as a separate suppression set in the diagnostic loop. This avoids polluting the position-aware scope model but means hover/signature info may not show for package-internal symbols resolved this way.

- **Roxygen preferred, NAMESPACE fallback** — Detection heuristic: any `#' @export` in any `R/*.R` file → roxygen-managed. This is simple but could misfire on packages that have a single `@export` in a legacy file while primarily using hand-written NAMESPACE.

- **Full mutual visibility (no Collate ordering)** — All R/*.R symbols are visible everywhere. This matches how most developers think about R packages but doesn't respect `Collate:` field ordering for side-effect-dependent code.

## Test Results

All 3349 existing tests pass. New tests added in `package_namespace.rs` (detection, NAMESPACE parsing, roxygen aggregation) and `roxygen.rs` (namespace tag extraction).
