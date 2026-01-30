# Design Document: Cross-File Debugging

## Overview

This design provides a systematic approach to debugging and fixing the cross-file awareness feature in Rlsp. The feature has been implemented but is not working in practice - symbols from sourced files are not being recognized, and backward directives are reporting "parent file not found" errors.

The debugging strategy follows a layered approach:
1. Add comprehensive logging to trace execution flow
2. Create test cases that reproduce real-world failures
3. Verify each component in isolation (metadata extraction, dependency graph, path resolution, scope resolution)
4. Verify integration points between components
5. Identify root causes through systematic analysis
6. Implement fixes with regression testing

## Architecture

### Debugging Infrastructure

The debugging infrastructure consists of three main components:

1. **Logging System**: Structured logging at key points in the cross-file system
   - Uses Rust's `log` crate with trace/info/warn/error levels
   - Logs include contextual information (file paths, positions, symbol counts)
   - Logs are emitted at component boundaries for traceability

2. **Test Harness**: Integration tests that reproduce real-world scenarios
   - Test cases match actual failure scenarios (validation_functions/collate.r, @lsp-run-by directive)
   - Tests verify end-to-end behavior (symbol availability, diagnostic absence)
   - Tests can be run in isolation or as a suite

3. **Verification Tools**: Helper functions to inspect system state
   - Functions to query dependency graph state
   - Functions to dump cache contents
   - Functions to trace scope resolution paths

### Component Verification Strategy

Each component will be verified in isolation before testing integration:

```
Metadata Extraction → Dependency Graph → Path Resolution → Scope Resolution → LSP Handlers
       ↓                    ↓                  ↓                  ↓                ↓
   Verify source()      Verify edges      Verify paths      Verify symbols    Verify calls
   detection            are created       resolve           are returned      are made
```

### Integration Point Verification

Integration points between components will be verified:

1. **Metadata → Dependency Graph**: Verify extracted metadata flows into graph
2. **Dependency Graph → Cache**: Verify graph updates trigger cache invalidation
3. **Content Provider → Scope Resolution**: Verify file content is supplied correctly
4. **Scope Resolution → LSP Handlers**: Verify handlers use cross-file scope
5. **Revalidation → Diagnostics**: Verify affected files get updated diagnostics

## Components and Interfaces

### Logging Infrastructure

**Module**: Throughout `crates/rlsp/src/cross_file/`

**Key Logging Points**:

```rust
// In metadata extraction (source_detect.rs, directive.rs)
log::trace!("Extracting metadata for file: {}", uri);
log::trace!("Found source() call: {} at line {}", path, line);
log::trace!("Parsed backward directive: {:?}", directive);

// In dependency graph (dependency.rs)
log::trace!("Adding edge: {} -> {} at line {}", parent, child, line);
log::trace!("Dependency graph now has {} edges", edge_count);

// In path resolution (path_resolve.rs)
log::trace!("Resolving path '{}' relative to '{}'", path, base);
log::trace!("Resolved to canonical path: {}", canonical);
log::warn!("Failed to resolve path '{}': {}", path, error);

// In scope resolution (scope.rs)
log::trace!("Resolving scope at {}:{}", file, position);
log::trace!("Found {} symbols in scope", symbol_count);
log::trace!("Traversing to sourced file: {}", sourced_file);

// In LSP handlers (handlers.rs)
log::trace!("Completion request at {}:{}", uri, position);
log::trace!("Using cross-file scope resolution: {}", enabled);
```

**Interface**:
- No new interfaces needed - uses existing `log` crate
- Logging controlled by `RUST_LOG` environment variable
- Format: `RUST_LOG=rlsp=trace` for maximum verbosity



### Test Harness

**Module**: `crates/rlsp/src/cross_file/integration_tests.rs` (new file)

**Test Structure**:

