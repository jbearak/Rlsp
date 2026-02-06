# AGENTS.md - LLM Guidance for Raven

## Documentation Requirements

**IMPORTANT**: When making significant changes to the codebase, you MUST update this file:

1. **Architecture changes**: Update the relevant architecture section (LSP Architecture, R Integration Architecture, Cross-File Architecture, etc.) when adding new modules, changing initialization flow, or modifying how components interact.

2. **New APIs or methods**: Document new public APIs in the appropriate section, including purpose and performance characteristics where relevant.

3. **Learnings**: Add to the "Learnings" section when you discover:
   - Non-obvious gotchas or pitfalls
   - Performance insights
   - Patterns that worked well (or didn't)
   - Debugging techniques that helped

4. **Performance optimizations**: Update the "Performance & Profiling" section with:
   - What was optimized and why
   - Measured before/after improvements
   - New profiling tools or techniques added

Do not wait to be asked - proactively update this documentation as part of completing any significant task.

## Project Overview

Raven is a static R Language Server with cross-file awareness for scientific research workflows. It provides LSP features without embedding R runtime. Uses tree-sitter for parsing, subprocess calls for help.

**Provenance**: Raven combines code from two sources:
- **[Ark](https://github.com/posit-dev/ark)** (MIT License, Posit Software, PBC) - Raven began as a fork of Ark's LSP component. The core LSP infrastructure derives from Ark.
- **[Sight](https://github.com/jbearak/sight)** (GPL-3.0) - The cross-file awareness system was ported from Sight, a Stata language server with similar goals.

Both Sight and Raven were written by the same author to address the same problem: scientific research codebases that span many files.

## Repository Structure

- `crates/raven/`: Main LSP implementation
- `crates/raven/src/cross_file/`: Cross-file awareness module
- `editors/vscode/`: VS Code extension
- `sight/`: Git submodule containing the Sight Stata language server (reference implementation)
- `Cargo.toml`: Workspace root
- `setup.sh`: Build and install script

### Sight Submodule

The `sight/` directory is a git submodule containing the Sight Stata language server. It exists as a **reference implementation** for features being ported to Raven. When implementing new features (especially directive handling or cross-file awareness), consult the Sight codebase for patterns and approaches:

- `sight/src/providers/completion.ts` - File path completion implementation
- `sight/src/providers/definition.ts` - Go-to-definition for file paths
- `sight/src/directive-parser/` - Directive parsing patterns
- `sight/src/scope-resolver/` - Cross-file scope resolution
- `sight/src/utils/file-path-utils.ts` - Path resolution utilities

Sight is TypeScript-based while Raven is Rust-based, so implementations need adaptation, but the algorithms and UX patterns should be consistent.

## Build Commands

- `cargo build -p raven` - Debug build
- `cargo build --release -p raven` - Release build
- `cargo test -p raven` - Run tests
- `cargo bench --bench startup` - Run performance benchmarks
- `RAVEN_PERF=1 cargo run -p raven -- --stdio` - Run with performance timing enabled
- `./setup.sh` - Build and install everything

### Profiling Scripts (`scripts/`)

Python scripts for profiling LSP startup latency:

- `profile_startup.py` - Full profiling: spawns LSP, opens files, measures time to first diagnostic
- `profile_simple.py` - Simpler profiling: just initialize and track stderr timing

Usage:
```bash
cargo build --release -p raven
python3 scripts/profile_startup.py
```

Both scripts set the `RAVEN_PERF` environment variable to enable timing logs and parse stderr for performance metrics (`profile_startup.py` uses `RAVEN_PERF=1`, `profile_simple.py` uses `RAVEN_PERF=verbose`).

## LSP Architecture

- Static analysis using tree-sitter-r
- Workspace symbol indexing (functions, variables)
- Package awareness (library() calls, NAMESPACE)
- Help via R subprocess (tools::Rd2txt)
- Thread-safe caching (RwLock)
- Cross-file awareness via source() detection and directives

## R Integration Architecture

Raven uses a **static-first** strategy for package information, with R subprocess as a selective fallback. This eliminates R subprocess calls for 94% of packages.

### Package Export Loading Strategy

Raven uses a three-tier loading strategy to minimize R subprocess usage while maintaining accuracy:

```text
┌─────────────────────────────────────────────────────────────┐
│                    Package Export Loading                    │
├─────────────────────────────────────────────────────────────┤
│  Tier 1: Static NAMESPACE Parsing (~1-5ms)                  │
│  ├── Parse NAMESPACE file (export(), S3method())            │
│  └── If no exportPattern() → DONE (94% of packages)         │
│                                                             │
│  Tier 2: R Subprocess for Pattern Packages (~100-300ms)     │
│  ├── Detected exportPattern() in NAMESPACE                  │
│  └── Call R subprocess for accurate pattern expansion       │
│                                                             │
│  Tier 3: INDEX File Fallback (if R unavailable)             │
│  ├── Parse INDEX file for documented exports                │
│  └── ~95% accuracy for documented functions                 │
└─────────────────────────────────────────────────────────────┘
```

**Key insight**: 94% of CRAN packages use explicit `export()` directives (generated by roxygen2). Only ~6% use `exportPattern()`, mostly base R and R.oo packages.

### Static Package Loading (`namespace_parser.rs`)

| Function | Purpose | Performance |
|----------|---------|-------------|
| `parse_namespace_exports()` | Extract exports from NAMESPACE file | ~1-2ms |
| `parse_index_exports()` | Extract documented exports from INDEX file | ~1-2ms |
| `parse_description_depends()` | Extract Depends from DESCRIPTION | ~1ms |

**Pattern detection**: NAMESPACE exports containing `exportPattern()` are stored as `__PATTERN__:` markers. When detected, Tier 2 (R subprocess) or Tier 3 (INDEX fallback) is used.

### Subprocess Lifecycle
- **Ephemeral**: Subprocesses are spawned on-demand for specific queries and destroyed immediately. No persistent REPL or session state is maintained.
- **Lightweight**: All commands run with `--vanilla --slave` to suppress user profiles (`.Rprofile`, `.Renviron`) and startup messages.
- **Selective**: Only called for packages with `exportPattern()` (~6%), not for the majority of packages.

### Initialization Architecture
To minimize user-perceived startup latency:

1. **Background Workspace Scanning**: The full workspace scan runs in background via `tokio::spawn()` and does not block LSP responsiveness.

2. **Static PackageLibrary Init**: Initialization uses static NAMESPACE/INDEX parsing for base packages, with R subprocess called only for packages using `exportPattern()` (e.g., `base` package has no NAMESPACE file).

3. **On-Demand File Indexing**: When a user opens a file, `did_open()` immediately indexes the file and its dependencies.

4. **Tiered Prefetch**: `prefetch_packages()` categorizes packages by pattern usage:
   - Static packages (94%): Loaded instantly with no R subprocess
   - Pattern packages (6%): Batched into single R subprocess call

### R Subprocess API (`r_subprocess.rs`)

Key methods for querying R (used selectively):

| Method | Purpose | When Used |
|--------|---------|-----------|
| `get_lib_paths()` | R library paths | Initialization fallback |
| `get_multiple_package_exports()` | Batch export queries for pattern packages | Prefetch (~6% of packages) |
| `get_package_exports()` | Single package exports | Pattern packages only |

**Note**: `initialize_batch()` is no longer the primary initialization path. Static loading is preferred.

All methods validate package names to prevent R code injection.

### Project Environment Support (`renv`)
Raven supports project-local package libraries (like `renv`):
- **Working Directory**: The R subprocess is spawned with the workspace root as its working directory.
- **Activation**: When querying `.libPaths()`, Raven checks for `renv/activate.R` in the project root. To prevent path traversal attacks, it verifies that the file resides within the `renv` directory of the workspace root before sourcing it.

## Cross-File Architecture

### Overview

Cross-file awareness enables Raven to understand symbol definitions and relationships across multiple R files connected via `source()` calls and LSP directives. This allows:

- **Symbol resolution**: Functions and variables from sourced files appear in completions, hover, and go-to-definition
- **Diagnostics suppression**: Symbols from sourced files are not marked as "undefined variable"
- **Dependency tracking**: Changes to a file trigger revalidation of dependent files
- **Workspace indexing**: Closed files are indexed and their symbols are available

**Key Features**:
1. **Automatic detection**: Parses `source()` and `sys.source()` calls from R code
2. **Manual directives**: `@lsp-sourced-by`, `@lsp-run-by` for files not explicitly sourced
3. **Declaration directives**: `@lsp-var`, `@lsp-func` for dynamically created symbols
4. **Working directory support**: `@lsp-cd` directive affects source() path resolution
5. **Position-aware scope**: Symbols only available after their source() call
6. **Cycle detection**: Prevents infinite loops in circular dependencies
7. **Real-time updates**: Changes propagate to dependent files automatically

### Module Structure (`crates/raven/src/cross_file/`)

- `background_indexer.rs` - Background indexing queue for transitive dependencies
- `types.rs` - Core types (CrossFileMetadata, BackwardDirective, ForwardSource, DeclaredSymbol, CallSiteSpec)
- `directive.rs` - Directive parsing (@lsp-sourced-by, @lsp-source, @lsp-var, @lsp-func, etc.) with optional colon/quotes
- `source_detect.rs` - Tree-sitter based source() call detection with UTF-16 columns
- `path_resolve.rs` - Path resolution with working directory support
- `dependency.rs` - Dependency graph with directive-vs-AST conflict resolution
- `scope.rs` - Scope resolution and symbol extraction (graph-based via `scope_at_position_with_graph`)
- `config.rs` - Configuration options including severity settings
- `cache.rs` - Caching with interior mutability
- `parent_resolve.rs` - Parent resolution with match= and call-site inference
- `revalidation.rs` - Real-time update system with debouncing
- `workspace_index.rs` - Workspace indexing for closed files
- `file_cache.rs` - Disk file cache with snapshots
- `content_provider.rs` - Unified content provider

### Directive Syntax

All directives support optional colon and quotes:
- `# @lsp-sourced-by ../main.R`
- `# @lsp-sourced-by: ../main.R`
- `# @lsp-sourced-by "../main.R"`
- `# @lsp-sourced-by: "../main.R"`

Backward directive synonyms: `@lsp-sourced-by`, `@lsp-run-by`, `@lsp-included-by`
Forward directive synonyms: `@lsp-source`, `@lsp-run`, `@lsp-include`
Working directory synonyms: `@lsp-working-directory`, `@lsp-working-dir`, `@lsp-current-directory`, `@lsp-current-dir`, `@lsp-wd`, `@lsp-cd`
Variable declaration synonyms: `@lsp-var`, `@lsp-variable`, `@lsp-declare-var`, `@lsp-declare-variable`
Function declaration synonyms: `@lsp-func`, `@lsp-function`, `@lsp-declare-func`, `@lsp-declare-function`

### Declaration Directives

Declaration directives allow users to declare symbols (variables and functions) that cannot be statically detected by the parser. This suppresses false-positive diagnostics for dynamically created symbols from `eval()`, `assign()`, `load()`, or external data loading.

**Syntax:**
- `# @lsp-var myvar` - Declare a variable
- `# @lsp-func myfunc` - Declare a function
- Supports optional colon: `# @lsp-var: myvar`
- Supports quotes for special characters: `# @lsp-var "my.var"`

**Key behaviors:**
- **Position-aware**: Declared symbols are available starting from the line after the directive (line N+1), matching `source()` semantics
- **Cross-file inheritance**: Declarations propagate to sourced child files if declared before the `source()` call
- **Diagnostic suppression**: Declared symbols suppress "undefined variable" warnings
- **LSP features**: Completions, hover (shows "declared via @lsp-var directive at line N"), go-to-definition (navigates to directive)
- **Interface hash**: Declaration changes trigger revalidation of dependent files

**Conflicting declarations** (same symbol as both variable and function):
- Later declaration (by line) determines symbol kind for completions/hover
- First declaration used for go-to-definition
- Diagnostic suppression applies regardless of kind

**Implementation:**
- `DeclaredSymbol` struct in `types.rs` stores name, line, and is_function flag
- `CrossFileMetadata` has `declared_variables` and `declared_functions` fields
- `ScopeEvent::Declaration` variant in `scope.rs` for timeline integration
- Column set to `u32::MAX` (end-of-line sentinel) ensures symbol available from line+1

### Path Resolution: Critical Distinction

**IMPORTANT**: Path resolution behaves differently depending on the directive type:

#### Backward Directives (Ignore @lsp-cd)
**Always resolve relative to the file's directory, ignoring @lsp-cd:**
- `@lsp-sourced-by: ../parent.R` - Resolved from file's directory
- `@lsp-run-by: ../parent.R` - Resolved from file's directory
- `@lsp-included-by: ../parent.R` - Resolved from file's directory
- **Rationale**: Backward directives describe static file relationships from the child's perspective. They declare "this file is sourced by that parent file" - a relationship that should not change based on runtime working directory.

**Implementation**:
- Uses `PathContext::new()` which excludes working_directory from metadata
- Applied in both `dependency.rs` (graph building) and `handlers.rs` (diagnostics)
- See `do_resolve_backward()` helper in `dependency.rs`

#### Forward Directives (Use @lsp-cd)
**Resolve using @lsp-cd working directory when present:**
- `@lsp-source: utils.R` - Resolved from @lsp-cd directory if specified, else file's directory
- `@lsp-run: ../data.R` - Resolved from @lsp-cd directory if specified, else file's directory
- `@lsp-include: helpers.R` - Resolved from @lsp-cd directory if specified, else file's directory
- **Rationale**: Forward directives are semantically equivalent to `source()` calls. They describe runtime execution behavior ("this file sources that child file"), so they should respect the working directory just like actual `source()` calls.

**Implementation**:
- Uses `PathContext::from_metadata()` which includes working_directory from metadata
- Applied via `do_resolve()` helper in `dependency.rs`

#### source() Statements (AST-detected)
**Resolve using @lsp-cd working directory when present:**
- `source("utils.R")` - Resolved from @lsp-cd directory if specified, else file's directory
- `source("../data.R")` - Resolved from @lsp-cd directory if specified, else file's directory
- **Rationale**: source() calls execute at runtime and are affected by working directory

**Implementation**:
- Uses `PathContext::from_metadata()` which includes working_directory from metadata
- Applied via `do_resolve()` helper in `dependency.rs`

#### Workspace-Root Fallback for Unannotated Codebases

For codebases without LSP directives (no @lsp-cd), source() paths are often written relative to the workspace root (e.g., the project is typically run from the root directory). To support these codebases without requiring LSP annotations:

**Fallback Behavior** (source() statements only):
1. First, try resolving relative to file's directory (standard behavior)
2. If file doesn't exist AND the file has no @lsp-cd directive AND no inherited working directory:
   - Try resolving relative to workspace root
   - Use workspace-resolved path if it exists

**Example**:
```r
# File: scripts/analysis.R (no LSP directives)
# Workspace root: /project

source("data/load.R")  # File is at /project/data/load.R
```

Resolution:
1. Try `/project/scripts/data/load.R` (file-relative) - doesn't exist
2. Try `/project/data/load.R` (workspace-root fallback) - exists, use this

**Important**: This fallback only applies to source() statements, NOT to LSP directives like @lsp-sourced-by.

**Implementation**:
- `resolve_path_with_workspace_fallback()` in `path_resolve.rs`
- Checks `has_explicit_wd` (from @lsp-cd) and `has_inherited_wd` (from parent chain) to determine if fallback should apply

#### Example Scenario
```r
# File: subdir/child.r
# @lsp-cd: /some/other/directory
# @lsp-run-by: ../parent.r
# @lsp-source: utils.r

source("helpers.r")
```

**Resolution behavior**:
- `@lsp-run-by: ../parent.r` → Resolves to `parent.r` in workspace root (backward directive ignores @lsp-cd)
- `@lsp-source: utils.r` → Resolves to `/some/other/directory/utils.r` (forward directive uses @lsp-cd)
- `source("helpers.r")` → Resolves to `/some/other/directory/helpers.r` (uses @lsp-cd)

#### Path Types
1. **File-relative**: `utils.R`, `../parent.R`, `./data.R`
   - LSP directives: relative to file's directory
   - source() calls: relative to @lsp-cd or file's directory

2. **Workspace-root-relative**: `/data/utils.R`
   - Both: relative to workspace root (requires workspace_root parameter)

3. **Absolute**: `/absolute/path/to/file.R`
   - Both: used directly after canonicalization

#### PathContext Types
- `PathContext::new(uri, workspace_root)` - For backward directives (no working_directory)
- `PathContext::from_metadata(uri, metadata, workspace_root)` - For forward directives and source() calls (includes working_directory)

### Call-Site Resolution

1. `line=N` - Explicit line number (highest precedence)
2. `match="pattern"` - Pattern search in parent file
3. Text inference - Scan parent for source() calls to child
4. Config default - `assumeCallSite` setting ("end" or "start")

### Directive-vs-AST Conflict Resolution

- Directive with known call site: only overrides AST edge at same call site
- Directive without call site: suppresses all AST edges to that target (emits warning)
- AST edges to different targets are always preserved

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
- Selective invalidation based on interface hash comparison
- Debounced diagnostics fanout to affected open files
- Cancellation of outdated pending revalidations
- Freshness guards prevent stale diagnostic publishes
- Monotonic publishing: never publish older version than last published

### Interface Hash Optimization

When a file changes, Raven compares the old and new `interface_hash` to determine if dependents need revalidation:

**What triggers dependent revalidation:**
- Adding/removing/renaming exported functions or variables
- Changes to library() calls (affects loaded_packages in hash)

**What does NOT trigger dependent revalidation:**
- Editing comments
- Changing local variables inside functions
- Modifying function bodies (without changing the function signature)
- Whitespace changes

**Implementation:**
- `did_change`: Captures old interface_hash before applying changes, compares after
- `did_open`: Compares against workspace index if file was previously indexed
- Only calls `get_transitive_dependents()` and marks force republish when `interface_changed`

This optimization significantly reduces cascading revalidation in codebases with deep transitive dependency chains.

### On-Demand Background Indexing

The BackgroundIndexer handles asynchronous indexing of files not currently open in the editor:

**Indexing Categories**:
- Sourced files: Files directly sourced by open documents (indexed synchronously before diagnostics)
- Backward directive targets: Files referenced by @lsp-run-by, @lsp-sourced-by (indexed synchronously before diagnostics)
- Transitive dependencies: Files sourced by indexed files (queued for background indexing)

**Architecture**:
- Single worker thread processes queue sequentially (FIFO order)
- Depth tracking prevents infinite transitive chains
- Duplicate detection avoids redundant work

**Configuration** (via `crossFile.onDemandIndexing.*`):
- `enabled`: Enable/disable on-demand indexing (default: true)
- `maxTransitiveDepth`: Maximum depth for transitive indexing (default: 2)
- `maxQueueSize`: Maximum queue size (default: 50)

**Flow**:
1. File opened → sourced files and backward directive targets indexed synchronously
2. Transitive dependencies queued for background indexing
3. Worker processes queue → reads file, extracts metadata, computes artifacts
4. Updates workspace index and dependency graph
5. Queues further transitive dependencies (if depth allows)

## Learnings

- When building a new struct from `&T`, avoid `..*ref`/`..ref` in struct update syntax; clone the base (`..ref.clone()`) or construct fields explicitly.
- In tests that locate identifiers in generated code, avoid substring matches (e.g., `inner_func` inside `outer_func`). Prefer delimiter-aware search or node positions from the AST.
- Don’t recurse on identifier nodes just to “keep traversal going” — they have no children and the extra recursion can be removed for clarity.
- Avoid blocking filesystem I/O on LSP request threads; if a fallback check is needed, do it off-thread and revalidate via cache updates.
- Guard against deadlocks by avoiding nested async lock acquisition (especially around background indexer/state access).
- Use `saturating_add` (or equivalent) for sentinel end-of-line columns to prevent overflow.
- Don’t “paper over” missing files: file-existence checks must preserve accurate diagnostics instead of always returning `true`.
- Validate user-controlled package names/paths and skip suspicious values before using them in indexing or diagnostics.
- Avoid `expect()` in long-lived server paths (e.g., parser init); propagate errors with `Option`/`Result` instead.
- When adding new enum variants used in test match expressions, update all test helpers to keep matches exhaustive.
- Avoid hard-coded line/column positions in property tests; compute stable positions from the generated code or parsed nodes.
- When using `tokio::sync::watch`, guard waiters with in-flight state or revisions so late callers don’t hang on `changed()`.
- When using `u32::MAX` as an EOF sentinel, ensure function-scope filtering treats it as “outside any function” to avoid leaking locals.
- Distinguish “full EOF” (both line and column MAX) from end-of-line sentinels so scope filtering doesn’t drop function-local symbols at call sites.
- In proptest, prefer `prop_assume!` for invalid generated cases instead of returning early with non-unit values.
- Keep requirements docs aligned with the actual API surface (e.g., static construction vs insertion guarantees).
- Convert UTF-16 columns to byte offsets before constructing tree-sitter `Point`s; avoid mixing column units.
- When expanding point windows by a byte, advance to the next UTF-8 boundary to avoid mid-codepoint positions.
- Watch for O(n²) scope detection or duplicate-detection paths in large files; prefer indexed lookups when possible.
- Avoid duplicating local-scoping condition logic across functions; centralize to reduce drift.
- Be careful with hover/definition range calculations at line boundaries to avoid off-by-one bugs or invalid points.
- For removal events, use strict position comparisons (before, not at) to avoid removing symbols at their definition position.
- In hot scope-resolution paths, avoid repeated scans over large lists (e.g., function scopes per event); precompute mappings or cache lookups to prevent O(R·F) regressions.
- Keep doc comments and markdown examples aligned with current behavior (e.g., list= string literals support).
- Normalize markdown table spacing to match project lint expectations when adding spec tables.
- Avoid interpolating user-controlled strings into R code; pass help topics/packages as command args instead.
- R's `help()` function uses non-standard evaluation (NSE) for the `package` argument; wrap variables in parentheses to force evaluation: `help(topic, package = (pkg))` not `help(topic, package = pkg)`.
- Add language identifiers (e.g., `text`) to ASCII diagram/timeline fences to satisfy markdownlint (MD040).
- `tree_sitter::Tree` implements `Clone`; preserve ASTs in cloned index entries when reference searches depend on them.
- For intentionally-unused public APIs, either wire them into a caller or add a localized `#[allow(dead_code)]` with a brief comment to avoid warning noise.
- Base exports must be gated by `package_library_ready` to avoid using empty exports before R subprocess initialization completes.
- When adding parameters to scope resolution functions, update all callers including test helpers.
- For unannotated codebases (no @lsp-cd directives), use workspace-root fallback for source() path resolution - many R projects assume scripts run from the project root.
- The workspace-root fallback should ONLY apply when the file has no explicit @lsp-cd and no inherited working directory from parent chain; don't override intentional working directory configuration.
- When profiling LSP startup, use binary mode (not `text=True`) in Python subprocess calls - text mode can cause 30+ second delays due to buffering/encoding issues with LSP protocol.
- Profile with realistic workspaces: a workspace with many `library()` calls across files reveals bottlenecks that toy examples miss.
- Package export prefetching happens in background after `did_open` returns, but diagnostics wait for it - batch queries to minimize wait time.
- Filter already-cached packages before querying R to avoid redundant subprocess calls.
- 94% of CRAN packages use explicit `export()` directives (roxygen2); only ~6% use `exportPattern()`. Static NAMESPACE parsing eliminates R subprocess calls for most packages.
- The `base` package is special: it has no NAMESPACE file (only DESCRIPTION and INDEX). Handle this by checking for DESCRIPTION existence, not just NAMESPACE.
- INDEX files provide ~95% accuracy for packages using `exportPattern()` when R subprocess is unavailable - they list documented exports.
- Tiered loading (static → R → INDEX fallback) provides both speed and accuracy: sub-5ms for 94% of packages, accurate exports for all.
- When parsing structured R output (markers like `__PKG:name__`), handle missing end markers gracefully to avoid losing partial results.

### Performance & Profiling

- R subprocess spawning is expensive (~75-350ms each); batch multiple queries into single R invocations where possible.
- Use `RAVEN_PERF=1` environment variable to enable timing logs for diagnosing startup latency.
- Workspace scanning runs in background (`tokio::spawn`) while PackageLibrary initialization is awaited - LSP becomes responsive after ~100ms (package init) rather than waiting for full workspace scan.
- `SKIP_DIRECTORIES` in `state.rs` filters node_modules, .git, and target during workspace scan - keep this list minimal to avoid skipping legitimate R code locations.
- Files opened by the user are indexed on-demand via `did_open`, with their dependencies taking priority over background workspace scanning.
- For criterion benchmarks, use `async_tokio` feature and run with `cargo bench --bench startup`.
- The `perf.rs` module provides `TimingGuard` for RAII-style timing and `PerfMetrics` for aggregated startup analysis.
- Key instrumentation points: `initialized()` for overall init, `scan_workspace()` for file scanning, `execute_r_code()` for R subprocess calls.

#### Static Package Loading (Current Architecture)

**Tiered loading strategy** eliminates R subprocess for 94% of packages:
- **Tier 1**: Static NAMESPACE/DESCRIPTION parsing (~1-5ms per package)
- **Tier 2**: R subprocess for packages with `exportPattern()` (~6% of packages)
- **Tier 3**: INDEX file fallback if R unavailable

**Implementation**:
- `parse_namespace_exports()` extracts exports from NAMESPACE file
- `parse_index_exports()` extracts documented exports from INDEX file (for pattern packages); reads via `spawn_blocking` and caches per package dir to avoid blocking LSP request handlers
- `parse_description_depends()` extracts dependencies from DESCRIPTION
- Pattern detection: exports starting with `__PATTERN__:` marker

**Expected improvements** (compared to R-subprocess-first approach):
- Startup: ~50-100ms (static) vs ~100ms+ (R subprocess)
- Package loading: <5ms for 94% of packages (no R subprocess)
- R subprocess calls: 0-3 (only for pattern packages) vs 8-90 (previous)

#### R Subprocess Batching (Selective Usage)

**R subprocess is now used selectively** for packages with `exportPattern()`:
- `get_multiple_package_exports()` batches pattern package queries into single R call
- Used by `prefetch_packages()` after static packages are loaded
- Falls back to INDEX file if R fails or is unavailable

**Previous approach** (R-subprocess-first, for reference):
- `initialize_batch()`: Batched lib_paths, base_packages, and all base package exports
- Reduced startup from 700-2100ms (sequential) to ~100ms (batched)
- Still useful as fallback path if static loading fails

### Rust/Clippy Best Practices

- Use `strip_prefix()` instead of manual `starts_with()` + slice indexing (e.g., `path[1..]`); clippy flags this as `manual_strip`.
- On `DoubleEndedIterator`s, use `next_back()` instead of `last()` to avoid iterating the entire collection.
- Name methods `as_*` (not `to_*`) when they return a cheap view/conversion on `Copy` types that take `self` by value.
- Name methods `as_*` (not `from_*`) when they convert `&self` to another type; `from_*` conventionally takes no `self`.
- Use `&Path` instead of `&PathBuf` in function parameters; `&PathBuf` coerces to `&Path` and avoids unnecessary type constraints.
- Use `split_once()` instead of `splitn(2, ...).nth(1)` for cleaner single-split operations.
- Prefer `for item in iter` over `while let Some(item) = iter.next()` unless you need to call other iterator methods mid-loop.
- When a function legitimately needs many parameters (e.g., recursive scope resolution), add `#[allow(clippy::too_many_arguments)]` rather than forcing awkward refactors.
- Mark test-only helper functions with `#[cfg(test)]` to avoid dead-code warnings in non-test builds.
- For struct fields that are set but not yet read (future use), add `#[allow(dead_code)]` with a comment explaining the intent.
- In doc comments, separate list items from following paragraphs with a blank line to avoid `doc_lazy_continuation` warnings.
- Run `cargo clippy` before committing to catch style issues early; many have auto-fix suggestions via `cargo clippy --fix`.

### Thread-Safety

- WorldState protected by Arc<tokio::sync::RwLock>
- Concurrent reads from request handlers
- Serialized writes for state mutations
- Interior-mutable caches allow population during read operations
- Background tasks reacquire locks, never hold borrowed &mut WorldState

### Common Issues and Debugging

#### "Parent file not found" Error
**Symptom**: Backward directive reports parent file not found despite file existing

**Common Causes**:
1. **Incorrect path resolution**: Ensure path is relative to file's directory, not @lsp-cd
2. **File not in workspace**: Parent file must be within workspace or accessible on disk
3. **Typo in path**: Check for correct use of `..` for parent directory navigation

**Debug Steps**:
1. Enable trace logging: `RUST_LOG=rlsp=trace`
2. Check logs for "Resolving path" messages in `path_resolve.rs`
3. Verify file exists at resolved canonical path
4. Ensure backward directive uses separate PathContext (without @lsp-cd)

#### Symbols Not Available from Sourced File
**Symptom**: Completions don't show functions from sourced files

**Common Causes**:
1. **Position before source() call**: Symbols only available after source() line
2. **Path resolution failed**: source() path doesn't resolve to actual file
3. **Cycle detected**: Circular dependencies stop traversal
4. **Max depth exceeded**: Chain longer than configured max_chain_depth

**Debug Steps**:
1. Check dependency graph: Look for edge from parent to child
2. Verify metadata extraction: Ensure source() call was detected
3. Check scope resolution logs: Verify traversal reaches sourced file
4. Test with simple two-file case to isolate issue

#### @lsp-cd Not Affecting Backward Directives (Expected Behavior)
**Symptom**: Backward directive path resolution ignores @lsp-cd

**This is correct behavior**: Backward directives (`@lsp-sourced-by`, `@lsp-run-by`, `@lsp-included-by`) always resolve relative to file's directory, ignoring @lsp-cd. Forward directives (`@lsp-source`, `@lsp-run`, `@lsp-include`) and source() statements use @lsp-cd for path resolution.

**If you need @lsp-cd to affect a path**:
- Use a forward directive (`@lsp-source`) instead of a backward directive
- Or use a `source()` call instead of a directive
- Or specify absolute/workspace-relative path in the backward directive

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

## Extension Guide

### Adding New Directives

1. **Define the directive type** in `types.rs`:
   - Add variant to existing enum or create new struct
   - Include all necessary fields (path, line, parameters)

2. **Parse the directive** in `directive.rs`:
   - Add regex pattern to `DIRECTIVE_PATTERNS` or create new pattern
   - Handle optional colon and quotes: `@name:? "?path"?`
   - Parse any parameters (e.g., `line=N`, `match="pattern"`)
   - Add to `parse_directives()` function

3. **Process in dependency graph** (`dependency.rs`):
   - Handle in `update_file()` method
   - Create appropriate `DependencyEdge` entries
   - Consider directive-vs-AST conflict resolution

4. **Update scope resolution** (`scope.rs`) if directive affects symbol availability

5. **Add tests**:
   - Unit tests in the module's `#[cfg(test)]` section
   - Property tests in `property_tests.rs` for invariants

### Extending Scope Resolution

1. **Modify `ScopeArtifacts`** in `scope.rs`:
   - Add new fields to track additional scope information
   - Update `compute_artifacts()` to populate new fields

2. **Update timeline events** (`ScopeEvent` enum):
   - Add new event types for scope-affecting constructs
   - Handle in `scope_at_position_*` functions

3. **Modify traversal** in `scope_at_position_with_graph_recursive()`:
   - Process new event types
   - Maintain correct symbol precedence (local > inherited)

4. **Update interface hash** if changes affect cross-file invalidation

### Adding Cross-File Diagnostics

1. **Define diagnostic type** in `handlers.rs`:
   - Create `collect_*_diagnostics()` function
   - Use `state.cross_file_config.*_severity` for configurable severity

2. **Add configuration** in `config.rs`:
   - Add severity field to `CrossFileConfig`
   - Add to `from_initialization_options()`

3. **Wire into diagnostics collection** in `handlers.rs`:
   - Call from `collect_diagnostics()` or `publish_diagnostics()`
   - Ensure proper position (line, column) in UTF-16

4. **Add tests** for diagnostic generation and severity configuration

### Symbol Provider Architecture

The document symbol and workspace symbol providers use a two-phase extraction and hierarchy building approach:

**Components:**

1. **SymbolExtractor** (`handlers.rs`):
   - Extracts raw symbols from parsed R documents
   - Detects assignments (functions, variables, constants)
   - Detects S4 methods (`setMethod`, `setClass`, `setGeneric`)
   - Detects R code sections (single-line `# Section ----` and banner-style)
   - Classifies symbol kinds (ALL_CAPS → CONSTANT, R6Class → CLASS, etc.)
   - Extracts function signatures for detail field

2. **HierarchyBuilder** (`handlers.rs`):
   - Builds hierarchical `DocumentSymbol[]` from flat symbols
   - Computes section ranges (from comment to next section or EOF)
   - Nests symbols within sections based on position
   - Nests symbols within function bodies based on containment
   - Supports arbitrary nesting depth

3. **DocumentSymbolKind** (`handlers.rs`):
   - Extended symbol kind enum for richer LSP mapping
   - Variants: Function, Variable, Constant, Class, Method, Interface, Module
   - `to_lsp_kind()` method for LSP SymbolKind conversion

4. **SymbolConfig** (`state.rs`):
   - Configuration for symbol providers
   - `workspace_max_results`: Limits workspace symbol results (default: 1000)
   - `hierarchical_document_symbol_support`: Client capability flag

**Key Patterns:**

- Single-line R code sections use regex: `^\s*#(#*)\s*(%%)?\s*(\S.+?)\s*(#{4,}|-{4,}|={4,}|\*{4,}|\+{4,})\s*$`
- Banner-style sections: delimiter line above and below a comment name line (e.g., `# ====` / `# Name` / `# ====`). Delimiters must use the same character type (`#`, `-`, `=`, `*`, `+`) but don't need to be the same length. Banner sections are always heading level 1.
- ALL_CAPS constants: `^[A-Z][A-Z0-9_.]+$` (min 2 chars)
- Reserved words are filtered from both document and workspace symbols
- Workspace symbols include `containerName` (filename without extension)

### Testing Strategies

**Unit Tests:**
- Test individual functions in isolation
- Use `#[cfg(test)]` module at end of each file
- Mock dependencies with closures

**Property Tests** (`property_tests.rs`):
- Test invariants that must hold for all inputs
- Use `proptest!` macro with custom strategies
- Focus on: edge deduplication, scope precedence, path resolution

**Integration Tests:**
- Test full LSP request/response cycles
- Use `handlers::integration_tests` module
- Test with realistic R code patterns

**Test Patterns:**
```rust
// Unit test pattern
#[test]
fn test_specific_behavior() {
    let input = /* setup */;
    let result = function_under_test(input);
    assert_eq!(result, expected);
}

// Property test pattern
proptest! {
    #[test]
    fn prop_invariant_holds(input in strategy()) {
        let result = function_under_test(input);
        prop_assert!(invariant(result));
    }
}
```