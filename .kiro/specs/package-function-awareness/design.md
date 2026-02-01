# Design: Package Function Awareness

## Overview

This document describes the technical design for adding package function awareness to Rlsp. The feature enables the LSP to recognize symbols (functions, variables, and datasets) exported by R packages loaded via `library()`, `require()`, or `loadNamespace()` calls, suppressing false-positive "undefined variable" warnings and providing completions, hover, and go-to-definition for package exports.

The design integrates with the existing cross-file scope resolution system, adding package loading as a new type of scope event in the timeline. Package exports are resolved via R subprocess calls with NAMESPACE file parsing as a fallback.

## Architecture

The package function awareness feature consists of four main components:

1. **Package Library** - Manages installed packages, their exports, and caching
2. **Library Call Detection** - Detects `library()`, `require()`, `loadNamespace()` calls in R code
3. **Package Scope Integration** - Integrates package loading into the scope timeline
4. **R Subprocess Interface** - Queries R for package exports and library paths

```text
┌─────────────────────────────────────────────────────────────────────┐
│                           WorldState                                 │
├─────────────────────────────────────────────────────────────────────┤
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────────┐ │
│  │  PackageLibrary │  │  RSubprocess    │  │  CrossFileState     │ │
│  │  ─────────────  │  │  ───────────    │  │  ──────────────     │ │
│  │  - base_pkgs    │◄─┤  - get_exports  │  │  - dependency_graph │ │
│  │  - pkg_cache    │  │  - lib_paths    │  │  - scope_cache      │ │
│  │  - lib_paths    │  │  - base_pkgs    │  │  - timeline events  │ │
│  └────────┬────────┘  └─────────────────┘  └──────────┬──────────┘ │
│           │                                           │             │
│           ▼                                           ▼             │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │                    Scope Resolution                          │   │
│  │  ─────────────────────────────────────────────────────────  │   │
│  │  - process PackageLoad events in timeline                   │   │
│  │  - check package exports for symbol resolution              │   │
│  │  - propagate packages through source() chains               │   │
│  └─────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────┘
```

## Components and Interfaces

### PackageLibrary

Manages the collection of installed R packages and their cached exports.

```rust
/// Cached package information
pub struct PackageInfo {
    /// Package name
    pub name: String,
    /// Exported symbols (functions, variables, and datasets)
    pub exports: HashSet<String>,
    /// Packages from Depends field
    pub depends: Vec<String>,
    /// Whether this is a meta-package with special handling
    pub is_meta_package: bool,
    /// Packages attached by meta-package
    pub attached_packages: Vec<String>,
    /// Lazy-loaded dataset names
    pub lazy_data: Vec<String>,
}

/// Package library manager
pub struct PackageLibrary {
    /// Library paths (from R or configuration)
    lib_paths: Vec<PathBuf>,
    /// Cached package information (lazy-loaded)
    packages: RwLock<HashMap<String, Arc<PackageInfo>>>,
    /// Base packages (always available)
    base_packages: HashSet<String>,
    /// Base package exports (combined)
    base_exports: HashSet<String>,
    /// R subprocess interface
    r_subprocess: Option<RSubprocess>,
}

impl PackageLibrary {
    /// Initialize with R subprocess query or fallback
    pub async fn new(r_path: Option<PathBuf>) -> Self;
    
    /// Get package info, loading from cache or R subprocess
    pub async fn get_package(&self, name: &str) -> Option<Arc<PackageInfo>>;
    
    /// Check if a symbol is exported by a package
    pub fn is_package_export(&self, symbol: &str, package: &str) -> bool;
    
    /// Check if a symbol is in base packages
    pub fn is_base_export(&self, symbol: &str) -> bool;
    
    /// Get all exports for a package including Depends
    pub async fn get_all_exports(&self, name: &str) -> HashSet<String>;
    
    /// Invalidate cache for a package
    pub fn invalidate(&self, name: &str);
}
```

### Library Call Detection

Extends the existing source detection to also detect library/require calls.

```rust
/// Detected library/require/loadNamespace call
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryCall {
    /// Package name (if statically determinable)
    pub package: String,
    /// 0-based line of the call
    pub line: u32,
    /// 0-based UTF-16 column of the call end position
    pub column: u32,
    /// Whether this is inside a function scope
    pub function_scope: Option<FunctionScopeInterval>,
}

/// Detect library(), require(), loadNamespace() calls in R code
pub fn detect_library_calls(tree: &Tree, content: &str) -> Vec<LibraryCall>;
```

### Scope Event Extension

Extends the existing `ScopeEvent` enum to include package loading.

