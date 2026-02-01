# Requirements: Package Function Awareness

## Overview

This document specifies requirements for adding package function awareness to Rlsp, a static R Language Server written in Rust. Package function awareness enables the LSP to understand which packages are loaded via `library()`, `require()`, or `loadNamespace()` calls, and to recognize exported symbols (functions, variables, and datasets) from those packages as valid symbols.

Currently, Rlsp detects `library()` calls and extracts package names, but does not integrate package exports into the scope resolution system. This results in false-positive "undefined variable" warnings for commonly used package symbols like `ddply` (plyr), `rowMedians` (matrixStats), `mutate` (dplyr), or `penguins` (palmerpenguins dataset).

## Goals

- Suppress false-positive "undefined variable" diagnostics for symbols (functions, variables, datasets) exported by loaded packages
- Provide completions for package exports after `library()` or `require()` calls
- Support position-aware package loading (exports only available after the load call)
- Integrate with cross-file scope resolution (packages loaded in parent files propagate to children)
- Provide hover information showing which package a symbol comes from
- Handle package dependencies (Depends field) and meta-packages (tidyverse, tidymodels)
- Maintain performance through lazy loading and caching of package exports

## Non-Goals

- Runtime execution or evaluation of R code
- Dynamic package loading detection (e.g., `library(get("pkg"))`)
- Automatic package installation or management
- Support for packages not installed on the system
- Namespace-qualified calls (`pkg::func`) - these are already handled separately
- S3/S4 method dispatch resolution

## Glossary

- **Rlsp**: The R Language Server Protocol implementation being extended
- **Package_Export**: A function, variable, or dataset exported by an R package via its NAMESPACE file
- **Library_Call**: A call to `library()`, `require()`, or `loadNamespace()` that loads a package
- **Loaded_Packages**: The set of packages loaded at a given position in a file
- **Package_Library**: The collection of installed R packages on the system
- **NAMESPACE_File**: The file in an R package that declares which symbols are exported
- **Position_Aware_Loading**: The concept that package exports are only available after the library() call
- **Base_Packages**: The set of packages always loaded in R (base, methods, utils, grDevices, graphics, stats, datasets)
- **Meta_Package**: A package that attaches multiple other packages (e.g., tidyverse, tidymodels)
- **Depends_Field**: The DESCRIPTION field listing packages that are automatically attached when a package is loaded
- **Lazy_Data**: Datasets exported by a package that are lazily loaded when accessed

## Actors

- **R_Developer**: The primary user who writes R code using functions from external packages
- **LSP_Client**: The editor (VS Code, etc.) that communicates with Rlsp via the Language Server Protocol

## Requirements

### Requirement 1: Library Call Detection

**User Story:** As an R developer, I want the LSP to detect when I load packages, so that symbols from those packages are recognized as valid.

#### Acceptance Criteria

1. WHEN a file contains `library(pkgname)`, THE Library_Call_Detector SHALL extract the package name and record the call position
2. WHEN a file contains `library("pkgname")`, THE Library_Call_Detector SHALL handle quoted package names
3. WHEN a file contains `library('pkgname')`, THE Library_Call_Detector SHALL handle single-quoted package names
4. WHEN a file contains `require(pkgname)`, THE Library_Call_Detector SHALL treat it equivalently to `library()`
5. WHEN a file contains `loadNamespace("pkgname")`, THE Library_Call_Detector SHALL treat it equivalently to `library()`
6. WHEN a library call uses a variable or expression for the package name, THE Library_Call_Detector SHALL skip that call without error
7. WHEN a library call includes `character.only = TRUE`, THE Library_Call_Detector SHALL skip that call (dynamic package name)
8. WHEN multiple library calls exist in a file, THE Library_Call_Detector SHALL process them in document order

### Requirement 2: Position-Aware Package Loading

**User Story:** As an R developer, I want package exports to only be available after the library() call, so that the LSP accurately reflects R's runtime behavior.

#### Acceptance Criteria

1. WHEN resolving scope at a position before a library() call, THE Scope_Resolver SHALL NOT include exports from that package
2. WHEN resolving scope at a position after a library() call, THE Scope_Resolver SHALL include exports from that package
3. WHEN a library() call is on the same line as a symbol usage, THE Scope_Resolver SHALL treat the package as loaded for positions after the call's end position
4. WHEN a library() call is inside a function body, THE Scope_Resolver SHALL make package exports available only within that function scope from the call position forward
5. WHEN a library() call is at the top level, THE Scope_Resolver SHALL make package exports available globally from that point forward

### Requirement 3: Package Export Resolution

**User Story:** As an R developer, I want the LSP to know which symbols (functions, variables, datasets) each package exports, so that only valid package symbols are recognized.

#### Acceptance Criteria

