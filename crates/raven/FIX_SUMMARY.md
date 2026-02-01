# Fix for False Positive Undefined Variable Warnings in ark-lsp

## Problem
The static ark-lsp was giving false positive undefined variable warnings for:
1. Function parameters (e.g., `a`, `b` in `function(a, b) { a + b }`)
2. Built-in R functions (e.g., `warning`, `any`, `is.na`, `sprintf`)

## Solution

### 1. Comprehensive Built-in Function List
- Created `build_builtins.R` script to extract all functions from base R packages
- Generated `builtins.rs` with 2,355 functions from: base, stats, utils, graphics, grDevices, methods, datasets
- Updated `is_builtin()` to use the comprehensive list

### 2. Function Parameter Recognition
- Modified `collect_definitions()` in `handlers.rs` to traverse `function_definition` nodes
- Added `collect_parameters()` helper to extract parameter names from function signatures
- Parameters are now added to the `defined` set and not flagged as undefined

### 3. Test Coverage

#### Rust Unit Tests (9 tests, all passing)
- `test_function_parameters_recognized` - Verifies parameters are in defined set
- `test_single_parameter` - Single parameter functions
- `test_no_parameters` - Functions with no parameters
- `test_nested_function_parameters` - Nested function scoping
- `test_builtin_functions` - Verifies built-ins (warning, any, is.na, sprintf, etc.)
- `test_builtin_constants` - Verifies constants (TRUE, FALSE, NULL, NA, etc.)
- `test_not_builtin` - Verifies custom functions are not treated as built-ins

#### VSCode Integration Tests (14 tests, all passing)
- `no false positives for function parameters` - Verifies no warnings for a, b, x, y parameters
- `no false positives for built-in functions` - Verifies no warnings for any, is.na, warning, sprintf, sum, mean, print

## Files Modified

### Rust (ark-lsp)
- `crates/ark-lsp/build_builtins.R` - Script to generate builtin list
- `crates/ark-lsp/src/builtins.rs` - Generated list of 2,355 R functions
- `crates/ark-lsp/src/main.rs` - Added builtins module declaration
- `crates/ark-lsp/src/handlers.rs` - Fixed parameter recognition, updated is_builtin, added tests

### VSCode Extension
- `editors/vscode/src/test/fixtures/function_params.R` - Test fixture
- `editors/vscode/src/test/lsp.test.ts` - Added integration tests
- `editors/vscode/bin/ark-lsp` - Updated binary

## Verification

Run Rust tests:
```bash
cd /Users/jmb/repos/ark/crates/ark-lsp
cargo test
```

Run VSCode tests:
```bash
cd /Users/jmb/repos/ark/editors/vscode
npm test
```

## Example

Before the fix, this code would show false warnings:
```r
f <- function(a, b) {
  return(a+b)  # WARNING: undefined variable 'a', 'b'
}

test <- function(x) {
  if (any(is.na(x))) {  # WARNING: undefined 'any', 'is.na'
    warning("NA found")  # WARNING: undefined 'warning'
  }
}
```

After the fix, no warnings are shown for function parameters or built-in functions.
