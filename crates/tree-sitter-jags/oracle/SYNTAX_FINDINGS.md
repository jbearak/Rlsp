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

- Empty and comment-only input are accepted. If program blocks are present, a
  `var` declaration may precede `data` and `model`; the tested `data` block
  precedes `model`, and the `model` body is non-empty.
- Deterministic relations use either `<-` or `=`. Stochastic relations use `~`.
- A call may appear on the left of a deterministic relation for link syntax.
- `T(lower, upper)` and `I(lower, upper)` follow stochastic relations; either
  tested boundary may be omitted.
- `for (name in range) { ... }` uses a mandatory braced body in the tested
  syntax.
- Distribution calls may be empty. Other calls require at least one argument.
  Nested calls, arithmetic/comparison operators, `&&`, `||`, `**`, `%%`,
  `%/%`, arbitrary non-whitespace `%name%` infix operators, dotted ASCII names,
  underscores after the first character, and omitted array indices parse.
- Unparenthesized comparison and colon chains reject, while explicitly
  parenthesized nesting parses.
- `model`, `data`, and `var` are rejected as bare names but accepted as call
  names. `for` is accepted as a bare name but rejected as a call name; `in` is
  rejected in both positions outside its loop role.
- Semicolons are optional between newline-separated relations.
- `#` line comments and `/* ... */` block comments parse. CRLF and non-ASCII
  comment text parse. A final `#` comment must include its line terminator.

## Observed rejected forms

- An empty model block, multiple model blocks, a relation outside a program
  block, an unbraced `for` body, and a semicolon after a `for` body.
- `//` comments and a leading UTF-8 BOM.
- Zero-argument deterministic calls, omitted call arguments, a trailing call
  comma, unary `!`, single `&` or `|`, and bare `%`.
- Chained subsets, unparenthesized chained comparisons/colons, and link-call
  left-hand sides with multiple, numeric, expression, or nested-call arguments.
- Leading-dot, leading-underscore, backtick-quoted, and non-ASCII identifiers
  in the tested forms.
- Tested R-only forms: function definitions, `if`, `while`, strings, named
  arguments, `$`, `::`, right assignment, and the native pipe.
- Tested Stan-only forms: typed declarations and target accumulation.
- Missing operands, delimiters, relation operators, and `in` in a loop header.

The matrix is the authoritative record; this summary intentionally makes no
claim about unprobed syntax or other BUGS-family languages.