```rust
/// Extended scope event (in scope.rs)
pub enum ScopeEvent {
    // ... existing variants ...
    
    /// A package load that introduces symbols from a package
    PackageLoad {
        line: u32,
        column: u32,
        /// Package name
        package: String,
        /// Function scope if inside a function (None = global)
        function_scope: Option<FunctionScopeInterval>,
    },
}
```

### R Subprocess Interface

Handles communication with R for package information.

```rust
/// R subprocess interface for package queries
pub struct RSubprocess {
    /// Path to R executable
    r_path: PathBuf,
}

impl RSubprocess {
    /// Create new subprocess interface
    pub fn new(r_path: PathBuf) -> Self;
    
    /// Get library paths from R
    pub async fn get_lib_paths(&self) -> Result<Vec<PathBuf>>;
    
    /// Get base/startup packages from R
    pub async fn get_base_packages(&self) -> Result<Vec<String>>;
    
    /// Get exports for a package
    pub async fn get_package_exports(&self, package: &str) -> Result<Vec<String>>;
    
    /// Get package DESCRIPTION info (Depends field)
    pub async fn get_package_depends(&self, package: &str) -> Result<Vec<String>>;
}
```

### NAMESPACE Parser (Fallback)

Parses NAMESPACE files when R subprocess is unavailable.

```rust
/// Parse NAMESPACE file for exports
pub fn parse_namespace_exports(namespace_path: &Path) -> Result<Vec<String>>;

/// Parse DESCRIPTION file for Depends
pub fn parse_description_depends(description_path: &Path) -> Result<Vec<String>>;
```

## Data Models

### Package Loading Timeline

Package loads are tracked in the scope timeline alongside definitions and source() calls:

```text
Timeline for file.R:
  Line 1: Def { x <- 1 }
  Line 2: PackageLoad { dplyr, global }
  Line 3: Source { utils.R }
  Line 5: PackageLoad { ggplot2, global }
  Line 10: FunctionScope { my_func, lines 10-20 }
  Line 12: PackageLoad { stringr, function_scope: my_func }
```

### Cross-File Package Propagation

When resolving scope for a child file sourced by a parent:

```text
parent.R:
  library(dplyr)      # Line 1
  source("child.R")   # Line 5
  library(ggplot2)    # Line 10

child.R (sourced at line 5):
  # Has access to: dplyr exports (loaded before source())
  # Does NOT have: ggplot2 exports (loaded after source())
```

### Meta-Package Expansion

```rust
const TIDYVERSE_PACKAGES: &[&str] = &[
    "dplyr", "readr", "forcats", "stringr", "ggplot2",
    "tibble", "lubridate", "tidyr", "purrr"
];

const TIDYMODELS_PACKAGES: &[&str] = &[
    "broom", "dials", "dplyr", "ggplot2", "infer", "modeldata",
    "parsnip", "purrr", "recipes", "rsample", "tibble", "tidyr",
    "tune", "workflows", "workflowsets", "yardstick"
];
```



## Correctness Properties

*A property is a characteristic or behavior that should hold true across all valid executions of a system—essentially, a formal statement about what the system should do. Properties serve as the bridge between human-readable specifications and machine-verifiable correctness guarantees.*

Based on the prework analysis, the following properties capture the core correctness requirements:

### Property 1: Library Call Detection Completeness

*For any* R source file containing `library()`, `require()`, or `loadNamespace()` calls with static string package names, the Library_Call_Detector SHALL detect all such calls and return them in document order with correct package names and positions.

**Validates: Requirements 1.1, 1.4, 1.5, 1.8**

### Property 2: Dynamic Package Name Exclusion

*For any* R source file containing library calls with variable or expression package names (including `character.only = TRUE`), the Library_Call_Detector SHALL NOT include those calls in the detected results.

**Validates: Requirements 1.6, 1.7**

### Property 3: Position-Aware Package Scope

*For any* R source file with a library() call at position P, and any symbol S exported by that package:
- Scope resolution at any position before P SHALL NOT include S
- Scope resolution at any position after P SHALL include S

**Validates: Requirements 2.1, 2.2, 2.3**

### Property 4: Function-Scoped Package Loading

*For any* R source file with a library() call inside a function body, the package exports SHALL only be available within that function's scope, not at the global level or in other functions.

**Validates: Requirements 2.4, 2.5**

### Property 5: Package Export Round-Trip

*For any* installed R package with a valid NAMESPACE file, the exports obtained via R subprocess (`getNamespaceExports()`) SHALL match the exports obtained by parsing the NAMESPACE file directly.