1. WHEN a package is loaded, THE Package_Resolver SHALL query R subprocess to get the package's exported symbols using `getNamespaceExports()`
2. IF R subprocess is unavailable, THE Package_Resolver SHALL fall back to parsing the package's NAMESPACE file directly
3. WHEN a NAMESPACE file contains `export(name)`, THE Package_Resolver SHALL include `name` in the package's exports
4. WHEN a NAMESPACE file contains `exportPattern("pattern")`, THE Package_Resolver SHALL include matching symbols from the package
5. WHEN a NAMESPACE file contains `S3method(generic, class)`, THE Package_Resolver SHALL include the S3 method in exports
6. WHEN a package has lazy-loaded datasets (LazyData in DESCRIPTION), THE Package_Resolver SHALL include dataset names in exports
7. WHEN a package's exports cannot be determined, THE Package_Resolver SHALL emit a warning and treat the package as having no exports
8. THE Package_Resolver SHALL cache package exports to avoid repeated subprocess calls or filesystem access

### Requirement 4: Package Dependency Handling

**User Story:** As an R developer, I want packages that my loaded packages depend on to also be available, so that transitive dependencies work correctly.

#### Acceptance Criteria

1. WHEN a package is loaded, THE Package_Resolver SHALL read the package's DESCRIPTION file to find the `Depends` field
2. WHEN a package has dependencies in the `Depends` field, THE Package_Resolver SHALL also load exports from those packages at the same position
3. WHEN the package is `tidyverse`, THE Package_Resolver SHALL also load exports from: dplyr, readr, forcats, stringr, ggplot2, tibble, lubridate, tidyr, purrr
4. WHEN the package is `tidymodels`, THE Package_Resolver SHALL also load exports from: broom, dials, dplyr, ggplot2, infer, modeldata, parsnip, purrr, recipes, rsample, tibble, tidyr, tune, workflows, workflowsets, yardstick
5. THE Package_Resolver SHALL handle circular dependencies in the `Depends` chain by tracking visited packages

### Requirement 5: Cross-File Package Propagation

**User Story:** As an R developer, I want packages loaded in parent files to be available in sourced child files, so that my multi-file projects work correctly.

#### Acceptance Criteria

1. WHEN a parent file loads a package before a source() call, THE Scope_Resolver SHALL make that package's exports available in the sourced file from the start
2. WHEN a child file has a backward directive to a parent, THE Scope_Resolver SHALL inherit loaded packages from the parent up to the call site
3. WHEN multiple parents load different packages, THE Scope_Resolver SHALL combine all loaded packages respecting call-site positions
4. WHEN a package is loaded in a sourced file, THE Scope_Resolver SHALL NOT propagate it back to the parent (forward-only propagation)
5. WHEN computing cross-file scope, THE Scope_Resolver SHALL track package loading events in the timeline alongside symbol definitions and source() calls

### Requirement 6: Base Package Handling

**User Story:** As an R developer, I want base R functions to always be available without explicit library() calls, so that standard R functions work out of the box.

#### Acceptance Criteria

1. THE LSP SHALL query R subprocess at initialization to get the default search path using `.packages()`
2. IF R subprocess is unavailable at initialization, THE LSP SHALL use a hardcoded list of base packages: base, methods, utils, grDevices, graphics, stats, datasets
3. THE Base_Packages SHALL be available at all positions in all files without requiring explicit library() calls
4. THE Base_Packages SHALL NOT require position-aware loading (always available everywhere)
5. THE LSP SHALL document the base package behavior in README.md

### Requirement 7: Library Path Discovery

**User Story:** As an R developer, I want the LSP to automatically find my installed packages, so that I don't need to manually configure library paths.

#### Acceptance Criteria

1. THE LSP SHALL query R subprocess to get library paths using `.libPaths()`
2. IF R subprocess is unavailable, THE LSP SHALL use standard R library path locations for the platform
3. THE Configuration SHALL support `packages.additionalLibraryPaths` to specify additional library paths
4. WHEN library paths change, THE LSP SHALL invalidate cached package information

### Requirement 8: Undefined Variable Suppression

**User Story:** As an R developer, I want the LSP to not warn about undefined variables when they are package functions, so that I don't get false positive warnings.

#### Acceptance Criteria

1. WHEN checking for undefined variables, THE Diagnostic_Engine SHALL check if the symbol is exported by any loaded package at that position
2. WHEN a symbol matches a package export, THE Diagnostic_Engine SHALL NOT emit an "undefined variable" warning
3. WHEN a symbol is used before its package is loaded, THE Diagnostic_Engine SHALL emit an "undefined variable" warning
4. WHEN a package is loaded but the symbol is not in its exports, THE Diagnostic_Engine SHALL emit an "undefined variable" warning
5. WHEN `@lsp-ignore` is present, THE Diagnostic_Engine SHALL suppress the diagnostic regardless of package loading

### Requirement 9: Package Export Completions

**User Story:** As an R developer, I want completions to include symbols from loaded packages, so that I can discover and use package functions and variables easily.

