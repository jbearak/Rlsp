# Syntax Highlighting

Raven contributes syntax highlighting in three ways: LSP semantic tokens for R function names (including inside R chunks of `.Rmd` / `.Rmarkdown` / `.qmd` documents), TextMate grammars for R, R Markdown, JAGS, Stan, and R package development files, and a GitHub-themed highlighter for the rendered HTML output of `Raven: Knit Preview`. The R and R Markdown grammars are vendored from [REditorSupport/vscode-R-syntax](https://github.com/REditorSupport/vscode-R-syntax) (MIT) so `.R`, `.Rmd`, and `.Rmarkdown` files highlight out of the box, including in remote workspaces; siblings that ship the same grammar are preferred when installed (see below). For JAGS, Stan, and the package infrastructure files, Raven ships the grammar itself because VS Code doesn't bundle one.

## R

Raven emits LSP semantic tokens for R function names. The token legend contains one entry — the standard LSP `function` token type — and Raven emits it for:

- Function-definition names (identifiers assigned a `function(...)` value, with `<-`, `=`, `<<-`, `->`, or `->>`).
- Function-call heads, including namespace-qualified calls like `pkg::fn()`.

This is intentionally narrow. The goal is to catch call and definition sites reliably via the tree-sitter AST, then let the TextMate grammar handle everything else (comments, strings, numbers, operators, roxygen tags, constants, storage types, brackets). Semantic tokens augment the grammar; they don't replace it.

Semantic tokens fire inside `R` chunks of `.Rmd` / `.Rmarkdown` / `.qmd` documents too. Raven walks the document chunk-by-chunk (via the same chunk detector used for the document outline), parses each R chunk body in isolation with tree-sitter, and rebases the tokens onto the full document so VS Code paints them in the editor. Non-R chunks (Python, SQL, Bash, etc.) are intentionally skipped — Raven is an R language server.

Raven isn't the only R language server that emits semantic tokens — the one bundled with the full `REditorSupport.r` extension (distinct from the grammar-only `REditorSupport.r-syntax` discussed below) also does, with broader coverage. See [Coexistence](./coexistence.md) if you're running both.

Raven ships its own copy of the REditorSupport R grammar so the editor and the knit pipeline have a working grammar in every deployment shape (local, Remote SSH, Dev Container, WSL, Codespaces). If `REditorSupport.r-syntax`, `REditorSupport.r`, or VS Code's built-in `vscode.r` is installed, those win — Raven's vendored grammar is the self-resolving fallback.

## Rendered HTML (Raven: Knit Preview)

`Raven: Knit Preview` writes a self-contained `.html` to a per-session temp directory and displays it in the Knit Preview panel. Code blocks in that HTML are re-highlighted with a GitHub light/dark palette using whichever VS Code grammar contributes the chunk's language:

- For R chunks, Raven walks the installed extensions in priority order — `REditorSupport.r-syntax`, then `REditorSupport.r`, then VS Code's built-in `vscode.r`, then Raven's own vendored grammar — and uses the first grammar it finds. Function names get an additional `function` color via Raven's LSP semantic-token overlay, layered on top of the grammar's TextMate scopes.
- For non-R chunks (Python, SQL, Bash, Julia, …), Raven uses whichever grammar VS Code's installed extensions contribute for that language. Unknown languages render as plain monospace.
- Untagged fences (``` ``` ``` without a language tag) are left as-is — same convention as the editor.

The palette is picked at render time: the in-VS-Code panel uses the variant matching VS Code's body class (`vscode-light` or `vscode-dark`), and the standalone file (used by **Open in Browser**) ships both variants behind a `@media (prefers-color-scheme: dark)` query so the browser picks based on the system theme.

Math (`$x$`, `$$x = y$$`, LaTeX environments) is rendered through VS Code's built-in `vscode.markdown-math` extension, which bundles KaTeX. Raven inlines that CSS into the standalone HTML so math also renders when the file is opened directly in a browser.

### Why Raven vendors the R and R Markdown grammars

VS Code's built-in `vscode.r` extension and [`REditorSupport.r-syntax`](https://marketplace.visualstudio.com/items?itemName=REditorSupport.r-syntax) are both pure-declarative extensions — no `main` field, no runtime code — so VS Code defaults their `extensionKind` to `"ui"` and installs them on the **UI extension host** only. The built-in is a periodic sync of REditorSupport upstream (`vscode-grammar-updater` against [`REditorSupport/vscode-R-syntax`](https://github.com/REditorSupport/vscode-R-syntax)); all three copies share the same source.

Raven runs on the **workspace extension host** because it ships an LSP binary. In a local workspace the two hosts coincide, but in Remote SSH, Dev Container, WSL, and Codespaces setups the workspace host runs on a different machine from the UI. The workspace host cannot see UI-only extensions, and even if it could, those extensions' grammar files live on the user's local UI machine's filesystem — unreachable from the remote host. The knit-preview tokenizer is on the workspace side and needs grammar bytes reachable there.

Raven therefore vendors the R and R Markdown grammars from REditorSupport (MIT) directly into `editors/vscode/syntaxes/r.tmLanguage.json` and `editors/vscode/syntaxes/rmd.tmLanguage.json`. The provenance and sync procedure live in `editors/vscode/syntaxes/SOURCE.md`. With this contribution in place:

- `.Rmd` / `.Rmarkdown` files highlight out of the box, with Markdown prose styling, R code chunks, embedded highlighting for a dozen other languages in their fenced blocks, and inline `` `r ` `` expressions.
- The knit pipeline's R-chunk tokenizer always finds a grammar, even in fresh remote sessions where the user hasn't (and can't) install a UI-only sibling.
- Sibling grammars still win when present. The priority lists differ by file type:
  - **`.R` files:** `REditorSupport.r-syntax` → `REditorSupport.r` → `vscode.r` → Raven's vendored copy.
  - **`.Rmd` / `.Rmarkdown` files:** `REditorSupport.r-syntax` → `REditorSupport.r` → Raven's vendored copy. `vscode.r` is intentionally absent — the VS Code built-in only contributes `source.r`, never the R Markdown grammar.
  A user with the upstream extension keeps the freshest grammar; everyone else gets the synced snapshot Raven shipped.

Raven still does **not** ship a `quarto` grammar — install [`quarto.quarto`](https://marketplace.visualstudio.com/items?itemName=quarto.quarto) for `.qmd` highlighting and preview.

## R Package Development Files

VS Code doesn't ship grammars for the infrastructure files that ship inside every R package, so Raven adds TextMate grammars and language configurations for four file types. Each file gets syntax highlighting, bracket matching, and comment toggling (`#` for DCF/NAMESPACE/.Rbuildignore, `%` for Rd).

### DCF (`DESCRIPTION`, `.Rproj`, `.lintr`)

Registered via the `r-dcf` language ID for `DESCRIPTION`, `.Rproj`, and `.lintr`. DCF (Debian Control File) is the format used for R package metadata and project configuration. The grammar recognizes:

- **Field names** — Identifiers before `:` (e.g., `Package`, `Version`, `Depends`), styled as keywords.
- **Field values** — The text after `:` on the same line, styled as strings.
- **Continuation lines** — Lines starting with whitespace that continue a multi-line field value.
- **Comments** — `#` to end of line.

### NAMESPACE

Registered via the `r-namespace` language ID for the `NAMESPACE` file. The grammar recognizes:

- **Directives** — `export`, `exportClasses`, `exportMethods`, `exportPattern`, `import`, `importClassesFrom`, `importFrom`, `importMethodsFrom`, `S3method`, `S4method`, `useDynLib`, styled as keywords.
- **Symbol names** — Package and function identifiers inside directive arguments.
- **Strings** — Double-quoted names.
- **Comments** — `#` to end of line.

### Rd (`.Rd`, `.rd`)

Registered via the `rd` language ID. Rd is R's documentation format. The grammar recognizes:

- **Section tags** — `\title`, `\usage`, `\arguments`, `\description`, `\details`, `\value`, `\examples`, and the rest of the standard Rd section set, styled as keywords.
- **Inline tags** — `\code`, `\emph`, `\strong`, `\link`, `\pkg`, `\url`, `\var`, and the full inline tag set, styled as entity names.
- **Escape sequences** — `\{`, `\}`, `\\`, `\%`.
- **Comments** — `%` to end of line (but not `\%`, which is an escaped literal percent).

### `.Rbuildignore`

Registered via the `r-buildignore` language ID. `.Rbuildignore` contains one extended regular expression per line listing paths to exclude from the package tarball. The grammar recognizes:

- **Patterns** — Non-comment lines, styled as `string.regexp`.
- **Comments** — `#` to end of line.

## JAGS and Stan

VS Code doesn't bundle grammars for JAGS or Stan, so Raven ships its own TextMate grammars for both languages. There are no LSP semantic tokens for these files — highlighting comes entirely from the grammar.

### JAGS

Registered for `.jags`, `.Jags`, `.JAGS`, `.bugs`, `.Bugs`, `.BUGS`. The grammar recognizes:

- **Comments** — `#` to end of line.
- **Strings** — double-quoted, with `\\`-style escapes.
- **Block keywords** — `model`, `data`.
- **Control flow** — `for`, `in`, `if`, `else`.
- **Distributions** — `dnorm`, `dbern`, `dgamma`, `dunif`, `dpois`, `dbin`, `dbeta`, `dexp`, `dt`, `dweib`, `dlnorm`, `dchisqr`, `dlogis`, `dmulti`, `ddirch`, `dwish`, `dmnorm`, `dmt`, `dinterval`, `dcat`.
- **Math functions** — `abs`, `sqrt`, `log`, `exp`, `pow`, `sin`, `cos`, `sum`, `prod`, `min`, `max`, `mean`, `sd`, `inverse`, `logit`, `probit`, `cloglog`, `ilogit`, `phi`, `step`, `equals`, `round`, `trunc`, `inprod`, `interp.lin`, `logfact`, `loggam`, `rank`, `sort`, `ifelse`, `T`.
- **Numeric literals** — integer, decimal, and scientific notation.
- **Operators** — `<-`, `~`, `&&`, `||`, comparisons (`==`, `!=`, `<=`, `>=`), and arithmetic (`+`, `-`, `*`, `/`, `^`).

### Stan

Registered for `.stan`, `.Stan`, `.STAN`. The grammar recognizes:

- **Comments** — `//` line comments and `/* ... */` block comments.
- **Strings** — double-quoted, with `\\`-style escapes.
- **Block keywords** — `functions`, `data`, `transformed data`, `parameters`, `transformed parameters`, `model`, `generated quantities`.
- **Type keywords** — `int`, `real`, `vector`, `row_vector`, `matrix`, `simplex`, `unit_vector`, `ordered`, `positive_ordered`, `corr_matrix`, `cov_matrix`, `cholesky_factor_corr`, `cholesky_factor_cov`, `void`, `array`, `complex`, `complex_vector`, `complex_row_vector`, `complex_matrix`, `tuple`.
- **Constraint keywords** — `lower`, `upper`, `offset`, `multiplier`.
- **Control flow** — `for`, `in`, `while`, `if`, `else`, `return`, `break`, `continue`, `print`, `reject`, `profile`.
- **Distribution-suffix functions** — any identifier ending in `_lpdf`, `_lpmf`, `_lcdf`, `_lccdf`, or `_rng`, so user-defined and library-defined density/sampling functions both highlight consistently.
- **Numeric literals** — integer, decimal, and scientific notation.
- **Operators** — `<-`, `~`, `&&`, `||`, comparisons, arithmetic, and the Stan-specific `'` (transpose), `%` (mod), `!`, `?`, `:`.
