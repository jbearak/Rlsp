# Cross-File Awareness

Raven understands relationships between R source files through `source()` calls and special comment directives, providing accurate symbol resolution, diagnostics, and navigation across file boundaries.

## Automatic source() Detection

The LSP automatically detects `source()` and `sys.source()` calls:
- Supports both single and double quotes: `source("path.R")` or `source('path.R')`
- Handles named arguments: `source(file = "path.R")`
- Detects `local = TRUE` and `chdir = TRUE` parameters
- Skips dynamic paths (variables, expressions) gracefully

## LSP Directives

All directives support optional colon and quotes: `# @lsp-sourced-by: "../main.R"` is equivalent to `# @lsp-sourced-by ../main.R`.

### Backward Directives

Declare that this file is sourced by another file:
```r
# @lsp-sourced-by ../main.R
# @lsp-run-by ../main.R        # synonym
# @lsp-included-by ../main.R   # synonym
```

Optional parameters:
- `line=N` - Specify 1-based line number in parent where source() call occurs
- `match="pattern"` - Specify text pattern to find source() call in parent

Example with line number:
```r
# @lsp-sourced-by ../main.R line=15
my_function <- function(x) { x + 1 }
```

Example with match pattern:
```r
# @lsp-sourced-by ../main.R match="source("
# The LSP will search for "source(" in main.R and use the first match
# on a line containing a source() call to this file
```

**Call-site inference:** When neither `line=` nor `match=` is specified, the LSP will scan the parent file for `source()` or `sys.source()` calls that reference this file and use the first match as the call site. If no match is found, the configured default (`assumeCallSite`) is used.

### Forward Directives

Explicitly declare source() calls (useful for dynamic paths):
```r
# @lsp-source utils/helpers.R
```

### Working Directory Directives

Set working directory context for path resolution:
```r
# @lsp-working-directory /data/scripts
# @lsp-working-dir /data/scripts     # synonym
# @lsp-current-directory /data/scripts  # synonym
# @lsp-current-dir /data/scripts     # synonym
# @lsp-wd /data/scripts              # synonym
# @lsp-cd /data/scripts              # synonym
```

Path resolution:
- Paths starting with `/` are workspace-root-relative (e.g., `/data` -> `<workspace>/data`)
- Other paths are file-relative (e.g., `../shared` -> parent directory's `shared`)

### Diagnostic Suppression

```r
# @lsp-ignore           # Suppress diagnostics on current line
# @lsp-ignore-next      # Suppress diagnostics on next line
```

## Position-Aware Symbol Availability

Symbols from sourced files are only available AFTER the source() call site:
```r
x <- 1
source("a.R")  # Symbols from a.R available after this line
y <- foo()     # foo() from a.R is now in scope
```

## Symbol Recognition (v1 Model)

The LSP recognizes the following R constructs as symbol definitions:

**Function definitions:**
- `name <- function(...) ...`
- `name = function(...) ...`
- `name <<- function(...) ...`

**Variable definitions:**
- `name <- <expr>`
- `name = <expr>`
- `name <<- <expr>`

**String-literal assign():**
- `assign("name", <expr>)` - only when the name is a string literal

**Limitations:**
- Dynamic `assign()` calls (e.g., `assign(varname, value)`) are not recognized
- `set()` calls are not recognized
- Only top-level assignments are tracked for cross-file scope

Undefined variable diagnostics are only suppressed for symbols recognized by this model.

## Symbol Removal Tracking (rm/remove)

The LSP tracks when variables are removed from scope via `rm()` or `remove()` calls. This enables accurate undefined variable diagnostics when code uses `rm()` to delete variables.

**Supported Patterns:**

| Pattern | Extracted Symbols |
|---------|-------------------|
| `rm(x)` | `["x"]` |
| `rm(x, y, z)` | `["x", "y", "z"]` |
| `rm(list = "x")` | `["x"]` |
| `rm(list = c("x", "y"))` | `["x", "y"]` |
| `remove(x)` | `["x"]` |
| `rm(x, list = c("y", "z"))` | `["x", "y", "z"]` |

**Unsupported Patterns (No Symbols Extracted):**

| Pattern | Reason |
|---------|--------|
| `rm(list = var)` | Dynamic variable - cannot determine symbols at static analysis time |
| `rm(list = ls())` | Dynamic expression - result depends on runtime state |
| `rm(list = ls(pattern = "..."))` | Pattern-based removal - cannot determine matching symbols statically |
| `rm(x, envir = my_env)` | Non-default environment - removal doesn't affect global scope tracking |

**Behavior:**
- `rm()` and `remove()` are treated identically (they are aliases in R)
- Removals inside functions only affect that function's local scope
- Removals at the top-level affect global scope
- Symbols can be re-defined after removal and will be back in scope
- The `envir=` argument is checked: calls with `envir = globalenv()` or `envir = .GlobalEnv` are processed normally, but any other `envir=` value causes the call to be ignored for scope tracking

**Example:**
```r
x <- 1
y <- 2
rm(x)
# x is no longer in scope here - using x would trigger undefined variable diagnostic
# y is still in scope
x <- 3  # x is back in scope after re-definition
```

## Usage Examples

### Basic Cross-File Setup
```r
# main.R
source("utils.R")
result <- helper_function(42)  # helper_function from utils.R
```

```r
# utils.R
helper_function <- function(x) { x * 2 }
```

### Backward Directive with Call-Site
```r
# child.R
# @lsp-sourced-by ../main.R line=10
# Symbols from main.R (lines 1-9) are available here
my_var <- parent_var + 1
```

### Working Directory Override
```r
# scripts/analysis.R
# @lsp-working-directory /data
source("helpers.R")  # Resolves to <workspace>/data/helpers.R
```

### Forward Directive for Dynamic Paths
```r
# main.R
# When source() path is computed dynamically, use @lsp-source to tell the LSP
config_file <- paste0(env, "_config.R")
source(config_file)  # LSP can't resolve this dynamically

# @lsp-source configs/dev_config.R
# Now the LSP knows about symbols from dev_config.R
```

### Circular Dependency Detection
```r
# a.R
source("b.R")  # ERROR: Circular dependency if b.R sources a.R
```

```r
# b.R
source("a.R")  # Creates cycle back to a.R
```