```rust
#[cfg(test)]
mod integration_tests {
    use super::*;
    
    // Test case 1: source() call with relative path
    #[test]
    fn test_source_call_symbol_resolution() {
        // Setup: Create two files, A sources B, B defines function
        // Action: Request completion in A after source() call
        // Assert: Function from B appears in completions
    }
    
    // Test case 2: backward directive with ../ path
    #[test]
    fn test_backward_directive_parent_resolution() {
        // Setup: Create file with @lsp-run-by: ../parent.r
        // Action: Extract metadata and build dependency graph
        // Assert: No "parent file not found" error, edge exists
    }
    
    // Test case 3: validation_functions scenario
    #[test]
    fn test_validation_functions_scenario() {
        // Setup: Recreate validation_functions/collate.r structure
        // Action: Request diagnostics for collate.r
        // Assert: get_colnames() is not marked as undefined
    }
}
```

**Interface**:
- Standard Rust test functions using `#[test]` attribute
- Helper functions to create test workspace with files
- Helper functions to simulate LSP requests
- Assertions on LSP responses (completions, diagnostics, etc.)

### Metadata Extraction Verification

**Module**: `crates/rlsp/src/cross_file/source_detect.rs`, `directive.rs`

**Verification Approach**:

1. Add logging before and after tree-sitter parsing
2. Log each detected source() call with path and position
3. Log each parsed directive with type and parameters
4. Add unit tests for edge cases (paths with quotes, optional colons, ../ paths)

**Enhanced Functions**:

```rust
// In source_detect.rs
pub fn detect_source_calls(uri: &Url, content: &str) -> Result<Vec<ForwardSource>> {
    log::trace!("Detecting source() calls in {}", uri);
    let tree = parse_r_code(content)?;
    let calls = extract_calls_from_tree(&tree, content);
    log::trace!("Found {} source() calls", calls.len());
    for call in &calls {
        log::trace!("  source('{}') at line {}", call.path, call.line);
    }
    Ok(calls)
}

// In directive.rs
pub fn parse_directives(uri: &Url, content: &str) -> Vec<BackwardDirective> {
    log::trace!("Parsing directives in {}", uri);
    let directives = parse_with_regex(content);
    log::trace!("Found {} directives", directives.len());
    for directive in &directives {
        log::trace!("  {:?}", directive);
    }
    directives
}
```

### Dependency Graph Verification

**Module**: `crates/rlsp/src/cross_file/dependency.rs`

**Verification Approach**:

1. Add logging when edges are added/removed
2. Add function to dump graph state for inspection
3. Add unit tests for edge deduplication and conflict resolution
4. Verify edges are stored with correct call site positions

**Enhanced Functions**:

```rust
impl DependencyGraph {
    pub fn add_edge(&mut self, edge: DependencyEdge) {
        log::trace!("Adding edge: {} -> {} at line {}", 
                   edge.parent, edge.child, edge.call_site.line);
        // ... existing logic ...
        log::trace!("Graph now has {} edges", self.edges.len());
    }
    
    pub fn dump_state(&self) -> String {
        // For debugging: return human-readable graph state
        let mut output = String::new();
        output.push_str(&format!("Dependency Graph ({} edges):\n", self.edges.len()));
        for edge in &self.edges {
            output.push_str(&format!("  {} -> {} (line {})\n", 
                                    edge.parent, edge.child, edge.call_site.line));
        }
        output
    }
}
```

### Path Resolution Verification

**Module**: `crates/rlsp/src/cross_file/path_resolve.rs`

**Verification Approach**:

1. Add logging for every path resolution attempt
2. Log the input path, base directory, and result (success or error)
3. Add unit tests for ../ paths, ./ paths, absolute paths
4. Verify working directory handling

**Enhanced Functions**:

```rust
pub fn resolve_path(path: &str, base_dir: &Path, working_dir: Option<&Path>) -> Result<PathBuf> {
    let base = working_dir.unwrap_or(base_dir);
    log::trace!("Resolving path '{}' relative to '{}'", path, base.display());
    
    let resolved = base.join(path);
    match resolved.canonicalize() {
        Ok(canonical) => {
            log::trace!("Resolved to: {}", canonical.display());
            Ok(canonical)
        }
        Err(e) => {
            log::warn!("Failed to resolve '{}' from '{}': {}", path, base.display(), e);
            Err(anyhow!("Path resolution failed for '{}': {}", path, e))
        }
    }
}
```

### Scope Resolution Verification

**Module**: `crates/rlsp/src/cross_file/scope.rs`

**Verification Approach**:

