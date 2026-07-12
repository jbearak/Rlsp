# Smart Indentation

Raven combines the editor's built-in indentation with AST-aware indentation
for R code.

## Overview

Both indentation mechanisms are active by default.

| Mechanism | What it does | How it works |
|---|---|---|
| **Built-in indentation** | Basic indentation for pipes, operators, assignments, and brackets | Declarative VS Code rules |
| **AST-aware indentation** | Configurable argument and infix indentation informed by the syntax tree | LSP `onTypeFormatting` using the indentation lint's expectation engine |

When you press Enter, built-in indentation runs first. AST-aware indentation then repairs the incomplete buffer, asks the shared expectation engine for both supported forms, and selects the configured producer style. If AST-aware indentation is disabled, the relevant axis is `off`, or the judge cannot answer, Raven emits no indentation edit and preserves the built-in result.

The formatter and lint have independent policies: the formatter emits its configured style faithfully, and the lint checks its configured style. Compatible infix pairs (`aligned` with `aligned`, `indented` with `indented`, or either producer with lint `either`) do not conflict. A mismatched pair is a valid user configuration state. When the lint flags a non-assignment infix continuation at exactly the column the enabled auto-indenter produces, the diagnostic names both settings and suggests matching the lint to the producer (or using lint `either`), or changing the producer to the lint style. Advice is omitted for genuinely mis-indented or syntactically malformed code, when no producer policy is available, and in documents where `editor.insertSpaces` or `editor.formatOnType` disables AST-aware indentation (synced per document from VS Code) — no column can have come from it there.

The indentation unit is deliberately shared when Raven's indentation lint is enabled: the judge uses the lint's per-document resolved unit, including `"auto"` and `[[linting.overrides]]`. If that lint rule is disabled, the judge uses the editor's `tabSize`. The lint's style setting never steers the producer.

## Configuration

```json
{
  "raven.indentation.enabled": true,
  "raven.indentation.argumentStyle": "aligned",
  "raven.indentation.infixContinuationStyle": "aligned",
  "raven.linting.infixContinuationStyle": "either"
}
```

| Setting | Values | Default | Effect |
|---|---|---|---|
| `raven.indentation.enabled` | `true`, `false` | `true` | AST-aware indentation master switch |
| `raven.indentation.argumentStyle` | `aligned`, `indented`, `off` | `aligned` | Parenthesized argument axis |
| `raven.indentation.infixContinuationStyle` | `aligned`, `indented`, `off` | `aligned` | Infix-operator continuation axis |

For the argument axis, `aligned` is ESS `RStudio`: a same-line argument aligns after the opener. `indented` is ESS `RStudio-`: arguments use one level from the opener line. If the opener is followed immediately by a newline, both forms coincide.

Aligned arguments:

```r
result <- function_call(first_arg,
                        second_arg,
                        third_arg)
```

Indented arguments:

```r
result <- function_call(first_arg,
  second_arg,
  third_arg)
```

A newline immediately after the opener uses column 2 in either mode with a 2-space unit:

```r
result <- function_call(
  first_arg,
  second_arg
)
```

For infix chains:

- `aligned` lines operands up at the first operand's actual column, floored one level from the owning statement: `max(first_operand_column, statement_indent + unit)`.
- `indented` uses a uniform one-level continuation anchored at the chain-start line. It does not reproduce the per-line cascade produced by built-in indentation.
- `off` makes AST-aware indentation stand down only when that axis decides the probe. Brace bodies, assignment continuations, and other style-neutral answers remain active.

Out of the box Raven emits aligned chains and the lint accepts both traditions. A project that runs real `lintr`, styler, or Air in CI should set both infix settings to `indented`. A strict alignment project sets both to `aligned`.

### Permanent compatibility alias

`raven.indentation.style` remains accepted permanently:

| Alias value | Applicable field |
|---|---|
| `rstudio` | `argumentStyle = aligned` |
| `rstudio-minus` | `argumentStyle = indented` |
| `off` | `enabled = false` |

The alias never changes the infix axis. Resolution is per field:

| Resolved field | Highest precedence | Then | Default |
|---|---|---|---|
| `enabled` | explicit `indentation.enabled` | alias `style = off` | `true` |
| `argumentStyle` | explicit `indentation.argumentStyle` | alias `rstudio` / `rstudio-minus` | `aligned` |
| `infixContinuationStyle` | explicit `indentation.infixContinuationStyle` | — | `aligned` |

Thus `enabled = true` overrides alias `off`, and `argumentStyle = aligned` overrides alias `rstudio-minus` without changing either infix setting.

### Disabling AST-aware indentation

