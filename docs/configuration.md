# Configuration

Most settings are exposed as VS Code settings — search for "raven" in Settings (Cmd/Ctrl-,). A handful of advanced server-side knobs are only available via LSP initialization options; those are noted in the tables below.

> Looking for a specific key? See [Settings reference](settings-reference.md) for an alphabetical index of every `raven.*` setting with default, type, `raven.toml` path, and a one-line description. The sections below add context: when each setting matters, how the project file interacts with VS Code settings, and which knobs hang together.

## Project config: `raven.toml`

The recommended way to configure Raven is a `raven.toml` file at the project root. Every editor and the `raven check` / `raven lint` CLIs read this file, so a single committed config governs both interactive editing and CI.

### Discovery

Raven walks upward from the active project root looking for `raven.toml`: the first workspace folder in editors, `--workspace` for `raven check`, and the invocation working directory for `raven lint`. If none is found, `.lintr` is read for linting settings only (subset; see [Linting](linting.md#migrating-from-lintr)), except the literal home-directory `~/.lintr` is ignored by default so user-level `lintr` preferences do not silently affect every project. In VS Code, set `raven.linting.readHomeLintr = true` to include it; in the CLI, pass it explicitly with `--config ~/.lintr`. In multi-root workspaces, open a project folder directly when you need a different folder-specific project config.

### Precedence

Per-key. For each setting, project values win over the LSP client's `initializationOptions` / `did_change_configuration` payload. Keys not pinned by the project file continue to come from client settings (or Raven's defaults if neither layer specifies them).

### Schema

Most `raven.toml` keys mirror the LSP `initializationOptions` shape. The reference tables below cover every key the server reads from `raven.toml` (top-level sections: `workspace`, `linting`, `crossFile`, `packages`, `diagnostics`, `indentation`, `symbols`, `completion`), plus a handful of client-only settings whose behavior is most useful to document alongside them. Those client-only rows have no `raven.toml` path and say so in their description. Other VS Code-only client settings — `raven.sendToR.*`, `raven.rTerminal.*`, `raven.dataViewer.*`, `raven.chunks.*`, `raven.knit.*`, `raven.pandoc.*` — only apply inside VS Code and aren't read from `raven.toml`; they're documented on their feature pages ([R Console](r-console.md), [Data Viewer](data-viewer.md), [Chunks](chunks.md), [Knit](knit.md)). `raven.trace.server` is the standard `vscode-languageclient` LSP-trace setting (`off` / `messages` / `verbose`) — useful when filing bug reports, but otherwise not Raven-specific. The same key in `raven.toml` is at the path indicated.

```toml
[workspace]
exclude = ["generated/**", "archive/**", "!generated/keep.R"]

[linting]
enabled = true
lineLength = 100
lineLengthSeverity = "warning"

[[linting.overrides]]
files = ["tests/**/*.R"]
lineLength = 120

[crossFile]
# tighter than the default of 64
maxChainDepth = 10

[crossFile.diagnostics]
missingFile = "warning"
caseMismatch = "auto"

[packages]
enabled = true

[diagnostics]
enabled = true
jags = "on"
stan = "on"

[diagnostics.severity]
undefinedVariable = "warning"
```

### Per-file overrides

`[[linting.overrides]]` is an array of glob → patch entries. Globs are anchored at the project root. Order matters: later entries win on conflicts. Setting `enabled = false` in an override skips matching files entirely.

Canonical paths shared with Sight and their permanent compatibility aliases
are specified in [Shared project configuration schema](shared-config-schema.md).

### Project exclusions

`[workspace].exclude` is a `raven.toml`-only list of project-root-relative globs. It is not exposed as a VS Code/LSP client setting. The default is `[]`.

These exclusions are broader than lint overrides: Raven ignores matching files for background workspace indexing, dependency discovery, file-watcher/on-demand indexing, package-mode disk seeding, and default `raven check` discovery. Existing index entries that become excluded after a live `raven.toml` reload are removed from Raven's indexes and dependency graph.

Use directory globs such as `generated/**`, `archive/**`, or `**/cache/**` to ignore whole trees. Raven compiles the glob list once when config is recomputed and matches paths relative to the containing workspace root. `*` and `?` do not cross `/`; use `**` for recursive matches, such as `**/*.log` for `.log` files at any depth. Patterns are evaluated in order; a leading `!` re-includes matching paths, so `["generated/**", "!generated/keep.R"]` excludes the generated tree except `generated/keep.R`.

Raven prunes directories during the serial workspace walk only when a positive directory glob proves every descendant is excluded, such as `generated/**`. If any negated pattern is present, directory pruning is disabled for the list so re-included files are never dropped before they can match. Files are still filtered by the full ordered matcher.

Explicit CLI file arguments bypass `[workspace].exclude`: `raven check generated/one.R` reports that file even if it matches an exclusion. Directory arguments are discovery walks, so exclusions still apply inside them. Likewise, a matching file opened in the editor is diagnosed live; exclusion controls bulk discovery and symbol indexing, not an explicit open buffer's diagnostics.

### Live reload

In the LSP/editor, edits to `raven.toml` are picked up live for every section: `[workspace]` (`exclude`), `[linting]` (including `overrides`), `[crossFile]`, `[packages]` (including `packageMode`, `watchLibraryPaths`, `watchDebounceMs`), `[diagnostics]`, `[indentation]`, `[symbols]`, `[completion]`. The discovered `.lintr` is also watched and live-reloaded, but only for the supported linting subset described in [Linting](linting.md#migrating-from-lintr). Workspace and non-home ancestor `.lintr` files are discovered by default; the literal home-directory `~/.lintr` is discovered only when the VS Code/LSP-client setting `raven.linting.readHomeLintr = true` is enabled. Open documents re-publish diagnostics automatically — no Raven restart required. The CLI reads config once per command invocation; pass `--config ~/.lintr` to opt into a literal home `.lintr` for that run.

Package-affecting changes (toggling `[packages].enabled`, `packageMode`, `rprofilePrelude`, `rPath`, `additionalLibraryPaths`, or the watcher knobs) reuse the same reconciliation path as `workspace/didChangeConfiguration`: the package library is rebuilt via R if needed, the libpath watcher is restarted, and any updated completion-trigger registration is re-applied — all asynchronously, off the LSP write lock.

## Diagnostics

| Setting | Default | Description |
|---|---|---|
| `raven.diagnostics.enabled` | `true` | Master switch for all diagnostics. When false, the model-language switches below cannot enable findings. |
| `raven.diagnostics.jags` | `"off"` | Set to `"on"` to enable native syntax diagnostics for standalone JAGS/BUGS files. Portable as `[diagnostics] jags = "on"` in `raven.toml`. Parsing and language intelligence remain active when off. |
| `raven.diagnostics.stan` | `"off"` | Set to `"on"` to enable native syntax and conservative undeclared-variable diagnostics for standalone Stan files. Portable as `[diagnostics] stan = "on"` in `raven.toml`. `undefinedVariableSeverity = "off"` disables only the semantic findings. Parsing and language intelligence remain active when off. |
| `raven.diagnostics.maxSyntaxDiagnosticsPerFile` | `500` | Maximum native Tree-sitter syntax findings retained per Stan or JAGS/BUGS file after exact deduplication and stable source ordering. `0` means unlimited. Does not cap R diagnostics. Portable as `[diagnostics] maxSyntaxDiagnosticsPerFile = 500` in `raven.toml`; the editor and `raven check` use the same value. |

The two model-language switches layer per key like every other project setting:
a value in `raven.toml` overrides the corresponding editor/LSP-client value,
while an unpinned key continues to use the client value or its built-in `"off"`
default. The global `enabled` switch is always dominant.

## Cross-File Settings

| Setting | Default | Description |
|---|---|---|
| `raven.crossFile.indexWorkspace` | `true` | Enable background workspace indexing |
| `raven.crossFile.backwardDependencies` | `"auto"` | How backward dependencies are resolved. `"auto"`: infer from workspace scan. `"explicit"`: require backward directives (e.g. `# raven: sourced-by`) to declare backward dependencies. See [Backward Dependency Modes](cross-file.md#backward-dependency-modes) |
| `raven.crossFile.hoistGlobalsInFunctions` | `true` | Hoist global definitions inside function bodies (late-binding semantics). See [Global Symbol Hoisting](cross-file.md#global-symbol-hoisting). *LSP init-only — not exposed in the VS Code Settings UI.* |
| `raven.crossFile.assumeCallSite` | `"end"` | Default call site when not specified by directive (`"end"` or `"start"`) |
| `raven.crossFile.maxBackwardDepth` | `10` | Maximum depth for backward directive traversal |
| `raven.crossFile.maxForwardDepth` | `10` | Maximum depth for forward source() traversal |
| `raven.crossFile.maxChainDepth` | `64` | Maximum total chain depth (emits diagnostic when exceeded). Also bounds the bidirectional neighborhood-walk depth. |
| `raven.crossFile.maxTransitiveDependentsVisited` | `50000` | Maximum files visited while traversing the cross-file dependency graph. When this budget truncates analysis, dropped `source()` edges can surface as false-positive `undefined-variable` warnings; the editor shows a throttled warning and `raven check` prints a one-line note (on stdout with the diagnostics for `text`, on stderr for `json`/`sarif`). Raise this for very large workspaces. |
| `raven.crossFile.maxRevalidationsPerTrigger` | `10` | Max open documents to revalidate per change |
| `raven.crossFile.revalidationDebounceMs` | `200` | Debounce delay for dependent file diagnostics (ms) |
| `raven.crossFile.editedFileDebounceMs` | `50` | Debounce delay for the actively-edited file (ms) |

### On-demand indexing

| Setting | Default | Description |
|---|---|---|
| `raven.crossFile.onDemandIndexing.enabled` | `true` | Index files referenced by `source()` / directives that aren't currently open, so cross-file features work without opening every dependency. Depth is bounded by `maxForwardDepth` / `maxBackwardDepth` |

### Cache sizes

LRU-evicted; raise these if you have a very large workspace and see repeated re-indexing, lower them to reduce memory. The minimums quoted below are the lower bounds enforced by the VS Code Settings UI; the server itself only clamps each cache to a minimum of `1`, so `raven.toml` and other LSP clients can go lower if they really want to.

| Setting | Default | Description |
|---|---|---|
| `raven.crossFile.cache.metadataMaxEntries` | `1000` | Parsed file metadata (directives, source calls). VS Code UI minimum `100`. |
| `raven.crossFile.cache.fileContentMaxEntries` | `500` | Full file text used during resolution. VS Code UI minimum `50`. |
| `raven.crossFile.cache.existenceMaxEntries` | `2000` | Cached `Path::exists` results for resolved references. VS Code UI minimum `100`. |
| `raven.crossFile.cache.workspaceIndexMaxEntries` | `5000` | Closed-file entries in the cross-file workspace index (parsed metadata + scope artifacts). VS Code UI minimum `100`. |

## Diagnostic Severity Settings

Each accepts: `"error"`, `"warning"`, `"information"` (or its `"info"`
alias), `"hint"`, or `"off"`.

| Setting | Default | Description |
|---|---|---|
| `raven.diagnostics.undefinedVariableSeverity` | `"warning"` | Variable used but not defined in scope, sourced files, or loaded packages |
| `raven.crossFile.missingFileSeverity` | `"warning"` | Missing file referenced by source() or directive |
| `raven.crossFile.caseMismatchSeverity` | `"auto"` | A `source()`/forward-directive **or** backward-directive (`# raven: sourced-by` etc.) path that resolves only by a case difference from the real filename. Also accepts `"auto"` (in addition to the levels above): **information** on a case-insensitive filesystem, **warning** on a case-sensitive one. Independent of `missingFileSeverity`. See [Diagnostics → Source path case mismatch](diagnostics.md#source-path-case-mismatch). |
| `raven.crossFile.circularDependencySeverity` | `"error"` | Circular dependency detected |
| `raven.crossFile.maxChainDepthSeverity` | `"warning"` | Source chain exceeds max depth |
| `raven.crossFile.outOfScopeSeverity` | `"warning"` | Symbol used before it's in scope |
| `raven.crossFile.redundantDirectiveSeverity` | `"hint"` | Redundant `# raven: source` directive |
| `raven.diagnostics.mixedLogicalSeverity` | `"warning"` | `\|` / `\|\|` whose immediate operand is a bare `&` / `&&` (not wrapped in parentheses). Since `&` binds tighter than `\|` in R, the grouping is silent — the rule asks for explicit parentheses. Applies everywhere, not just inside `if` / `while` conditions. |
| `raven.diagnostics.conditionAssignmentSeverity` | `"warning"` | Binary `=` used directly inside an `if` / `while` condition (likely `==` intended). |
| `raven.diagnostics.reportUnusedSuppressions` | `false` | Report **every** suppression directive that suppressed nothing as an `unused-suppression` hint, not just `# raven: expect[...]` directives. With the default `false`, only `expect` directives are checked; a plain `# raven: ignore` / `# @lsp-ignore` / `# nolint` stays silent even when it matched no diagnostic. Pyright-style; the hint is HINT severity, so it never gates `raven check --max-severity error` by default. Available in `raven.toml` (`[diagnostics] reportUnusedSuppressions = true`) and honored by `raven check`. See [Directives → Ignore Directives](directives.md#ignore-directives). |

## Package Settings

| Setting | Default | Description |
|---|---|---|
| `raven.packages.enabled` | `true` | Enable package function awareness |
| `raven.packages.rPath` | `""` | Path to R executable for subprocess calls. Empty by default, in which case Raven searches `PATH`. Must point to vanilla `R` — not `radian` or `arf`, which are interactive REPL wrappers and cannot run the non-interactive scripts Raven uses for package introspection. For the interactive terminal program, see [`raven.rTerminal.program`](r-console.md#choosing-the-r-program). |
| `raven.packages.additionalLibraryPaths` | `[]` | Additional R library paths for package discovery |
| `raven.packages.missingPackageSeverity` | `"warning"` | Severity for missing package diagnostics (`"off"` to disable) |
| `raven.packages.watchLibraryPaths` | `true` | Watch `.libPaths()` directories and invalidate caches on install/remove |
| `raven.packages.watchDebounceMs` | `500` | Coalesce rapid filesystem events into a single invalidation pass (ms) |
| `raven.packages.packageMode` | `"auto"` | R package workspace mode: `"auto"` (detect DESCRIPTION), `"enabled"` (always), `"disabled"` (never). See [R Package Development](r-package-dev.md). |
| `raven.packages.rprofilePrelude` | `true` | Use the workspace-root `.Rprofile` startup prelude for ordinary script scope. See [`.Rprofile` Startup Prelude](rprofile.md). |

### Refresh Command

**Raven: Refresh package cache** (`raven.refreshPackages`) — re-runs `.libPaths()`, rebuilds the package library, restarts the filesystem watcher, clears the cache, and republishes diagnostics. Use after `renv::activate()`, `.libPaths()` changes, or if the watcher misses an event. See [When Raven calls R](cross-file.md#when-raven-calls-r) for what these R queries do.

## Scaffold Commands

These Command Palette entries write starter R config files to the first workspace folder. If the target file already exists, Raven prompts before overwriting.

| Command | File | Contents |
|---|---|---|
| `Raven: Create raven.toml` | `raven.toml` | A starter linting-focused project config at the workspace root, with the `[linting]` keys Raven maps from VS Code settings. Add other sections from this reference as needed (`crossFile`, `packages`, `diagnostics`, `indentation`, `symbols`, `completion`) |
| `Raven: Create .gitignore` | `.gitignore` | Standard R ignores (`.Rhistory`, `.RData`, `.Rproj.user/`), OS files (`.DS_Store`, `Thumbs.db`), R Markdown/Quarto/`R CMD check` artifacts, local scratch dirs, and AI-tool user-local overrides |
| `Raven: Create linting settings` | `.vscode/settings.json` | Every project-scoped `raven.linting.*` key Raven maps to `raven.toml`, each prefaced with a `//` comment naming its `lintr` equivalent. Merges into an existing `settings.json` without disturbing unrelated keys or comments, preserves client-only linting settings such as `raven.linting.readHomeLintr`, and prompts before overwriting an existing project-scoped `raven.linting.*` block |

## R Console Activation

| Setting | Default | Description |
|---|---|---|
| `raven.rConsole.activation` | `"auto"` | When Raven's R console — and the surfaces gated alongside it (plot viewer, data viewer, chunk navigation / highlighting / active-cell indicator, `.R` cell mode, and the `r.json` snippets contributed to `.Rmd` / `.Rmarkdown` / `.qmd`) — activates. `"enabled"`: always activate. `"disabled"`: never activate. `"auto"`: activate unless the REditorSupport extension is enabled or VS Code is running as Positron. See [R Console](r-console.md) and [Coexistence](coexistence.md). |

## Plot Settings

| Setting | Default | Description |
|---|---|---|
| `raven.plot.viewerColumn` | `"beside"` | Initial editor column for an R session's plot viewer panel when its first plot arrives. Once you move the panel, Raven leaves it in its new location. Values: `active`, `beside`. See [Plot Viewer](plot-viewer.md). |

## Help Viewer Settings

| Setting | Default | Description |
|---|---|---|
| `raven.help.viewerColumn` | `"beside"` | Initial editor column when the R help viewer first opens. Once you move the panel, Raven leaves it where you put it. Values: `active`, `beside`. See [Help Viewer](help-viewer.md). |

## Symbol Settings

| Setting | Default | Description |
|---|---|---|
| `raven.symbols.workspaceMaxResults` | `1000` | Maximum symbols returned by workspace symbol search (Cmd/Ctrl+T). Range: 100–10000. |

## Completion Settings

| Setting | Default | Description |
|---|---|---|
| `raven.completion.triggerOnOpenParen` | `true` | Register `(` as a completion trigger character for parameter suggestions |

## Indentation Settings

| Setting | Default | Description |
|---|---|---|
| `raven.indentation.enabled` | `true` | Syntax-aware indentation master switch |
| `raven.indentation.argumentStyle` | `"aligned"` | Parenthesized arguments: `"aligned"`, `"indented"`, or `"off"` |
| `raven.indentation.infixContinuationStyle` | `"aligned"` | Infix continuations: `"aligned"`, `"indented"`, or `"off"` |
| `raven.indentation.style` | `"rstudio"` | Deprecated permanent alias: `rstudio` → argument `aligned`, `rstudio-minus` → argument `indented`, `off` → syntax-aware indentation disabled |

Explicit new settings win per field over the alias. The alias never changes the infix setting. See [Smart Indentation](indentation.md#permanent-compatibility-alias) for the full precedence table and examples.

Raven sets `editor.formatOnType` to `true` for R, R Markdown, and Quarto by default (the lowest-priority VS Code default). Syntax-aware indentation applies to plain R and to R chunk bodies; prose, YAML, and non-R chunks stand down. Disable it per language when desired:

```json
"[r]": {
  "editor.formatOnType": false
},
"[rmd]": {
  "editor.formatOnType": false
},
"[quarto]": {
  "editor.formatOnType": false
}
```

Out of the box Raven emits aligned infix continuations and its lint accepts both forms. Projects using real `lintr`, styler, or Air in CI should set both infix settings to `"indented"`; strict-alignment projects should set both to `"aligned"`.

## Linting Settings

Native style/lint diagnostics. Tri-state master switch `raven.linting.enabled` (default `"auto"`); auto turns on when a `.lintr` that contains linting configuration (a blank/empty `.lintr` does not count) or a `raven.toml` opts in — except the literal home-directory `~/.lintr` is ignored unless the VS Code/LSP-client setting `raven.linting.readHomeLintr = true` is enabled (or the CLI receives `--config ~/.lintr`), and any `.lintr` is ignored when REditorSupport's own `lintr` diagnostics are live or you're in Positron ([details](linting.md#auto-and-reditorsupport--positron)) — set `true`/`false` for explicit overrides. Implemented in Rust against the tree-sitter AST — no `lintr` install required. All rules default to severity `information`, matching REditorSupport `languageserver`'s mapping for `lintr` style findings. See [Style Lints](diagnostics.md#style-lints) for the full rule list and suppression conventions, and [Linting](linting.md) for the master-switch behavior matrix, quick-start configuration, mapping from a `.lintr` file, and the suppression matrix.

| Setting | Default | Description |
|---|---|---|
| `raven.linting.enabled` | `"auto"` | Master switch (`"auto"` / `"on"` / `"off"` / `true` / `false`). See the [behavior matrix](linting.md#behavior-matrix). |
| `raven.linting.readHomeLintr` | `false` | VS Code/LSP-client-only. Include the literal home-directory `~/.lintr` in discovery. Workspace and non-home parent `.lintr` files are still discovered when this is `false`; the CLI uses literal `~/.lintr` only when passed explicitly with `--config ~/.lintr`. |
| `raven.linting.lineLength` | `80` | Maximum line length (characters) |
| `raven.linting.objectLength` | `30` | Maximum identifier length for the object-length lint |
| `raven.linting.indentationUnit` | `"auto"` | Spaces per indent level used by the indentation lint. In VS Code, `"auto"` tracks each file's resolved `editor.tabSize`; set an integer `1..=8` for a fixed unit. |
| `raven.linting.infixContinuationStyle` | `"either"` | Infix checking policy: strict `"indented"` (`lintr` parity), strict floored `"aligned"`, or their `"either"` union. Raven-only; see [Indentation](linting.md#indentation). |
| `raven.linting.assignmentOperator` | `"<-"` | Preferred assignment operator (`"<-"` or `"="`) |
| `raven.linting.stringDelimiter` | `"\""` | Preferred string-literal delimiter (`"\""` or `"'"`); used by the quotes lint |
| `raven.linting.lineLengthSeverity` | `"information"` | Severity for over-long lines (or `"off"`) |
| `raven.linting.trailingWhitespaceSeverity` | `"information"` | Severity for trailing whitespace |
| `raven.linting.noTabSeverity` | `"information"` | Severity for tab characters |
| `raven.linting.trailingBlankLinesSeverity` | `"information"` | Severity for blank lines or missing newline at end of file |
| `raven.linting.assignmentOperatorSeverity` | `"information"` | Severity for mismatched assignment operator |
| `raven.linting.objectNameStyleFunction` | `["snake_case", "symbols"]` | Naming schemes for functions. Accepts one style string or an array of styles (`"snake_case"` \| `"camelCase"` \| `"dotted.case"` \| `"UPPER_CASE"` \| `"lowercase"` \| `"symbols"` \| `"any"`); styles are ORed with function regexes. |
| `raven.linting.objectNameStyleVariable` | `["snake_case", "symbols"]` | Naming schemes for variables (same string-or-array enum as above); styles are ORed with variable regexes. |
| `raven.linting.objectNameStyleArgument` | `["snake_case", "symbols"]` | Naming schemes for function formal arguments (same string-or-array enum as above); styles are ORed with argument regexes. |
| `raven.linting.objectNameRegexesFunction` | `[]` | Additional Rust regexes accepted for function names. Partial match against the full identifier, including any leading `.`; use `^...$` for whole-name matches. Empty strings are rejected. |
| `raven.linting.objectNameRegexesVariable` | `[]` | Additional Rust regexes accepted for variable names (same matching rules as above). |
| `raven.linting.objectNameRegexesArgument` | `[]` | Additional Rust regexes accepted for function formal argument names (same matching rules as above). |
| `raven.linting.objectNameSeverity` | `"information"` | Severity for the object-name lint (set to `"off"` to disable entirely; include `"any"` in a kind's style setting to disable just that kind) |
| `raven.linting.infixSpacesSeverity` | `"information"` | Severity for the infix-spaces lint (whitespace around operators) |
| `raven.linting.commentedCodeSeverity` | `"information"` | Severity for the commented-code lint (standalone comments whose body parses as R code) |
| `raven.linting.quotesSeverity` | `"information"` | Severity for the quotes lint (string-literal delimiter style) |
| `raven.linting.commasSeverity` | `"information"` | Severity for the commas lint (spacing around `,`) |
| `raven.linting.tAndFSymbolSeverity` | `"information"` | Severity for the T/F-symbol lint (bare `T` / `F` used as `TRUE` / `FALSE`) |
| `raven.linting.semicolonSeverity` | `"information"` | Severity for the semicolon lint (`;` separators in source) |
| `raven.linting.equalsNaSeverity` | `"information"` | Severity for the equals-NA lint (`x == NA`, `x != NA`, typed-`NA` variants) |
| `raven.linting.objectLengthSeverity` | `"information"` | Severity for the object-length lint |
| `raven.linting.vectorLogicSeverity` | `"information"` | Severity for the vector-logic lint (`&` / `\|` in `if` / `while` conditions) |
| `raven.linting.functionLeftParenthesesSeverity` | `"information"` | Severity for the function-left-parentheses lint (whitespace between `function` and `(`) |
| `raven.linting.spacesInsideSeverity` | `"information"` | Severity for the spaces-inside lint (whitespace inside `(`, `[`, `[[`) |
| `raven.linting.indentationSeverity` | `"information"` | Severity for the indentation lint (lines whose leading whitespace doesn't match the surrounding syntax) |

To disable an individual rule while leaving the rest enabled, set its severity to `"off"`. For the object-name lint, you can also include `"any"` in any of the three style settings to disable just that symbol kind while keeping the others active. An explicit empty style array with regexes is regex-only mode; empty styles and empty regexes together disable that kind.

## Editor Integration Settings (VS Code only)

| Setting | Default | Description |
|---|---|---|
| `raven.editor.dotInWord` | `"yes"` | Whether to treat `.` as part of a word in R and JAGS files. `"yes"` (the default) treats dots as word characters by overriding `editor.wordSeparators` for `[r]` / `[jags]` — matching RStudio and Positron — `"no"` leaves dots as separators, and `"ask"` prompts on first use. Change it any time by editing the setting. (Renamed from `raven.editor.dotInWordSeparators`, which is migrated automatically.) |

## Server Settings

| Setting | Default | Description |
|---|---|---|
| `raven.server.path` | `""` | Path to the `raven` binary. Empty by default, in which case the bundled binary is used. |
