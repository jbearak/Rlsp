# Implementation Plan: Package Function Awareness

## Overview

This implementation plan breaks down the package function awareness feature into discrete coding tasks. The feature enables Rlsp to recognize functions exported by R packages loaded via `library()`, `require()`, or `loadNamespace()` calls.

## Tasks

- [x] 1. Implement R Subprocess Interface
  - [x] 1.1 Create RSubprocess struct and basic infrastructure
    - Create `crates/rlsp/src/r_subprocess.rs` module
    - Implement `RSubprocess::new(r_path: Option<PathBuf>)` constructor
    - Implement R path discovery (check PATH, common locations)
    - Add async subprocess execution helper using `tokio::process::Command`
    - _Requirements: 7.1, 7.2_

  - [x] 1.2 Implement library path discovery
    - Implement `get_lib_paths()` method that calls `.libPaths()` in R
    - Parse R output to extract library paths
    - Add fallback to standard platform-specific paths
    - _Requirements: 7.1, 7.2_

  - [x] 1.3 Implement base package discovery
    - Implement `get_base_packages()` method that calls `.packages()` in R
    - Add hardcoded fallback list: base, methods, utils, grDevices, graphics, stats, datasets
    - _Requirements: 6.1, 6.2_

  - [x] 1.4 Implement package export query
    - Implement `get_package_exports(package: &str)` method
    - Call `getNamespaceExports(asNamespace("pkg"))` in R
    - Parse output to extract export names
    - _Requirements: 3.1_

  - [x] 1.5 Implement package depends query
    - Implement `get_package_depends(package: &str)` method
    - Call R to read DESCRIPTION and extract Depends field
    - Parse output to extract package names
    - _Requirements: 4.1_

  - [x] 1.6 Write unit tests for R subprocess
    - Test R path discovery
    - Test output parsing
    - Test error handling when R is unavailable
    - _Requirements: 15.2, 15.3_

- [x] 2. Implement NAMESPACE Parser (Fallback)
  - [x] 2.1 Create NAMESPACE parsing module
    - Create `crates/rlsp/src/namespace_parser.rs` module
    - Implement `parse_namespace_exports(path: &Path) -> Result<Vec<String>>`
    - Handle `export(name)` directives
    - Handle `exportPattern("pattern")` directives
    - Handle `S3method(generic, class)` directives
    - _Requirements: 3.2, 3.3, 3.4, 3.5_

  - [x] 2.2 Create DESCRIPTION parsing
    - Implement `parse_description_depends(path: &Path) -> Result<Vec<String>>`
    - Parse Depends field from DESCRIPTION file
    - Handle version constraints (e.g., `R (>= 3.5)`)
    - _Requirements: 4.1_

  - [x] 2.3 Write property test for NAMESPACE parsing round-trip
    - **Property 5: Package Export Round-Trip**
    - **Validates: Requirements 3.1, 3.2, 3.3, 3.4, 3.5**

  - [x] 2.4 Write unit tests for NAMESPACE parser
    - Test export() directive parsing
    - Test exportPattern() directive parsing
    - Test S3method() directive parsing
    - Test malformed file handling
    - _Requirements: 3.3, 3.4, 3.5, 15.3_

- [x] 3. Implement PackageLibrary
  - [x] 3.1 Create PackageLibrary struct
    - Create `crates/rlsp/src/package_library.rs` module
    - Define `PackageInfo` struct with name, exports, depends, is_meta_package, attached_packages
    - Define `PackageLibrary` struct with lib_paths, packages cache, base_packages, base_exports
    - Use `RwLock<HashMap<String, Arc<PackageInfo>>>` for thread-safe caching
    - _Requirements: 13.1, 13.4_

  - [x] 3.2 Implement initialization
    - Implement `PackageLibrary::new(r_subprocess: Option<RSubprocess>)` 
    - Query R for lib_paths and base_packages at init
    - Fall back to hardcoded values if R unavailable
    - Pre-populate base_exports from base packages
    - _Requirements: 6.1, 6.2, 6.3, 7.1_

  - [x] 3.3 Implement package loading
    - Implement `get_package(&self, name: &str) -> Option<Arc<PackageInfo>>`
    - Check cache first, then query R subprocess
    - Fall back to NAMESPACE parsing if subprocess fails
    - Handle meta-packages (tidyverse, tidymodels) with hardcoded attached packages
    - _Requirements: 3.1, 3.2, 4.3, 4.4_

  - [x] 3.4 Implement transitive dependency loading
    - Implement `get_all_exports(&self, name: &str) -> HashSet<String>`
    - Load package and all Depends packages
    - Track visited packages to handle circular dependencies
    - _Requirements: 4.2, 4.5_

  - [x] 3.5 Implement base export checking
    - Implement `is_base_export(&self, symbol: &str) -> bool`
    - Check against pre-populated base_exports set
    - _Requirements: 6.3, 6.4_

  - [x] 3.6 Write property test for cache idempotence
    - **Property 15: Cache Idempotence**
    - **Validates: Requirements 3.7, 13.1, 13.2**

  - [x] 3.7 Write property test for circular dependency handling
    - **Property 7: Circular Dependency Handling**
    - **Validates: Requirement 4.5**

