# R Markdown knit

Raven ships **`Raven: Knit Preview`** for previewing R Markdown and
three companion **export** commands (`Knit: Export to HTML…`,
`Knit: Export to PDF…`, `Knit: Export to Word…`) for saving the
rendered document next to the source `.Rmd` / `.Rmarkdown`. The pipeline is
intentionally narrow:

- The preview is always HTML, regardless of the YAML `output:`
  field. `pdf_document`, `word_document`, etc. still preview as HTML;
  the `output:` field only affects the named formats during export.
- The preview is a static viewer with a manual **Knit again**
  button — re-knitting is deliberate, not background-driven. The
  intended workflow is `Shift+Cmd+Enter` (or `Shift+Ctrl+Enter` on
  Windows/Linux), which saves the file if it's unsaved and re-knits
  in one keystroke. Re-rendering only on demand keeps the preview
  deterministic (you decide when it changes) and avoids spurious
  re-renders while you're mid-edit.
- The preview **persists across window reload/restart**. If a preview
  panel is open when you reload the window or quit and reopen VS Code,
  Raven restores it automatically showing the **last rendered output** —
  no re-knit, no R subprocess. The restored panel is a static snapshot
  of the previous render; press **Knit again** to refresh it. This is
  controlled by `raven.knit.persistPreview` (default on); disable it to
  go back to the previous behavior, where the panel disappears on reload
  and its temp files are removed immediately. Surviving a machine reboot
  is out of scope — the OS may clear the temp directory. If a restored
  preview's temp files are gone (swept, or cleaned up), the panel shows a
  "press Knit again" placeholder.
- The preview is saved to a per-session temp directory under
  `<os.tmpdir()>/raven-knit/<workspaceHash>/<sessionId>/preview/<sourceHash>/`,
  so the source document's own directory stays clean. Export commands write the
  final file (HTML / PDF / DOCX) next to the source document.
- Export commands shell out to Pandoc. PDF export additionally needs a
  PDF engine — `xelatex` (a LaTeX engine) by default, or `wkhtmltopdf`
  (WebKit-based, no LaTeX required).
- No `.qmd` rendering in the Knit Preview pipeline. Raven's separate
  [Quarto Preview and Render](quarto.md) commands delegate `.qmd` rendering to
  the Quarto CLI; the two pipelines are independent.