1. Add logging at entry and exit of scope resolution
2. Log each file traversed during resolution
3. Log symbols found at each step
4. Add tests for chained source() calls and cycle detection

**Enhanced Functions**:

```rust
pub fn scope_at_position(
    uri: &Url,
    position: Position,
    graph: &DependencyGraph,
    // ... other params
) -> Result<Vec<Symbol>> {
    log::trace!("Resolving scope at {}:{}", uri, position);
    
    let symbols = scope_at_position_recursive(uri, position, graph, /* ... */)?;
    
    log::trace!("Found {} symbols in scope", symbols.len());
    for symbol in &symbols {
        log::trace!("  {} from {}", symbol.name, symbol.source_file);
    }
    
    Ok(symbols)
}

fn scope_at_position_recursive(/* ... */) -> Result<Vec<Symbol>> {
    log::trace!("Traversing to file: {}", uri);
    // ... existing logic with logging at key points ...
}
```

### LSP Handler Integration Verification

**Module**: `crates/rlsp/src/handlers.rs`

**Verification Approach**:

1. Add logging at the start of each LSP handler
2. Log whether cross-file resolution is enabled
3. Log whether cross-file functions are being called
4. Verify handlers actually use the returned symbols

**Enhanced Handlers**:

```rust
pub async fn handle_completion(
    state: Arc<RwLock<WorldState>>,
    params: CompletionParams,
) -> Result<Option<CompletionList>> {
    log::trace!("Completion request at {}:{}", 
               params.text_document_position.text_document.uri, 
               params.text_document_position.position);
    
    let state = state.read().await;
    let cross_file_enabled = state.cross_file_config.enabled;
    log::trace!("Cross-file resolution enabled: {}", cross_file_enabled);
    
    if cross_file_enabled {
        log::trace!("Calling cross-file scope resolution");
        let symbols = scope_at_position(/* ... */)?;
        log::trace!("Got {} symbols from cross-file scope", symbols.len());
        // ... use symbols for completions ...
    }
    
    // ... rest of handler ...
}
```

### Configuration Verification

**Module**: `crates/rlsp/src/cross_file/config.rs`, `crates/rlsp/src/main.rs`

**Verification Approach**:

1. Log configuration at startup
2. Verify cross-file is enabled by default
3. Add tests for configuration parsing
4. Verify configuration is passed to all components

**Enhanced Initialization**:

```rust
pub fn initialize(params: InitializeParams) -> Result<InitializeResult> {
    let config = CrossFileConfig::from_initialization_options(&params.initialization_options);
    
    log::info!("Cross-file configuration:");
    log::info!("  enabled: {}", config.enabled);
    log::info!("  max_chain_depth: {}", config.max_chain_depth);
    log::info!("  diagnostic severities: {:?}", config.diagnostic_severities);
    
    // ... rest of initialization ...
}
```

## Data Models

### LogContext

A structure to carry contextual information for logging:

```rust
pub struct LogContext {
    pub file: String,
    pub operation: String,
    pub details: HashMap<String, String>,
}

impl LogContext {
    pub fn new(file: impl Into<String>, operation: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            operation: operation.into(),
            details: HashMap::new(),
        }
    }
    
    pub fn with_detail(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.details.insert(key.into(), value.into());
        self
    }
    
    pub fn log_trace(&self, message: &str) {
        log::trace!("[{}::{}] {} {:?}", self.file, self.operation, message, self.details);
    }
}
```

### TestWorkspace

A structure to help create test workspaces:

```rust
pub struct TestWorkspace {
    root: PathBuf,
    files: HashMap<String, String>,
}

impl TestWorkspace {
    pub fn new() -> Result<Self> {
        let root = tempdir()?.into_path();
        Ok(Self {
            root,
            files: HashMap::new(),
        })
    }
    
    pub fn add_file(&mut self, path: &str, content: &str) -> Result<Url> {
        let full_path = self.root.join(path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full_path, content)?;
        self.files.insert(path.to_string(), content.to_string());
        Ok(Url::from_file_path(&full_path).unwrap())
    }
    
    pub fn get_uri(&self, path: &str) -> Url {
        let full_path = self.root.join(path);
        Url::from_file_path(&full_path).unwrap()
    }
}
```

