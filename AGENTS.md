# AGENTS.md - LLM Guidance for Rlsp

## Project Overview

Rlsp is a static R Language Server extracted from Ark. It provides LSP features without embedding R runtime. Uses tree-sitter for parsing, subprocess calls for help.

## Repository Structure

- `crates/rlsp/`: Main LSP implementation
- `crates/rlsp/src/cross_file/`: Cross-file awareness module
- `editors/vscode/`: VS Code extension
- `Cargo.toml`: Workspace root
- `setup.sh`: Build and install script

## Build Commands

- `cargo build -p rlsp` - Debug build
- `cargo build --release -p rlsp` - Release build
- `cargo test -p rlsp` - Run tests
- `./setup.sh` - Build and install everything

## LSP Architecture

- Static analysis using tree-sitter-r
- Workspace symbol indexing (functions, variables)
- Package awareness (library() calls, NAMESPACE)
- Help via R subprocess (tools::Rd2txt)
- Thread-safe caching (RwLock)
- Cross-file awareness via source() detection and directives

## Cross-File Architecture

### Module Structure (`crates/rlsp/src/cross_file/`)

- `types.rs` - Core types (CrossFileMetadata, BackwardDirective, ForwardSource)
- `directive.rs` - Directive parsing (@lsp-sourced-by, @lsp-source, etc.)
- `source_detect.rs` - Tree-sitter based source() call detection
- `path_resolve.rs` - Path resolution with working directory support
- `dependency.rs` - Dependency graph (forward edges only)
- `scope.rs` - Scope resolution and symbol extraction
- `config.rs` - Configuration options
- `cache.rs` - Caching with interior mutability
- `parent_resolve.rs` - Parent resolution with stability
- `revalidation.rs` - Real-time update system
- `workspace_index.rs` - Workspace indexing for closed files
- `file_cache.rs` - Disk file cache
- `content_provider.rs` - Unified content provider

### Dependency Graph

- Forward edges only (parent sources child)
- Backward directives create/confirm forward edges
- Edges store call site position (line, column in UTF-16)
- Stores local, chdir, is_sys_source flags
- Deduplication by canonical key

### Scope Resolution

- Position-aware: scope depends on (line, character) position
- Two-phase: compute per-file artifacts (non-recursive), then traverse
- Artifacts include: exported interface, timeline of scope events, interface hash
- Timeline contains: symbol definitions, source() calls, working directory changes
- Traversal bounded by max_chain_depth and visited set

### Caching Strategy

- Three caches with interior mutability: MetadataCache, ArtifactsCache, ParentSelectionCache
- Fingerprinted entries: self_hash, edges_hash, upstream_interfaces_hash, workspace_index_version
- Invalidation triggers: interface hash change OR edge set change

### Real-Time Updates

- Metadata extraction on document change
- Dependency graph update
- Selective invalidation based on interface/edge changes
- Debounced diagnostics fanout to affected open files
- Cancellation of outdated pending revalidations
- Freshness guards prevent stale diagnostic publishes
- Monotonic publishing: never publish older version than last published

### Thread-Safety

- WorldState protected by Arc<tokio::sync::RwLock>
- Concurrent reads from request handlers
- Serialized writes for state mutations
- Interior-mutable caches allow population during read operations
- Background tasks reacquire locks, never hold borrowed &mut WorldState

## VS Code Extension

- TypeScript client in `editors/vscode/src/`
- Bundles platform-specific rlsp binary
- Configuration: `rlsp.server.path`
- Sends activity notifications for revalidation prioritization

## Coding Style

- No `bail!`, use explicit `return Err(anyhow!(...))`
- Omit `return` in match expressions
- Direct formatting: `anyhow!("Message: {err}")`
- Use `log::trace!` instead of `log::debug!`
- Fully qualified result types

## Testing

Property-based tests with proptest, integration tests

## Built-in Functions

`build_builtins.R` generates `src/builtins.rs` with 2,355 R functions

## Release Process

Manual tagging (`git tag vX.Y.Z && git push origin vX.Y.Z`) triggers GitHub Actions