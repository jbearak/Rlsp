# JAGS 4.3.2 black-box syntax findings

These findings are observations from the independently authored matrix in
`syntax-matrix.json`. JAGS source, parser files, and manual prose were not
inspected or copied.

## Phase boundary

The public `model in` command is a usable syntax oracle. Unknown function and
distribution names pass `model in`, then fail after `compile`; malformed
operators and delimiters fail during `model in`. This permits syntax tests to
remain silent for semantic failures and extension-module names.

## Observed accepted forms

- A `var` declaration may precede `data` and `model`; the tested `data` block
  precedes `model`, and a non-empty `model` block is required.
- Deterministic relations use either `<-` or `=`. Stochastic relations use `~`.
- A call may appear on the left of a deterministic relation for link syntax.
- `T(lower, upper)` and `I(lower, upper)` follow stochastic relations; either
  tested boundary may be omitted.
- `for (name in range) { ... }` uses a mandatory braced body in the tested
  syntax.
- Calls with at least one argument, nested calls, arithmetic/comparison
  operators, `&&`, `||`, `**`, `%%`, `%/%`, dotted ASCII names, underscores
  after the first character, and omitted array indices parse.
- Semicolons are optional between newline-separated relations.
- `#` line comments and `/* ... */` block comments parse. CRLF and non-ASCII
  comment text parse.

## Observed rejected forms

- An empty model, multiple model blocks, a relation outside a program block,
  and an unbraced `for` body.
- `//` comments and a leading UTF-8 BOM.
- Zero-argument calls, omitted call arguments, a trailing call comma, unary
  `!`, single `&` or `|`, and bare `%`.
- Leading-dot, leading-underscore, backtick-quoted, and non-ASCII identifiers
  in the tested forms.
- Tested R-only forms: function definitions, `if`, `while`, strings, named
  arguments, `$`, `::`, right assignment, and the native pipe.
- Tested Stan-only forms: typed declarations and target accumulation.
- Missing operands, delimiters, relation operators, and `in` in a loop header.

The matrix is the authoritative record; this summary intentionally makes no
claim about unprobed syntax or other BUGS-family languages.
