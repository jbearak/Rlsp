# Configuration

All settings can be configured via VS Code settings or LSP initialization options.

## Cross-File Settings

| Setting | Default | Description |
|---------|---------|-------------|
| `raven.crossFile.maxBackwardDepth` | 10 | Maximum depth for backward directive traversal |
| `raven.crossFile.maxForwardDepth` | 10 | Maximum depth for forward source() traversal |
| `raven.crossFile.maxChainDepth` | 20 | Maximum total chain depth (emits diagnostic when exceeded) |
| `raven.crossFile.assumeCallSite` | "end" | Default call site when not specified ("end" or "start") |
| `raven.crossFile.indexWorkspace` | true | Enable workspace file indexing |
| `raven.crossFile.maxRevalidationsPerTrigger` | 10 | Max open documents to revalidate per change |
| `raven.crossFile.revalidationDebounceMs` | 200 | Debounce delay for cross-file diagnostics (ms) |

## Diagnostic Severity Settings

| Setting | Default | Description |
|---------|---------|-------------|
| `raven.crossFile.missingFileSeverity` | "warning" | Severity for missing file diagnostics |
| `raven.crossFile.circularDependencySeverity` | "error" | Severity for circular dependency diagnostics |
| `raven.crossFile.maxChainDepthSeverity` | "warning" | Severity for max chain depth exceeded diagnostics |
| `raven.crossFile.outOfScopeSeverity` | "warning" | Severity for out-of-scope symbol diagnostics |
| `raven.crossFile.ambiguousParentSeverity` | "warning" | Severity for ambiguous parent diagnostics |
| `raven.diagnostics.undefinedVariables` | true | Enable undefined variable diagnostics |

## Package Settings

| Setting | Default | Description |
|---------|---------|-------------|
| `raven.packages.enabled` | true | Enable/disable package function awareness |
| `raven.packages.additionalLibraryPaths` | [] | Additional R library paths for package discovery |
| `raven.packages.rPath` | auto-detect | Path to R executable for subprocess calls |
| `raven.packages.missingPackageSeverity` | "warning" | Severity for missing package diagnostics |

## Severity Values

All severity settings accept one of:
- `"error"` - Displayed as error (red squiggle)
- `"warning"` - Displayed as warning (yellow squiggle)
- `"information"` - Displayed as info (blue squiggle)
- `"hint"` - Displayed as hint (subtle indicator)
- `"off"` - Diagnostic disabled
