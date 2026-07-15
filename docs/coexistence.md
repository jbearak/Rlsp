# Coexistence with Other R Extensions

Raven's VS Code extension includes both a **language server** (completions, diagnostics, navigation) and **R-session features** (R console, plot viewer, data viewer, help viewer). If you also use the [REditorSupport extension](https://marketplace.visualstudio.com/items?itemName=REditorSupport.r) or [Positron](https://github.com/posit-dev/positron), Raven is designed to stay out of the way by default.

## The `raven.rConsole.activation` setting

The `raven.rConsole.activation` setting (default: `"auto"`) controls whether Raven's R console — and therefore its plot and data viewers, chunk run commands, Knit Preview, and Quarto Preview/Render — activates:

- **`"auto"`** (default) — Raven's R-session features activate *unless* the REditorSupport extension is enabled or VS Code is running as Positron. This keeps Raven out of the way when you already have R-session integration.
- **`"enabled"`** — Always activate Raven's R console, plot viewer, and data viewer, even when another R extension is present. With REditorSupport also enabled, both extensions' R consoles are available; `Cmd+Enter` / `Ctrl+Enter` can only be bound to one extension's send command, and VS Code's keybinding editor lets you rebind either.
- **`"disabled"`** — Never activate Raven's R-session features, even when no other R extension is present.

Code intelligence and the help viewer are unaffected by this setting. The help viewer shells out to R on demand to render Rd documentation as HTML, so it works whether or not Raven's R console is active — provided Raven can run the configured R executable.


## What steps aside and what doesn't

Raven's **language server** and **help viewer** always activate — they don't overlap with a running R session.

Raven's **R console**, **plot viewer**, and **data viewer** are the features that can overlap with another extension's R-session integration. The plot and data viewers are reached *through* Raven's R console: the R console boots a profile that overrides `View()` (data viewer) and starts httpgd (plot viewer). When Raven's R console isn't activated, neither of those viewers is wired up — `View(df)` and `plot(...)` go to whatever R session your other extension manages.

Several editor surfaces that overlap with REditorSupport's R Markdown / Quarto tooling are gated behind the same R-console-activation switch:

- **Chunk navigation** commands and keybindings (`raven.goToNextChunk`, `raven.goToPreviousChunk`, `raven.selectCurrentChunk`).
- **Chunk background highlighting** and the **active-cell indicator** in `.Rmd` / `.Rmarkdown` / `.qmd` documents.
- **`.R` cell mode** support: `# %%` cell markers and RStudio-style `# Section ----` dividers used for chunk navigation / highlighting in plain `.R` files.
- **R-language snippets in `.Rmd` / `.Rmarkdown` / `.qmd` fenced chunks** — Raven's `r.json` snippets (`if`, `fun`, `for`, etc.) are registered for `rmd` / `quarto` only when Raven's R console is active. R-Markdown- and Quarto-specific snippets that scaffold new chunks (`rchunk`, `setupchunk`, ...) always register, since REditorSupport doesn't ship equivalents.
- **Knit Preview** keybinding (`Shift+Cmd+Enter` / `Shift+Ctrl+Enter`) and the `Raven: Knit Preview` command — gated on the R-console switch together with the `raven.rmdKnit.enabled` flag.
- **Quarto Preview and Render** commands, editor-title button, and Preview keybinding — gated on the R-console switch. **Stop Quarto Preview** stays available from the Command Palette so a running process can always be cleaned up.

## Who benefits from Raven's R console?

Raven's R console, plot viewer, and data viewer overlap with [REditorSupport](https://marketplace.visualstudio.com/items?itemName=REditorSupport.r)'s equivalents. Whether Raven's versions help you depends on your workflow:

