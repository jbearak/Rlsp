# Hover

Hovering over an identifier shows what the symbol is and, when available, its signature or documentation. R hover uses the same position-aware, cross-file scope model as [completions](completion.md), [diagnostics](diagnostics.md), and [go-to-definition](go-to-definition.md), so the package attributed at the cursor matches what's in scope under Raven's static model — namely, packages brought in via `library()` / `require()` (or via `loadNamespace()`, which Raven treats as an attach signal even though R itself only loads the namespace), plus namespace qualifiers and declared symbols. Stan has a separate compiler-catalog hover described below.

## What You See

| At the cursor | Hover shows |
|---|---|
| Member side of `pkg::name` / `pkg:::name` | Bold `pkg::name` help-panel link + R help text for that topic (nothing if `name` is not in the package's *complete* export set — see below) |
| Package side of `pkg::name` (the `pkg`) | The package's `Title` — `Description` from its installed `DESCRIPTION` (nothing when the package is not installed — the [`package-not-installed`](diagnostics.md#package-diagnostics) diagnostic already reports that on the same token) |
| Local or cross-file definition | A code block with the definition statement and a file-location line |
| Named-argument label resolving to a user-defined function's formal (`param` in `f(param = …)`) | A code block with the formal (and its default), *parameter of* `f`, and the formal's `@param` doc when documented |
| Function-parameter name at a definition site (`x` in `f <- function(x)`) | The parameter's `@param` roxygen — when the enclosing *named* function is documented |
| `obj$name` / `obj@slot` member with a *local* member definition | A code block with that member's definition statement and a file-location line (parity with go-to-definition) |
| Exported member of a static `box::use()` namespace alias (`mod$name`, `mod@name`, `mod[["name"]]`) | The original local-module definition statement and location, including through renamed/wildcard re-exports; package-backed members retain package attribution without fabricated source navigation |
| Symbol declared via `# raven: var` / `# raven: func` | `name (declared function\|variable)` + the directive and line where it's declared |
| Package export in scope (via `library()` / `require()` / `loadNamespace()` / directives) | Bold `pkg::name` help-panel link + R help text |
| Built-in or otherwise unresolved symbol | R help text, if R has a topic for it |

Hover returns nothing for symbols R doesn't recognize and that aren't in scope, and for structural labels it cannot resolve (see step 2 below).

## Stan Hover

In a `.stan` file, hovering a compiler-known function call shows representative compiler signatures and a link to the versioned [Stan Functions Reference](https://mc-stan.org/docs/2_39/functions-reference/functions_index.html). The link targets the callable's alphabetical-index anchor; compiler aliases that the reference does not document separately land at the index. Raven bundles metadata generated through the `stanc3` npm package 2.39.1, whose bundled compiler identifies itself as 2.39.0, so hover requires neither a local Stan installation nor a runtime subprocess or network request.

Distribution-statement syntax is distribution-aware: hovering `normal` in `y ~ normal(mu, sigma)` explains that the bare name is notation after `~`. The statement contributes the unnormalized log density from `normal_lupdf` to `target`. The hover labels the displayed `normal_lpdf` compiler signatures as the normalized log-density function and links to that reference entry. Discrete distributions are described analogously with `_lupmf` and `_lpmf`; truncation syntax may add normalization adjustments beyond the unnormalized probability-function contribution.

Outside a distribution statement, an exact callable wins when a name is ambiguous; for example, `beta(1, 2)` describes the beta special function rather than the beta distribution. Bare distribution names with no exact callable still get distribution hover for incomplete code, but the hover clearly says that forms such as `normal(...)` are not standalone Stan functions and are valid only on the right side of `~`.

Some Stan functions have hundreds of overloads. Hover shows at most 12 representative signatures and reports the full compiler overload count. Compiler-known statement-like callables such as `print()` and `reject()` have a reference link even when stanc does not expose a conventional signature for them.

Stan hover is intentionally limited to call sites. It returns nothing for names in comments or strings, plain non-call identifiers, unknown functions, and ordinary calls resolved to a user-defined function in the current file. Same-named variables do not suppress callable hover, and distribution-statement syntax remains distribution-aware even when an ordinary user function has the distribution's name. Raven does not yet extract documentation for user-defined Stan functions.

## Resolution Order

For R documents, hover tries sources in this order and stops at the first match. This matches the R path in `crates/raven/src/handlers.rs::hover`:

1. **Namespace qualifier.** If the cursor is inside a `pkg::name` or `pkg:::name` expression, the qualifier wins — even if the file-local scope would resolve `name` to something else. Without this rule, hovering `filter` inside `dplyr::filter(...)` could show `stats::filter` whenever the workspace happened to surface that one first. The two sides differ: hovering the **member** (`filter`) shows that topic's help, while hovering the **package** (`dplyr`) shows the package's `Title`/`Description` from its installed `DESCRIPTION` (and nothing when it is not installed, since the `package-not-installed` diagnostic already reports that) — not a `dplyr::dplyr` help artifact.
2. **Structural labels: resolve where possible, otherwise suppress.** An identifier that never refers to a value at runtime — a named-argument label (`title` in `labs(title = ...)`), a function-parameter name, or the member name in `obj$name` — is not a plain value lookup, so hover must never attribute it to a definition or package (the misleading `from {base}` bug). But where Raven has something *correct* to show, it resolves rather than suppressing:
   - a named-argument label that maps to a **user-defined** function's exact formal → that formal (+ default, + `@param` doc);
   - a parameter name at a definition site → its `@param` roxygen, when the enclosing *named* function is documented;
   - an `obj$name` / `obj@slot` member with a **local** member definition → that definition (reusing go-to-definition's resolver).

   This is **resolve-or-suppress, never resolve-or-attribute**: anything that does not resolve to one of the above (an unknown callee, a package/builtin's data-keyword like `list(a = 1)`, a runtime/`$`-member with no local definition) produces *nothing* rather than a guess. Hover and the diagnostics pass still share the underlying structural-label predicate, so they agree on what counts as a label — these are hover-only carve-outs layered on top of it. An **assignment target** (the `add` in `add <- function(...)`) is treated differently again: the diagnostics pass counts it as a non-reference, but hover does *not* suppress it — it is a definition site, so hover continues to the next step and surfaces the definition.
3. **Cross-file scope.** Raven resolves the identifier through the dependency graph at the cursor's position. If it finds a local definition, a sourced definition, a declared symbol, or a package export that's in scope, it builds the hover from that symbol.
4. **Package exports from loaded packages.** If the identifier isn't in the cross-file scope but it matches a symbol exported by any package loaded at the cursor (including packages inherited from parent files), hover shows that package's help topic.
5. **R help fallback.** For anything left over — base/recommended built-ins, or symbols whose origin Raven can't infer — hover asks R for a help topic and returns it verbatim in a code block.

Each step takes the first hit and stops; steps 3–5 never run once a match is found.

### box module members

Static [`box::use()` imports](modules.md#box-modules-boxuse) are resolved before ordinary structural-member scanning. Hover on an exported local-module member follows exact definition provenance through named, renamed, and wildcard re-exports, including when the defining module is closed but workspace-indexed. Private members and members proven absent from a complete export set produce no hover. Partial/unknown export metadata remains conservative. Installed-package imports use Raven's package metadata and help attribution, but Raven does not invent a source-file location for installed code.

## File Location Lines

When the hover is built from a local or sourced definition, Raven adds one line underneath the code block so you can jump to the definition (package exports use the help-panel link instead — see below):

- Defined in the same file: `this file, line N`.
- Defined in another file: `[rel/path.R](file:///…), line N` — click the link to open that file. The link carries no line fragment, so the file opens at the top; the `line N` text is informational. To land on the definition, use go-to-definition instead.
- Definition statement couldn't be extracted (e.g. the defining file has since moved or changed): Raven falls back to a `*Defined in rel/path.R*` italic attribution.

The relative path is computed against the workspace root when one is available, so hovering a symbol defined in `R/utils.R` shows exactly that — not an absolute URI. The link points at the same file go-to-definition navigates to (go-to-definition additionally positions the cursor on the definition); see [Go-to-Definition](go-to-definition.md#cross-file-navigation).

## Declared Symbols

Symbols declared via [`# raven: var` or `# raven: func`](directives.md#declaration-directives) hover as an R code block with the declaration, followed by the directive line and (for cross-file declarations) a file attribution. For a function declared in another file, the hover renders the code block:

```r
name (declared function)
```

…then, as markdown beneath it, `Declared via # raven: func directive at line 12` and *Defined in analysis/helpers.R*.

The "Defined in" line is omitted when the declaration lives in the current file. If the same name is declared more than once in the providing file, hover shows the **most recent declaration at or before the cursor** — the same position-aware scope [completions](completion.md) and [diagnostics](diagnostics.md) use. [Go-to-definition](go-to-definition.md#declared-symbols) differs here: it navigates to the *first* declaration by line number.

## Package Exports and the Help-Panel Link

When hover resolves to a package export and R has a help topic for it, the first line of the hover is a bold Markdown link of the form `pkg::name`. In VS Code, clicking that link opens the [help viewer](help-viewer.md) beside the editor with the rendered Rd documentation — the same topic the hover is showing, but in a resizable panel with back/forward navigation.

If R hasn't rendered help text for that topic yet (cold cache, large package, or a symbol R doesn't have a topic for), hover falls back to a shorter form that omits the bold heading and just shows the name or Raven's parsed signature followed by a `from {pkg}` attribution. Subsequent hovers on the same topic hit Raven's help cache and render the full help text with the clickable heading.

Namespace-qualified calls (`dplyr::filter`) get the bold heading whether or not help text is available — Raven honors the qualifier even when R can't render a topic for it. The one exception is **resolve-or-suppress**: if Raven holds a *complete* export set for the package and the member is not in it (the same condition that raises the [`namespace-member-not-found`](diagnostics.md#namespace-member-references-pkgmember) diagnostic), hover shows **nothing** rather than fabricating a `from {pkg}` attribution for a member that does not exist. This never applies to `pkg:::name` (internal symbols are real members, just unexported) or when the package's metadata is incomplete/not-yet-warmed.

## Scope-Aware Package Attribution

Because hover goes through the cross-file scope engine, the package attributed at each cursor depends on what's in scope at that position:

```r
library(stats)
filter(x, filt, method = "convolution")  # → stats::filter

library(dplyr)
filter(df, x > 0)                        # → dplyr::filter (dplyr loaded after stats)
```

Explicit qualifiers short-circuit this — `dplyr::filter` and `stats::filter` always hover as their qualified packages regardless of what else is loaded. For how this differs from other R hover implementations, see [Comparison: Hover help](comparison.md#hover-help).

## With REditorSupport Also Installed

VS Code runs hover providers from every enabled extension and stacks their output, separated by horizontal rules. If you have both Raven and the [REditorSupport extension](https://marketplace.visualstudio.com/items?itemName=REditorSupport.r) active, you'll see REditorSupport's bold help link at the top of the hover and Raven's result below it — and sometimes REditorSupport contributes several bold links instead of one.

REditorSupport's hover doesn't resolve scope at the cursor. When its `guess_namespace` heuristic can't narrow a name down to a single package, it falls through to an unqualified `utils::help((topic))` lookup, which returns matches across every installed package. Hovering `filter` after `library(dplyr)`, for example, may show **both** `dplyr::filter` and `stats::filter` as bold links at the top — see [Comparison: Hover help](comparison.md#hover-help) for the mechanism. Raven's single scope-aware result (`dplyr::filter`, in this case) appears underneath.

This isn't a Raven bug. Both extensions are answering the hover request independently, and VS Code is concatenating their output. If you'd rather see only Raven's hover, disable REditorSupport's language server with `"r.lsp.enabled": false` — see [Coexistence](coexistence.md#language-servers-raven-alone-vs-both).

## Limits

- **No navigation to installed package sources.** Hover attributes a symbol to its package and links to the help viewer, but cmd-click on a package export does not jump into the package's source. See [Go-to-Definition: Package Exports](go-to-definition.md#package-exports).
- **R-help fallback is async.** The first hover for a topic spawns an R subprocess to render help; re-hovering the same topic uses the cached result.
- **R Markdown / Quarto: chunk bodies only.** Hover (and its help-panel link) works on identifiers inside R code chunks of `.Rmd` and `.qmd` documents. Hovering prose, YAML front matter, or a non-R chunk produces nothing.
- **Stan user functions.** Raven suppresses built-in attribution when a name is declared locally, but does not yet show the local Stan function's signature or comments.

## Related

- [Cross-File & Package Awareness](cross-file.md) — the scope and dependency model hover uses
- [Go-to-Definition](go-to-definition.md) — follows the same file-location link hover displays
- [Help Viewer](help-viewer.md) — the panel opened by the bold `pkg::name` link at the top of a hover
- [Directives](directives.md) — `# raven: var` / `# raven: func` declaration format
- [Completions](completion.md) — shares the position-aware scope model with hover
- [Diagnostics](diagnostics.md) — if hover resolves a symbol, the diagnostics pass won't flag it as undefined
- [Comparison: Hover help](comparison.md#hover-help) — how Raven's scope-aware attribution differs from REditorSupport's
