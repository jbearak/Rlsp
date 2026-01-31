# Manual Testing Guide for Task 16.3

## Overview

This guide provides step-by-step instructions for manually testing the cross-file awareness feature in VS Code after building and installing the rlsp extension.

## Prerequisites

✅ **Build Complete**: The rlsp extension has been successfully built and installed using `./setup.sh`
- Binary installed to: `~/bin/rlsp`
- Extension installed: `rlsp-0.1.0.vsix`

## Test Files Created

The following test files have been created in the workspace:

1. **validation_functions/get_colnames.r** - Defines `get_colnames()` function
2. **validation_functions/collate.r** - Sources get_colnames.r and uses the function
3. **oos.r** - Parent file that sources subdir/child.r
4. **subdir/child.r** - Child file with `@lsp-run-by: ../oos.r` directive

## Test Scenarios

### Test 1: validation_functions/collate.r Scenario

**Objective**: Verify that `get_colnames()` is not marked as undefined when sourced from another file.

**Steps**:
1. Open VS Code in the rlsp workspace directory
2. Open `validation_functions/collate.r`
3. Observe the `get_colnames(my_data)` line (line 5)

**Expected Results**:
- ✅ `get_colnames` should NOT be underlined with a red squiggle (undefined error)
- ✅ Hovering over `get_colnames` should show function information
- ✅ Ctrl+Click (or Cmd+Click on Mac) on `get_colnames` should jump to its definition in get_colnames.r

**Actual Results**:
- [ ] Pass / [ ] Fail
- Notes: _______________________

### Test 2: Completion for get_colnames

**Objective**: Verify that completion shows `get_colnames` after the source() call.

**Steps**:
1. Open `validation_functions/collate.r`
2. Position cursor after line 2 (after the source() call)
3. Type `get_` and trigger completion (Ctrl+Space)

**Expected Results**:
- ✅ Completion list should include `get_colnames`
- ✅ Completion should show function signature

**Actual Results**:
- [ ] Pass / [ ] Fail
- Notes: _______________________

### Test 3: Backward Directive - No "parent file not found" Error

**Objective**: Verify that the backward directive `@lsp-run-by: ../oos.r` does not produce an error.

**Steps**:
1. Open `subdir/child.r`
2. Observe the first line with the directive: `# @lsp-run-by: ../oos.r`

**Expected Results**:
- ✅ No diagnostic error about "parent file not found"
- ✅ No red squiggle on the directive line
- ✅ The directive should be recognized (check LSP logs if available)

**Actual Results**:
- [ ] Pass / [ ] Fail
- Notes: _______________________

### Test 4: Symbol from Child in Parent

**Objective**: Verify that `my_function()` from child.r is available in oos.r.

**Steps**:
1. Open `oos.r`
2. Observe the `my_function()` call on line 5

**Expected Results**:
- ✅ `my_function` should NOT be marked as undefined
- ✅ Hovering over `my_function` should show function information
- ✅ Ctrl+Click should jump to definition in subdir/child.r

**Actual Results**:
- [ ] Pass / [ ] Fail
- Notes: _______________________

## Debugging Tips

### Enable LSP Logging

To see detailed logs from the rlsp server:

1. Open VS Code settings (Cmd+, or Ctrl+,)
2. Search for "rlsp"
3. Enable any logging options if available
4. Check the Output panel (View > Output) and select "Rlsp" from the dropdown

### Check Server Status

1. Open Command Palette (Cmd+Shift+P or Ctrl+Shift+P)
2. Type "Developer: Show Running Extensions"
3. Verify that the Rlsp extension is running

### Restart Language Server

If the server seems unresponsive:

1. Open Command Palette
2. Type "Developer: Reload Window" or restart VS Code

### Check Binary Path

Verify the extension is using the correct binary:

1. Open VS Code settings
2. Search for "rlsp.server.path"
3. Should be empty (uses bundled binary) or point to `~/bin/rlsp`

## Known Issues

Based on previous testing, the following issues may still exist:

1. **Cross-file symbols not recognized**: If symbols from sourced files are still marked as undefined, this indicates the cross-file resolution is not working in the LSP handlers.

2. **Backward directive errors**: If "parent file not found" errors appear, this indicates path resolution issues with the `../` relative path.

3. **No completion for sourced symbols**: If completion doesn't show symbols from sourced files, this indicates the completion handler is not using cross-file scope resolution.

## Reporting Results

After completing all tests, document the results:

1. Mark each test as Pass or Fail
2. Add detailed notes about any failures
3. Include screenshots if helpful
4. Check LSP logs for any error messages
5. Report findings back to the development team

## Next Steps

Based on test results:

- **All tests pass**: Cross-file awareness is working correctly! ✅
- **Some tests fail**: Document failures and investigate root causes
- **All tests fail**: Verify the extension is installed correctly and the server is running

## Additional Verification

### Check Extension Installation

```bash
code --list-extensions | grep rlsp
```

Should show the rlsp extension.

### Check Binary Version

```bash
~/bin/rlsp --version
```

Should show version information (if implemented).

### Verify Test Files

```bash
ls -la validation_functions/
ls -la subdir/
```

Should show the test files created.

## Conclusion

This manual testing phase is critical to verify that the cross-file awareness feature works in real-world usage, not just in automated tests. Take your time with each test scenario and document all observations.
