# Shared project configuration schema

Raven and Sight use the same canonical TOML path for settings that represent
the same concept. They do not need identical schemas: language-specific
settings keep their natural project-specific sections and may have different
defaults.

## Canonical shared paths

| Concept | Canonical TOML path |
|---|---|
| Workspace exclusions | `workspace.exclude` |
| Diagnostics master switch | `diagnostics.enabled` |
| Undefined-variable severity | `diagnostics.severity.undefinedVariable` |
| Workspace indexing | `crossFile.indexWorkspace` |
| Backward dependency mode | `crossFile.backwardDependencies` |
| Assumed call site | `crossFile.assumeCallSite` |
| Backward traversal depth | `crossFile.maxBackwardDepth` |
| Forward traversal depth | `crossFile.maxForwardDepth` |
| Combined traversal depth | `crossFile.maxChainDepth` |
| Missing-file severity | `crossFile.diagnostics.missingFile` |
| Case-mismatch severity | `crossFile.diagnostics.caseMismatch` |

Canonical keys use camel case. Defaults and accepted values may differ when
the underlying languages require different behavior.

## Compatibility aliases

Accepted historical and sibling-project paths remain permanent aliases:

| Project | Alias | Canonical path |
|---|---|---|
| Raven | `exclude` | `workspace.exclude` |
| Raven | `diagnostics.undefinedVariableSeverity` | `diagnostics.severity.undefinedVariable` |
| Raven | `crossFile.missingFileSeverity` | `crossFile.diagnostics.missingFile` |
| Raven | `crossFile.caseMismatchSeverity` | `crossFile.diagnostics.caseMismatch` |
| Sight | `exclude` | `workspace.exclude` |
| Sight | `diagnostics.undefinedVariableSeverity` | `diagnostics.severity.undefinedVariable` |
| Sight | `crossFile.missingFileSeverity` | `crossFile.diagnostics.missingFile` |
| Sight | `crossFile.caseMismatchSeverity` | `crossFile.diagnostics.caseMismatch` |

When an alias and its canonical path both occur, the canonical value wins and
the loader emits a warning. Documentation and generated examples use only the
canonical path. A malformed containing section does not disable an otherwise
valid alias when the canonical path is absent.

## Scope

This contract applies only to genuinely equivalent settings. Raven's
`[linting]`, `[packages]`, and `[symbols]` settings and Sight's formatting,
ADO-path, and Stata-specific settings are intentionally outside the shared
schema. Similarly named cache and revalidation limits are not shared unless
their behavior is also equivalent.