- **You'd benefit** — e.g., you send large blocks to R (Raven's temp-file `source()` sidesteps bracketed-paste delays); you regularly view large frames (Raven's data viewer streams Arrow rows on demand); you work with labelled data (Raven's data viewer reads variable and value labels from `haven` imports of Stata / SPSS files, showing value labels instead of the underlying codes); or you want a themeable `.Rmd` / `.Rmarkdown` preview (`Raven: Knit Preview` renders through VS Code's built-in Markdown pipeline rather than a Pandoc round-trip, and its **Apply VS Code theme** toggle recolors the whole rendered document — prose, syntax highlighting, and plots — to match your editor theme in real time). See [Comparison: R session integration](./comparison.md#r-session-integration).
- **You'd lose something** — REditorSupport ships a few R-session features Raven doesn't: a workspace viewer, an htmlwidget / Shiny viewer, and `View()` on lists and environments (Raven's data viewer handles only data frames and matrices).

For a broader list of Raven's gaps across features, see [Limitations](./limitations.md).

## Language servers: Raven alone vs. both

Raven's language server traces `source()` chains across your project, so its completions, diagnostics, and navigation reflect actual execution order at the cursor position — including cross-file go-to-definition, find-references, and detection of circular dependencies and scope violations. It doesn't need a running R session.

REditorSupport's language server provides [`lintr`](https://lintr.r-lib.org/) diagnostics, which covers a different surface than Raven's diagnostics: it catches style violations and certain correctness issues that Raven doesn't flag, while Raven catches cross-file scope problems that lintr doesn't see. Raven also has its own [opt-in style linter](./linting.md) that re-implements 18 of `lintr`'s default linters — most of its default rule set; for the rules Raven doesn't ship, run `lintr` via REditorSupport alongside Raven.

To avoid double-reporting style issues, Raven's default `raven.linting.enabled: "auto"` will *not* auto-enable its native linter from a discovered `.lintr` while REditorSupport's `lintr` path is live (installed and enabled with `r.lsp.enabled` and `r.lsp.diagnostics` on) or while you're in Positron — that `.lintr` is REditorSupport's / Positron's to lint. If you want both linters running on the same project on purpose, set `raven.linting.enabled` to `true`. See [Linting § `"auto"` and REditorSupport / Positron](./linting.md#auto-and-reditorsupport--positron).

If you want both, leave `r.lsp.enabled` at its default (`true`). Both language servers will run, with some overlap in completions and diagnostics.

One reason to keep REditorSupport installed even if Raven covers your language-intelligence needs is its workspace viewer — a sidebar panel that introspects your active R session, showing live objects, their types, and dimensions. Raven doesn't have an equivalent.

If you keep REditorSupport installed but don't need its language server (e.g. you only want the workspace viewer), you can disable it to avoid the overhead of running two LSPs:

```json
"r.lsp.enabled": false
```

See [Editor Integrations](./editor-integrations.md) for setup details across editors.

## Coexistence with the Quarto extension

When the [Quarto extension](https://marketplace.visualstudio.com/items?itemName=quarto.quarto) is installed alongside Raven, *and* Raven's R console is active, you'll see two CodeLens rows above every R chunk in a `.qmd` file: Quarto's (**Run Cell · Run Next Cell · Run Above**) and Raven's (**▷ Run Chunk · ↘ Run Next Chunk · ↥ Run Above**, configurable via `raven.chunks.codeLens.commands`).

Raven's chunk CodeLens activates with Raven's R console — see [The `raven.rConsole.activation` setting](#the-ravenrconsoleactivation-setting). Under the default `"auto"`, Raven's R console activates only when no other R extension (REditorSupport.r, or Positron) is enabled. So the dueling-row case appears when you have Quarto installed but not REditorSupport.r, or when you've explicitly set `raven.rConsole.activation` to `"enabled"` alongside REditorSupport.r.

What each row does depends on whether REditorSupport.r is installed:

- **REditorSupport.r not installed**. Quarto's row shows "Executing r cells requires the R extension" and does nothing — its dispatcher checks for REditorSupport's extension ID, not for the underlying `r.runSelection` command. Raven's row works as usual: clicking **▷ Run Chunk** sends the chunk to Raven's R terminal.
- **REditorSupport.r installed, `raven.rConsole.activation` set to `"enabled"`**. Both rows work — Quarto's dispatches to REditorSupport's `r.runSelection`; Raven's runs the chunk in Raven's terminal.

Both extensions register CodeLens providers for `.qmd`, and VS Code doesn't expose a way for one extension to suppress another's lenses, so when both rows appear they stay independent. Quarto's check looks for the REditorSupport extension by ID, so Raven can't satisfy it from this side regardless of which commands Raven registers — the only clean fix lives upstream in the Quarto extension.

If the duplication bothers you, a standalone Raven workflow is now viable with `quarto.quarto` disabled. Raven maps `.qmd` to its bundled R Markdown grammar for syntax highlighting, supplies Quarto and R snippets, provides chunk navigation and CodeLens execution through Raven's R console, and offers CLI-backed [Quarto Preview and Render](./quarto.md). With no competing R extension, the default `raven.rConsole.activation: "auto"` enables Raven's R console; set it to `"enabled"` if another R extension would otherwise close that gate.

The trade-off is that disabling `quarto.quarto` also removes its Visual Mode, Quarto-aware YAML intelligence, and Python / Julia cell executors. Raven's chunk executor handles R through Raven's R console; it does not replace those non-R execution surfaces. If you need them, keep both extensions and use Raven's CodeLens row for R chunks. Quarto's row remains useful for other languages when their respective executors are installed.

### `.Rmd` / `.Rmarkdown` files belong to the `rmd` language

Raven anchors `.Rmd` / `.Rmarkdown` files to its `rmd` language by default, so the conflict described here doesn't bite you out of the box — the rest of this section explains why that default exists and how to undo it if you'd rather Quarto own `.rmd` variants.

The Quarto extension contributes both `.qmd` and `.rmd` under `editorLangId == quarto`. When only Raven and Quarto are installed, VS Code's `contributes.languages` extension resolver can pick Quarto's claim over Raven's `rmd` contribution — the resolution order across extensions is not a documented invariant — so `.Rmd` files can end up tagged as `quarto`. Most Raven affordances handle both lang ids (run-line, navigation, CodeLens, the send-to-R menus), but **Knit Preview** is gated on `editorLangId == rmd || == r` (with a `resourceExtname =~ /\.(rmd|Rmd|RMD|rmarkdown|Rmarkdown|RMARKDOWN)$/` guard) and silently stops firing when the `.Rmd` buffer is tagged `quarto`. The visible symptom is `Shift+Cmd+Enter` (`Shift+Ctrl+Enter` on Linux/Windows) triggering Quarto's "Editor selection is not within an executable cell" message instead of opening the knit preview.

Raven ships a `files.associations` default pinning six explicit case variants — `*.rmd`, `*.Rmd`, `*.RMD`, `*.rmarkdown`, `*.Rmarkdown`, and `*.RMARKDOWN` — all to the `rmd` language (matching the extensions in Raven's `contributes.languages` block). `files.associations` is registered ahead of `contributes.languages` in VS Code's resolver, so this anchors R Markdown files to `rmd` regardless of whether REditorSupport.r-syntax is installed alongside.

Trade-off: this also disables Quarto's `.Rmd`-targeted UI (Run Cell, Insert Cell, preview-related menus, the chunk CodeLens) for users who keep the default, because those gate on `editorLangId == quarto`. Quarto's `.qmd` handling is untouched. If you'd rather hand `.rmd` variants back to Quarto, override the default explicitly in user or workspace settings:

```json
"files.associations": {
  "*.rmd": "quarto",
  "*.Rmd": "quarto",
  "*.RMD": "quarto"
}
```

`files.associations` is an object-merge setting, so the override above replaces only the keys it lists — other Raven `files.associations` defaults (none today, but possible in the future) stay in effect.