#### Acceptance Criteria

1. WHEN providing completions after a library() call, THE Completion_Handler SHALL include exports (functions, variables, datasets) from the loaded package
2. WHEN a completion item comes from a package, THE Completion_Handler SHALL indicate the package name in the detail field (e.g., "{dplyr}")
3. WHEN multiple packages export the same symbol, THE Completion_Handler SHALL show all sources with package attribution
4. WHEN completing, THE Completion_Handler SHALL prefer local definitions over package exports (shadowing)
5. WHEN completing, THE Completion_Handler SHALL prefer package exports over cross-file symbols from source() chains

### Requirement 10: Package Export Hover

**User Story:** As an R developer, I want hover information to show which package a symbol comes from, so that I can understand my code's dependencies.

#### Acceptance Criteria

1. WHEN hovering over a package export (function or variable), THE Hover_Handler SHALL display the package name
2. WHEN hovering over a package function, THE Hover_Handler SHALL display the function signature if available from R help
3. WHEN a symbol is exported by multiple loaded packages, THE Hover_Handler SHALL show the effective source (first loaded)
4. WHEN a local definition shadows a package export, THE Hover_Handler SHALL show the local definition

### Requirement 11: Package Export Go-to-Definition

**User Story:** As an R developer, I want go-to-definition to navigate to package symbol documentation or source, so that I can learn how functions work.

#### Acceptance Criteria

1. WHEN invoking go-to-definition on a package export, THE Definition_Handler SHALL navigate to the package's R source file if available
2. WHEN package source is not available, THE Definition_Handler SHALL indicate the package and symbol name
3. WHEN a local definition shadows a package export, THE Definition_Handler SHALL navigate to the local definition

### Requirement 12: Configuration Options

**User Story:** As an R developer, I want to configure package function awareness behavior, so that I can tune it for my project's needs.

#### Acceptance Criteria

1. THE Configuration SHALL support `packages.enabled` boolean, defaulting to true, to enable or disable package function awareness
2. THE Configuration SHALL support `packages.additionalLibraryPaths` array to specify additional R library paths
3. THE Configuration SHALL support `packages.rPath` string to specify the path to R executable for subprocess calls
4. WHEN configuration changes, THE LSP SHALL re-resolve package exports for open documents

### Requirement 13: Caching and Performance

**User Story:** As an R developer, I want the LSP to respond quickly even when using many packages, so that my editing experience remains smooth.

#### Acceptance Criteria

1. THE Package_Cache SHALL store parsed exports per package
2. THE Package_Cache SHALL use lazy loading to avoid querying unused packages
3. THE Package_Cache SHALL invalidate entries when package files change on disk (via workspace file watchers)
4. THE Package_Cache SHALL support concurrent read access from multiple LSP handlers
5. WHEN a package is first accessed, THE Package_Resolver SHALL load exports asynchronously to avoid blocking LSP requests
6. THE implementation SHALL NOT perform blocking filesystem I/O or subprocess calls while holding the WorldState lock

### Requirement 14: Timeline Integration

**User Story:** As an R developer, I want package loading to integrate with the existing scope timeline, so that position-aware resolution works correctly.

#### Acceptance Criteria

1. THE Scope_Artifacts SHALL include a new `PackageLoad` event type in the timeline
2. WHEN computing scope at a position, THE Scope_Resolver SHALL process `PackageLoad` events in document order
3. THE `PackageLoad` event SHALL store the package name, load position (line, column), and whether it's a global or function-local load
4. WHEN a `PackageLoad` event is inside a function scope, THE Scope_Resolver SHALL only apply it within that function
5. THE interface hash computation SHALL include loaded packages to trigger proper cache invalidation

### Requirement 15: Error Handling

**User Story:** As an R developer, I want clear feedback when package loading fails, so that I can fix issues in my code.

#### Acceptance Criteria

1. WHEN a library() call references a non-installed package, THE Diagnostic_Engine SHALL emit a warning diagnostic at the call site
2. WHEN R subprocess fails to return package exports, THE Package_Resolver SHALL fall back to NAMESPACE parsing
3. WHEN package resolution fails completely, THE LSP SHALL log the error and continue without blocking other features
4. THE Diagnostic_Engine SHALL support configurable severity for package-related diagnostics via `packages.missingPackageSeverity`

### Requirement 16: Documentation

**User Story:** As an R developer or contributor, I want comprehensive documentation of package function awareness behavior, so that I can effectively use and understand the feature.

#### Acceptance Criteria

1. THE README.md SHALL document how package function awareness works
2. THE README.md SHALL document the base package handling (always available, queried from R or hardcoded fallback)
3. THE README.md SHALL document all configuration options related to packages
4. THE README.md SHALL document the meta-package special handling (tidyverse, tidymodels)
5. THE README.md SHALL document how package loading integrates with cross-file scope resolution
