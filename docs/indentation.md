# Smart Indentation

Raven combines the editor's built-in indentation with syntax-aware indentation
for R code. The settings UI calls the second mechanism "AST-aware"
indentation.

## Overview

Both indentation mechanisms are active by default.

| Mechanism | What it does | How it works |
|---|---|---|
| **Built-in indentation** | Makes an immediate first pass for pipes, operators, assignments, and brackets | Declarative VS Code line rules |
| **Syntax-aware indentation** | Refines argument and infix indentation using the surrounding R structure | LSP `onTypeFormatting` using syntax analysis shared with the indentation lint |

> [!NOTE]
> **Why two indentation mechanisms?** Indentation is requested immediately
> after you press Enter, when the R expression is often temporarily incomplete.
> The editor's built-in rules make a quick, line-based first pass. Raven then
> repairs the temporary buffer enough to understand the surrounding call,
> assignment, block, or operator chain, and replaces the first-pass indent only
> when it can determine a safe column. The built-in rules are therefore the
> baseline and fallback; syntax awareness adds context when the structure is
> clear.

## Configuration

These are the default settings:

```json
{
  "raven.indentation.enabled": true,
  "raven.indentation.argumentStyle": "aligned",
  "raven.indentation.infixContinuationStyle": "aligned",
  "raven.linting.enabled": "auto",
  "raven.linting.indentationUnit": "auto",
  "raven.linting.infixContinuationStyle": "either",
  "raven.linting.indentationSeverity": "information"
}
```

### Auto-indentation settings

| Setting | Values | Default | Effect |
|---|---|---|---|
| `raven.indentation.enabled` | `true`, `false` | `true` | Syntax-aware indentation master switch |
| `raven.indentation.argumentStyle` | `aligned`, `indented`, `off` | `aligned` | How to indent parenthesized arguments |
| `raven.indentation.infixContinuationStyle` | `aligned`, `indented`, `off` | `aligned` | How to indent infix-operator continuations |

For parenthesized arguments, `aligned` follows RStudio's default vertical
alignment: a same-line argument aligns after the opener. ESS calls this style
`RStudio`. The `indented` value, called `RStudio-` in ESS, uses one level from
the opener line. If the opener is followed immediately by a newline, both forms
coincide.

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

- `aligned` lines operands up at the first operand's column, but never less than one level past the owning statement: `max(first_operand_column, statement_indent + unit)`.
- `indented` adds one continuation level beyond the enclosing indentation context, matching strict `lintr` behavior. It does not reproduce the per-line cascade produced by built-in indentation.
- `off` leaves infix continuations to the editor's built-in indentation. Raven still handles braces, assignment continuations, and other constructs this setting does not control.

### Corresponding lint settings

Auto-indentation writes leading whitespace as you type; the indentation lint
checks the whitespace already in a document. They use separate settings so a
project can choose what Raven writes without silently imposing that preference
on existing code.

| Setting | Values | Default | Effect |
|---|---|---|---|
| `raven.linting.enabled` | `"auto"`, `"on"`, `"off"`, `true`, `false` | `"auto"` | Master switch for Raven's style lints; `"auto"` is normally off until project configuration opts in |
| `raven.linting.indentationSeverity` | `error`, `warning`, `information`, `hint`, `off` | `information` | Severity for indentation diagnostics; `off` disables this rule |
| `raven.linting.indentationUnit` | `"auto"`, integer `1` through `8` | `"auto"` | Expected spaces per level; in VS Code, `"auto"` follows the file's `editor.tabSize` |
| `raven.linting.infixContinuationStyle` | `aligned`, `indented`, `either` | `either` | Whether infix continuations must use one strict style or may use either |

There is deliberately no lint-side argument-style setting: the lint accepts
both common argument layouts shown above. For infix chains, matching `aligned`
or `indented` settings ensure the lint accepts the layout auto-indent writes.
The default lint value, `either`, accepts both. A mismatched pair of strict
settings is allowed; when Raven can tell that auto-indent produced the flagged
column, the diagnostic suggests the settings that will resolve the conflict.

