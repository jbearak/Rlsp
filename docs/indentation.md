# Smart Indentation

Raven provides AST-aware indentation for R code through a two-tier system.

## Overview

Both tiers are active by default.

| Tier | What It Does | How It Works |
|------|-------------|--------------|
| **Tier 1** | Basic indentation for pipes, operators, brackets | Declarative regex rules in VS Code's language configuration |
| **Tier 2** | AST-aware indentation with style-specific alignment | LSP `onTypeFormatting` via the indentation lint's expectation engine, with a legacy context fallback |

When you press Enter, Tier 1 applies first (regex-based), then Tier 2 replaces the result with a more precise indentation computed from the AST. If Tier 2 is disabled, Tier 1's result stands.

## How It Works

1. You press Enter in an R file
2. VS Code applies Tier 1 rules (basic regex indentation)
3. Raven's LSP asks the same expectation engine that powers Raven's indentation lint which columns are valid at the cursor
4. The LSP chooses a valid column according to the configured indentation style and returns a TextEdit that replaces the indentation
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
| `"rstudio"` | (Default) Prefer aligned forms for same-line arguments and operator chains; next-line arguments indent from the opener line |
| `"rstudio-minus"` | Prefer block-indented forms for arguments and operator chains, regardless of opener position |
| `"off"` | Disables Tier 2; only Tier 1 declarative rules remain active |

Style names follow the [ESS (Emacs Speaks Statistics)](https://ess.r-project.org/) conventions: `rstudio` matches the RStudio IDE's default alignment; `rstudio-minus` (`RStudio-` in ESS) drops same-line paren alignment.

The style controls which lint-accepted layout Tier 2 prefers; it does not change which layouts the indentation lint accepts. `rstudio` prefers paren-argument and operator-chain alignment, while `rstudio-minus` prefers the corresponding block indent. The lint's separate [`infixContinuationStyle`](linting.md#indentation) setting determines which operator-continuation layouts are accepted, and Tier 2 always chooses from that accepted set. Consequently, the indentation lint never flags a column that auto-indent just produced. (On inputs where the expectation engine cannot answer — an unterminated multiline string, tab-indented context or a tabs-mode editor, or surrounding code that doesn't sit at a lint-accepted column — Tier 2 falls back to its legacy context-based indenter, which anchors to physical indentation instead and approximates rather than guarantees lint acceptance.)

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

A line ending in an assignment operator (`<-`, `<<-`, `=`, `:=`, `->`, `->>`) normally indents the next line one level — in every style, because an assignment's right-hand side is not a peer operand of the left-hand side, so there is nothing to align against. With a 2-space indentation unit, the right-hand side starts at column 2:

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

Breaking after the operator is the escape hatch for a long left-hand side: the chain that follows starts at column 2, and subsequent continuations stay at column 2 — the assignment already paid for the level, so the chain adds none (mirroring lintr's `assignment_as_infix`, which the linter's [indentation rule](linting.md) applies identically):

```r
result <-
  data %>%
  filter(x > 0) %>%
  select(y)
```

The same flattening applies to chained broken assignments: `a <-` ⏎ `b <-` ⏎ puts both `b <-` and the final right-hand side at column 2, not columns 2 and 4.

An enclosing bracket opened on an earlier line does not override the assignment continuation. In each of these shapes, `b <-` starts at column 2 and its right-hand side starts one level deeper, at column 4:

```r
f(
  b <-
    value

(
  b <-
    value

x[
  b <-
    value

f(a,
  b <-
    value
```

An opener on the assignment's own line still supplies its hanging or aligned form. Under the default `rstudio` style, this right-hand side starts at column 19:

```r
long_function_name(x <-
                   value
```

Likewise, the default style puts this `:=` right-hand side at the lint-accepted aligned argument/chain column 5 (`rstudio-minus` chooses the block form at column 4):

```r
dt[, y :=
     value
```

When the assignment is followed by a call whose same-line argument starts a chain, the assignment and opener contributions are both preserved. The continuation below starts at column 6 in both styles:

```r
result <-
  f(data %>%
      value
```

Broken assignments still flatten later operator continuations unless a bracket opened on the chain's own line contributes a hanging level. Thus `a <-` ⏎ `(data %>%` ⏎ puts the next operand at column 4; a chain inside same-line `if (…)` condition parens similarly retains the condition's hanging level.

An assignment operator ending a line *inside* call arguments — including a named argument's or formal default's `=` — likewise keeps a lint-accepted argument layout for the enclosing call ([see above](#styles)).

Tier 1 has a matching declarative rule for `<-` / `<<-` only, so those two indent even with Tier 2 off; `=`, `:=`, `->`, `->>` need Tier 2 to classify correctly. Like every Tier 1 rule, the regex sees only one line and cannot flatten a chain. Tier 2 first asks the indentation lint's expectation engine, so every answer it emits is lint-accepted; when that engine cannot answer, Tier 2 falls back to Raven's legacy context-based indenter.

### Pipe Chains

Under the default `rstudio` indentation style and `indented` lint style, continuation lines in a pipe chain align under the chain start — for a chain on the right-hand side of an assignment, that is the first operand after the assignment operator:

```r
result <- data %>%
          filter(x > 0) %>%
          mutate(y = x * 2) %>%
          select(y)
```

With the default `indented` lint style, a continuation whose chain starts in the first column gets one indent level instead of aligning to column 0:

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

Tier 2 first asks the indentation lint's expectation engine and falls back to its legacy context detection when that path cannot answer. Both paths treat a blank line as a chain break, so check that:

1. There's no blank line breaking the chain
2. Each line ends with a continuation operator (`%>%`, `|>`, `+`, `~`, or `%infix%`)