### VerificationReport

A structure to collect verification results:

```rust
pub struct VerificationReport {
    pub component: String,
    pub checks: Vec<VerificationCheck>,
}

pub struct VerificationCheck {
    pub name: String,
    pub passed: bool,
    pub details: String,
}

impl VerificationReport {
    pub fn new(component: impl Into<String>) -> Self {
        Self {
            component: component.into(),
            checks: Vec::new(),
        }
    }
    
    pub fn add_check(&mut self, name: impl Into<String>, passed: bool, details: impl Into<String>) {
        self.checks.push(VerificationCheck {
            name: name.into(),
            passed,
            details: details.into(),
        });
    }
    
    pub fn all_passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }
    
    pub fn summary(&self) -> String {
        let passed = self.checks.iter().filter(|c| c.passed).count();
        let total = self.checks.len();
        format!("{}: {}/{} checks passed", self.component, passed, total)
    }
}
```



## Correctness Properties

*A property is a characteristic or behavior that should hold true across all valid executions of a system—essentially, a formal statement about what the system should do. Properties serve as the bridge between human-readable specifications and machine-verifiable correctness guarantees.*

### Metadata Extraction Properties

**Property 1: Source call detection completeness**
*For any* R file containing source() calls, when metadata extraction runs, all source() calls should be detected with their correct file paths and line numbers.
**Validates: Requirements 2.1**

**Property 2: Directive parsing flexibility**
*For any* backward directive with optional colon and quotes (e.g., `@lsp-run-by ../file.r`, `@lsp-run-by: "../file.r"`), the parser should correctly extract the target file path regardless of syntax variation.
**Validates: Requirements 2.2**

**Property 3: Working directory extraction**
*For any* file containing working directory directives, metadata extraction should correctly extract the working directory path.
**Validates: Requirements 2.3**

**Property 4: Directive path resolution**
*For any* backward directive with a relative path, the path should be resolved relative to the directory containing the directive file.
**Validates: Requirements 2.4**

**Property 5: Metadata caching**
*For any* successful metadata extraction, the extracted metadata should be stored in the cache and retrievable.
**Validates: Requirements 2.5**

**Property 6: Extraction error resilience**
*For any* file that causes metadata extraction to fail, the system should log the error and continue operating without crashing.
**Validates: Requirements 2.6**

**Property 7: Metadata cache invalidation**
*For any* file modification, metadata should be re-extracted and the cache should be updated with the new metadata.
**Validates: Requirements 2.7**

### Dependency Graph Properties

**Property 8: Forward edge creation from source calls**
*For any* file with source() calls, after metadata extraction, the dependency graph should contain forward edges from the parent file to each sourced child file.
**Validates: Requirements 3.1**

**Property 9: Forward edge creation from directives**
*For any* file with backward directives, after metadata extraction, the dependency graph should contain forward edges from the referenced parent to the directive file.
**Validates: Requirements 3.2**

**Property 10: Parent query correctness**
*For any* file in the dependency graph, querying its parents should return exactly the set of files that have edges pointing to it.
**Validates: Requirements 3.3**

**Property 11: Child query correctness**
*For any* file in the dependency graph, querying its children should return exactly the set of files it has edges pointing to.
**Validates: Requirements 3.4**

**Property 12: Directive-AST conflict resolution**
*For any* scenario where both directives and AST source() calls create edges, the conflict resolution should follow the documented rules (directive with call site overrides AST at same call site, directive without call site suppresses all AST edges to that target).
**Validates: Requirements 3.5**

**Property 13: Call site UTF-16 encoding**
*For any* edge in the dependency graph, the call site position should be stored in UTF-16 column units.
**Validates: Requirements 3.6**

### Path Resolution Properties

**Property 14: Working directory path resolution**
*For any* relative path resolved with a working directory specified, the resolution should be relative to the working directory, not the file's directory.
**Validates: Requirements 4.1**

**Property 15: File directory path resolution**
*For any* relative path resolved without a working directory, the resolution should be relative to the file's directory.
**Validates: Requirements 4.2**

