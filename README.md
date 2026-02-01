# Raven

A static R Language Server with cross-file awareness for scientific research workflows.

## Installation

### Building from Source
```bash
git clone <repository-url>
cd raven
./setup.sh
```

### Download from Releases
Pre-built binaries are available from the [releases page](../../releases).

### VS Code Extension
The extension is bundled with the binary. After running `setup.sh`, reload VS Code to activate.

## Quick Start

Raven tracks `source()` chains and understands scope. Consider this project:

```r
# main.R
source("utils.R")
result <- helper_function(42)  # Raven knows helper_function comes from utils.R
```

```r
# utils.R
helper_function <- function(x) { x * 2 }
```

When you open `main.R`, Raven:
1. Detects the `source("utils.R")` call
2. Indexes symbols from `utils.R`
3. Provides completions, hover, and go-to-definition for `helper_function`
4. Only shows `helper_function` as available *after* the source() line

For files that aren't explicitly sourced, add a directive:
```r
# child.R
# @lsp-sourced-by ../main.R
# Now Raven knows this file's context
```

## When to Use Raven

| Feature | Raven | [Ark](https://github.com/posit-dev/ark) | [R Language Server](https://github.com/REditorSupport/languageserver) |
|---------|-------|-----|-------------------|
| Cross-file `source()` tracking | Yes | No | No |
| Position-aware scope | Yes | No | No |
| Workspace symbol indexing | Yes | Completions only | Open files only |
| Works in VS Code | Yes | Positron only | Yes |
| Package export awareness | Yes | Yes | Yes |
| Embedded R runtime | No | Yes | Yes |
| Jupyter kernel | No | Yes | No |
| Debug Adapter (DAP) | No | Yes | No |

**Use Raven when:**
- Your R project spans multiple files connected by `source()`
- You want accurate "undefined variable" diagnostics across file boundaries
- You use VS Code and want cross-file intelligence
- You want fast startup without loading R

**Use [Ark](https://github.com/posit-dev/ark) when:**
- You use Positron IDE
- You need Jupyter notebook or debugging support

**Use [R Language Server](https://github.com/REditorSupport/languageserver) when:**
- You work primarily with single-file scripts
- You need runtime introspection

## Features

- **Cross-file awareness** - Symbol resolution across `source()` chains with position-aware scope
- **Diagnostics** - Undefined variable detection that understands sourced files
- **Go-to-definition** - Navigate to symbol definitions across file boundaries
- **Find references** - Locate all symbol usages project-wide
- **Completions** - Intelligent completion including symbols from sourced files
- **Hover** - Symbol information on hover
- **Document symbols** - Outline view for R files
- **Workspace indexing** - Background indexing of your entire project
- **Package awareness** - Recognition of `library()` calls and package exports

## Documentation

- [Cross-File Awareness](docs/cross-file.md) - Directives, source() detection, symbol resolution
- [Package Function Awareness](docs/packages.md) - library() support, meta-packages, base packages
- [Configuration](docs/configuration.md) - All settings and options

## Provenance

Raven combines code from two sources:

**[Ark](https://github.com/posit-dev/ark)** (MIT License, Posit Software, PBC) - Raven began as a fork of Ark's LSP component. The core LSP infrastructure derives from Ark.

**[Sight](https://github.com/jbearak/sight)** (GPL-3.0) - The cross-file awareness system was ported from Sight, a Stata language server with similar goals.

Both Sight and Raven were written by the same author to address the same problem: scientific research codebases that span many files.

## License

[GPL-3.0](LICENSE). See [NOTICE](NOTICE) for third-party attributions.