**Validates: Requirements 3.1, 3.2, 3.3, 3.4, 3.5**

### Property 6: Transitive Dependency Loading

*For any* package P with packages D1, D2, ... in its Depends field, loading P SHALL make available all exports from P, D1, D2, ... at the same position.

**Validates: Requirements 4.1, 4.2**

### Property 7: Circular Dependency Handling

*For any* set of packages with circular dependencies in their Depends fields, the Package_Resolver SHALL terminate and return exports without infinite loops.

**Validates: Requirement 4.5**

### Property 8: Cross-File Package Propagation

*For any* parent file that loads package P before a source() call to child file C, scope resolution in C SHALL include exports from P.

**Validates: Requirements 5.1, 5.2, 5.3**

### Property 9: Forward-Only Package Propagation

*For any* child file C that loads package P, scope resolution in the parent file SHALL NOT include exports from P (packages do not propagate backward).

**Validates: Requirement 5.4**

### Property 10: Base Package Universal Availability

*For any* R source file and any position within that file, exports from base packages (base, methods, utils, grDevices, graphics, stats, datasets) SHALL be available in scope.

**Validates: Requirements 6.2, 6.3, 6.4**

### Property 11: Package Export Diagnostic Suppression

*For any* symbol S that is exported by a package P loaded before position X, the Diagnostic_Engine SHALL NOT emit an "undefined variable" warning for S at position X.

**Validates: Requirements 8.1, 8.2**

### Property 12: Pre-Load Diagnostic Emission

*For any* symbol S that is exported by a package P loaded at position X, the Diagnostic_Engine SHALL emit an "undefined variable" warning for S at any position before X.

**Validates: Requirement 8.3**

### Property 13: Non-Export Diagnostic Emission

*For any* symbol S that is NOT exported by any loaded package at position X, the Diagnostic_Engine SHALL emit an "undefined variable" warning for S at position X.

**Validates: Requirement 8.4**

### Property 14: Package Completion Inclusion

*For any* position X after a library(P) call, completions at X SHALL include all exports from package P with package attribution.

**Validates: Requirements 9.1, 9.2**

### Property 15: Cache Idempotence

*For any* package P, repeated calls to get_package_exports(P) SHALL return identical results (cache consistency).

**Validates: Requirements 3.7, 13.1, 13.2**


## Error Handling

### R Subprocess Failures

When R subprocess calls fail:
1. Log the error with context (package name, operation attempted)
2. Fall back to NAMESPACE file parsing for exports
3. Fall back to hardcoded base packages for initialization
4. Continue LSP operation without blocking

### Invalid Package Names

When a library() call references a non-installed package:
1. Emit a warning diagnostic at the call site
2. Do not add any exports to scope
3. Continue processing other library calls

### Malformed NAMESPACE Files

When NAMESPACE parsing fails:
1. Log the parsing error
2. Treat the package as having no exports
3. Emit a warning diagnostic if the package was explicitly loaded

### Circular Dependencies

When circular dependencies are detected in Depends chains:
1. Track visited packages during traversal
2. Skip already-visited packages
3. Log a warning about the circular dependency
4. Return partial results (exports from non-circular packages)

## Testing Strategy

### Unit Tests

Unit tests should cover:
- Library call detection with various syntax forms
- NAMESPACE file parsing with different directive types
- DESCRIPTION file parsing for Depends field
- Meta-package expansion (tidyverse, tidymodels)
- Position comparison for scope resolution

### Property-Based Tests

Property-based tests should use a property testing library (proptest) with minimum 100 iterations per test. Each test should be tagged with the property it validates.

**Test Categories:**

1. **Library Call Detection Tests**
   - Generate R code with random library/require/loadNamespace calls
   - Verify detection completeness and ordering
   - Verify dynamic package names are excluded

2. **Scope Resolution Tests**
   - Generate files with library calls at random positions
   - Verify position-aware availability of exports
   - Verify function-scoped loading

3. **Cross-File Tests**
   - Generate parent/child file pairs with library calls
   - Verify forward propagation through source() chains
   - Verify no backward propagation

4. **Diagnostic Tests**
   - Generate code using package functions at various positions
   - Verify correct diagnostic emission/suppression

### Integration Tests

Integration tests should cover:
- End-to-end diagnostic generation with real packages
- Completion responses with package exports
- Hover information for package functions
- Cross-file scope resolution with packages

### Test Configuration

```rust
// Property test configuration
proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]
    
    // Feature: package-function-awareness, Property 1: Library Call Detection Completeness
    #[test]
    fn prop_library_call_detection(code in r_code_with_library_calls()) {
        // ...
    }
}
```