**Property 16: Parent directory navigation**
*For any* path containing ".." components, the path resolver should correctly navigate up the directory hierarchy.
**Validates: Requirements 4.3**

**Property 17: Absolute path handling**
*For any* absolute path, the path resolver should canonicalize it without changing its target location.
**Validates: Requirements 4.4**

**Property 18: Path normalization**
*For any* path containing "." or ".." components, the path resolver should normalize them to a canonical form.
**Validates: Requirements 4.6**

**Property 19: Cross-platform slash handling**
*For any* path using either forward slashes or backslashes, the path resolver should handle it correctly on all platforms.
**Validates: Requirements 4.7**

**Property 20: Directive base directory**
*For any* backward directive path resolution, the base directory should be the directory containing the directive file.
**Validates: Requirements 4.8**

### Scope Resolution Properties

**Property 21: Sourced symbol availability**
*For any* position in a file after a source() call, scope resolution should include symbols defined in the sourced file.
**Validates: Requirements 5.1**

**Property 22: Multiple source aggregation**
*For any* file with multiple source() calls, scope resolution should include symbols from all sourced files.
**Validates: Requirements 5.2**

**Property 23: Chain traversal with depth limit**
*For any* chain of source() calls, scope resolution should traverse the chain up to max_chain_depth and no further.
**Validates: Requirements 5.3**

**Property 24: Local symbol precedence**
*For any* symbol name that exists both locally and in a sourced file, scope resolution should return the local symbol.
**Validates: Requirements 5.4**

**Property 25: Cycle detection**
*For any* cyclic dependency in the source graph, scope resolution should detect the cycle and terminate traversal without infinite looping.
**Validates: Requirements 5.5**

**Property 26: Symbol structure completeness**
*For any* symbol returned by scope resolution, it should include the symbol name, type, and source file path.
**Validates: Requirements 5.6**

### Configuration Properties

**Property 27: Cross-file disable behavior**
*For any* LSP request when cross-file is disabled in configuration, cross-file scope resolution should not be performed.
**Validates: Requirements 8.2**

**Property 28: Chain depth limit enforcement**
*For any* source chain longer than the configured max_chain_depth, scope resolution should stop at the configured limit.
**Validates: Requirements 8.3**

**Property 29: Diagnostic severity configuration**
*For any* cross-file diagnostic generated, the severity level should match the configured severity for that diagnostic type.
**Validates: Requirements 8.4**

### Error Handling Properties

**Property 30: Error logging with context**
*For any* cross-file operation that fails, an error should be logged with full context including file paths and relevant details.
**Validates: Requirements 9.1**

**Property 31: Non-fatal error resilience**
*For any* non-fatal error in cross-file operations, the system should continue operating and handling subsequent requests.
**Validates: Requirements 9.6**

### Integration Properties

**Property 32: Metadata to graph flow**
*For any* metadata extraction, the extracted metadata should flow into the dependency graph and result in appropriate edge updates.
**Validates: Requirements 10.1**

**Property 33: Graph to cache invalidation**
*For any* dependency graph update that changes edges, affected cache entries should be invalidated.
**Validates: Requirements 10.2**

**Property 34: Content provider integration**
*For any* scope resolution request needing file content, the content provider should supply the content from either open documents or disk.
**Validates: Requirements 10.3**

**Property 35: Revalidation fanout**
*For any* revalidation trigger, the revalidation system should identify all affected files and publish updated diagnostics for them.
**Validates: Requirements 10.4**

**Property 36: Workspace indexing coverage**
*For any* workspace indexing run, closed files should be discovered and their symbols should be extracted and indexed.
**Validates: Requirements 10.5**

### Compatibility Property

**Property 37: Backward compatibility**
*For any* existing directive syntax or configuration option, fixes should maintain compatibility and not break existing usage.
**Validates: Requirements 11.6**



## Error Handling

### Error Categories

The debugging process will encounter several categories of errors:

1. **Path Resolution Errors**: Files not found, invalid paths, permission issues
2. **Parse Errors**: Invalid R syntax, malformed directives
3. **Graph Errors**: Cycles, missing nodes, invalid edges
4. **Cache Errors**: Serialization failures, corruption
5. **LSP Protocol Errors**: Invalid requests, malformed responses

