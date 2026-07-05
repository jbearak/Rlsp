# R Code Chunks

Raven recognizes R code chunks in R Markdown / Quarto documents and `# %%`-delimited cells in plain `.R` files. You can run a single chunk, every chunk above the cursor, or every chunk in the document; navigate forward and backward between chunks; and see a subtle background tint that makes chunks easy to scan.

This is the daily-driver workflow for `.Rmd` / `.Rmarkdown` / `.qmd` users coming from RStudio or vscode-R.

> [!NOTE]
> All chunk-related features — navigation commands (Go to Next/Previous Chunk, Select Current Chunk), chunk background highlighting and the active-cell indicator, `.R` cell mode (`# %%` markers and RStudio-style `# Section ----` dividers), the **run** commands (for example, Run Current Chunk, Run Above Chunks, Run All Chunks — see [Commands](#commands) for the full set), and every chunk CodeLens button (defaults `▷ Run Chunk` / `↘ Run Next Chunk` / `↥ Run Above`, plus any others added via `raven.chunks.codeLens.commands`) — are gated behind `raven.rConsole.activation`. The run commands and CodeLens additionally require Raven's R console because they create or reuse the R terminal Raven manages. If the R console is disabled (or `auto` defers to another R extension), none of these surfaces register, so REditorSupport / Positron handle chunks instead. See [Coexistence](./coexistence.md).

## Chunk forms

| Form | File types | Example header |
|------|------------|----------------|
| Fenced block | `.Rmd`, `.Rmarkdown`, `.qmd` | ```` ```{r setup, eval=FALSE} ```` |
| Cell marker | `.R` | `# %% Section 1` |

Fenced blocks may use either backticks or tildes, and four-or-more-character fences nest naturally (so a chunk can contain a literal `` ``` ``).

Only **R** chunks are sent to the R console. Chunks tagged with other languages (`{python}`, `{bash}`, `{julia}`, …) are still recognized for navigation and outline but not for execution.

Chunks also appear in the document outline (`Cmd/Ctrl+Shift+O`) as a distinct symbol kind, separate from section headers — see [Document Outline](./document-outline.md#r-markdown--quarto-chunks).

## Language features inside chunks

The R code inside chunk bodies is first-class. Raven analyzes all R chunk bodies as a single R program, so the editor's R language features work inside chunks of `.Rmd` / `.Rmarkdown` / `.qmd` documents and resolve symbols across chunks (a function defined in one chunk can be used, navigated to, and completed in a later chunk):

- [Diagnostics](./diagnostics.md), [completion](./completion.md), [hover](./hover.md), signature help, [go-to-definition](./go-to-definition.md), [find-references](./find-references.md), folding, expand-selection, and on-type [indentation](./indentation.md).

These features apply only inside R chunk bodies. On prose, YAML front matter, or non-R chunks they stand down — completion, signature help, and on-type indentation produce nothing rather than treating the line as top-level R. Non-R chunks (`{python}`, `{bash}`, …) are not analyzed as R.

### Cross-file resolution from chunks

Cross-file awareness ([Cross-File Analysis](./cross-file.md)) reads only chunk bodies, never prose. A `source("helpers.R")` written inside an R chunk creates a real dependency edge — symbols defined in the sourced file resolve in later chunks, and a missing target is flagged — while the same text in prose or a comment outside a chunk is ignored. `library()` calls and `# raven: cd` / `# raven: sourced-by`-style directives are likewise honored only inside chunks.

The relationship also works in the other direction: a plain `.R` file can declare `# raven: sourced-by analysis.Rmd`, and Raven will read the Rmd's chunks (not its prose) to supply the helper's inherited scope. `.Rmd` / `.Rmarkdown` / `.qmd` files are not added to the proactive workspace scan, so this incoming direction is established by the directive on the `.R` file, or by opening the Rmd itself.

### Suppressing diagnostics in a chunk

Two ways to silence Raven's diagnostics for an entire chunk:

- **knitr chunk option** `raven.ignore` in the chunk header:

  ````markdown
  ```{r, raven.ignore=TRUE}
  result <- some_undefined_thing   # no diagnostics in this chunk
  ```
  ````

  `raven.ignore=TRUE` (or `=T`) suppresses everything in the chunk body;
  `raven.ignore=FALSE` (the default) does nothing. Narrow it to specific codes
  with a quoted, comma-separated list:
  `raven.ignore="undefined-variable, line-length"`. An explicitly empty list
  (`raven.ignore=""`) is the empty set of codes and therefore suppresses
  nothing — use `raven.ignore=TRUE` to silence the whole chunk.

- **in-chunk directive** `# raven: ignore-chunk` anywhere in the chunk body
  (optionally `# raven: ignore-chunk[undefined-variable]`):

  ````markdown
  ```{r}
  # raven: ignore-chunk
  result <- some_undefined_thing
  ```
  ````

Both map the chunk body onto the same machinery as the file/line/block
[ignore directives](directives.md#ignore-directives), so the `[code]` selector
is enforced the same way. For finer control you can still use ordinary
`# raven: ignore` / `# raven: ignore-next` / `# raven: ignore-start` …
`# raven: ignore-end` markers inside a chunk body — they work exactly as in a
plain `.R` file.

## Keyboard shortcuts

| Mac | Windows/Linux | Action |
|-----|---------------|--------|
| `Cmd+Enter` | `Ctrl+Enter` | Run Line or Selection |
| `Cmd+Shift+Enter` | `Ctrl+Shift+Enter` | Source File (`.R`) / Knit (`.Rmd`, `.Rmarkdown`) |
| `Cmd+Alt+Enter` | `Ctrl+Alt+Enter` | Run Current Chunk |
| `Cmd+Alt+Shift+Enter` | `Ctrl+Alt+Shift+Enter` | Run Current Chunk and Move |
| `Cmd+Alt+P` | `Ctrl+Alt+P` | Run Previous Chunk |
| `Cmd+Alt+N` | `Ctrl+Alt+N` | Run Next Chunk |
| `Cmd+Alt+Shift+P` | `Ctrl+Alt+Shift+P` | Run Above Chunks |
| `Cmd+Alt+Shift+N` | `Ctrl+Alt+Shift+N` | Run Below Chunks |
| `Cmd+PageDown` | `Ctrl+PageDown` | Go to Next Chunk |
| `Cmd+PageUp` | `Ctrl+PageUp` | Go to Previous Chunk |

The letter-key shortcuts (`P`/`N`) are execution commands aligned with the [Quarto VS Code extension](https://quarto.org/docs/tools/vscode.html): unshifted runs a single neighbor chunk, Shift escalates to all chunks above/below. Navigation uses `PageDown`/`PageUp`. See [Keybinding Comparison](./keybinding-comparison.md) for a cross-tool reference.

In `.R` files without `# %%` cell markers the current-chunk shortcuts surface an information notification; use [cell mode](#plain-r-cell-mode) to make them useful in plain `.R` files.

## Commands

| Command | Description |
|---------|-------------|
| **Run Current Chunk** | Sends the chunk at the cursor to R. |
| **Run Current Chunk and Move** | Runs the current chunk, then moves the cursor into the next R chunk. |
| **Run Above Chunks** | Runs every R chunk that ends before the cursor. The current chunk is not included. |
| **Run Below Chunks** | Runs every R chunk whose header is strictly below the cursor. The current chunk is not included. |
| **Run Current and Below Chunks** | Runs the chunk at the cursor plus every R chunk after it. |
| **Run Previous Chunk** | Runs the R chunk immediately above the cursor (skipping non-R chunks). The cursor does not move. |
| **Run Previous Chunk and Move** | Runs the previous R chunk and moves the cursor into its first body line (or the header line if the chunk is empty). Quarto's "Run Previous Chunk" behavior. |
| **Run Next Chunk** | Runs the R chunk immediately below the cursor (skipping non-R chunks). The cursor does not move. |
| **Run Next Chunk and Move** | Runs the next R chunk and moves the cursor into its first body line (or the header line if the chunk is empty). Quarto's "Run Next Chunk" behavior. |
| **Run All Chunks** | Runs every R chunk in the document, top to bottom. |
| **Go to Next Chunk** | Moves the cursor to the body of the next chunk (or its header line if the chunk is empty). Navigation visits every chunk, not just R chunks. |
| **Go to Previous Chunk** | Moves the cursor to the body of the previous chunk (or its header line if the chunk is empty). Navigation visits every chunk, not just R chunks. |
| **Select Current Chunk** | Selects the body of the chunk at the cursor (excludes header and closing fence). |

## CodeLens buttons

By default each R chunk header shows up to three buttons:

```text
▷ Run Chunk    ↘ Run Next Chunk    ↥ Run Above
```{r}
…
```

Sibling-targeted lenses are hidden on chunks where they have nothing to point at: the first runnable chunk drops `↥ Run Above` and `← Run Previous` (and `↖ Run Previous Chunk`); the last runnable chunk drops `↧ Run Below`, `→ Run Next`, and `↘ Run Next Chunk`. Buttons for non-R languages are intentionally omitted.

The set of buttons (and their order) is controlled by `raven.chunks.codeLens.commands` — an array of run-command ids. The available ids are:

| Command id | Default label |
|------------|---------------|
| `raven.runCurrentChunk` | `▷ Run Chunk` |
| `raven.runCurrentChunkAndMove` | `▷⇣ Run & Move` |
| `raven.runAboveChunks` | `↥ Run Above` |
| `raven.runBelowChunks` | `↧ Run Below` |
| `raven.runCurrentAndBelowChunks` | `▷↓ Run Current and Below` |
| `raven.runPreviousChunk` | `← Run Previous` |
| `raven.runPreviousChunkAndMove` | `↖ Run Previous Chunk` |
| `raven.runNextChunk` | `→ Run Next` |
| `raven.runNextChunkAndMove` | `↘ Run Next Chunk` |
| `raven.runAllChunks` | `↻ Run All` |

Example — show four buttons in a custom order:

```jsonc
{
  "raven.chunks.codeLens.commands": [
    "raven.runCurrentChunk",
    "raven.runCurrentAndBelowChunks",
    "raven.runAboveChunks",
    "raven.runAllChunks"
  ]
}
```

Set the array to `[]` to hide all lenses while keeping the commands available from the palette and keybindings. Unknown command ids are silently ignored.

When a chunk's header sets `eval = FALSE`, the `▷ Run Chunk` and `▷⇣ Run & Move` labels gain a `(eval = FALSE)` suffix so you know the chunk would otherwise be skipped by `knitr` / `quarto render`.

If you also have the Quarto extension installed, `.qmd` files will show two CodeLens rows — Raven's (when Raven's R console is active) and Quarto's. See [Coexistence with the Quarto extension](./coexistence.md#coexistence-with-the-quarto-extension) for what each row does and how to choose between them.

## Chunk options

Raven parses the header inside `{…}` and recognizes:

- The **label** — the first bare identifier (e.g. `setup` in `{r setup}`).
- **Key-value options** — comma-separated `key=value` pairs.

Two options affect Raven specifically:

- `eval = FALSE` (or `eval = F`) — Raven dims the chunk's background tint to signal that it will not be evaluated by `knitr` or `quarto render`. The CodeLens still offers to run the chunk manually if you want.
- `raven.ignore` — suppresses Raven's diagnostics for the chunk body: `raven.ignore=TRUE` silences everything, `raven.ignore="undefined-variable, line-length"` narrows it to the listed codes (`raven.ignore=FALSE` is the default no-op). See [Suppressing diagnostics in a chunk](#suppressing-diagnostics-in-a-chunk).

Every other option is preserved on the parsed chunk but Raven does not interpret it.

## Highlighting

Each R chunk gets a faint background tint via two themable colors:

| Color id | Used for |
|----------|----------|
| `raven.chunk.activeBackground` | Default runnable R chunks. |
| `raven.chunk.inactiveBackground` | Chunks with `eval = FALSE`. Lower opacity by default. |

Customize them in `settings.json`:

```jsonc
{
  "workbench.colorCustomizations": {
    "raven.chunk.activeBackground": "#1f7fff10",
    "raven.chunk.inactiveBackground": "#1f7fff05"
  }
}
```

Set `raven.chunks.highlight.enabled` to `false` to turn the background off entirely.

## Plain `.R` cell mode

A line matching `# %%`, `## %%`, `### %%`, … starts a new cell. The cell extends until **whichever comes first**:

1. The next `# %%` marker.
2. An RStudio-style section divider (a comment line ending in 4+ `-`, `#`, `+`, `=`, or `*` characters — for example `# Title ====`, `# Setup ----`, `# Section #####`). Roxygen doc-comment lines (`#'`) are excluded, so a line like `#' @param x A value -----` does not end a cell.
3. End of file.

This matches VS Code's native interactive-cell convention used by the Jupyter extension and brings parity with vscode-R's section dividers.

```r
# %% Load
library(dplyr)

# %% Transform
mtcars |>
    group_by(cyl) |>
    summarise(mean(mpg))
```

Run Current Chunk on any line inside a cell sends that cell to the R console.

### Section dividers as cell boundaries

When you mix `# %%` cells with RStudio section dividers, the divider terminates the surrounding cell. The divider line itself stays in the prior cell; any code between the divider and the next `# %%` is **not** part of a cell and won't be sent by `Run Current Chunk`.

```r
# %% load
library(dplyr)

# Setup ----
helper <- function() 1
# %% transform
mtcars |> mutate(x = helper())
```

In the example above the `load` cell ends at `# Setup ----`. The `helper <- function() 1` line is orphan — it belongs to neither cell. A line that matches both forms (for example `# %% ====`) is treated as a cell marker, so you can still use section-style decoration on a cell header without losing the cell-start meaning.

### Active-cell border

The cell containing the cursor gets a top and bottom border so you can see at a glance which cell `Run Current Chunk` will run. Turn it off with `raven.chunks.activeCellIndicator: false`. The colors are themable via `raven.chunk.activeCellBorderTop` and `raven.chunk.activeCellBorderBottom`.

## Limitations

- `Raven: Knit Preview` (in this extension) runs `knitr::knit` plus Raven's HTML render pipeline for `.Rmd` / `.Rmarkdown` files. Pandoc export (HTML/PDF/Word) is invoked separately by the `Raven: Knit: Export to …` commands. See [docs/knit.md](knit.md). For `.qmd` files Raven defers to `quarto.quarto`'s `Quarto: Render` / `Quarto: Preview`.
- Nested chunks (the inner chunk inside an outer chunk's body) are not supported — the inner chunk header is treated as ordinary content of the outer chunk.
- `eval = !my_condition` and other dynamic option expressions are read literally; Raven does not evaluate R to determine `eval`.
