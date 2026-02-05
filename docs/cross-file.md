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

Explicitly declare that this file sources another file (useful for dynamic or conditional paths):
```r
# @lsp-source utils/helpers.R
# @lsp-run utils/helpers.R        # synonym
# @lsp-include utils/helpers.R    # synonym
```

All syntax variations are supported:
- With or without `@` prefix: `@lsp-source` or `lsp-source`
- With or without colon: `@lsp-source: path` or `@lsp-source path`
- With quotes: `@lsp-source "path/with spaces.R"` or `@lsp-source 'path.R'`
- Without quotes: `@lsp-source path.R`

Optional `line=N` parameter specifies the call-site (1-based line number):
```r
# @lsp-source utils/helpers.R line=25
# Symbols from helpers.R become available at line 25 (not at this directive's line)
```

When `line=N` is omitted, symbols become available after the directive's own line.

**Path resolution:** Forward directives use `@lsp-cd` for path resolution (unlike backward directives which ignore it). This matches the behavior of `source()` calls, since forward directives are semantically equivalent to `source()` calls.

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

#### Critical: @lsp-cd Affects Forward Directives and source() Only

| Directive Type | Examples | Uses @lsp-cd? | Rationale |
|----------------|----------|---------------|-----------|
| **Forward** | `@lsp-source`, `@lsp-run`, `@lsp-include` | **YES** | Semantically equivalent to `source()` calls |
| **Backward** | `@lsp-sourced-by`, `@lsp-run-by`, `@lsp-included-by` | **NO** | Describes static file relationships |
| **source() calls** | `source("file.R")` | **YES** | Runtime behavior affected by working directory |

**Why this distinction?**
- **Forward directives** and **source() calls** describe runtime execution behavior. They are semantically equivalent to R's `source()` function, which is affected by the current working directory at runtime.
- **Backward directives** describe static file relationships from the child's perspective. They declare "this file is sourced by that parent file" - a relationship that should NOT change based on runtime working directory.

**Example:**
```r
# File: subdir/child.R
# @lsp-cd /some/other/directory
# @lsp-run-by ../parent.R      # Resolves to parent.R in workspace root (ignores @lsp-cd)
# @lsp-source utils.R          # Resolves to /some/other/directory/utils.R (uses @lsp-cd)

source("helpers.R")            # Resolves to /some/other/directory/helpers.R (uses @lsp-cd)
```

### Working Directory Inheritance

When a child file uses a backward directive (`@lsp-sourced-by`, `@lsp-run-by`, `@lsp-included-by`) without its own explicit `@lsp-cd`, it inherits the working directory from its parent file. This enables accurate `source()` path resolution in child files that are run from a parent's working directory context.

**Precedence rules:**
1. Explicit `@lsp-cd` in the child file takes precedence (no inheritance)
2. If no explicit `@lsp-cd`, the child inherits from its parent's effective working directory
3. Inheritance is transitive: if the parent also inherits, the chain is followed

**Example:**
```r
# parent.R
# @lsp-cd /data/project
source("child.R")  # Resolves to /data/project/child.R
```

```r
# child.R
# @lsp-run-by ../parent.R
# No explicit @lsp-cd, so inherits /data/project from parent
source("utils.R")  # Resolves to /data/project/utils.R (not relative to child.R's directory)
```

**Effect on source() resolution:** The inherited working directory affects how `source()` calls in the child file resolve their paths. This matches R's runtime behavior when a parent script sets the working directory before sourcing a child.

### Diagnostic Suppression

```r
# @lsp-ignore           # Suppress diagnostics on current line
# @lsp-ignore-next      # Suppress diagnostics on next line
```

### Declaration Directives

Declare symbols that are created dynamically and cannot be statically detected:

**Variable declarations:**
```r
# @lsp-var myvar
# @lsp-variable myvar           # synonym
# @lsp-declare-var myvar        # synonym
# @lsp-declare-variable myvar   # synonym
```

**Function declarations:**
```r
# @lsp-func myfunc
# @lsp-function myfunc          # synonym
# @lsp-declare-func myfunc      # synonym
# @lsp-declare-function myfunc  # synonym
```

All syntax variations are supported:
- With or without colon: `@lsp-var: myvar` or `@lsp-var myvar`
- With quotes for special characters: `@lsp-var "my.var"` or `@lsp-var 'my.var'`

**Important:** Unlike source directives, declaration directives require the `@` prefix. `# lsp-var myvar` (without `@`) is NOT recognized.

**Position-aware behavior:** Declared symbols are available starting from the line *after* the directive:
```r
# @lsp-var data_from_api
x <- data_from_api  # ERROR: data_from_api not yet in scope (same line as directive effect)

# @lsp-var data_from_api
# Now on a new line...
x <- data_from_api  # OK: data_from_api is in scope
```

**Cross-file inheritance:** Declarations propagate to sourced child files:
```r
# parent.R
# @lsp-var shared_data
source("child.R")  # child.R can use shared_data
```

**Use cases:**
```r
# Dynamic assignment via assign()
assign(paste0("var_", i), value)
# @lsp-var var_1
# @lsp-var var_2

# Loading data from external sources
load("data.RData")  # Creates objects dynamically
# @lsp-var model_fit
# @lsp-var training_data

# eval() with constructed expressions
eval(parse(text = code_string))
# @lsp-func dynamic_function
```

**LSP features for declared symbols:**
- **Completions**: Declared symbols appear in completion lists with appropriate kind (variable/function)
- **Hover**: Shows "Declared via @lsp-var directive at line N"
- **Go-to-definition**: Navigates to the directive line
- **Diagnostics**: Suppresses "undefined variable" warnings for declared symbols

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

### Forward Directive with Working Directory
```r
# scripts/runner.R
# @lsp-cd /data/project
# @lsp-source utils.R
# utils.R resolves to <workspace>/data/project/utils.R (uses @lsp-cd)
```

### Forward Directive with Explicit Call-Site
```r
# main.R
# @lsp-source helpers.R line=50
# Symbols from helpers.R become available at line 50
# This is useful when the actual source() call is later in the file
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