- [x] 4. Checkpoint - Core package infrastructure complete
  - Ensure all tests pass, ask the user if questions arise.

- [x] 5. Implement Library Call Detection
  - [x] 5.1 Create library call detection in source_detect.rs
    - Add `LibraryCall` struct with package, line, column, function_scope fields
    - Implement `detect_library_calls(tree: &Tree, content: &str) -> Vec<LibraryCall>`
    - Detect `library()`, `require()`, `loadNamespace()` calls
    - Extract package name from first argument (bare identifier or string literal)
    - Skip calls with `character.only = TRUE`
    - Skip calls with variable/expression package names
    - _Requirements: 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 1.7_

  - [x] 5.2 Integrate with metadata extraction
    - Add `library_calls: Vec<LibraryCall>` to `CrossFileMetadata`
    - Call `detect_library_calls` in `extract_metadata()`
    - _Requirements: 1.8_

  - [x] 5.3 Write property test for library call detection
    - **Property 1: Library Call Detection Completeness**
    - **Validates: Requirements 1.1, 1.4, 1.5, 1.8**

  - [x] 5.4 Write property test for dynamic package exclusion
    - **Property 2: Dynamic Package Name Exclusion**
    - **Validates: Requirements 1.6, 1.7**

- [x] 6. Integrate Package Loading into Scope Timeline
  - [x] 6.1 Add PackageLoad event to ScopeEvent enum
    - Add `PackageLoad { line, column, package, function_scope }` variant to `ScopeEvent`
    - _Requirements: 14.1, 14.3_

  - [x] 6.2 Update compute_artifacts to include PackageLoad events
    - In `compute_artifacts()`, detect library calls and add PackageLoad events to timeline
    - Determine function_scope for each library call using the function scope tree
    - Sort timeline by position
    - _Requirements: 14.2, 14.4_

  - [x] 6.3 Update interface hash to include packages
    - Include loaded packages in the interface hash computation
    - Ensure cache invalidation when packages change
    - _Requirements: 14.5_

  - [x] 6.4 Write property test for position-aware package scope
    - **Property 3: Position-Aware Package Scope**
    - **Validates: Requirements 2.1, 2.2, 2.3**

  - [x] 6.5 Write property test for function-scoped package loading
    - **Property 4: Function-Scoped Package Loading**
    - **Validates: Requirements 2.4, 2.5**

- [x] 7. Implement Package-Aware Scope Resolution
  - [x] 7.1 Add PackageLibrary to WorldState
    - Add `package_library: PackageLibrary` field to `WorldState`
    - Initialize in `WorldState::new()`
    - _Requirements: 13.4_

  - [x] 7.2 Update scope_at_position to process PackageLoad events
    - In `scope_at_position()`, process PackageLoad events
    - For each PackageLoad before the query position, add package exports to scope
    - Respect function_scope (only add if query is inside same function)
    - _Requirements: 2.1, 2.2, 2.4, 2.5_

  - [x] 7.3 Add base package exports to scope
    - Always include base package exports in scope (no position check)
    - _Requirements: 6.3, 6.4_

  - [x] 7.4 Write property test for base package availability
    - **Property 10: Base Package Universal Availability**
    - **Validates: Requirements 6.2, 6.3, 6.4**

- [x] 8. Checkpoint - Scope resolution with packages complete
  - Ensure all tests pass, ask the user if questions arise.

