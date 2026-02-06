# Plan: Replace Full-Clear Cache Eviction with LRU

## Context

Raven's cross-file caches use a "full-clear" eviction strategy: when a cache hits its size limit and a new entry arrives, the **entire cache is discarded**. This creates a sawtooth pattern — the cache thrashes between full and empty. Additionally, `CrossFileWorkspaceIndex` (the most memory-expensive cache) is completely **unbounded**.

Research into major language servers (tsserver, Pyright, rust-analyzer, clangd, gopls) shows that LRU is the consensus choice for bounded caches. This plan replaces full-clear with LRU eviction and adds bounds to the unbounded workspace index.

**Note**: `ArtifactsCache` and `ParentSelectionCache` are dead code (never populated in production — only invalidated). They are left unchanged.

## Files to Modify

| File | Change |
|------|--------|
| `crates/raven/Cargo.toml` | Add `lru` dependency |
| `crates/raven/src/cross_file/cache.rs` | MetadataCache → LRU |
| `crates/raven/src/cross_file/file_cache.rs` | Content + existence caches → LRU |
| `crates/raven/src/cross_file/workspace_index.rs` | CrossFileWorkspaceIndex → bounded LRU |
| `crates/raven/src/workspace_index.rs` | WorkspaceIndex → LRU eviction (replaces reject-at-limit) |
| `crates/raven/src/cross_file/config.rs` | Add cache limit config fields |
| `crates/raven/src/state.rs` | Add `resize_caches()` method, wire config to caches |
| `crates/raven/src/backend.rs` | Parse cache config from JSON, call resize |
| `editors/vscode/package.json` | Add VS Code settings for cache limits |
| `editors/vscode/src/extension.ts` | Map settings to initialization options |

## Implementation Steps

### Step 1: Add `lru` crate

Add `lru = "0.14"` to `crates/raven/Cargo.toml` dependencies. The `lru` crate provides O(1) `put`/`get`/`peek`/`pop` with automatic LRU eviction on capacity overflow.

### Step 2: Migrate MetadataCache (cache.rs)

Replace `HashMap<Url, CrossFileMetadata>` with `LruCache<Url, CrossFileMetadata>`:

- Remove `METADATA_CACHE_MAX_ENTRIES` constant
- Add `with_capacity(cap: usize)` constructor; keep `new()` defaulting to 1000
- **Read path** (`get`): Use `inner.read()` + `peek()` (doesn't need `&mut self`, works under read lock)
- **Write path** (`insert`): Use `inner.write()` + `push()` (auto-evicts LRU if over capacity). Remove the full-clear block entirely.
- `remove`: Use `inner.write()` + `pop()`
- `invalidate_many`: Loop calling `pop()` under single write lock
- Add `resize(cap: usize)` method calling `LruCache::resize()`

The `peek()` approach means eviction is "LRU by insertion/update time" not "LRU by access time." This is acceptable — the read path stays fully concurrent via `RwLock`, and the primary goal is bounding memory, not optimizing hit rates.

### Step 3: Migrate CrossFileFileCache (file_cache.rs)

Replace both internal `HashMap`s with `LruCache`:
- Content cache: `LruCache<Url, CachedFile>` (default capacity 500)
- Existence cache: `LruCache<PathBuf, bool>` (default capacity 2000)

Changes:
- Remove `FILE_CACHE_MAX_ENTRIES` and `EXISTENCE_CACHE_MAX_ENTRIES` constants
- Add `with_capacities(content_cap, existence_cap)` constructor
- Read methods (`path_exists`, `get_if_fresh`, `get`): Use `peek()` under read lock
- Write methods (`cache_existence`, `insert`): Use `push()` under write lock — remove all full-clear blocks
- Add `resize(content_cap, existence_cap)` method

### Step 4: Migrate CrossFileWorkspaceIndex (cross_file/workspace_index.rs)

Replace unbounded `HashMap<Url, IndexEntry>` with `LruCache<Url, IndexEntry>`:
- Add `with_capacity(cap: usize)` constructor; default 5000
- Read methods (`get_if_fresh`, `get_metadata`, `get_artifacts`, `contains`): Use `peek()` under read lock
- `uris()`: Use `inner.read()` + `iter()` to collect keys
- Write methods (`update_from_disk`, `insert`): Use `push()` under write lock
- `invalidate`/`invalidate_all`: Use `pop()`/`clear()`
- Version semantics unchanged — `increment_version()` still called on mutations
- Add `resize(cap: usize)` method

### Step 5: Migrate WorkspaceIndex (workspace_index.rs)

Replace `HashMap<Url, IndexEntry>` with `LruCache<Url, IndexEntry>`:
- Use `config.max_files` as LRU capacity
- **Key behavioral change**: `insert()` now **evicts LRU** instead of **rejecting** at capacity. This is strictly better — the old approach meant once at capacity, new files could never be indexed.
- `insert()` always returns `true` (unless oversized or lock fails). Remove the reject-at-limit block.
- Read methods (`get`, `get_if_fresh`, `get_metadata`, `get_artifacts`, `contains`): Use `peek()` under read lock
- `uris()` / `iter()`: Use `LruCache::iter()` to collect
- `len()` / `is_empty()`: Use `LruCache::len()` / check `len() == 0`
- Update tests: `test_max_files_limit` changes to verify eviction (oldest entry gone, new entry present) instead of rejection

### Step 6: Add config fields (config.rs)

Add to `CrossFileConfig`:
```rust
pub cache_metadata_max_entries: usize,      // default: 1000
pub cache_file_content_max_entries: usize,   // default: 500
pub cache_existence_max_entries: usize,      // default: 2000
pub cache_workspace_index_max_entries: usize, // default: 5000
```

These do NOT affect `scope_settings_changed()` — cache size changes don't affect scope resolution correctness.

### Step 7: Wire config to caches (state.rs + backend.rs)

**state.rs**: Add `pub fn resize_caches(&self, config: &CrossFileConfig)` to `WorldState` that calls `resize()` on each cache.

**backend.rs**: In `parse_cross_file_config()`, parse `crossFile.cache.*` settings:
```json
{
  "crossFile": {
    "cache": {
      "metadataMaxEntries": 1000,
      "fileContentMaxEntries": 500,
      "existenceMaxEntries": 2000,
      "workspaceIndexMaxEntries": 5000
    }
  }
}
```

In `initialize()`, call `resize_caches()` after parsing config. Caches start with defaults, then get resized — `LruCache::resize()` handles capacity changes correctly (evicts LRU entries if shrinking).

### Step 8: VS Code extension settings

**package.json**: Add settings under `raven.crossFile.cache.*` with type, default, minimum, and description.

**extension.ts**: Read settings via `getExplicitSetting()` and include in initialization options under `crossFile.cache`.

## Testing Strategy

1. **Existing tests pass**: All existing unit and property tests should still pass after migration (API is preserved).

2. **New LRU-specific tests** per cache:
   - Insert beyond capacity → oldest entry evicted, newest preserved
   - Recently inserted entries survive eviction
   - `resize()` shrinks cache and evicts entries
   - `invalidate_many()` works correctly with LRU

3. **WorkspaceIndex behavioral change test**: Replace `test_max_files_limit` — verify that inserting at capacity evicts LRU entry instead of rejecting.

4. **Config parsing tests**: Verify `parse_cross_file_config()` correctly parses cache settings.

5. **Run full test suite**: `cargo test -p raven`

## Verification

```bash
cargo build -p raven        # Compiles cleanly
cargo clippy -p raven        # No warnings
cargo test -p raven          # All tests pass
```