While Raven's indentation lint is active, syntax-aware indentation shares its
resolved `indentationUnit`, including `"auto"` and any
`[[linting.overrides]]`. Otherwise, auto-indent uses the editor's `tabSize`.
The lint's infix style never changes what auto-indent writes. In `.lintr`,
`indentation_linter(indent = N)` maps to `indentationUnit`, and disabling that
linter maps to `indentationSeverity = "off"`; `.lintr` cannot configure Raven's
infix style. See
[Linting § Indentation](linting.md#indentation) for the complete checking
rules and [Linting § Quick start](linting.md#quick-start) for how the opt-in
master switch works.

### Why Raven's defaults differ

Raven does not copy either RStudio or `lintr` wholesale. RStudio's default
vertical-alignment preference aligns arguments after a same-line opener, which
Raven's `argumentStyle = aligned` matches. Its operator indentation is
contextual: it can align continuations in some parenthesized control-flow
expressions, but statement-level operator chains use a fixed continuation
offset.
[`lintr`'s indentation rule](https://lintr.r-lib.org/reference/indentation_linter.html)
instead expects non-assignment infix continuations one additional level beyond
the enclosing indentation context.

Raven deliberately extends alignment to non-assignment infix chains generally.
Raven's authors find that keeping peer operands in one visual column makes
equations, conditions, and pipelines easier to scan:

```r
total <- first_term +
         second_term +
         third_term
```

The one-level minimum keeps a top-level continuation visibly indented even when
its first operand begins at column 0.

To keep Raven's auto-indent preference from becoming a default lint mandate,
Raven writes `aligned` infix chains by default while its lint defaults to
`either`, accepting both styles. A project that runs `lintr`, styler, or Air in
CI should set both `raven.indentation.infixContinuationStyle` and
`raven.linting.infixContinuationStyle` to `indented`. If the project runs
`lintr`, Raven should also resolve the same indentation width as `lintr`'s
configured `indent` value (default `2`). In VS Code, the simplest approach is
to leave `raven.linting.indentationUnit` as `"auto"` and set `editor.tabSize` to
that value; a fixed `indentationUnit` also controls syntax-aware indentation
while Raven's indentation lint is active. Raven deliberately retains some
extra argument-layout leniency, documented under
[Linting § Indentation](linting.md#indentation). A strict alignment project
sets both infix settings to `aligned`.

### Permanent compatibility alias

`raven.indentation.style` remains accepted permanently:

| Alias value | Applicable field |
|---|---|
| `rstudio` | `argumentStyle = aligned` |
| `rstudio-minus` | `argumentStyle = indented` |
| `off` | `enabled = false` |

The alias never changes the infix setting. Each field resolves independently:

| Resolved field | Highest precedence | Then | Default |
|---|---|---|---|
| `enabled` | explicit `indentation.enabled` | alias `style = off` | `true` |
| `argumentStyle` | explicit `indentation.argumentStyle` | alias `rstudio` / `rstudio-minus` | `aligned` |
| `infixContinuationStyle` | explicit `indentation.infixContinuationStyle` | — | `aligned` |

Thus `enabled = true` overrides alias `off`, and `argumentStyle = aligned` overrides alias `rstudio-minus` without changing either infix setting.

### Disabling syntax-aware indentation

Set `raven.indentation.enabled` to `false` to disable all syntax-aware indentation edits while retaining built-in indentation. Setting both style settings to `off` is different: Raven still handles braces, assignments, and other indentation those settings do not control. Setting `[r]`-scoped `editor.formatOnType` to `false` prevents the request entirely.

Turning the master switch off also disables Raven's cleanup of duplicated
closing delimiters after Enter. Setting an individual style to `off` does not
disable that cleanup.

## Infix examples

The examples below use a 2-space indentation unit. That value is illustrative,
not built in: every column scales with the resolved indentation unit. With a
4-space unit, for example, one level is column 4.

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

At statement level the one-level minimum matters. With a 2-space unit, a top-level chain continues at column 2 (one unit), never column 0. Inside a brace, a statement at column 2 continues at column 4 (one unit deeper):

```r
data |>
  transform()

if (condition) {
  data |>
    transform()
}
```

A chain begun in a next-line call argument also continues one unit past the argument line (column 4 vs. column 2 with a 2-space unit) with either auto-indent style:

```r
output <- some_function(
  data %>%
    filter(x > 0) %>%
    select(y)
)
```

With the infix style `off`, syntax-aware indentation does not adjust the chain. The built-in line-local operator rule typically cascades instead of finding a uniform chain anchor:

```r
x |>
  y |>
    z
```

Likewise, `argumentStyle = off` leaves argument indentation to the editor's built-in bracket handling rather than applying Raven's opener alignment.

## Assignment continuations

Assignment operators (`<-`, `<<-`, `=`, `:=`, `->`, `->>`) always continue one level in, regardless of the argument, infix, or lint style; an assignment RHS is not a peer operand of its LHS.

At top level the RHS begins one unit in (column 2 with a 2-space unit):

```r
result <-
  compute_something(x)
```

When the right-hand side starts on the next line, the chain remains at that one-unit column (2 here) in every mode:

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

A same-line opener follows the argument setting. In this incomplete assignment shape, `aligned` selects column 21 after the opener; `indented` selects column 4 (two 2-space units — the call's plus the assignment's):

```r
long_function_name(x <-
                     value
```

The analogous `:=` shape selects column 5 (after the opener) in aligned mode and column 4 (two 2-space units) in indented mode:

```r
dt[, y :=
     value
```

When a broken assignment is followed by a same-line call that starts a chain, the argument and infix choices can legitimately produce different columns: with a 2-space unit, aligned mode selects column 4, while indented mode selects column 6.

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

Built-in indentation has a matching assignment rule for `<-` and `<<-`. The other assignment operators need syntax-aware indentation because a line regex cannot classify them safely.

## Brace blocks

Brace body indentation does not depend on either style setting and remains active even when both are `off`. With a 2-space unit the body begins at column 2:

```r
if (condition) {
  do_something()
}
```

## When syntax-aware indentation cannot answer

Raven emits no edit inside multiline strings or backticks, when the editor uses
tabs or the active context contains a real tab, or when incomplete syntax and
surrounding indentation do not establish a safe column. That preserves the
editor's built-in indentation. In tabs mode, Raven also omits lint advice that
would otherwise attribute a conflicting column to syntax-aware auto-indent.

Syntax-aware indentation applies inside R chunk bodies in R Markdown and Quarto, but stands down in prose, YAML, and non-R chunks.

## Troubleshooting

If indentation is inactive, check the language mode, `raven.indentation.enabled`, the relevant style setting, and `editor.formatOnType`. Setting changes take effect immediately.

If a chain uses the wrong tradition, compare `raven.indentation.infixContinuationStyle` (what auto-indent writes) with `raven.linting.infixContinuationStyle` (what the lint accepts). Use matching values for strict projects or lint `either` when both layouts are welcome.