- [x] 9. Implement Cross-File Package Propagation
  - [x] 9.1 Update cross-file scope resolution
    - In `scope_at_position_with_graph_recursive()`, propagate PackageLoad events from parent files
    - Only propagate packages loaded before the source() call site
    - _Requirements: 5.1, 5.2, 5.3_

  - [x] 9.2 Ensure forward-only propagation
    - Do not propagate packages from child files back to parents
    - _Requirements: 5.4_

  - [x] 9.3 Write property test for cross-file package propagation
    - **Property 8: Cross-File Package Propagation**
    - **Validates: Requirements 5.1, 5.2, 5.3**

  - [x] 9.4 Write property test for forward-only propagation
    - **Property 9: Forward-Only Package Propagation**
    - **Validates: Requirement 5.4**

- [x] 10. Update Diagnostics for Package Awareness
  - [x] 10.1 Update is_package_export function
    - Modify `is_package_export()` in handlers.rs to use PackageLibrary
    - Check position-aware loaded packages, not just document-level
    - _Requirements: 8.1, 8.2_

  - [x] 10.2 Update collect_undefined_variables_position_aware
    - Get loaded packages at each usage position from scope resolution
    - Check package exports for each symbol
    - _Requirements: 8.1, 8.3, 8.4_

  - [x] 10.3 Add diagnostic for missing packages
    - Emit warning when library() references non-installed package
    - _Requirements: 15.1_

  - [x] 10.4 Write property test for diagnostic suppression
    - **Property 11: Package Export Diagnostic Suppression**
    - **Validates: Requirements 8.1, 8.2**

  - [x] 10.5 Write property test for pre-load diagnostics
    - **Property 12: Pre-Load Diagnostic Emission**
    - **Validates: Requirement 8.3**

  - [x] 10.6 Write property test for non-export diagnostics
    - **Property 13: Non-Export Diagnostic Emission**
    - **Validates: Requirement 8.4**

- [x] 11. Update Completions for Package Functions
  - [x] 11.1 Add package exports to completions
    - In `completion()`, get loaded packages at cursor position
    - Add exports from loaded packages to completion items
    - Include package name in detail field (e.g., "{dplyr}")
    - _Requirements: 9.1, 9.2_

  - [x] 11.2 Handle completion precedence
    - Local definitions > package exports > cross-file symbols
    - _Requirements: 9.4, 9.5_

  - [x] 11.3 Handle duplicate exports
    - When multiple packages export same symbol, show all with attribution
    - _Requirements: 9.3_

  - [x] 11.4 Write property test for package completions
    - **Property 14: Package Completion Inclusion**
    - **Validates: Requirements 9.1, 9.2**

- [x] 12. Update Hover for Package Functions
  - [x] 12.1 Add package info to hover
    - In `hover()`, check if symbol is from a loaded package
    - Display package name in hover content
    - _Requirements: 10.1_

  - [x] 12.2 Get function signature from R help
    - Use existing help system to get function signature
    - _Requirements: 10.2_

  - [x] 12.3 Handle shadowing
    - Show local definition if it shadows package function
    - _Requirements: 10.4_

- [x] 13. Update Go-to-Definition for Package Functions
  - [x] 13.1 Handle package function definitions
    - In `definition()`, check if symbol is from a loaded package
    - Navigate to package source if available
    - _Requirements: 11.1, 11.2_

  - [x] 13.2 Handle shadowing
    - Navigate to local definition if it shadows package function
    - _Requirements: 11.3_

- [x] 14. Add Configuration Options
  - [x] 14.1 Add package configuration to CrossFileConfig
    - Add `packages_enabled: bool` (default: true)
    - Add `packages_additional_library_paths: Vec<PathBuf>`
    - Add `packages_r_path: Option<PathBuf>`
`    - Add `packages_missing_package_severity: DiagnosticSeverity`
`    - _Requirements: 12.1, 12.2, 12.3, 15.4_

  - [x] 14.2 Parse configuration from initialization options
    - Update `from_initialization_options()` to parse package settings
    - _Requirements: 12.4_

- [x] 15. Update Documentation
  - [x] 15.1 Update README.md with package awareness documentation
    - Document how package function awareness works
    - Document base package handling
    - Document configuration options
    - Document meta-package handling
    - Document cross-file integration
    - _Requirements: 16.1, 16.2, 16.3, 16.4, 16.5_

- [x] 16. Final Checkpoint
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation
- Property tests validate universal correctness properties
- Unit tests validate specific examples and edge cases
- All tasks are required for comprehensive implementation
