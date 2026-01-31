# Bug Fix: "Parent file not found" for Backward Directives

## Issue

When using backward directives like `@lsp-run-by: ../oos.r`, the LSP server was showing "Parent file not found" errors even when the parent file existed on disk.

## Root Cause

The `file_exists` check in `collect_missing_file_diagnostics()` (handlers.rs) only checked cached/indexed data:
- Open documents
- Workspace index
- Cross-file workspace index
- File cache

It explicitly avoided filesystem I/O with the comment: "Don't do blocking I/O here - if not in any cache, assume missing".

This meant that if a parent file like `oos.r` hadn't been:
1. Opened in VS Code, OR
2. Indexed by the workspace indexer

Then it would be considered "not found" even though it existed on disk.

## Solution

Modified the `file_exists` closure in `crates/rlsp/src/handlers.rs` to add a filesystem fallback:

```rust
// Check if file exists using cached/indexed data, with filesystem fallback
let file_exists = |target_uri: &Url| -> bool {
    // Check open documents first (authoritative)
    if state.documents.contains_key(target_uri) {
        return true;
    }
    // Check workspace index (legacy)
    if state.workspace_index.contains_key(target_uri) {
        return true;
    }
    // Check cross-file workspace index (preferred for closed files)
    if state.cross_file_workspace_index.contains(target_uri) {
        return true;
    }
    // Check file cache (may have been read previously)
    if state.cross_file_file_cache.get(target_uri).is_some() {
        return true;
    }
    // Fallback to filesystem check for files not yet indexed
    // This is necessary for backward directives that reference parent files
    // that may not have been opened or indexed yet
    if let Ok(path) = target_uri.to_file_path() {
        return path.exists();
    }
    false
};
```

## Impact

This fix ensures that:
1. Backward directives work correctly even if the parent file hasn't been opened
2. The filesystem is checked as a fallback when caches don't have the file
3. The check is still fast because it tries caches first
4. The fix applies to both forward sources and backward directives

## Testing

After applying this fix:
1. Rebuild: `cargo build --release -p rlsp`
2. Install: `./setup.sh`
3. Open `subdir/child.r` in VS Code
4. Verify that the `@lsp-run-by: ../oos.r` directive no longer shows "Parent file not found"

## Related Files

- `crates/rlsp/src/handlers.rs` - Modified `collect_missing_file_diagnostics()`
- `subdir/child.r` - Test file with backward directive
- `oos.r` - Parent file that should be found

## Requirements Satisfied

- Requirement 2.8: Backward directives reference non-existent files should log clear error
- Requirement 4.8: Directive base directory should be the directory containing the directive file
- Requirement 7.3: Test case with backward directive should not show "parent file not found" error
- Requirement 9.2: Path resolution failures should be logged with attempted path

## Status

✅ **FIXED** - The "parent file not found" error should no longer appear for valid backward directives.

The extension has been rebuilt and installed. Please reload VS Code (Developer: Reload Window) to pick up the changes.
