# R Help Viewer

The extension provides a built-in help viewer that renders R help (Rd) documentation directly in VS Code. When you click on a function name in a hover, the help panel opens beside the editor and displays the topic's documentation, usage, arguments, and examples. Navigate across topics via cross-references, with back/forward history support.

## Why we built this

Raven's help viewer uses the language server's scope analysis to disambiguate which package's help to show. If you hover over `filter(...)` after `library(dplyr)`, Raven's static scope model picks `dplyr::filter` over `stats::filter`. The scope-aware resolution looks at namespace qualifiers (`pkg::fn`), `library()` / `require()` / `loadNamespace()` calls in this file and any sourced files, and the standard package-search-path order. (In-scope `# raven: var` / `# raven: func` declarations short-circuit to a declaration hover that shows where the symbol was declared, rather than contributing to package help selection.) See [Comparison: Hover help](./comparison.md#hover-help) for how this differs from other R hover implementations.

> [!NOTE]
> Code intelligence and the help viewer are unaffected by `raven.rConsole.activation`. Code intelligence doesn't depend on a live R session at all — Raven's semantic analysis is static, driven by scope resolution over your source files and installed package metadata. The help viewer, unlike the plot and data viewers, doesn't need the R session managed by Raven either — it shells out to R on demand to render Rd documentation as HTML.

## How to open it

The help viewer is triggered from a hover:

- **From a hover**: Hover over an identifier (e.g., `dplyr::filter` or `plot`). When the symbol resolves to a known package, the hover bubble displays a bold `pkg::name` heading at the top — click it to open the help panel.

> [!NOTE]
> In R Markdown (`.Rmd` / `.Rmarkdown`) and Quarto (`.qmd`) files, hover-triggered help works on identifiers inside R code chunks (the same as in `.R` files). Hovering prose, YAML front matter, or a non-R chunk produces no hover, so there's no help link there.

There is no command-palette entry: the panel needs a resolved topic and package, which only the hover link supplies, so `raven.openHelpPanel` (and the `raven.help.back` / `raven.help.forward` navigation commands) are hidden from the palette.

## Navigation

The help panel toolbar includes back and forward arrows, populated as you click cross-reference links (labeled "See also: X") within rendered help pages. Navigation works like a browser:

- Click a cross-reference to jump to that topic.
- The back arrow becomes enabled once you've navigated away from the initial topic.
- Back takes you to the previous topic and restores scroll position.
- Forward is only available after you've used back to return to an earlier topic.
- Navigating to a new topic from a back-position clears the forward stack.

Internal cross-references are rewritten to a custom URL scheme that correctly round-trips operator topics like `` \`[\` `` and `` \`%in%\` ``.

**Panel placement**: The initial column is controlled by `raven.help.viewerColumn` (default `beside`). Once you move the panel manually in VS Code, Raven leaves it in its new location.

## What works

- Most installed-package help pages render, including titles, descriptions, usage, arguments, examples, and see-also sections.
- Cross-references within and across packages navigate in-panel. When `Rd2HTML(dynamic = TRUE)` mis-attributes a link to its source package (e.g. `base::plot` linking to `base/plot.default` even though `plot.default` lives in `graphics`, or `graphics::plot.default` linking to `graphics/finite` for the `is.finite` alias in `base`), the renderer falls back to a global `help()` lookup so the link still resolves.
- Operator topics (`` \`[\` ``, `` \`%in%\` ``, `+`, `if`, etc.) render and navigate correctly.
- Images embedded in help pages (e.g., `?ggplot2::theme`) render — local files are served via webview URIs from package help directories.
- External links (`https://`, `http://`, `mailto:`) are handed off to VS Code's built-in webview link handling, which shows a single "Do you want to open this URL?" trust prompt and then opens the link in your default browser (or mail client).
- References to R's canonical bundled manuals — `R-intro`, `R-admin`, `R-data`, `R-exts`, `R-FAQ`, `R-ints`, `R-lang` — are rewritten from the local `<a href="/doc/manual/<name>.html">` form that `Rd2HTML` emits to the canonical CRAN URL (`https://cran.r-project.org/doc/manuals/r-release/<name>.html`) so they open in the user's browser. This is how the `Writing R Extensions` link in `?utils::package.skeleton` resolves. Anchors are percent-encoded and preserved. Manual paths outside this allowlist (e.g. `rw-FAQ.html`, custom or third-party docs) are not rewritten and click does nothing — those targets either live elsewhere on CRAN or aren't published.
- `Run examples` and per-package `Index` footer links emitted by `Rd2HTML` are stripped before rendering — they pointed at endpoints that have no analog in the panel and would render as no-op links.
- A failed in-panel navigation (e.g. a topic that genuinely cannot be resolved) leaves the previously rendered topic visible, with the error shown in the toolbar banner. Back/forward continues to operate from the last successful topic, not from the failed attempt.

## v1 Limitations

- **No search**: There is no way to search across help topics from the panel. Use `?topic` or `??topic` in the R console.
- **No examples runner**: Clicking inside an examples block does not execute the code. Copy-paste it into your R console. (The `Run examples` link `Rd2HTML` would otherwise emit is stripped before rendering, since it points at an R dynamic-help-server endpoint we don't run.)
- **No vignettes**: Vignette links (`` \`../../<pkg>/doc/<vignette>.html\` ``) are neutralized in the rendered HTML; clicking them does nothing. Vignettes are out of scope for v1.
- **Remote images dropped**: `<img>` tags pointing to `https://` or any non-local source are stripped by the sanitizer. Only local images shipped with installed packages render.
- **Singleton panel**: Only one help panel per VS Code window. Navigating to a new topic reuses the same panel.
- **Help format support**: `tools::Rd2HTML()` output is sanitized via ammonia. Topics with unusual Rd structure may render slightly differently than RStudio's help pane.

## Configuration

| Setting | Default | Description |
|---|---|---|
| `raven.help.viewerColumn` | `"beside"` | Initial editor column when the help panel first opens. Values: `"active"`, `"beside"`. Once you move the panel, Raven leaves it where you put it. |

## Manual smoke test plan

1. Hover over `dplyr::filter` in an R file → bold `dplyr::filter` heading at the top of hover; click → panel opens beside.
2. Panel shows R help with package header, title, usage, arguments, examples.
3. Click "See also: arrange" → panel navigates, back arrow now enabled.
4. Back arrow → returns to filter, scroll position restored.
5. Hover `plot(1:5)` → bold `graphics::plot` heading; click → navigates correctly even cross-package.
6. Hover an operator: `` ?\`[\` `` or `` ?\`%in%\` `` → bold heading uses the operator, click navigates and renders correctly (verifies percent-encoding round-trip and `is_valid_help_topic`).
7. Trigger a help page with images (e.g., `?ggplot2::theme` if installed) → images load.
8. Trigger an unknown topic (hover a symbol whose topic cannot be resolved, or invoke `raven.openHelpPanel` programmatically with a bogus topic) → panel shows the not-found message; previous content & history preserved.
9. Configure a non-default R via `raven.packages.rPath` and verify help renders against that R installation (open a topic only available in a package installed for that R; should succeed where it would fail against system R).