### Error Handling Strategy

**Logging Requirements**:
- All errors must be logged with `log::warn!` or `log::error!`
- Error logs must include:
  - File path or URI where error occurred
  - Operation being performed
  - Error message and cause chain
  - Relevant context (line numbers, symbol names, etc.)

**Recovery Strategy**:
- **Non-fatal errors**: Log and continue with degraded functionality
  - Example: If one source() call fails to resolve, continue processing others
- **Fatal errors**: Log and return error to LSP client
  - Example: If WorldState lock is poisoned, cannot continue

**Error Propagation**:
```rust
// Use Result types throughout
pub fn extract_metadata(uri: &Url, content: &str) -> Result<CrossFileMetadata> {
    // ... operation ...
    match risky_operation() {
        Ok(result) => Ok(result),
        Err(e) => {
            log::warn!("Metadata extraction failed for {}: {}", uri, e);
            Err(anyhow!("Failed to extract metadata: {}", e))
        }
    }
}
```

**Specific Error Handling**:

1. **Path Resolution Failures**:
   ```rust
   match resolve_path(path, base_dir, working_dir) {
       Ok(canonical) => canonical,
       Err(e) => {
           log::warn!("Failed to resolve '{}' from '{}': {}", 
                     path, base_dir.display(), e);
           // Continue without this edge
           continue;
       }
   }
   ```

2. **Parse Failures**:
   ```rust
   match parse_r_code(content) {
       Ok(tree) => tree,
       Err(e) => {
           log::warn!("Parse failed for {}: {}", uri, e);
           // Return empty metadata rather than failing completely
           return Ok(CrossFileMetadata::empty());
       }
   }
   ```

3. **Cache Failures**:
   ```rust
   match cache.get(key) {
       Some(value) => value,
       None => {
           log::trace!("Cache miss for {}, recomputing", key);
           // Recompute rather than failing
           let value = compute_value()?;
           cache.insert(key, value.clone());
           value
       }
   }
   ```

### Diagnostic Error Messages

When cross-file operations fail, diagnostics should provide actionable information:

- **"Parent file not found"**: Include the attempted path and base directory
- **"Cycle detected"**: Show the cycle path (A → B → C → A)
- **"Max chain depth exceeded"**: Show the chain and the configured limit
- **"Symbol not found"**: Indicate whether cross-file resolution was attempted

## Testing Strategy

### Dual Testing Approach

The debugging and fixing process requires both unit tests and property-based tests:

**Unit Tests**: Verify specific scenarios and edge cases
- Test specific failure cases (validation_functions/collate.r, @lsp-run-by: ../oos.r)
- Test logging output for specific operations
- Test error handling for specific error conditions
- Test integration between specific components

**Property Tests**: Verify universal properties across all inputs
- Test that all source() calls are detected (Property 1)
- Test that all directive syntaxes parse correctly (Property 2)
- Test that path resolution works for all path types (Properties 14-20)
- Test that scope resolution maintains invariants (Properties 21-26)

### Property-Based Testing Configuration

**Library**: Use `proptest` crate (already used in Rlsp)

**Configuration**:
```rust
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 100,  // Minimum 100 iterations per property
        .. ProptestConfig::default()
    })]
    
    #[test]
    fn prop_source_call_detection(
        // Generate random R files with source() calls
        file_content in arb_r_file_with_sources()
    ) {
        // Feature: cross-file-debugging, Property 1: Source call detection completeness
        let metadata = extract_metadata(&file_content)?;
        // Assert all source() calls were detected
        prop_assert_eq!(count_source_calls(&file_content), metadata.forward_sources.len());
    }
}
```

**Test Tags**: Each property test must include a comment:
```rust
// Feature: cross-file-debugging, Property N: [property description]
```

### Test Organization

**Unit Tests** (`#[cfg(test)]` modules):
- `crates/rlsp/src/cross_file/source_detect.rs` - Test source() detection
- `crates/rlsp/src/cross_file/directive.rs` - Test directive parsing
- `crates/rlsp/src/cross_file/path_resolve.rs` - Test path resolution
- `crates/rlsp/src/cross_file/dependency.rs` - Test graph operations
- `crates/rlsp/src/cross_file/scope.rs` - Test scope resolution

