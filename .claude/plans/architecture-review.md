# Architecture Review Plan

## Summary of Findings

### Critical Performance Issues

1. **No R subprocess timeout** (`r_subprocess.rs:270`): `execute_r_code()` has no timeout wrapper. A hung R process blocks initialization indefinitely.

2. **Per-identifier scope resolution in diagnostics** (`handlers.rs:3967`): `collect_undefined_variables_position_aware()` calls `get_cross_file_scope()` for every identifier. In a file with 500+ identifiers, this means 500+ expensive cross-file graph traversals. Scope should be computed once per unique (line, col) position or batched.

3. **Three separate AST walks in SymbolExtractor** (`handlers.rs:349-359`): `extract_all()` does three full tree traversals (assignments, S4 methods, sections). These can be merged into a single pass.

4. **Unbounded HelpCache** (`help.rs:11`): Simple HashMap with no eviction. Long-running sessions accumulate entries indefinitely. Also uses blocking `std::process::Command` on the tokio runtime.

5. **O(n) duplicate detection in BackgroundIndexer** (`background_indexer.rs:118,404`): Queue uses `iter().any()` for dedup instead of a HashSet.

6. **Unbounded workspace_symbol collection** (`handlers.rs:2237`): Collects symbols from all files before truncating. Should short-circuit when `max_results` is reached.

7. **O(n²) package deduplication in completions** (`handlers.rs:4503-4509`): Uses Vec `.contains()` in a loop.

## Implementation Plan

### Phase 1: Critical Fixes
- Add 30s timeout to R subprocess calls
- Add LRU bound to HelpCache
- Convert help.rs to async (tokio::process::Command)
- Add HashSet for BackgroundIndexer queue dedup

### Phase 2: Performance Optimizations
- Batch scope resolution for undefined variable diagnostics
- Merge SymbolExtractor AST walks into single pass
- Add early termination to workspace_symbol
- Use HashSet for package dedup in completions

### Phase 3: Documentation
- Update CLAUDE.md with findings and new patterns
