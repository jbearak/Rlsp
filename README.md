# Rlsp

A static R Language Server with workspace symbol indexing for fast, dependency-free R development support.

## Quick Start

```bash
./setup.sh
```

## Features

- **Diagnostics** - Static code analysis and error detection
- **Go-to-definition** - Navigate to symbol definitions
- **Find references** - Locate all symbol usages
- **Completions** - Intelligent code completion
- **Hover** - Symbol information on hover
- **Document symbols** - Outline view for R files
- **Workspace indexing** - Project-wide symbol resolution
- **Package-aware analysis** - Understanding of R package structure
- **Cross-file awareness** - Symbol resolution across `source()` chains

## Cross-File Awareness

Rlsp understands relationships between R source files through `source()` calls and special comment directives, providing accurate symbol resolution, diagnostics, and navigation across file boundaries.

### Automatic source() Detection

The LSP automatically detects `source()` and `sys.source()` calls:
- Supports both single and double quotes: `source("path.R")` or `source('path.R')`
- Handles named arguments: `source(file = "path.R")`
- Detects `local = TRUE` and `chdir = TRUE` parameters
- Skips dynamic paths (variables, expressions) gracefully

### LSP Directives

All directives support optional colon and quotes: `# @lsp-sourced-by: "../main.R"` is equivalent to `# @lsp-sourced-by ../main.R`.

#### Backward Directives
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

#### Forward Directives
Explicitly declare source() calls (useful for dynamic paths):
```r
# @lsp-source utils/helpers.R
```