For HTML, Raven calls
[`knitr::knit`](https://yihui.org/knitr/) directly — not
[`rmarkdown::render`](https://rmarkdown.rstudio.com/) — and renders
the post-knit markdown through VS Code's built-in markdown pipeline
(KaTeX math, image rewriting, registered `markdown-it` plugins).
Code blocks are re-highlighted with the GitHub light/dark palette
using whichever R / Python / SQL / Bash grammar your installed VS
Code extensions contribute, with Raven's `function` semantic-token
overlay layered on top of R blocks.

While you edit the source `.Rmd`, `.Rmarkdown`, or `.qmd`, VS Code's **Outline**
view and breadcrumbs reflect the document's Markdown heading structure —
the same structure the preview renders — so you can navigate the document
as you preview it. See [docs/document-outline.md](document-outline.md).

See [docs/coexistence.md](coexistence.md) for the surfaces Raven
defers to other extensions (most notably `quarto.quarto`). The R and
R Markdown grammars are vendored from REditorSupport upstream and ship
with Raven, so `.Rmd` / `.Rmarkdown` files highlight in fresh installs and remote
workspaces; sibling grammars (`REditorSupport.r-syntax`,
`REditorSupport.r`, the built-in `vscode.r`) are still preferred when
installed.

## When the command is available

`Raven: Knit Preview` is gated by `raven.rConsole.activation`. With the
default `"auto"` setting it is **disabled** when the
`REditorSupport.r` extension is enabled or VS Code is running as
Positron — both already provide their own knit affordances. In every
other environment it is enabled and appears in the command palette on
`.Rmd` / `.Rmarkdown` files.

The command appears in the command palette only when the resolved gate
is open and the active file is `.rmd`, `.Rmd`, `.RMD`, `.rmarkdown`,
`.Rmarkdown`, or `.RMARKDOWN`. Invoke it from
the active editor, the Command Palette, or the editor-title Raven menu
when an R Markdown file is open.

## What it does, step by step

1. **Trust check.** The command is disabled in untrusted workspaces and
   offers a button to open the trust manager.
2. **YAML front matter parse.** Raven parses the `---` ... `---` block
   with js-yaml's JSON schema (`JSON_SCHEMA`), which preserves YAML
   `null`, booleans, and numbers as their JS equivalents. Malformed
   YAML opens the `Raven: Knit` output channel with the parse error.
3. **Deferred-feature detection.** Raven refuses three document
   shapes it doesn't implement — Shiny documents (`runtime: shiny` or
   `server: shiny`), a custom YAML `knit:` hook, and the `site:` field
   for `rmarkdown::render_site` / `bookdown::bookdown_site`. Each
   refusal includes a copy-pasteable R command you can run yourself in
   the R console. This check belongs to the preview pipeline but gates
   the export commands too: they re-knit through the preview (or reuse
   its cached output), so a refused document is never exportable
   either.
4. **YAML output options.** Any `output:` format (`html_document`,
   `pdf_document`, `word_document`, `bookdown::pdf_document2`, etc.)
   previews as HTML. Nested options are partially honored — see the
   "YAML output options" section below. When the
   matching HTML options do not specify `dev`, Raven uses SVG figures
   for the preview so plot colors can be themed in the panel. An
   explicit `dev:` in YAML or in a chunk is still honored.
5. **Working-directory resolution.** Controlled by
   `raven.knit.workingDirectory`:
   - `document` (default) — directory containing the R Markdown source.
   - `project` — workspace folder containing the R Markdown source. Refuses if
     the document is outside every workspace folder.
   - `current` — substitutes `getwd()` for `root.dir`, so chunks
     evaluate from R's startup working directory.
6. **R expression construction.** Raven validates the file and output
   paths (rejecting NUL, most control characters, DEL) and escapes
   each interpolated value as a single-quoted R literal. The result is
   a single `local({ ... })` expression. Simplified, it looks like:
   ```r
   local({
     knitr::opts_knit$set(root.dir = '...');
     out <- knitr::knit(input = '...', output = '....md',
                        envir = new.env(), quiet = TRUE);
     cat('Output created: ', out, '\n', sep = '')
   })
   ```
   The real expression also sets `base.dir` and a chunk `fig.path`, and
   writes the markdown to a `.tmp` sidecar that is atomically renamed
   into place, so a failed render never overwrites a previous result.
7. **Subprocess spawn.** Raven spawns `R --no-save --no-restore -e
   <expression>` via `child_process.spawn` (never a shell), inheriting
   the current environment. The R binary path is taken from
   `raven.packages.rPath` if set, otherwise from `PATH`.
8. **Streaming.** Standard output goes to the `Raven: Knit` output
   channel verbatim. Standard error lines are prefixed `[stderr]`. A
   notification with a "Cancel" button runs alongside.
9. **Cancellation / timeout.** Cancel and timeout use the same kill
   ladder: SIGINT, then SIGTERM after 5 s, then SIGKILL after another
   5 s. The default timeout is 10 minutes
   (`raven.knit.timeoutMs = 600000`). Windows uses `taskkill /T /F`
   instead of POSIX signals.
10. **Post-knit render.** `knitr::knit` writes `<basename>.md` into the
    per-session preview temp directory (`<os.tmpdir()>/raven-knit/...`),
    not next to the source. Raven reads that markdown, **strips the
    YAML frontmatter from the in-memory copy** (so the preview never
    shows a frontmatter table — the on-disk `.md` keeps its YAML for
    Pandoc export), then calls VS Code's `markdown.api.render` to
    convert it to HTML (KaTeX math, image rewriting, scroll-sync
    metadata, and any registered `markdown-it` plugins all happen
    here), and then walks the result for
    `<pre><code class="language-X">` blocks. Each block is
    re-highlighted using:

    - the GitHub light/dark palette, selected by
      `@media (prefers-color-scheme: dark)` so the same file
      auto-detects the OS color scheme when opened in a browser
      and follows VS Code's editor theme (which usually mirrors
      the OS) when shown in the panel iframe;
    - whichever TextMate grammar your installed VS Code extensions
      contribute for the chunk's language. For R the resolution
      priority is `REditorSupport.r-syntax` →
      `REditorSupport.r` → the built-in `vscode.r` → Raven's own
      vendored grammar (so R chunks always highlight, even in fresh
      remote sessions);
    - Raven's `function` LSP semantic-token overlay on top of R
      blocks, so function definitions and call heads pick up the
      `function` color even when the TextMate grammar doesn't
      classify them.

    Only the input chunks (the blocks that carry a `language-X`
    class) get the bordered code-panel surface. Output blocks —
    emitted by `knitr` as untagged fenced blocks — render as bare
    monospace text, so a reader can tell input from output the
    same way Quarto's preview does.

    The result is written atomically to `<basename>.html` via a
    temp-and-rename inside the per-session preview temp directory
    (alongside the intermediate `<basename>.md`), so a concurrent
    re-knit can never expose a half-written file to the panel.

