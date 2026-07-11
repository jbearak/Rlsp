# Smart Indentation

Raven provides AST-aware indentation for R code through a two-tier system.

## Overview

Both tiers are active by default.

| Tier | What It Does | How It Works |
|------|-------------|--------------|
| **Tier 1** | Basic indentation for pipes, operators, brackets | Declarative regex rules in VS Code's language configuration |
| **Tier 2** | AST-aware indentation with style-specific alignment | LSP `onTypeFormatting` via tree-sitter |

When you press Enter, Tier 1 applies first (regex-based), then Tier 2 replaces the result with a more precise indentation computed from the AST. If Tier 2 is disabled, Tier 1's result stands.

## How It Works

1. You press Enter in an R file
2. VS Code applies Tier 1 rules (basic regex indentation)
3. Raven's LSP analyzes the tree-sitter AST at your cursor position
4. The LSP returns a TextEdit that replaces the indentation with the configured amount for the surrounding construct
5. Your cursor lands at the column computed for the configured indentation style

## Configuration

### Indentation Style

```json
{
  "raven.indentation.style": "rstudio"
}
```

| Value | Description |
|-------|-------------|
| `"rstudio"` | (Default) Same-line arguments align to the column after the opening paren; next-line arguments indent from function line |
| `"rstudio-minus"` | All arguments indent from the function call's line (the opener line), regardless of paren position |
| `"off"` | Disables Tier 2; only Tier 1 declarative rules remain active |

Style names follow the [ESS (Emacs Speaks Statistics)](https://ess.r-project.org/) conventions: `rstudio` matches the RStudio IDE's default alignment; `rstudio-minus` (`RStudio-` in ESS) drops same-line paren alignment.

The style choice governs **paren-argument alignment only**. Operator-chain continuation alignment (see [Pipe Chains](#pipe-chains) below) is not part of the style: it applies under both `rstudio` and `rstudio-minus` whenever Tier 2 is active, and it deliberately goes beyond what the real RStudio IDE does (RStudio indents statement-level operator continuations by one level; it never aligns them under the chain start).

### Disabling Tier 2

Two ways to disable AST-aware indentation:

1. Set `raven.indentation.style` to `"off"` — the LSP returns no edits, Tier 1 still works
2. Set `editor.formatOnType` to `false` for R — VS Code won't send `onTypeFormatting` requests at all

The difference: `"off"` is a Raven setting that keeps `formatOnType` available for other languages. Disabling `formatOnType` is a VS Code editor setting that affects all languages (unless overridden per-language).

Raven sets `editor.formatOnType` to `true` for R, R Markdown, and Quarto files as a default. This is the lowest-priority setting in VS Code — if you explicitly set `editor.formatOnType` to `false` (globally or for `[r]`), your setting takes precedence. Tier 2 indentation also applies inside R code chunks of R Markdown and Quarto files — pressing Enter inside a chunk body indents exactly as it would in a plain `.R` file. On prose, YAML front matter, or a non-R chunk, Tier 2 stands down so markdown isn't reflowed with R rules (Tier 1 still applies).

## Styles

### RStudio Style (Default)

When the opening parenthesis is followed by content on the same line, continuation arguments align to the column after the paren:

```r
result <- function_call(first_arg,
                        second_arg,
                        third_arg)
```

When the opening parenthesis is followed by a newline, arguments indent by one level from the function line:

```r
result <- function_call(
  first_arg,
  second_arg,
  third_arg
)
```

### RStudio-Minus Style

All arguments indent from the function call's line (the opener line), regardless of where the opening paren is:

```r
result <- function_call(first_arg,
  second_arg,
  third_arg)
```

## Examples

### Assignment Continuations

A line ending in an assignment operator (`<-`, `<<-`, `=`, `:=`, `->`, `->>`) indents the next line one level — in every style, because an assignment's right-hand side is not a peer operand of the left-hand side, so there is nothing to align against:

```r
result <-
  compute_something(x)
```

The level is measured from the line where the assignment *statement* starts. For a right assignment at the end of a chain, that is the chain's first line:

```r
data %>%
  f() ->
  target
```

Breaking after the operator is the escape hatch for a long left-hand side: the chain that follows gets one level, and subsequent continuations stay at that same level — the assignment already paid for the level, so the chain adds none (mirroring lintr's `assignment_as_infix`, which the linter's [indentation rule](linting.md) applies identically):

```r
result <-
  data %>%
  filter(x > 0) %>%
  select(y)
```

The same flattening applies to chained broken assignments (`a <-` ⏎ `b <-` ⏎ puts the final right-hand side at one level, not two). It does **not** apply when the chain hangs in a bracket opened on its own line — in `a <-` ⏎ `(data %>%` ⏎ (or a chain inside `if (…)` condition parens) the bracket's hanging level survives, and continuations align under the chain start as usual.

An assignment operator ending a line *inside* call arguments — including a named argument's or formal default's `=` — keeps the argument alignment of the enclosing call instead ([see above](#styles)); `:=` inside `dt[...]` likewise keeps bracket-content alignment.

Tier 1 has a matching declarative rule for `<-` / `<<-` only, so those two indent even with Tier 2 off; `=`, `:=`, `->`, `->>` need the AST to classify correctly and are Tier 2-only. Like every Tier 1 rule, the regex sees a single line: it cannot recognize an operator inside an unterminated multiline string, and on its own it cannot flatten. Tier 2 corrects both while the buffer still parses; on unparseable buffers its regex fallback shares the single-line limitation.

### Pipe Chains

Continuation lines in a pipe chain align under the chain start — for a chain on the right-hand side of an assignment, that is the first operand after the assignment operator:

```r
result <- data %>%
          filter(x > 0) %>%
          mutate(y = x * 2) %>%
          select(y)
```

A continuation always gets at least one indent level from the chain-start line, so a chain whose first operand sits at the line's first column indents instead of aligning:

```r
data |>
  filter(x > 0) |>
  select(y)
```

Exception: when the chain is the right-hand side of an assignment whose operator ends its line, continuations stay at the chain-start line's indent — see [Assignment Continuations](#assignment-continuations).

### Nested Pipes in Function Calls

```r
output <- some_function(
  data %>%
    filter(x > 0) %>%
    select(y),
  other_arg
)
```

### Function Calls in Pipe Chains

```r
result <- data %>%
          mutate(new_col = complex_function(arg1,
                                            arg2,
                                            arg3)) %>%
          filter(new_col > 0)
```

### Brace Blocks

```r
if (condition) {
  do_something()
  do_something_else()
}
```

## Troubleshooting

### Indentation not working at all

1. Check the file's language mode (status bar): R, R Markdown, and Quarto are all supported — but in `.Rmd` / `.Rmarkdown` / `.qmd` files, Tier 2 indentation applies only inside R chunk bodies, not in prose or YAML
2. Verify Raven is running (check VS Code's status bar)
3. Reload VS Code: `Ctrl+Shift+P` → "Developer: Reload Window"

### Wrong indentation style

1. Check `raven.indentation.style` — valid values are `"rstudio"`, `"rstudio-minus"`, `"off"`
2. The change takes effect immediately — open a new line in an R file to test

### Indentation looks doubled

Tier 2 replaces Tier 1's indentation, so doubling shouldn't happen. If it does, check for conflicting R extensions that also handle `onTypeFormatting`.

### Pipe chains not aligning correctly

Tier 2 detects the chain start using the tree-sitter AST (falling back to walking backward through operator-terminated lines when the AST has errors). Check that:

1. There's no blank line breaking the chain
2. Each line ends with a continuation operator (`%>%`, `|>`, `+`, `~`, or `%infix%`)
