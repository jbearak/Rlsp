# Legacy Document Store Removal Plan

## Problem

Raven has a dual-store architecture where content is written to both legacy and new stores:

- **Legacy**: `state.documents: HashMap<Url, Document>` and `state.workspace_index: HashMap<Url, Document>`
- **New**: `state.document_store: DocumentStore` and `state.workspace_index_new: WorkspaceIndex`

The legacy stores lack cross-file metadata, scope artifacts, version tracking, memory bounds, and file freshness checks. All reads currently fall back through legacy stores, causing redundant recomputation and inconsistent access patterns.

## Migration Scope

- **~42 read sites in backend.rs** (contains_key, get version/revision, collect open URIs, apply_change)
- **~15 read sites in handlers.rs** (workspace symbols, go-to-definition, hover, completion)
- **~8 sites in state.rs** (struct fields, initialization, legacy methods)
- **~8 sites in content_provider.rs** (fallback chains)
- **~5 sites in background_indexer.rs** (document existence checks)

## Phased Plan

### Phase 1: Verify New Stores Are Feature-Complete
- Audit `DocumentStore` for all `Document` methods used by consumers
- Audit `WorkspaceIndex` for all `workspace_index` methods used by consumers
- Add any missing methods to new stores
- **Risk**: Low

### Phase 2: Eliminate Dual-Writes
- Remove legacy write in `backend.rs` did_open (keep DocumentStore write only)
- Update `apply_workspace_index()` to write only to new stores
- Run tests to verify no behavioral change
- **Risk**: Medium

### Phase 3: Consolidate Reads
- Update `backend.rs` reads (~42 changes) - mostly mechanical
- Update workspace symbol search in `handlers.rs` - complex, test thoroughly
- Update go-to-definition artifact resolution in `handlers.rs` - test thoroughly
- Simplify `content_provider.rs` fallback chains
- **Risk**: High (most changes)

### Phase 4: Remove Legacy API
- Delete `state.documents` and `state.workspace_index` fields
- Remove `open_document()`, `close_document()`, `apply_change()`, `get_document()` methods
- Simplify `content_provider()` method
- Update all test helpers
- **Risk**: High (breaking API change)

### Phase 5: Verification
- Run full test suite
- Stress test with large workspaces
- Profile for memory/performance regressions

## Expected Benefits
- Eliminate ~200 lines of legacy infrastructure
- Consolidate ~15 fallback chain implementations into 1
- Pre-computed artifacts instead of recomputation per request
- Memory-bounded stores with LRU eviction
- Version tracking prevents stale data