Set `raven.indentation.enabled` to `false` to disable all AST-aware indentation edits while retaining built-in indentation. Setting both axes to `off` is different: style-neutral AST-aware indentation remains enabled. Setting `[r]`-scoped `editor.formatOnType` to `false` prevents the request entirely.

The master switch preserves the old alias-`off` trigger behavior: Raven also skips its closing-delimiter duplicate cleanup. Axis-level `off` does not disable that cleanup.

## Infix examples

All numeric columns below use the real judge with a 2-space unit and are pinned by `documented_columns_match_the_real_judge_engine`. The 2-space values are illustrative, not built in: every column scales with the resolved indentation unit (`raven.linting.indentationUnit`, default `"auto"` = the file's `editor.tabSize`, with `[[linting.overrides]]` honored) — with a 4-space unit, one level is column 4.

For an assigned chain with a 2-space unit, `aligned` uses column 10 under `data`; `indented` uses column 2 (one unit):

```r
# aligned
result <- data %>%
          filter(x > 0) %>%
          select(y)

# indented
result <- data %>%
  filter(x > 0) %>%
  select(y)
```

At statement level the aligned floor bites. With a 2-space unit, a top-level chain continues at column 2 (one unit), never column 0. Inside a brace, a statement at column 2 continues at column 4 (one unit deeper):

```r
data |>
  transform()

if (condition) {
  data |>
    transform()
}
```

A chain begun in a next-line call argument also continues one unit past the argument line (column 4 vs. column 2 with a 2-space unit) in either producer mode:

```r
output <- some_function(
  data %>%
    filter(x > 0) %>%
    select(y)
)
```

With the infix axis `off`, AST-aware indentation does not adjust the chain. The built-in line-local operator rule typically cascades instead of finding a uniform chain anchor:

```r
x |>
  y |>
    z
```

Likewise, `argumentStyle = off` leaves argument indentation to the editor's built-in bracket handling rather than applying Raven's opener alignment.

## Assignment continuations

Assignment operators (`<-`, `<<-`, `=`, `:=`, `->`, `->>`) are style-neutral. Their continuation is one level in under every argument, infix, and lint mode; an assignment RHS is not a peer operand of its LHS.

At top level the RHS begins one unit in (column 2 with a 2-space unit):

```r
result <-
  compute_something(x)
```

Breaking before a chain pays that level once. The chain remains at that one-unit column (2 here) in every mode:

```r
result <-
  data %>%
  filter(x > 0) %>%
  select(y)
```

For a syntactically continuous arithmetic chain, all RHS operands remain mutually aligned at column 4 with a 4-space unit; the LHS column is irrelevant:

```r
x <-
    y +
    z +
    w
```

An earlier opener contributes its level before the assignment. With a 2-space unit, `b <-` sits at column 2 (the call's unit) and `value` at column 4 (plus the assignment's):

```r
f(
  b <-
    value
)
```

A same-line opener is governed by the argument axis. In this incomplete assignment shape, `aligned` selects column 21 after the opener; `indented` selects column 4 (two 2-space units — the call's plus the assignment's):

```r
long_function_name(x <-
                     value
```

The analogous `:=` shape selects column 5 (after the opener) in aligned mode and column 4 (two 2-space units) in indented mode:

```r
dt[, y :=
     value
```

When a broken assignment is followed by a same-line call that starts a chain, the producer axes legitimately differ: with a 2-space unit, aligned mode selects column 4, while indented mode selects column 6.

```r
# aligned
result <-
  f(data %>%
    value

# indented
result <-
  f(data %>%
      value
```

Built-in indentation has a matching assignment rule for `<-` and `<<-`. The other assignment operators need AST-aware indentation because a line regex cannot classify them safely.

## Brace blocks

Brace bodies are style-neutral and remain active even when both axes are `off`. With a 2-space unit the body begins at column 2:

```r
if (condition) {
  do_something()
}
```

## When AST-aware indentation cannot answer

Raven emits no edit for multiline string/backtick interiors, tabs-mode or a real tab in the active context, an unrepaired syntax-error window, a nonconforming reference line, or an ambiguous/out-of-bounds repair. That preserves the editor's built-in indentation. The tabs-mode stand-down also silences the lint's settings-mismatch advice for that document: the advice attributes a column to the AST-aware auto-indenter, which emits nothing there.

AST-aware indentation applies inside R chunk bodies in R Markdown and Quarto, but stands down in prose, YAML, and non-R chunks.

## Troubleshooting

If indentation is inactive, check the language mode, `raven.indentation.enabled`, the relevant axis, and `editor.formatOnType`. Setting changes take effect immediately.

If a chain uses the wrong tradition, compare `raven.indentation.infixContinuationStyle` (producer) with `raven.linting.infixContinuationStyle` (checker). Use matching values for strict projects or lint `either` when both layouts are welcome.
