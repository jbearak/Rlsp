# Trace Logging Instructions

To debug the "parent file not found" issue, I've added comprehensive trace logging to the path resolution and file existence checking logic.

## How to Enable Trace Logging

1. **Set the environment variable** before starting VS Code:
   ```bash
   export RUST_LOG=rlsp=trace
   code .
   ```

   Or on a single line:
   ```bash
   RUST_LOG=rlsp=trace code .
   ```

2. **Reload the window** in VS Code (Cmd+Shift+P → "Developer: Reload Window")

3. **Open the Output panel** (View → Output or Cmd+Shift+U)

4. **Select "Rlsp" from the dropdown** in the Output panel (it should now appear!)

5. **Open `subdir/child.r`** to trigger diagnostics

## What to Look For

The trace logs will show:

### Path Resolution
```
resolve_path: attempting to resolve '../oos.r' with context: file_path=/path/to/subdir/child.r, wd=None
resolve_path: resolved to PathBuf: /path/to/oos.r
resolve_path: converted to URI: file:///path/to/oos.r
```

### File Existence Check
```
file_exists: checking if URI exists: file:///path/to/oos.r
file_exists: filesystem check for path '/path/to/oos.r': true
```

## Expected Behavior

If the fix is working correctly, you should see:
- Path resolves to the correct absolute path
- URI is created correctly
- Filesystem check returns `true`
- **No diagnostic should appear**

## If Still Failing

If you still see the "Parent file not found" error, the trace logs will reveal:
- Whether path resolution is failing (no "resolved to PathBuf" message)
- Whether URI conversion is failing (no "converted to URI" message)
- Whether the filesystem check is returning false despite the file existing
- Whether there's a mismatch between the resolved URI and the actual file location

## Note

The extension has been updated to properly capture server logs in the Output panel. After reloading VS Code with the environment variable set, you should now see "Rlsp" in the Output dropdown.

## Sharing Logs

Please share the relevant trace log output showing:
1. The path resolution attempt for `../oos.r`
2. The file existence check
3. Any error messages

This will help identify exactly where the issue is occurring.