**Integration Tests** (new file):
- `crates/rlsp/src/cross_file/integration_tests.rs` - End-to-end scenarios

**Property Tests** (existing file):
- `crates/rlsp/src/cross_file/property_tests.rs` - Universal properties

### Test Scenarios

**Scenario 1: validation_functions/collate.r**
```rust
#[test]
fn test_validation_functions_scenario() {
    let workspace = TestWorkspace::new()?;
    
    // Create get_colnames.r with function definition
    workspace.add_file(
        "validation_functions/get_colnames.r",
        "get_colnames <- function(df) { colnames(df) }"
    )?;
    
    // Create collate.r that sources get_colnames.r
    workspace.add_file(
        "validation_functions/collate.r",
        r#"
        source("validation_functions/get_colnames.r")
        result <- get_colnames(my_data)
        "#
    )?;
    
    // Request diagnostics for collate.r
    let diagnostics = get_diagnostics(&workspace, "validation_functions/collate.r")?;
    
    // Assert: get_colnames() should NOT be marked as undefined
    assert!(!diagnostics.iter().any(|d| d.message.contains("get_colnames") && d.message.contains("undefined")));
}
```

**Scenario 2: @lsp-run-by: ../oos.r**
```rust
#[test]
fn test_backward_directive_parent_resolution() {
    let workspace = TestWorkspace::new()?;
    
    // Create parent file
    workspace.add_file("oos.r", "# Parent file")?;
    
    // Create child file in subdirectory with backward directive
    workspace.add_file(
        "subdir/child.r",
        "# @lsp-run-by: ../oos.r\nmy_function <- function() {}"
    )?;
    
    // Extract metadata and build graph
    let metadata = extract_metadata_for_file(&workspace, "subdir/child.r")?;
    let graph = build_dependency_graph(&workspace)?;
    
    // Assert: No "parent file not found" error
    // Assert: Edge exists from oos.r to subdir/child.r
    let parents = graph.get_parents(&workspace.get_uri("subdir/child.r"))?;
    assert!(parents.contains(&workspace.get_uri("oos.r")));
}
```

**Scenario 3: Logging verification**
```rust
#[test]
fn test_metadata_extraction_logging() {
    // Setup logging capture
    let logs = capture_logs(|| {
        let workspace = TestWorkspace::new()?;
        workspace.add_file("test.r", r#"source("other.r")"#)?;
        extract_metadata_for_file(&workspace, "test.r")?;
    })?;
    
    // Assert: Logs contain expected messages
    assert!(logs.iter().any(|log| log.contains("Extracting metadata for file")));
    assert!(logs.iter().any(|log| log.contains("Found 1 source() calls")));
    assert!(logs.iter().any(|log| log.contains("source('other.r')")));
}
```

### Debugging Workflow

1. **Add Logging**: Add trace logging to all cross-file components
2. **Run Tests**: Run integration tests that reproduce failures
3. **Analyze Logs**: Examine logs to identify where the flow breaks
4. **Isolate Component**: Write unit tests for the failing component
5. **Fix Bug**: Implement fix with proper error handling
6. **Verify Fix**: Run all tests including property tests
7. **Add Regression Test**: Add test case for the specific bug

### Test Execution

**Running tests**:
```bash
# Run all tests
cargo test -p rlsp

# Run only cross-file tests
cargo test -p rlsp cross_file

# Run with logging
RUST_LOG=rlsp=trace cargo test -p rlsp cross_file -- --nocapture

# Run specific test
cargo test -p rlsp test_validation_functions_scenario -- --nocapture
```

**Property test execution**:
```bash
# Run property tests with more cases
PROPTEST_CASES=1000 cargo test -p rlsp prop_ -- --nocapture
```

### Success Criteria

The debugging and fixing is complete when:

1. All integration tests pass (validation_functions scenario, backward directive scenario)
2. All property tests pass (100+ iterations each)
3. Logs show correct execution flow through all components
4. Real-world usage (opening files in VS Code) shows symbols from sourced files
5. No "parent file not found" errors for valid backward directives
6. Diagnostics do not mark sourced symbols as undefined