11. **Reveal.** Raven opens the rendered HTML in a **Knit Preview**
    webview panel beside the editor — no success popover, the panel
    itself is the signal. Each R Markdown source gets its own panel; knitting a
    second source opens a separate panel that stacks as a tab in the
    same "preview" column rather than replacing the first. Re-knitting
    the same source updates its panel in place. The panel toolbar
    is icon-only and stays single-row at every panel width. From left
    to right it carries:

    - **Knit again** (refresh icon) — re-knits the source document (the
      same code path as invoking `Raven: Knit Preview` from the
      palette).
    - **Export** (share icon) — opens an in-shell popover with three
      format choices (Pandoc HTML, PDF, Word). While an export is in
      flight the icon swaps to a stop-circle and the next click
      cancels the running pandoc invocation. The popover dismisses on
      outside click, on Escape, or when you click into the rendered
      report — the same UX the Plot Viewer's share popover uses.
    - **Open in Browser** (globe icon) — opens the rendered file in
      your OS default browser. In remote workspaces (Remote SSH, Dev
      Containers, WSL, Codespaces) the `file://` URI behind this
      action targets the remote machine, not where you are sitting,
      so the button (and the matching right-click "Open in Browser"
      menu item) is omitted from the toolbar. Use **Export** instead
      — its toast routes through the remote-aware Download flow that
      streams the file to your local machine.
    - **Apply VS Code theme** — toggle that overlays the rendered
      document with VS Code's active editor background, foreground,
      and link colors. Code blocks (both fenced and inline `<code>`)
      pick up the theme's `textCodeBlock` shading so they match the
      rest of the surface. SVG plots generated under the preview
      `figure/` directory are sanitized, inlined inside the sandboxed
      iframe, and recolored with the same background/text/stroke
      overlay used by Raven's plot viewer. Raster plots (`png`,
      `jpeg`, etc.) still render normally, but pixels cannot be
      recolored after knitting; use `dev: svg` if you explicitly set
      a device and want themed plots. Syntax-token colors are extracted from
      the active theme's TextMate `tokenColors` (plus any
      `semanticTokenColors` entries that map to one of Raven's
      coarse roles, plus your `editor.tokenColorCustomizations`)
      and applied to the rendered code spans. If Raven cannot
      resolve the active theme's JSON — for example because the
      theme ships in tmTheme XML format, has an unresolvable
      `include` chain, or fails JSON parsing — the toggle falls
      back to Raven's bundled light/dark GitHub palette and a
      single line is written to the `Raven: Knit` output channel.
      The button keeps its label and conveys the active state
      visually (pressed-button styling, `aria-pressed` for screen
      readers) — Rmd output doesn't have a "document theme" to
      switch back to, so the toggle simply represents whether the
      overlay is on. The preference is persisted to `globalState`
      so subsequent knits restore it, and the panel tracks VS Code
      theme switches and changes to
      `editor.tokenColorCustomizations` /
      `editor.semanticTokenColorCustomizations` in real time —
      no re-knit required.

      The role mapping is coarse (ten roles: keyword, string,
      number, comment, function, type, variable, operator,
      punctuation, constant). Semantic-token selectors with
      modifiers (`function.declaration`, `*.defaultLibrary`) are
      not honored; only bare type names are. The result is
      "theme-tinted" highlighting that matches the editor's mood
      and broad role palette, not a pixel-identical rendering of
      VS Code's own code coloring.

    Standard text copy works inside the rendered output:
    Cmd/Ctrl-C copies the current iframe selection to the system
    clipboard, Cmd/Ctrl-A selects all, and right-clicking opens a
    small menu with **Copy**, **Copy image**, **Select All**, and
    **Open in Browser**. VS Code suppresses the browser's default context menu
    and does not forward Cmd-C from a nested iframe to its own
    clipboard command, so Raven wires these by hand on the iframe's
    same-origin `contentWindow`.

    The rendered HTML loads inside an `<iframe srcdoc="..."
    sandbox="allow-same-origin">` — scripts, forms, popups, and
    top-navigation are blocked. The HTML is inlined into the iframe
    via `srcdoc` (a nested webview iframe cannot navigate to
    `webview.asWebviewUri(...)` URLs — Electron's resource handler
    does not intercept nested-frame navigations); a `<base href>`
    injected at the top of the inlined HTML lets relative images,
    CSS, and fonts resolve through the webview's resource handler.

    Local `<img>` sources are additionally read off disk and inlined
    as `data:` URLs before the HTML reaches the iframe, because a
    nested `<iframe srcdoc>` cannot fetch webview subresources. This
    covers both knitr's generated `figure/` plots and pre-existing
    workspace images referenced with `knitr::include_graphics()`. A
    relative source is resolved against the preview output directory
    first (where the generated `figure/` plots live) and then against
    the knit `root.dir` (where an author's
    `include_graphics("images/logo.png")` is rooted); an absolute source
    resolves as-is. Either way the file is inlined only if it resolves
    (after `realpath`, so symlinks cannot escape) inside an allowed
    root: the temp preview directory, the workspace folder containing
    the source document (its own directory when no workspace is open),
    or the knit `root.dir`. A path outside every root is left untouched.
    The `root.dir` used here is the one Raven configures from
    `raven.knit.workingDirectory` (document dir / project root / current
    dir); if R changes its effective directory at runtime — a setup
    chunk's `knitr::opts_knit$set(root.dir = …)` or an `.Rprofile`
    `setwd()` — that is not tracked for *relative* images. Reference
    those by absolute path (e.g. `normalizePath(...)`, as an
    existing-image workflow typically does).
    To keep `include_graphics()` from failing the knit by relativizing
    an absolute path against `root.dir`, Raven sets
    `options(knitr.graphics.rel_path = FALSE)` for its knit subprocess
    only. The on-disk `.html` keeps the original `<img>` paths, so
    **Open in Browser** stays small; only the in-memory copy handed to
    the iframe is rewritten. (A consequence: a relative
    `include_graphics` path rooted at `root.dir` renders in the panel but
    not via **Open in Browser** — the on-disk `.html` lives in Raven's
    temp preview directory, so a browser resolves the relative path under
    *that* directory, where the image isn't. Reference the image by
    absolute path if you need it in both surfaces. Generated `figure/`
    plots are unaffected because knitr writes them into the preview
    directory next to the `.html`.)

    Intra-document anchor links (`#section`) work; clicking an
    external `<a>` does nothing (use **Open in Browser** for full
    interactivity, including htmlwidgets). When the output-path
    parse fails Raven surfaces "Knit succeeded (output path
    unknown)" — the subprocess exit code is the ground truth.

    The on-disk artifacts after a successful preview are:

    ```text
    <os.tmpdir()>/raven-knit/<workspaceHash>/<sessionId>/preview/<sourceHash>/
      <basename>.md     — intermediate markdown knitr wrote
      <basename>.html   — final rendered HTML the panel reads
      figure/           — knitr-generated plots
    ```

    Where `<workspaceHash>` is a SHA-256 of the first workspace folder
    URI (or the source document's parent directory when no workspace is open),
    `<sessionId>` is a UUID generated at extension activation so two
    VS Code windows on the same workspace are isolated, and
    `<sourceHash>` is a SHA-256 of the source document's absolute path. The
    whole `preview/<sourceHash>` directory is removed when its panel is
    disposed (closed). What happens at window exit depends on
    `raven.knit.persistPreview`:

    - **On (default)** — the `preview/` artifacts of panels that are
      still open are **kept** so they can be restored on the next launch
      (only the throwaway `export/` subtree is removed). On restore,
      Raven adopts that old-session `preview/<sourceHash>` directory into
      the new session's tree, so a later **Knit again** is a normal
      in-place update. Orphaned leftovers (a window closed and never
      reopened) are reclaimed by the >7-day sweep and by **Raven: Clean
      Up Knit Preview Cache**.
    - **Off** — the entire session subtree is removed when VS Code exits,
      as before.

    Stale sibling sessions (>7 days) are swept on activation regardless.
    **Raven: Clean Up Knit Preview Cache** reclaims orphaned session
    directories on demand; it never touches the current session, nor any
    session written within the last few minutes (so a preview open in a
    concurrent window on the same workspace is spared).

## Exporting

The webview's `Export ▾` button and the editor-title Raven menu both
expose `Export to HTML…`, `Export to PDF…`, and `Export to Word…`.
The webview button reuses the cached preview `.md` — so it's fast and
won't re-run R chunks. The editor-title commands always knit fresh
(they don't peek at panel state).

Both paths shell out to Pandoc, which is resolved lazily on first use
from `raven.pandoc.path`, then from `PATH`, then from standard install
locations (Homebrew, RStudio's bundled Pandoc, etc.). If Pandoc is
missing Raven shows an actionable error with an "Install Pandoc…"
button.

The exported file is written next to the source document as
`<basename>.{html,pdf,docx}`. Writes are atomic (temp file + rename),
so a cancelled or failed export never corrupts a prior successful
output. A notification offers two buttons on success:

- A format-specific external-open button — **Open in Browser**
  (`html`), **Open PDF** (`pdf`), or **Open in Word** (`docx`) — which
  hands the file to the OS default handler. In a remote workspace
  (Remote SSH, Dev Containers, WSL, Codespaces) this button is
  replaced with **Download**, which reveals the file in the file
  explorer and runs VS Code's built-in `explorer.download` to copy
  it from the remote machine to a local path you pick; the
  OS-handler buttons can't reach your local apps from a remote
  workspace.
- **Open in Editor**, which opens the file inside your editor via
  `vscode.open`. Useful when you don't want to leave the editor, or
  on a remote workspace as an alternative to downloading. The viewing
  experience for PDF/Word depends on whichever editor or extension
  the host has registered for that file type.

PDF export uses the PDF engine configured at `raven.pandoc.pdfEngine`
(default `xelatex`, a LaTeX engine; `wkhtmltopdf` renders via WebKit
instead and needs no LaTeX). If the engine isn't found Raven surfaces an
"Install TinyTeX…" hint.

### YAML output options

| Key | Where it's applied |
|---|---|
| `fig_width`, `fig_height`, `fig_retina`, `dpi`, `dev` | `knitr::opts_chunk$set` before knitting (preview and export). HTML preview defaults `dev` to `svg` when you do not set it, so the theme toggle can recolor plots; explicit raster devices are honored but cannot be recolored live. |
| `toc`, `toc_depth` | Pandoc `--toc` / `--toc-depth` (export only) |
| `number_sections` | Pandoc `--number-sections` (export only) |
| `highlight` | Pandoc `--highlight-style` (export only; validated against the known list) |
| `self_contained` | HTML export always passes Pandoc `--embed-resources` for portable output. `self_contained: false` is logged to the `Raven: Knit` output channel and ignored — the linked-assets workflow it implies would require shipping the temp `figure/` dir next to the exported `.html`, and that dir gets purged after the preview panel closes. Ignored for PDF/Word too. |
| `css` | Pandoc `--css=<absolute path>` (containment-checked against the workspace folder / source-document parent) |
| `mathjax` | Pandoc `--mathjax` |
| `pandoc_args` | Appended verbatim to the Pandoc argv during export (after Raven's own flags), except entries that set destination (`-o`, `--output`) or output format (`-t`, `--to`, `-w`, `--write`) — those are stripped because the editor menu owns them. Stripped entries are logged to the `Raven: Knit` output channel. Preview never invokes Pandoc, so `pandoc_args` does not affect preview output. |

Keys **not honored (in preview *and* export)**: `theme`,
`code_folding`, `df_print`, `code_download`, `template`, `includes`.
They are logged to the `Raven: Knit` output channel when present in
your YAML. The omission is deliberate and spans both pipelines: these
are produced by `rmarkdown::html_document`'s own R-side machinery
(Bootstrap theming, a JS runtime, data-frame print methods), and
neither of Raven's pipelines goes through it — the preview renders
knitr's markdown with VS Code's markdown engine, and export hands that
same markdown straight to Pandoc. Pandoc alone has no equivalent, so
exporting to HTML/PDF/Word doesn't honor them either. For full
template fidelity, run `rmarkdown::render(...)` in the R console.

## Settings

| Setting | Default | Description |
|---|---|---|
| `raven.rConsole.activation` | `auto` | Gates the knit command (and the R console / plot / data viewers / chunk run commands). |
| `raven.knit.workingDirectory` | `document` | `document` / `project` / `current`. |
| `raven.knit.timeoutMs` | `600000` | Hard timeout (ms) for the knit R subprocess. |
| `raven.knit.persistPreview` | `true` | Keep the Knit Preview panel across window reload/restart (restores the last rendered output without re-knitting). Disable to drop previews and their temp files on window close. |
| `raven.knit.export.timeoutMs` | `120000` | Hard timeout (ms) for the Pandoc subprocess during export. |
| `raven.knit.fontFamily` | `""` | Body/prose font for the preview. Empty inherits `markdown.preview.fontFamily`. |
| `raven.knit.monospaceFontFamily` | `""` | Monospace font for code chunks and output. Empty inherits `editor.fontFamily`. |
| `raven.pandoc.path` | `""` | Absolute path to a Pandoc binary. Empty uses PATH + standard install locations. |
| `raven.pandoc.pdfEngine` | `xelatex` | PDF engine for export. `xelatex`, `pdflatex`, `lualatex`, `tectonic` are LaTeX engines; `wkhtmltopdf` renders via WebKit instead. |
| `raven.packages.rPath` | (auto) | Path to the R binary. Empty means "search PATH". |

### Fonts

The two font settings accept the same comma-separated form as CSS
`font-family` — quoted names with spaces are fine, e.g.
`"JetBrains Mono", "Fira Code", monospace`.

Resolution order per slot:

1. Your `raven.knit.fontFamily` / `raven.knit.monospaceFontFamily` if
   non-empty.
2. VS Code's `markdown.preview.fontFamily` (body) or
   `editor.fontFamily` (mono). These resolve to OS-specific defaults
   when you have not set them, so the preview always looks reasonable
   on the machine you're knitting on.
3. A hard-coded fallback if step 2 somehow yields an invalid value.

Both settings are **resource-scoped** — you can override them
per-folder in a multi-root workspace via `.vscode/settings.json`. The
mono fallback also honors VS Code's `[rmd]` / `[quarto]` language-scoped
`editor.fontFamily` blocks, so a per-language editor font flows into
the preview.

Fonts are **baked into the rendered `.html` at knit time**, AND the
open preview panel updates **live** when you change any of the four
settings above. No re-knit is needed while the panel is open — the
extension recomputes the fallback chain on every
`onDidChangeConfiguration` event and pushes the result into the
webview. The on-disk `.html` keeps the snapshot from the last knit, so
"Open in Browser" picks up the new fonts the next time you re-knit.
The same is true if you email or host the file: the recipient sees
whatever fonts were active at knit time.

**Browser portability.** The `.html` that "Open in Browser" produces
is the same file the panel reads. The browser reads font names
verbatim from the CSS, so a reader without your configured fonts
installed will fall through the comma list. Raven automatically
appends a generic terminator (`, monospace` for code, `, sans-serif`
for body) when your value doesn't already end with one, so the browser
always lands on a sensible generic family rather than reverting to
Times. For the most robust portability across machines include your
own fallback list, e.g.
`"JetBrains Mono", "Fira Code", Menlo, monospace`.

**Rejection rules.** A value is rejected (and the next item in the
fallback chain is used) when it:

- Exceeds 500 characters.
- Contains any of `;` `{` `}` `<` `>` `\` or a control character
  (`\n` `\r` `\t` `\f` `\v` `\0`).
- Contains the CSS comment sequences `/*` or `*/`.
- Has an unmatched `"` or `'` (CSS string would run past the
  declaration and corrupt adjacent styles).
- Has any `(` or `)` outside a quoted family name. Parens inside
  quoted names are fine — `"Aptos (Body)", sans-serif` is accepted —
  but a bare `Foo(bar` would open a CSS function-token that escapes
  the declaration.
- Has a leading, trailing, or consecutive comma — `Georgia,` or
  `Arial,,Times` would produce an empty entry that the browser drops
  via IACVT.
- Is exactly one of the CSS-wide keywords (`inherit`, `initial`,
  `unset`, `revert`, `revert-layer`). The iframe has no useful parent
  for those to resolve against, so the fallback chain produces a
  better outcome.

The VS Code Settings UI also enforces the character and length rules
at edit time via a JSON schema `pattern`, so most character-class
rejections are caught before they reach the knit pipeline. The
schema cannot enforce balanced-paren / balanced-quote semantics, so
those checks live in the runtime sanitizer only.

## What Raven does **not** do

| Capability | Where it lives |
|---|---|
| Live preview of `.Rmd` / `.Rmarkdown` | `quarto.quarto`'s `Quarto: Preview`. Raven's Knit Preview remains a static viewer with a manual Knit again button. |
| `.qmd` preview or rendering through the Knit Preview pipeline | Use Raven's separate CLI-backed [Quarto Preview and Render](quarto.md) commands, or `quarto.quarto`. Knit Preview itself never handles `.qmd`; the pipelines are independent. |
| Visual Mode and Quarto-aware YAML intelligence | `quarto.quarto`. Raven supplies `.qmd` highlighting and R-chunk language features, but not these Quarto-specific editor surfaces. |
| html_document-specific YAML options (`theme`, `code_folding`, `df_print`, …) | Out of scope. Honoring them requires becoming `rmarkdown::html_document` (Bootstrap + JS runtime). Use `rmarkdown::render(...)` in the R console for full template fidelity. |
| `pandoc_args:` *full* passthrough | The editor menu picks export destination (always sibling of the source document) and format, so `-o`/`--output`/`-t`/`--to`/`-w`/`--write` are stripped from YAML's `pandoc_args` and logged. Everything else flows through — see the honored-options table above. |
| Custom YAML `knit:` hook dispatch | Run the hook function manually in the R console. |
| Knit-with-Parameters dialog | Edit YAML defaults, or call `rmarkdown::render(params = list(...))`. |
| `runtime: shiny` documents | `rmarkdown::run('foo.Rmd')` in the R console. |
| `rmarkdown::render_site` | Run `rmarkdown::render_site()` in the R console. |