#### Working Directory Directives
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
- Paths starting with `/` are workspace-root-relative (e.g., `/data` → `<workspace>/data`)
- Other paths are file-relative (e.g., `../shared` → parent directory's `shared`)

#### Diagnostic Suppression
```r
# @lsp-ignore           # Suppress diagnostics on current line
# @lsp-ignore-next      # Suppress diagnostics on next line
```

### Position-Aware Symbol Availability

Symbols from sourced files are only available AFTER the source() call site:
```r
x <- 1
source("a.R")  # Symbols from a.R available after this line
y <- foo()     # foo() from a.R is now in scope
```

### Symbol Recognition (v1 Model)

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

### Symbol Removal Tracking (rm/remove)

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

### Configuration Options

Configure via VS Code settings or LSP initialization:

| Setting | Default | Description |
|---------|---------|-------------|
| `rlsp.crossFile.maxBackwardDepth` | 10 | Maximum depth for backward directive traversal |
| `rlsp.crossFile.maxForwardDepth` | 10 | Maximum depth for forward source() traversal |
| `rlsp.crossFile.maxChainDepth` | 20 | Maximum total chain depth (emits diagnostic when exceeded) |
| `rlsp.crossFile.assumeCallSite` | "end" | Default call site when not specified ("end" or "start") |
| `rlsp.crossFile.indexWorkspace` | true | Enable workspace file indexing |
| `rlsp.crossFile.maxRevalidationsPerTrigger` | 10 | Max open documents to revalidate per change |
| `rlsp.crossFile.revalidationDebounceMs` | 200 | Debounce delay for cross-file diagnostics (ms) |
| `rlsp.crossFile.missingFileSeverity` | "warning" | Severity for missing file diagnostics |
| `rlsp.crossFile.circularDependencySeverity` | "error" | Severity for circular dependency diagnostics |
| `rlsp.crossFile.maxChainDepthSeverity` | "warning" | Severity for max chain depth exceeded diagnostics |
| `rlsp.crossFile.outOfScopeSeverity` | "warning" | Severity for out-of-scope symbol diagnostics |
| `rlsp.crossFile.ambiguousParentSeverity` | "warning" | Severity for ambiguous parent diagnostics |
| `rlsp.diagnostics.undefinedVariables` | true | Enable undefined variable diagnostics |

### Usage Examples

#### Basic Cross-File Setup
```r
# main.R
source("utils.R")
result <- helper_function(42)  # helper_function from utils.R
```

```r
# utils.R
helper_function <- function(x) { x * 2 }
```

#### Backward Directive with Call-Site
```r
# child.R
# @lsp-sourced-by ../main.R line=10
# Symbols from main.R (lines 1-9) are available here
my_var <- parent_var + 1
```

#### Working Directory Override
```r
# scripts/analysis.R
# @lsp-working-directory /data
source("helpers.R")  # Resolves to <workspace>/data/helpers.R
```

#### Forward Directive for Dynamic Paths
```r
# main.R
# When source() path is computed dynamically, use @lsp-source to tell the LSP
config_file <- paste0(env, "_config.R")
source(config_file)  # LSP can't resolve this dynamically

# @lsp-source configs/dev_config.R
# Now the LSP knows about symbols from dev_config.R
```

#### Circular Dependency Detection
```r
# a.R
source("b.R")  # ERROR: Circular dependency if b.R sources a.R
```

```r
# b.R
source("a.R")  # Creates cycle back to a.R
```

## Package Function Awareness

Rlsp recognizes functions, variables, and datasets exported by R packages loaded via `library()`, `require()`, or `loadNamespace()` calls. This enables accurate diagnostics, completions, hover information, and go-to-definition for package symbols.

### How It Works

When you load a package with `library(dplyr)`, Rlsp:
1. Detects the library call and extracts the package name
2. Queries R (via subprocess) to get the package's exported symbols
3. Makes those symbols available for completions, hover, and diagnostics
4. Suppresses "undefined variable" warnings for package exports

### Base Package Handling

Base R packages are always available without explicit `library()` calls:
- **base** - Core R functions (`c`, `list`, `print`, `sum`, etc.)
- **methods** - S4 methods and classes
- **utils** - Utility functions (`head`, `tail`, `str`, etc.)
- **grDevices** - Graphics devices
- **graphics** - Base graphics functions
- **stats** - Statistical functions (`lm`, `t.test`, `cor`, etc.)
- **datasets** - Built-in datasets (`mtcars`, `iris`, etc.)

At startup, Rlsp queries R for the default search path using `.packages()`. If R is unavailable, it falls back to the hardcoded list above.

### Position-Aware Loading

Package exports are only available AFTER the `library()` call, matching R's runtime behavior:

```r
mutate(df, x = 1)  # Warning: undefined variable 'mutate'
library(dplyr)
mutate(df, y = 2)  # OK: dplyr is now loaded
```

### Function-Scoped Loading

When `library()` is called inside a function, the package exports are only available within that function's scope:

```r
my_analysis <- function(data) {
  library(dplyr)
  mutate(data, x = 1)  # OK: dplyr available inside function
}

mutate(df, y = 2)  # Warning: dplyr not available at global scope
```

### Meta-Package Support

Rlsp recognizes meta-packages that attach multiple packages:

**tidyverse** attaches:
- dplyr, readr, forcats, stringr, ggplot2, tibble, lubridate, tidyr, purrr

**tidymodels** attaches:
- broom, dials, dplyr, ggplot2, infer, modeldata, parsnip, purrr, recipes, rsample, tibble, tidyr, tune, workflows, workflowsets, yardstick

```r
library(tidyverse)
# All tidyverse packages are now available
mutate(df, x = 1)      # dplyr
ggplot(df, aes(x, y))  # ggplot2
str_detect(s, "pat")   # stringr
```

### Cross-File Integration

Packages loaded in parent files are available in sourced child files:

```r
# main.R
library(dplyr)
source("analysis.R")  # dplyr available in analysis.R
library(ggplot2)      # NOT available in analysis.R (loaded after source)
```

```r
# analysis.R
# @lsp-sourced-by main.R
result <- mutate(df, x = 1)  # OK: dplyr loaded in parent before source()
```

Packages loaded in child files do NOT propagate back to parent files (forward-only propagation).

### Diagnostics

Rlsp provides helpful diagnostics for package-related issues:

| Diagnostic | Description |
|------------|-------------|
| Undefined variable | Symbol used before package is loaded |
| Missing package | `library()` references a package not installed on the system |

### Configuration Options

| Setting | Default | Description |
|---------|---------|-------------|
| `rlsp.packages.enabled` | `true` | Enable/disable package function awareness |
| `rlsp.packages.additionalLibraryPaths` | `[]` | Additional R library paths for package discovery |
| `rlsp.packages.rPath` | auto-detect | Path to R executable for subprocess calls |
| `rlsp.packages.missingPackageSeverity` | `"warning"` | Severity for missing package diagnostics |

### Supported Library Call Patterns

| Pattern | Supported |
|---------|-----------|
| `library(pkgname)` | ✓ |
| `library("pkgname")` | ✓ |
| `library('pkgname')` | ✓ |
| `require(pkgname)` | ✓ |
| `loadNamespace("pkgname")` | ✓ |
| `library(pkg, character.only = TRUE)` | ✗ (dynamic) |
| `library(get("pkg"))` | ✗ (dynamic) |

Dynamic package names (variables, expressions, `character.only = TRUE`) are skipped gracefully.

## Differences from Other R Language Servers

### vs Ark LSP
Rlsp is the extracted and focused LSP component from Ark. Ark includes additional features like Jupyter kernel support and Debug Adapter Protocol (DAP), while Rlsp focuses solely on language server functionality.

### vs R Language Server
Rlsp provides static analysis without requiring an R runtime, while the R Language Server uses dynamic introspection with a running R session. This makes Rlsp faster to start and more suitable for environments without R installed.

## Why Use Rlsp

- **Fast startup** - No R runtime initialization required
- **No R dependencies** - Works without R installation for basic features
- **Workspace-wide symbol resolution** - Understands your entire project structure
- **Package-aware diagnostics** - Intelligent analysis of R package code
- **Cross-file awareness** - Understands source() chains and file relationships

## Installation

### Building from Source
```bash
git clone <repository-url>
cd rlsp
./setup.sh
```

### Download from Releases
Pre-built binaries are available from the [releases page](../../releases).

## Releases

Releases use semantic versioning with git tags. Creating a tag in the format `vX.Y.Z` automatically triggers CI to build and publish a new release.

## Attribution

**Rlsp is extracted from [Ark's](https://github.com/posit-dev/ark) static LSP implementation.** We gratefully acknowledge the Ark project for providing the foundation for this language server.

**Inspired by and complementary to the [R Language Server](https://github.com/REditorSupport/languageserver).** Both projects serve the R community with different approaches to language server functionality.

## License
[GPLv3](LICENSE) - This project is open source software. You can use, modify, and distribute it with attribution, but any derivative works must also be open source under GPLv3.
