# R Console

Raven provides an integrated R console inside VS Code, plus commands to send R code from the editor to that console for execution. It supports the standard R console as well as [arf](https://github.com/eitsupi/arf) and [radian](https://github.com/randy3k/radian) — modern third-party R consoles with syntax highlighting and richer interactive features.

The R console is the entry point to Raven's [plot viewer](./plot-viewer.md) and [data viewer](./data-viewer.md): plots produced in this console render in a VS Code panel via httpgd, and `View(df)` opens Raven's data viewer instead of R's default. Raven's [help viewer](./help-viewer.md) works independently of the R console.

> [!NOTE]
> Whether the R console activates is controlled by `raven.rConsole.activation` (default: `auto`). When the REditorSupport extension is enabled or VS Code is running as Positron, Raven's R console — and therefore its plot and data viewers — is off by default. See [Coexistence](./coexistence.md) for details.

## R Terminal Profile

The extension registers an "R" terminal profile in VS Code's terminal dropdown. You can open an R terminal manually from the terminal profile picker at any time.

### Choosing the R program

The `raven.rTerminal.program` setting controls which program is launched:

| Value | Description |
|-------|-------------|
| **R** (default) | Standard R console |
| **arf** | Rust-based R console with syntax highlighting, fuzzy history search (`Ctrl+R`), interactive help, and rig integration |
| **radian** | Python-based R console with syntax highlighting, multiline editing, and mouse support |

The selected program must be available on your PATH.

## Keyboard Shortcuts

| Mac | Windows/Linux | Action |
|-----|---------------|--------|
| `Cmd+Enter` | `Ctrl+Enter` | Run line or selection (every supported file type) |
| `Shift+Cmd+Enter` | `Shift+Ctrl+Enter` | Source File (`.R`), Knit Preview (`.Rmd` / `.Rmarkdown`, when Raven's R console is active), or Quarto Preview (`.qmd`) |

> [!TIP]
> You can also access these commands via the editor toolbar menu (`▶` button) or the command palette (`Cmd+Shift+P`).

## Commands

| Command | Description |
|---------|-------------|
| **Run Line or Selection** | Sends the current selection to R. If no selection, detects and sends the complete multi-line statement at the cursor. |
| **Run Upward Lines** | Sends all lines from the start of the file to the current line (extending to complete any multi-line statement). |
| **Run Downward Lines** | Sends all lines from the current line to the end of the file (extending upward to include the full statement start). |
| **Source File** | Runs `source("filepath", echo = TRUE)` in the R terminal. |

### Quick Inspection Commands

These commands wrap the word under the cursor (or the current selection) in a common inspection function and send it to the R terminal — one keystroke to check an object's shape. They're registered under the **Raven** category, so typing `Raven:` in the Command Palette surfaces them all. No default keybindings; bind them in `keybindings.json` if you want shortcuts.

| Command | Sends to R |
|---------|------------|
| **Raven: Show nrow** | `nrow(<target>)` |
| **Raven: Show length** | `length(<target>)` |
| **Raven: Show head** | `head(<target>)` |
| **Raven: Show head (transposed)** | `t(head(<target>))` |
| **Raven: Show names** | `names(<target>)` |
| **Raven: View** | `View(<target>)` |

If a selection is active, the entire selection is used as the target — handy for expressions like `df$col` or `subset(df, x > 0)`. Otherwise the word at the cursor is used. The commands run when the active editor's language is `r`, `rmd`, or `quarto` — placing the cursor on an R identifier inside an R chunk inside a `.Rmd` / `.Rmarkdown` / `.qmd` file is the intended use; placing it on a prose word will send something to R that R will reject.

Single-line wrapped expressions go straight to the terminal via direct paste; if the wrapped expression spans multiple lines (because the selection did), it honors the user's `raven.sendToR.sendMethod` setting. This avoids writing a one-liner like `nrow(x)` to a temp file just because `tempfile` mode is configured for normal sends.

## Editor Toolbar

A toolbar button (▶) appears in the editor title bar for `.R`, `.Rmd`, `.Rmarkdown`, and `.qmd` files, providing quick access to send commands relevant to the open file's type. The menu is organized into two sections:

- **Main commands** — Send code to the managed R terminal. If no R terminal is open, one is created automatically.
- **Terminal submenu** — Send code to whatever terminal is currently active in VS Code, regardless of type. This is useful for sending commands to R running inside `tmux`, a Docker container, or any other terminal session that isn't the extension's built-in R terminal.

For `.R` files, the menu shows **Run Line or Selection**, **Run Upward Lines**, **Run Downward Lines**, **Source File**, and the **Terminal** submenu (which mirrors the same four entries against the active terminal). Chunk commands are reserved for chunk-based documents — `.R` files with `# %%` cells access **Run Current Chunk** etc. through the CodeLens or the command palette, not the toolbar. For `.Rmd`, `.Rmarkdown`, and `.qmd` files, the entries that auto-include prose or YAML — **Run Upward Lines**, **Run Downward Lines**, and **Source File** — are hidden, and the menu shows **Run Line or Selection**, the current-chunk pair (**Run Current Chunk**, **Run Current Chunk and Move**), the directional pair (**Run Above Chunks**, **Run Below Chunks**), the whole-document pair (**Run All Chunks**, plus **Knit Preview** and the **Knit: Export to HTML / PDF / Word** commands for `.Rmd` / `.Rmarkdown` files when Raven's R console is active), and the **Terminal** submenu. Inside the Terminal submenu the same chunk pairs mirror the main menu (sans the knit commands, since knitr always renders against the document on disk rather than the active terminal). The Source-File keyboard shortcut (`Shift+Cmd+Enter` / `Shift+Ctrl+Enter`) is repurposed on `.Rmd` / `.Rmarkdown` files when knit is enabled to invoke **Knit Preview**, and on `.qmd` files to invoke **Quarto Preview**. Quarto Preview does not depend on Raven's R console, but it does require a trusted workspace. On R Markdown files when knit is disabled the chord is unbound — invoke **Run All Chunks** from the toolbar menu, the command palette, or your own keybinding.

Code sent via the Terminal submenu follows the same send method as the main commands. By default, Raven pastes short blocks directly and writes longer blocks to a temporary file, executing them with `source()`. **Terminal: Source File** runs `source()` directly against the document's saved path on disk (saving first if the buffer has unsaved changes).

> [!TIP]
> **Remote sessions and long-running jobs** — If you're connected to a remote host via VS Code Remote Development and want an R session that survives disconnections (closing your laptop, losing internet, etc.), launch a terminal multiplexer like `tmux` and start R inside it. Then use the Terminal submenu to send code to that tmux-hosted R session. Because tmux keeps running on the remote host independently of your VS Code connection, you can disconnect and reconnect hours or days later with your session — and any long-running computation (MCMC sampling in Stan or JAGS, large simulations, etc.) — still intact.

The Terminal submenu commands — `raven.terminal.runLineOrSelection`, `raven.terminal.runUpwardLines`, `raven.terminal.runDownwardLines`, `raven.terminal.sourceFile`, plus the chunk-mode counterparts `raven.terminal.runCurrentChunk`, `raven.terminal.runCurrentChunkAndMove`, `raven.terminal.runAboveChunks`, `raven.terminal.runBelowChunks`, and `raven.terminal.runAllChunks` — ship without default keybindings. Assign your own in `keybindings.json` if you want keyboard access. A common convention is to mirror the main send shortcuts with `Option` (Mac) or `Alt` (Windows/Linux) in place of `Cmd`/`Ctrl`, e.g. binding `alt+enter` (or `option+enter`) to `raven.terminal.runLineOrSelection`.

## Statement Detection

When no text is selected, **Run Line or Selection** uses heuristics to detect complete R statements spanning multiple lines. The extension recognizes:

- **Unmatched brackets** — `(`, `[`, `{` that haven't been closed
- **Trailing operators** — lines ending with `+`, `-`, `*`, `/`, `^`, `|>`, `%>%`, `%any%`, `||`, `&&`, `|`, `&`, `~`, `=`, `<-`, `->`, `<<-`, `->>`, `<=`, `>=`, `==`, `!=`, `<`, `>`, `,`, `::`, `:::`
- **Continuation lines** — lines starting with `|>`, `%>%`, `)`, `]`, `}`, `+`, or other operators

This means pipe chains, multi-line function calls, ggplot layers, and if/else blocks are sent as complete units:

```r
# Cursor anywhere in this block sends all 3 lines:
df |>
  filter(x > 1) |>
  select(a, b)

# Same for ggplot:
ggplot(df, aes(x, y)) +
  geom_point() +
  theme_minimal()

# And function calls:
result <- foo(
  x = 1,
  y = 2
)
```

## Cursor Advancement

By default, the cursor advances to the next line after sending a single statement (not a selection or file). This allows rapid line-by-line execution with repeated `Cmd+Enter` presses.

**Configuration**: `raven.sendToR.advanceCursorOnSend` (default: `true`)

## Send Method

By default (`auto`), Raven pastes short blocks directly and switches to a temporary file once a block has at least `raven.sendToR.autoTempFileThresholdLines` lines (≥ N — default 25, so paste up to 24 lines, temp-file 25 or more), running it with `source()`. Override this with `raven.sendToR.sendMethod`:

- **`paste`** — always pastes. For multi-line code, uses bracketed paste mode to deliver the block as a single unit.
- **`tempfile`** — always writes to a temp file and runs `source()`. Use this when you want larger and smaller sends to follow the same `source()` path.

### Why `auto` switches to a temp file for larger blocks

Pasting works well for short blocks. For longer blocks it gets slow — the terminal feeds characters to R's stdin one at a time, so code that would start executing sooner via `source()` can take noticeably longer to type out.

Writing larger blocks to a temp file and `source()`-ing them sidesteps this, avoiding the inter-line paste delay and bracketed-paste compatibility issues for those blocks.

The cutover point is controlled by `raven.sendToR.autoTempFileThresholdLines` — a block with at least this many lines (≥ N) goes through a temp file; smaller blocks are pasted. The default of 25 is arbitrary but reasonable — most blocks below it paste fast and reliably on a local terminal. Lower it for slow remote connections; raise it if you prefer to see your code echoed in the terminal.

For a comparison with how the [REditorSupport extension](https://marketplace.visualstudio.com/items?itemName=REditorSupport.r) sends code, see [Comparison: R console](./comparison.md#r-console). If you prefer paste-everywhere behavior — for example, because you want raw paste output rather than `source()` echoing — set `raven.sendToR.sendMethod` to `"paste"`.

## Configuration Options

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `raven.rConsole.activation` | enum | `"auto"` | When Raven's R console activates: `"enabled"`, `"disabled"`, or `"auto"` (off when REditorSupport.r is enabled or running in Positron). |
| `raven.rTerminal.program` | enum | `"R"` | Program for the R terminal: `"R"`, `"arf"`, or `"radian"` |
| `raven.sendToR.advanceCursorOnSend` | boolean | `true` | Advance cursor to next line after sending a single statement |
| `raven.sendToR.sendMethod` | enum | `"auto"` | How code is sent to R: `"auto"`, `"paste"`, or `"tempfile"` |
| `raven.sendToR.autoTempFileThresholdLines` | integer | `25` | In `auto` mode, blocks with **≥ N lines** use a temp file; smaller blocks are pasted (so `25` means: paste up to 24 lines, temp-file 25 or more) |
