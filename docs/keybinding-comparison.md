# Chunk Keybinding Comparison

Raven's chunk keybindings are aligned with the [Quarto VS Code extension](https://marketplace.visualstudio.com/items?itemName=quarto.quarto), which provides the most coherent separation of execution (letter keys) and navigation (PageDown/PageUp). This page documents how Raven's bindings compare to the other R tools.

## Design principles

1. **`Cmd/Ctrl+Alt+P` / `Cmd/Ctrl+Alt+N` = execution pair** — run the previous/next single chunk, matching Quarto and RStudio.
2. **Shift escalates** — adding Shift runs *all* chunks above/below rather than a single neighbor, matching Quarto.
3. **PageDown/PageUp = navigation** — cursor movement without execution, matching Quarto and RStudio's section navigation.
4. **Enter variants = current chunk** — run or run-and-move the chunk at the cursor.

## Raven keybindings

| Mac | Windows/Linux | Action |
|-----|---------------|--------|
| `Cmd+Enter` | `Ctrl+Enter` | Run Line or Selection |
| `Cmd+Shift+Enter` | `Ctrl+Shift+Enter` | Source File (`.R`) / Knit Preview (`.Rmd`) / Quarto Preview (`.qmd`) |
| `Cmd+Alt+Enter` | `Ctrl+Alt+Enter` | Run Current Chunk |
| `Cmd+Alt+Shift+Enter` | `Ctrl+Alt+Shift+Enter` | Run Current Chunk and Move |
| `Cmd+Alt+P` | `Ctrl+Alt+P` | Run Previous Chunk |
| `Cmd+Alt+N` | `Ctrl+Alt+N` | Run Next Chunk |
| `Cmd+Alt+Shift+P` | `Ctrl+Alt+Shift+P` | Run Above Chunks (all chunks above cursor) |
| `Cmd+Alt+Shift+N` | `Ctrl+Alt+Shift+N` | Run Below Chunks (all chunks below cursor) |
| `Cmd+PageDown` | `Ctrl+PageDown` | Go to Next Chunk |
| `Cmd+PageUp` | `Ctrl+PageUp` | Go to Previous Chunk |

## Cross-tool comparison

### Execution shortcuts

| Shortcut (Mac) | Raven | Quarto | RStudio | REditorSupport |
|----------------|-------|--------|---------|----------------|
| `Cmd+Enter` | Run Line or Selection | Run Current Code | Run current line/selection | Run Selection |
| `Cmd+Shift+Enter` | Source File (`.R`) / Knit Preview (`.Rmd`) / Quarto Preview (`.qmd`) | Run Current Cell | Source document (with echo) | Run Current Chunk (`.Rmd`) / Source with Echo (`.R`) |
| `Cmd+Alt+Enter` | Run Current Chunk | — | Send line to terminal | — |
| `Cmd+Alt+P` | Run Previous Chunk | Run Previous Cell | Re-run previous region | Run Above Chunks |
| `Cmd+Alt+N` | Run Next Chunk | Run Next Cell | Run next Sweave/Rmd chunk | *(unbound)* |
| `Cmd+Alt+Shift+P` | Run Above Chunks | Run Cells Above | Run previous Sweave/Rmd code | *(unbound)* |
| `Cmd+Alt+Shift+N` | Run Below Chunks | Run Cells Below | — | *(unbound)* |
| `Cmd+Alt+C` | — | — | Run current Sweave/Rmd chunk | — |
| `Cmd+Alt+R` | — | Run All Cells | Run current document | — |

### Navigation shortcuts

| Shortcut (Mac) | Raven | Quarto | RStudio | REditorSupport |
|----------------|-------|--------|---------|----------------|
| `Cmd+PageDown` | Go to Next Chunk | Go to Next Cell | Next section | *(unbound)* |
| `Cmd+PageUp` | Go to Previous Chunk | Go to Previous Cell | Previous section | *(unbound)* |

REditorSupport has `r.goToNextChunk` and `r.goToPreviousChunk` commands but ships no default keybinding for them.

### Key differences from each tool

**vs. Quarto** — Nearly identical. Raven uses `Cmd+Alt+Enter` for Run Current Chunk where Quarto uses `Cmd+Shift+Enter`; Raven adds `Cmd+Alt+Shift+Enter` for Run Current Chunk and Move. The P/N/Shift+P/Shift+N/PageDown/PageUp mappings match exactly.

**vs. RStudio** — RStudio uses `Cmd+Alt+P` for "re-run previous region" (which re-runs whatever you last ran, not necessarily a chunk) and `Cmd+Alt+N` for "run next chunk." The semantics are close but not identical. RStudio uses `Cmd+Alt+C` for run current chunk and `Cmd+PageDown`/`Cmd+PageUp` for section navigation (which includes chunks). RStudio has no "run below" shortcut.

**vs. REditorSupport** — REditorSupport only ships one chunk keybinding by default: `Cmd+Alt+P` = Run Above Chunks (all above, not single previous). All other chunk commands exist but are unbound. Users migrating from REditorSupport who relied on `Cmd+Alt+P` for "run all above" should use `Cmd+Alt+Shift+P` in Raven.

**vs. Positron** — Positron does not ship chunk-specific keybindings. Its RStudio keymap mode adds general RStudio shortcuts but not the chunk execution keys. Positron's built-in shortcuts focus on `Cmd+Enter` (run selection/statement) and `Cmd+Shift+Enter` (source file).

## Migration notes

If you're coming from REditorSupport and used `Cmd+Alt+P` to run all chunks above the cursor, that action is now at `Cmd+Alt+Shift+P`. The unshifted `Cmd+Alt+P` runs only the single previous chunk, matching Quarto and RStudio.

If you previously used `Cmd+Alt+N` in Raven for chunk navigation, that key now runs the next chunk. Use `Cmd+PageDown` / `Cmd+PageUp` for navigation instead.
