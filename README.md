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

#### Backward Directives
Declare that this file is sourced by another file:
```r
# @lsp-sourced-by ../main.R
# @lsp-run-by ../main.R        # synonym
# @lsp-included-by ../main.R   # synonym
```

Optional parameters:
- `line=N` - Specify 1-based line number in parent where source() call occurs
- `match="pattern"` - Specify text pattern to find source() call in parent (not yet implemented, falls back to default)

Example:
```r
# @lsp-sourced-by ../main.R line=15
my_function <- function(x) { x + 1 }
```

#### Forward Directives
Explicitly declare source() calls (useful for dynamic paths):
```r
# @lsp-source utils/helpers.R
```

#### Working Directory Directives
Set working directory context for path resolution:
```r
# @lsp-working-directory /data/scripts
# @lsp-wd /data/scripts          # synonym
# @lsp-cd /data/scripts          # synonym
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

### Configuration Options

Configure via VS Code settings or LSP initialization:

| Setting | Default | Description |
|---------|---------|-------------|
| `rlsp.crossFile.maxBackwardDepth` | 10 | Maximum depth for backward directive traversal |
| `rlsp.crossFile.maxForwardDepth` | 10 | Maximum depth for forward source() traversal |
| `rlsp.crossFile.maxChainDepth` | 20 | Maximum total chain depth |
| `rlsp.crossFile.assumeCallSite` | "end" | Default call site when not specified ("end" or "start") |
| `rlsp.crossFile.indexWorkspace` | true | Enable workspace file indexing |
| `rlsp.diagnostics.undefinedVariables` | true | Enable undefined variable diagnostics |

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

MIT License