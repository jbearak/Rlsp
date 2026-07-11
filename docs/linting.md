# Linting

Raven ships an opt-in, native style linter that re-implements 18 of [`lintr`](https://lintr.r-lib.org/)'s rules — most of `lintr`'s default rule set — in Rust against the tree-sitter AST. No R session or `lintr` install is needed — rules run on the parse tree Raven already builds for completions and diagnostics.

This page is the landing point for users coming from `lintr` or `REditorSupport`. For these rules alongside Raven's other diagnostic categories, see [Diagnostics § Style Lints](diagnostics.md#style-lints); the per-key configuration reference lives in [Configuration § Linting Settings](configuration.md#linting-settings).

> [!NOTE]
> In Raven, "linting" means subjective style rules — line length, naming, infix spacing, and similar — and the whole group is governed by the tri-state master switch `raven.linting.enabled` (default `"auto"`, see below). Correctness diagnostics (parse errors, semantic warnings, cross-file issues, assignment-target errors) are on by default under `raven.diagnostics.enabled`; most categories have a per-category severity that can silence them (`"off"`), while a few (parse errors, assignment-target errors) respond only to the master switch. None of these are controlled by `raven.linting.*`. If you're looking for things like the orphan-`else` parse error, see [Diagnostics](diagnostics.md), not this page.

## Quick start

By default (`"auto"`), Raven turns linting on when it discovers a workspace or non-home ancestor `.lintr` **that contains linting configuration**, or a `raven.toml` opt-in, and stays off otherwise. A blank or empty `.lintr` (one with no `linters:` or `exclusions:` directive) carries no opt-in and does **not** turn linting on — unlike `lintr` itself, Raven's native linting is off by default and a `.lintr` enables it only when the file actually configures linting (a `linters:` directive, including a bare `linters_with_defaults()`). The literal home-directory `~/.lintr` is ignored unless `raven.linting.readHomeLintr` is true in the editor, or the CLI is pointed at it explicitly with `--config ~/.lintr`. A discovered `.lintr` is also *ignored* when [REditorSupport's own `lintr` diagnostics are live or you're running in Positron](#auto-and-reditorsupport--positron) — there the `.lintr` belongs to `lintr` itself, so Raven doesn't pile its native lints on top. To force linting on regardless of project state, set:

```json
{
  "raven.linting.enabled": true
}
```

All style lint rules default to severity `information`, matching REditorSupport `languageserver`'s mapping for `lintr` style findings. To raise a rule (e.g. line length) to `warning`, lower it to `hint`, or disable an individual rule, set its severity:

```json
{
  "raven.linting.enabled": true,
  "raven.linting.lineLengthSeverity": "warning",
  "raven.linting.commentedCodeSeverity": "off"
}
```

To change the line-length threshold or pick a different naming scheme:

```json
{
  "raven.linting.enabled": true,
  "raven.linting.lineLength": 120,
  "raven.linting.objectNameStyleFunction": ["snake_case", "camelCase"],
  "raven.linting.objectNameStyleVariable": "snake_case",
  "raven.linting.objectNameStyleArgument": [],
  "raven.linting.objectNameRegexesArgument": ["^\\.?(x|y)$"]
}
```

Each `objectNameStyle*` accepts either a single style string or an array of styles. A name passes when it matches any named style for that kind or any regex in the matching `objectNameRegexes*` setting. Setting an `objectNameStyle*` to `"any"` (or including `"any"` in the array) disables the check for that symbol kind and ignores regexes for that kind while leaving the other two active. An explicit empty style array with regexes is regex-only mode; empty styles and empty regexes together disable that kind. Setting `raven.linting.objectNameSeverity` to `"off"` disables the rule entirely.

Lint diagnostics carry the `source` field `raven (lint)`, so they're easy to filter from Raven's other diagnostics in the Problems pane.

## Master switch (`raven.linting.enabled`)

`raven.linting.enabled` is tri-state: `"auto"` (the default), `true` (or `"on"`), or `false` (or `"off"`). Booleans are accepted for backward compatibility with existing settings.

- `"auto"` — lint when a project config opts in. Specifically: when a `.lintr` **that contains linting configuration** (a `linters:` or `exclusions:` directive — a bare `linters_with_defaults()` counts; a blank/empty file does not) is discovered on the upward walk from the active project root, or when a `raven.toml` sets `[linting] enabled = true`. The active project root is the first editor workspace folder, `raven check --workspace`, or the `raven lint` working directory. The literal home-directory `~/.lintr` is ignored by default; in VS Code and other LSP clients, set `raven.linting.readHomeLintr = true` to include it, while the CLI uses it only when passed explicitly with `--config ~/.lintr`. Otherwise off. The `.lintr` half of this is suppressed when REditorSupport / Positron already owns the `lintr` path — see [`"auto"` and REditorSupport / Positron](#auto-and-reditorsupport--positron) below.
- `true` / `"on"` — force linting on. Discovered rule severities still apply.
- `false` / `"off"` — disable linting unless a discovered `raven.toml` explicitly sets `enabled = true` (raven.toml always wins at the leaf — the project-policy contract). A discovered `.lintr` alone never re-enables linting.

### Behavior matrix

Resolution by client setting × project state:

<!-- markdownlint-disable MD013 -->

| Client (`raven.linting.enabled`) | Project state | Result |
|---|---|---|
| `"auto"` (default) | no `.lintr`, no `raven.toml` | off |
| `"auto"` | blank/empty `.lintr` (no `linters:` / `exclusions:` directive) | off — an empty `.lintr` carries no opt-in |
| `"auto"` | configured `.lintr` discovered (workspace or non-home ancestor) | on — **unless** REditorSupport's `lintr` path is live or you're in Positron, then off ([details](#auto-and-reditorsupport--positron)) |
| `"auto"` | literal `~/.lintr` exists, `raven.linting.readHomeLintr = false` (default) | off |
| `"auto"` | literal `~/.lintr` exists, `raven.linting.readHomeLintr = true` | on — **unless** REditorSupport's `lintr` path is live or you're in Positron, then off ([details](#auto-and-reditorsupport--positron)) |
| `"auto"` | `raven.toml` with `enabled = true` (or `"on"`) | on |
| `"auto"` | `raven.toml` with `enabled = false` (or `"off"`) | off — `.lintr` not consulted (raven.toml wins discovery) |
| `"auto"` | `raven.toml` with `enabled = "auto"` or no `[linting]` | off (no `.lintr` discovered; raven.toml was discovered instead) |
| `false` / `"off"` | no project config | off |
| `false` / `"off"` | `.lintr` discovered | off |
| `false` / `"off"` | `raven.toml` with `enabled = true` | on (raven.toml project layer wins at the leaf — project-policy contract) |
| `false` / `"off"` | `raven.toml` with `enabled = false` / `"auto"` / no `[linting]` | off |
| `true` / `"on"` | no project config | on with built-in defaults |
| `true` / `"on"` | `.lintr` discovered | on with `.lintr`'s rule severities |
| `true` / `"on"` | `raven.toml` with `enabled = true` | on |
| `true` / `"on"` | `raven.toml` with `enabled = false` | off (raven.toml project layer wins at the leaf — project-policy contract) |
| `true` / `"on"` | `raven.toml` with `enabled = "auto"` or no `[linting]` | on (project layer is silent on `enabled`; client value passes through) |

<!-- markdownlint-enable MD013 -->

`raven.toml` and `.lintr` are mutually exclusive at discovery: `raven.toml` wins on the same walk and `.lintr` is not consulted.

### `"auto"` and REditorSupport / Positron

Under `"auto"`, a discovered `.lintr` is treated as an opt-in *only* when nothing else is already running `lintr` against it. A `.lintr` is REditorSupport's / `lintr`'s own config file, so when that tool is already linting your project, letting the same file also flip Raven's native lints on would double-report style issues. Raven therefore ignores a discovered `.lintr` (under `"auto"`) when **either**:

- the **REditorSupport (R) extension** is installed and enabled **and** both `r.lsp.enabled` and `r.lsp.diagnostics` are on (its default) — i.e. its language server is actively emitting `lintr` diagnostics; **or**
- you are running inside **Positron**, which ships its own R-session linting.

In those environments, `"auto"` + a discovered `.lintr` resolves to **off**. This affects the `.lintr` path only:

- A `raven.toml` opt-in (`[linting] enabled = true`) is **unaffected** — it always turns Raven's linting on regardless of REditorSupport / Positron.
- An explicit client `true` / `"on"` (or `false` / `"off"`) is **unaffected** — only `"auto"` consults this heuristic.
- Turning REditorSupport's `r.lsp.diagnostics` (or `r.lsp.enabled`) **off**, or uninstalling/disabling the extension, re-enables the `.lintr` auto opt-in live — no window reload needed.

To run Raven's native lints *alongside* REditorSupport's `lintr` on purpose, set `raven.linting.enabled` to `true` (or migrate the project to `raven.toml`). See [Coexistence § Language servers](coexistence.md#language-servers-raven-alone-vs-both).

> This is a VS Code **environment** signal: it is computed by the extension (from REditorSupport's state and `r.lsp.*`) and is not something a project `raven.toml` can override. Editors other than VS Code, and the bare CLI, don't send it, so they keep the historical "discovered workspace/non-home *configured* `.lintr` ⇒ on" behavior (a blank/empty `.lintr` still never opts in). The literal home-directory `~/.lintr` remains default-off unless the editor setting is enabled or the CLI receives it explicitly with `--config ~/.lintr`.

## Settings reference by rule

Each rule lists the Raven settings that control it and the `lintr` linter it mirrors. Severities accept `"error"`, `"warning"`, `"information"`, `"hint"`, or `"off"`. See [Diagnostics § Style Lints](diagnostics.md#style-lints) to see these rules in context with Raven's other diagnostic categories.

### Line length

- **Raven:** `raven.linting.lineLength` (default `80`), `raven.linting.lineLengthSeverity` (default `"information"`).
- **`lintr` equivalent:** `lintr::line_length_linter(length = 80L)`.
- Line length is measured in characters, matching `lintr`'s `nchar()` (an emoji counts as 1).

### Trailing whitespace

- **Raven:** `raven.linting.trailingWhitespaceSeverity` (default `"information"`).
- **`lintr` equivalent:** `lintr::trailing_whitespace_linter()` with its defaults (`allow_empty_lines = FALSE`, `allow_in_strings = TRUE`).
- Whitespace-only lines are flagged; trailing whitespace inside a multi-line string literal is part of the string's value and is not.

### Tab characters

- **Raven:** `raven.linting.noTabSeverity` (default `"information"`).
- **`lintr` equivalent:** `lintr::whitespace_linter()`.
- Flags tabs used for *indentation* only (matching `lintr`): tabs in comments, strings, or between tokens are left alone, and a line starting inside a multi-line string is part of the string's value. One diagnostic per offending line, anchored at the tab run.

### Trailing blank lines

- **Raven:** `raven.linting.trailingBlankLinesSeverity` (default `"information"`).
- **`lintr` equivalent:** `lintr::trailing_blank_lines_linter()`.
- Also fires when the file is missing a final newline; a file can get both diagnostics (trailing blanks *and* the missing newline), as in `lintr`.
- Does not apply to `.Rmd` / `.qmd` documents: the rule describes the file's shape, and a chunk document's shape is Markdown, not R. (Deliberate deviation: `lintr` checks chunk contents in knitr documents.)

### Assignment operator

- **Raven:** `raven.linting.assignmentOperator` (default `"<-"`, alternative `"="`), `raven.linting.assignmentOperatorSeverity` (default `"information"`).
- **`lintr` equivalent:** `lintr::assignment_linter()`.
- Under the default `<-` style, the flagged operators are `=`, `->`, `->>`, and the magrittr assignment pipe `%<>%` (`<<-` is allowed, matching `lintr` 3.3's default `operator = c("<-", "<<-")`; `:=` is never linted). Under the `=` style, `<-`, `<<-`, `->`, `->>`, and `%<>%` are flagged. Named-argument `=` (`f(name = value)`) is never flagged, and `lintr`'s implicit-assignment exclusion is mirrored: an assignment nested inside a call argument, an `if`/`while` condition, or a `for` sequence is skipped (`lapply(xs, function(x) { y = x; y })` and `if ({a = TRUE}) 1` are clean) unless the enclosing expression is explicitly parenthesized (`fun((blah = fun(1)))` is still flagged).

### Object names

- **Raven:** `raven.linting.objectNameStyleFunction`, `raven.linting.objectNameStyleVariable`, `raven.linting.objectNameStyleArgument` (each defaults to `["snake_case", "symbols"]`, matching `lintr`'s default `styles`), `raven.linting.objectNameRegexesFunction`, `raven.linting.objectNameRegexesVariable`, `raven.linting.objectNameRegexesArgument` (each defaults to `[]`), `raven.linting.objectNameSeverity` (default `"information"`).
- **`lintr` equivalent:** `lintr::object_name_linter(styles = ..., regexes = ...)`.
- Each style key accepts `"snake_case"`, `"camelCase"`, `"dotted.case"`, `"UPPER_CASE"`, `"lowercase"`, `"symbols"` (names made up entirely of non-alphanumeric characters, e.g. an operator overload like `` `%+%` ``), `"any"` (disable that kind and ignore its regexes), or an array of those styles. Multiple styles are ORed. A name also passes if it matches any regex for that kind.
- Regexes are Rust regexes applied as partial matches against the full identifier, including any leading `.`. Use anchors such as `^...$` when you need the whole name to match. Rust's regex engine does not support PCRE lookaround such as `(?=...)`; unsupported patterns are warned about and skipped (for the settings/`raven.toml` path the warning goes to the server log; a `.lintr` file surfaces a visible warning notification). Empty regex strings are rejected because they would match every identifier. If regex-only mode supplies regexes but none are valid, Raven retains the default named style instead of silently disabling the check (in `.lintr` the invalid call still clears editor-level regexes, since it stated the project's regex policy).
- An explicit empty style array with regexes is regex-only mode. If both the style array and regex array are empty, that kind's object-name check is disabled. A style value with no recognized style names is warned about and treated as empty: with valid regexes configured the kind becomes regex-only, with no regex setting at all it is disabled, and if the regex setting is present but contains no valid patterns the default style is restored rather than silently disabling the check.
- Backtick-quoted and string names are normalized the way `lintr` does it (`strip_names`): surrounding backticks/quotes and `%` are removed, as is a trailing `<-` (so a replacement function `` `height<-` `` checks as `height`), and the remaining name goes through the normal matching — `` `myBadName` <- 1 `` is flagged exactly like `myBadName <- 1`, while `` `%+%` `` strips to `+` and passes via the default `symbols` style. A name that strips to nothing (e.g. `` `%%` ``) is skipped.
- Carve-outs: named styles always allow an optional leading `.`, but the rest of the name must still match the configured style (so `.helper` is fine under `snake_case`, but `.onLoad` is exempt as one of `lintr`'s special functions — `.onLoad`, `.onAttach`, `.onUnload`, `.onDetach`, `.Last.lib`, `.First`, `.Last`, and `...` are never flagged); S3-method names of the form `<generic>.<class>` (e.g. `print.MyClass`, `as.Date.character`, `` `+.foo` ``) are exempt when the generic is a base R S3 generic (the list is ported from `lintr`, including operator generics) or a generic declared in the same file via `UseMethod` (`lintr`'s `declared_s3_generics`). Hidden methods (`.print.MyClass`) are also exempt — a deliberate leniency over `lintr`, which flags them. Non-ASCII identifiers are skipped when no regexes are configured for that kind; when regexes are configured (regex-only or combined with styles), non-ASCII names are checked against the regexes — the named ASCII styles never match them (a deliberate leniency over `lintr`, whose ASCII style regexes flag non-ASCII names). A compound target checks its base object through `$`/`@` chains, subscripts, and replacement calls (`a$b$c <- 1`, `a[[1]] <- 1`, and `names(a) <- 1` all check `a`, matching `lintr`); symbols used as subscript indices (`x[i] <- 1`) are not targets. Literal binding names in `assign("name", …)` / `setGeneric("name", …)` are checked.

### Infix spaces

- **Raven:** `raven.linting.infixSpacesSeverity` (default `"information"`).
- **`lintr` equivalent:** `lintr::infix_spaces_linter()`.
- Lints exactly `lintr`'s default operator set (the style guide's low-precedence operators): arithmetic (`+`, `-`, `*`, `/`), comparison, logical, assignment (`<-`, `<<-`, `:=`, `->`, `->>`, `=` — including named-argument `=` in calls and formal defaults, `lintr`'s `EQ_SUB`/`EQ_FORMALS`), pipe (`|>`, `%>%`, and any `%...%` user-defined operator), and binary `~`. Each requires at least one space on both sides. High-precedence operators (`^`/`**`, `:`, `::`, `:::`, `$`, `@`, `?`) and unary `-`/`+`/`!`/`~` are never linted, matching `lintr` — `x^2`, `x ^ 2`, and `1:10` are all fine. Alignment whitespace (`x   <- 1`) is allowed; operator-at-end-of-line line continuations are skipped; `/` inside `box::use(...)` module paths is exempt. A named argument with a missing value still needs a space after `=` (`alist(a = )` is fine, `alist(a =)` is flagged) — pinned by `lintr`'s own test suite.

### Commented code

- **Raven:** `raven.linting.commentedCodeSeverity` (default `"information"`).
- **`lintr` equivalent:** `lintr::commented_code_linter()`.
- Flags a standalone comment block (consecutive `#` lines) whose body parses as R and contains a call, assignment, operator, or function definition. Prose that merely contains code shapes is rejected: juxtaposed expressions (`# use foo(x) instead`) are not valid R, and binary `-` / unary `-`, `+`, `?` alone are not evidence (`# 1-a`, `# ?data.frame` are prose, matching `lintr`). A dangling trailing `,` / `%>%` / `|>` or leading `,` is stripped before the parse test, so commented-out argument-list lines and pipe fragments are still caught. Roxygen (`#'`), shebangs, annotation comments (`# TODO:`, `# FIXME:`, `# NOTE:`, `# XXX:`, `# HACK:`, `# BUG:`, `# WARNING:`, `# OPTIMIZE:`), Emacs mode lines, and `# nolint` / `# raven:` / `# @lsp-…` directives are skipped. End-of-line comments next to real code are checked too, like `lintr`: `x <- 1 # other_call()` is flagged, `x <- 1 # explain` is not.

### Quotes

- **Raven:** `raven.linting.stringDelimiter` (default `"\""`, alternative `"'"`), `raven.linting.quotesSeverity` (default `"information"`).
- **`lintr` equivalent:** `lintr::quotes_linter()` / `lintr::single_quotes_linter()` (the two map to the two settings above).
- Raw strings are checked like ordinary literals (`R'(plain)'` is flagged under the double-quote default). Any literal whose source contains the preferred quote character is exempt — switching delimiters would force escaping (`'he said "hi"'`, `r'(")'`).

### Commas

- **Raven:** `raven.linting.commasSeverity` (default `"information"`).
- **`lintr` equivalent:** `lintr::commas_linter()`.
- Flags whitespace before `,` and missing whitespace after `,`. A newline after a comma is fine, so multi-line argument lists are not flagged. Matches `lintr`'s exemptions: a comma that starts its own line (leading-comma continuation style), a comma preceded by another comma (`a[1, , 2]` missing-argument style), and a comma preceded by a value-less named argument's `=` (`switch(op, x = , y = bar)`) are all clean. Matches `lintr`'s default `allow_trailing = FALSE` — a comma directly against a closing bracket (`a[1,]`) is still flagged.

### T / F symbol

- **Raven:** `raven.linting.tAndFSymbolSeverity` (default `"information"`).
- **`lintr` equivalent:** `lintr::T_and_F_symbol_linter()`.
- Flags bare `T` / `F` identifiers used as references to `TRUE` / `FALSE`. Assignment targets (`T <- 0`) get `lintr`'s dedicated "don't use `T` as a variable name" message. Named arguments (`foo(T = TRUE)`), formal parameters (`function(T) ...`), `$` / `@` field names (`obj$T`), formula terms (`y ~ T + F` — though a named-argument value inside a formula call is still a real read), subscripted uses (`T[1]`), and callees (`T(1)`) are exempt — those positions don't read the boolean.

### Semicolon

- **Raven:** `raven.linting.semicolonSeverity` (default `"information"`).
- **`lintr` equivalent:** `lintr::semicolon_linter()`.
- Flags `;` separators in source. A `;` inside any token — string literals, comments, backtick-quoted identifiers (`` `a;b` ``), user infix operators (`%;%`) — is left alone. One diagnostic per `;`.

### Equals NA

- **Raven:** `raven.linting.equalsNaSeverity` (default `"information"`).
- **`lintr` equivalent:** `lintr::equals_na_linter()`.
- Flags `x == NA`, `x != NA`, and the typed variants (`NA_integer_`, `NA_real_`, `NA_character_`, `NA_complex_`) on either side, plus `x %in% NA` (right-hand side only — `NA %in% x` is a legitimate membership test). The comparison always returns `NA`; use `is.na(x)` instead.

### Object length

- **Raven:** `raven.linting.objectLength` (default `30`), `raven.linting.objectLengthSeverity` (default `"information"`).
- **`lintr` equivalent:** `lintr::object_length_linter(length = 30)`.
- Flags assignment targets and formal parameters whose names exceed the configured length. Names are normalized like `object_name` (backticks/quotes stripped), and a leading `<generic>.` prefix is removed for S3 methods so only an overlong class part is flagged, matching `lintr`. All characters of the remaining name count, including a leading `.`.

### Vector logic

- **Raven:** `raven.linting.vectorLogicSeverity` (default `"information"`).
- **`lintr` equivalent:** `lintr::vector_logic_linter()`.
- Flags `&` or `|` in `if` / `while` conditions (where `&&` / `||` is the scalar short-circuit form) and in `expect_true()` / `expect_false()`. The scan recurses through nested logical operators but stops at call boundaries — `if (any(x & y))` is left alone because the `&` is evaluated on a vector inside `any()`. Bitwise arithmetic is exempt: an operand that is a string literal or an `as.raw()` / `as.octmode()` / `as.hexmode()` call (`if (info & as.raw(12))`) marks the operator as bitwise, matching `lintr`. The mirror check flags scalar `&&` / `||` inside `subset()` / `filter()` arguments (bare, `pkg::`-qualified except `stats::filter`, or as a pipe target) — subsetting is a vector context; nested function definitions inside those arguments are skipped.

### Function left parentheses

- **Raven:** `raven.linting.functionLeftParenthesesSeverity` (default `"information"`).
- **`lintr` equivalent:** `lintr::function_left_parentheses_linter()`.
- Flags whitespace between `function` (or the `\` lambda shorthand) and the parameter `(`, and between a call's function name and its argument `(` — `blah (1)`, `base::print (x)`, `` `+` (1, 1) ``, `x$foo (1)`. A `(` on a later line than the function name gets a dedicated message; `@` slot calls get only that cross-line check (same-line whitespace after `@` access is not flagged). String "callees" (`"print"(x)`, `base::"mean"(x)`) and computed callees (IIFEs, `f(x)(y)`) are left alone, matching `lintr`.

### Spaces inside

- **Raven:** `raven.linting.spacesInsideSeverity` (default `"information"`).
- **`lintr` equivalent:** `lintr::spaces_inside_linter()`.
- Flags whitespace immediately inside `(`, `[`, `[[` and their closing counterparts (e.g. `f( x )`, `df[ 1 ]`, `mat[[ i ]]`), including the parens of `if`/`while`/`for` and function/lambda parameter lists. Whitespace-only groupings (`f( )`, `x[ ]`) are flagged on both sides, as in `lintr`; multi-line wrapping is never flagged. Exemptions, matching `lintr`: a comma before the closer (`x[i, ]`, `f(a, )`), a value-less named argument's `=` before `)` only (`alist(a = )` is clean, `x[j = ]` is flagged), and an opener followed by a same-line comment.

### Indentation

- **Raven:** `raven.linting.indentationUnit` (default `"auto"`, or a fixed integer clamped to `1..=8`), `raven.linting.infixContinuationStyle` (default `"indented"`), `raven.linting.indentationSeverity` (default `"information"`). When `indentationUnit` is set to `"auto"` (the default), each R file is linted against VS Code's `editor.tabSize` for that specific file, so files with different tab-size settings in the same workspace are each linted correctly. Set to a fixed integer (e.g. `2` or `4`) to use the same unit for all R files regardless of editor settings. Note: if a `[[linting.overrides]]` entry explicitly sets `indentationUnit` for a file, it takes precedence over the per-file `editor.tabSize`.
- **`lintr` equivalent:** `lintr::indentation_linter()` with its tidy-default hanging style.
- Uses `lintr`'s accumulated "indent change" model (tidy hanging-indent style), verified against a 112-case differential corpus vs real `lintr`. Three kinds of tokens add indentation:
  - A bracket opener indents the lines up to (but not including) a closer that starts its own line — so a standalone closing delimiter aligns with its opener's context. A closer that trails content demands the hanging (aligned-after-the-opener) indent, and tidyverse double-indent function definitions are recognized.
  - An end-of-line operator (`+`, pipes, `$`/`@` chains, named-argument `=`) indents its continuation one more unit — except on the right-hand side of an assignment whose operator ends the line (`lintr`'s `assignment_as_infix` default), so this stays flat:

    ```r
    x <-
      a +
      b
    ```

  - An unbraced `if`/`else`/`for`/`while`/`repeat`/function body, or a multi-line condition, indents one unit.
- A run of consecutive lines mis-indented by the same amount produces a single diagnostic, matching `lintr`.
- Under the default `infixContinuationStyle = "indented"`, Raven additionally accepts (never requires) three shapes `lintr` would flag (all examples use a 2-space unit):
  - The aligned-argument style. `lintr` wants `b` at the block indent (column 2); Raven also accepts it aligned after the opener:

    ```r
    foo(a,
        b
    )
    ```

  - The block form where `lintr` demands a hanging or double indent. Here the closer trails the last argument, so `lintr` insists on alignment after the opener (column 4); Raven also accepts the plain block indent:

    ```r
    foo(a,
      b)
    ```

  - The operator chain-start column, but *only when it sits deeper than the block indent*. `lintr` wants `bar()` at column 2; Raven also accepts column 10, aligned under `foo()` — which is where Raven's [smart indentation](indentation.md) (the AST-aware auto-indent applied when you press Enter) puts it, so the linter never flags indentation its own auto-indent just produced:

    ```r
    result <- foo() +
              bar()
    ```

  These tolerances only ever *add* accepted layouts, so under the default Raven flags a strict subset of what `lintr` flags on those shapes. The chain-start tolerance is not the `"either"` setting: in the `"aligned"` example further below, the chain-start column (4) sits *left of* the block indent (8), so the default flags it — accepting that shape too is exactly what `"either"` adds.
- A standalone comment-only line aligned with the trailing-comment column of an adjacent line expecting the same indent is not flagged — a common intentional documentation style. Directive/suppression markers (`# nolint`, `# raven: ...`, `# @lsp-...`) never get this exemption and are never used as alignment anchors.
- `raven.linting.infixContinuationStyle` (Raven-specific; no `lintr` or `.lintr` equivalent — a `.lintr` can neither configure nor override it, so it stays `"indented"` unless set via `raven.toml` or editor settings) controls how a line continuing an end-of-line infix operator (`|`, `&`, `+`, comparisons, pipes, `%…%`, `$`/`@` chains) is judged. The aligned style is not a Raven invention: RStudio's *vertically align arguments in auto-indent* and Emacs/ESS's `ess-align-continuations-in-calls` (both on by default) produce exactly this layout for operator continuations inside parentheses — at statement level both editors use a fixed one-level continuation indent instead — and Raven's own [smart indentation](indentation.md) extends the aligned layout to operator chains generally. But no `lintr` configuration accepts it (`indent`, `hanging_indent_style`, and `assignment_as_infix` all still demand the extra level; verified against `lintr` 3.3.0.1). This setting closes that pre-existing gap:
  - `"indented"` (default) — require the extra continuation level described above, matching `lintr`. With a 4-space unit this is clean, while the aligned form below is flagged:

    ```r
    changed <- !(
        first_condition |
            second_condition
    )
    ```

  - `"aligned"` — require the continuation exactly at the operator chain's starting column, so peer operands line up. With a 4-space unit this is clean, while the extra-indent form is flagged:

    ```r
    changed <- !(
        first_condition |
        second_condition
    )
    ```

    This is a strict requirement, not an auto-indent-parity mode: [smart indentation](indentation.md) indents a continuation to at least one unit, so for a chain starting at a line's first column (e.g. a top-level `data |>` pipeline) auto-indent suggests one unit while `"aligned"` demands column 0. It also disables the aligned-argument/block-form tolerances on the specific lines an operator continuation covers.
  - `"either"` — accept both forms; anything clean under `"indented"` or `"aligned"` is clean under `"either"`. Genuinely under-indented code stays flagged because the chain-start line itself is still checked against its own expectation.

  In every mode, assignment operators (`<-`, `<<-`, `=`, `:=`, `->`, `->>`) and named-argument `=` continuations are unaffected — `"aligned"` never demands the right-hand side of `x <-` line up under `x`.

## Migrating from `.lintr`

The recommended path is to configure Raven via `raven.toml` at the project root (see [Configuration § Project config](configuration.md#project-config-raventoml)). The table below maps the `lintr` linters covered by Raven to their Raven equivalents. For each `lintr` linter you currently enable, set the corresponding `raven.linting.*` keys; for ones not listed, see [Gaps vs `lintr`](#gaps-vs-lintr).

> **Runtime support:** When no `raven.toml` is discovered on the upward walk from the active project root (first editor workspace folder, `raven check --workspace`, or the `raven lint` working directory), Raven reads a documented subset of the discovered `.lintr`. In the LSP/editor, that discovered `.lintr` is watched and live-reloaded; the CLI reads it once per command invocation. Workspace and non-home ancestor `.lintr` files are read by default. The literal home-directory `~/.lintr` is read only when the VS Code/LSP-client setting `raven.linting.readHomeLintr = true` is enabled, or when the CLI receives it explicitly with `--config ~/.lintr`. The mapping table below is the supported surface. Forms outside the supported subset log a single batch warning and are otherwise ignored.
>
> Multi-line `linters:` / `exclusions:` values are folded by **bracket balance**, so a closing `)` works whether it sits at column 0 or is indented, and `#` comments inside the value are handled. (Real `lintr` reads `.lintr` as strict DCF and *rejects* a column-0 continuation — `read.dcf`: "Regular lines must have a tag" — so Raven logs a one-line note suggesting you indent the closing line if you also run `lintr`; a column-0 `#` comment line is valid DCF and is not counted in that note.) A field whose brackets don't balance — a missing `)`, a stray extra one, or a mismatched type — is reported specifically (`field 'linters' has unbalanced brackets …`) and parsed best-effort rather than silently swallowing the next field. A recognized field still opts the project into linting even when it has a typo, so a single missing `)` never silently turns all linting off. Most linter arguments accept either lintr's named form or a leading positional value (`line_length_linter(120)`, `assignment_linter("=")`, `object_name_linter("snake_case")`). Numeric arguments accept R integer literals — decimal or hexadecimal, each with an optional `L` suffix (`line_length_linter(120L)`, `line_length_linter(0x50)`) — and whitespace before `(` is tolerated (`linters_with_defaults (...)`). The pre-3.0 `with_defaults(...)` spelling is accepted as an alias for `linters_with_defaults(...)`.

| `.lintr` linter | Raven settings |
|---|---|
| `line_length_linter(length = N)` | `raven.linting.lineLength = N`, `raven.linting.lineLengthSeverity` |
| `trailing_whitespace_linter()` | `raven.linting.trailingWhitespaceSeverity` |
| `whitespace_linter()` (no-tab portion) | `raven.linting.noTabSeverity` |
| `trailing_blank_lines_linter()` | `raven.linting.trailingBlankLinesSeverity` |
| `assignment_linter()` | `raven.linting.assignmentOperator`, `raven.linting.assignmentOperatorSeverity` |
| `object_name_linter(styles = c("snake_case"), regexes = c("^x$"))` | `raven.linting.objectNameStyleFunction`, `raven.linting.objectNameStyleVariable`, `raven.linting.objectNameStyleArgument`, `raven.linting.objectNameRegexesFunction`, `raven.linting.objectNameRegexesVariable`, `raven.linting.objectNameRegexesArgument`, `raven.linting.objectNameSeverity` |
| `infix_spaces_linter()` | `raven.linting.infixSpacesSeverity` |
| `commented_code_linter()` | `raven.linting.commentedCodeSeverity` |
| `quotes_linter()` / `single_quotes_linter()` | `raven.linting.stringDelimiter`, `raven.linting.quotesSeverity` |
| `commas_linter()` | `raven.linting.commasSeverity` |
| `T_and_F_symbol_linter()` | `raven.linting.tAndFSymbolSeverity` |
| `semicolon_linter()` | `raven.linting.semicolonSeverity` |
| `equals_na_linter()` | `raven.linting.equalsNaSeverity` |
| `object_length_linter(length = N)` | `raven.linting.objectLength = N`, `raven.linting.objectLengthSeverity` |
| `vector_logic_linter()` | `raven.linting.vectorLogicSeverity` |
| `function_left_parentheses_linter()` | `raven.linting.functionLeftParenthesesSeverity` |
| `spaces_inside_linter()` | `raven.linting.spacesInsideSeverity` |
| `indentation_linter(indent = N)` | `raven.linting.indentationUnit = N`, `raven.linting.indentationSeverity` |

Numeric-argument linters accept both the named and the first-positional form: `line_length_linter(80)` and `line_length_linter(length = 80)` are equivalent, as are `object_length_linter(40)` / `object_length_linter(length = 40)` and `indentation_linter(4)` / `indentation_linter(indent = 4)`.

`object_name_linter` accepts styles positionally or via `styles =`, as a scalar or a `c(...)` vector (e.g. `object_name_linter("camelCase")`, `object_name_linter(styles = c("snake_case", "camelCase"))`) and applies them to functions, variables, and arguments. Known style names (`snake_case`, `camelCase`, `dotted.case`, `UPPER_CASE`, `lowercase`, `symbols`, `any`) map to the three style arrays and are ORed. Regexes may be named (`regexes =`) or passed as the second positional argument, as a scalar or vector; names on vector entries (such as `c(public = "^[a-z]")`) are diagnostic labels and do not change the patterns. Matching `lintr`, explicitly supplying `regexes` **replaces** the default styles (regex-only mode) unless `styles` is also explicitly supplied — specify both to combine them. R string escapes are processed, so `"^\\.on[A-Z]"` means the regex `^\.on[A-Z]`; R raw strings (`r"(^\.on[A-Z])"`) are also supported and taken verbatim. R's numeric escapes (octal `"\056"`, hex `"\x2e"`, unicode `"\u{2e}"`) also decode with R semantics before the pattern reaches the regex engine; escape sequences R itself rejects (such as a bare `"\d"` — write `"\\d"`) are warned about as unrecognized. `lintr` styles Raven has no equivalent for (`CamelCase`, `UPPERCASE`, `SNAKE_CASE`) are warned about and skipped, as are case/punctuation typos of Raven's style names (with a did-you-mean hint); any other unknown non-empty `styles` element is treated leniently as a regex, a Raven extension over `lintr`. Empty style elements are still unrecognized because an empty regex would match every identifier. An explicit empty vector (`styles = c()`, `styles = character()`, or `character(0)`) maps to empty style arrays — regex-only mode when regexes are also given, otherwise "check disabled". Whenever a call states a pattern policy it states the regex policy too: a styles-only call (including the empty-vector forms) or any call with a `regexes` argument — even one whose patterns were all rejected — emits regex arrays (the accepted patterns, or `[]`), so editor-level `objectNameRegexes*` values are cleared rather than silently ORed in.

To disable a rule from a `.lintr` `linters_with_defaults(..., default = list())` setup, set its severity to `"off"`. To raise a rule that `lintr` would flag as a `warning`, raise its severity from `"information"` to `"warning"`.

### Exclusions

A `.lintr` `exclusions:` field that lists files and directories — e.g. `exclusions: list("R/legacy.R", "tests", "NAMESPACE")` — maps to a single `[[linting.overrides]]` entry with `enabled = false`, anchored at the project root. Each entry becomes one or more globs matched against the project-relative path:

| `exclusions:` entry | Globs emitted | Matches |
|---|---|---|
| `"R/"` (trailing slash → directory) | `R/**` | every file under `R/` |
| any other entry (`"R/foo.R"`, `"NAMESPACE"`, `"pkg.Rcheck"`, …) | `<entry>` **and** `<entry>/**` | the path itself if it's a file, or its contents if it's a directory |

Raven resolves exclusions without touching the filesystem, so it can't tell whether a bare entry like `NAMESPACE` (a file) or `pkg.Rcheck` (a directory) is which — and a dot is no help (`foo.R` is a file, `pkg.Rcheck` is a directory). It therefore emits both an exact glob and a recursive glob for every entry that doesn't already end in `/`. The extra glob only ever disables linting on a path that doesn't exist, so it is harmless. Add a trailing slash to force directory-only matching.

Not supported: the named line-range form `list("file.R" = 1:10)` (exclude specific lines of a file) has no Raven equivalent and is ignored with the batch warning. A `=` *inside* a quoted name (e.g. `"a=b.R"`) is treated as an ordinary filename, not this form.

> **Note:** `mixed_logical` and `condition_assignment` are not in this table because they have no `lintr` equivalent and are not style lints — they are always-on semantic warnings configured under `raven.diagnostics.mixedLogicalSeverity` and `raven.diagnostics.conditionAssignmentSeverity`. See [Diagnostics § Semantic Warnings](diagnostics.md#semantic-warnings).

If you'd like a starter project-scoped `raven.linting.*` block scaffolded into `.vscode/settings.json` — every linting key Raven maps to `raven.toml`, each prefaced with a `//` comment naming its `lintr` equivalent — run the **Raven: Create linting settings** command from the Command Palette ([Configuration § Scaffold Commands](configuration.md#scaffold-commands)). It merges into an existing `settings.json` without disturbing unrelated keys or comments, preserves client-only linting settings such as `raven.linting.readHomeLintr`, and prompts before overwriting any pre-existing project-scoped `raven.linting.*` values.

If you also want to run `lintr` itself alongside Raven, see [below](#filling-the-gaps-with-lintr-itself) — that path needs a `.lintr` file, which Raven doesn't generate.

## Gaps vs `lintr`

`lintr` ships more than 140 linters in total, of which about two dozen are enabled by default. Raven implements 18 of those defaults — the ones in the table above. Common `lintr` linters that have **no Raven equivalent** include (non-exhaustive):

- `object_usage_linter` — flags undefined globals inside function bodies via `codetools::checkUsage()`. Raven's [Undefined variable diagnostic](diagnostics.md#undefined-variables) covers similar ground at the file and `source()`-chain level (via static cross-file scope), but with different semantics: Raven's check is scope- and position-aware across `source()` chains, while `object_usage_linter` runs inside individual function bodies via R's own analyzer.
- `cyclocomp_linter` — cyclomatic complexity.
- `seq_linter`.
- `brace_linter`, `paren_body_linter`, `spaces_left_parentheses_linter`.
- `pipe_continuation_linter`, `pipe_call_linter`.
- `absolute_path_linter`.

If you rely on any of these, the recommended setup is to run `lintr` via the REditorSupport extension alongside Raven — Raven's language server is designed to coexist with REditorSupport. See [below](#filling-the-gaps-with-lintr-itself).

### Filling the gaps with `lintr` itself

The [REditorSupport extension](https://marketplace.visualstudio.com/items?itemName=REditorSupport.r) runs `lintr` from inside its own R-based language server, so it covers every linter `lintr` ships. To run both at once:

1. Keep Raven installed and enabled.
2. Install the REditorSupport extension. Leave `r.lsp.enabled` at its default (`true`).
3. Place a `.lintr` file at your project root. Raven does not scaffold this file — its format is `lintr`'s own DSL. (Raven reads a [documented subset](#migrating-from-lintr) of `.lintr` at runtime when no `raven.toml` is present, but the file primarily exists so `lintr` itself can consume it from REditorSupport's R session.) A minimal starter that mirrors the `lintr` default rule set with a 120-character line limit is one line:

   ```r
   linters: linters_with_defaults(line_length_linter(120))
   ```

4. Install `lintr` in the R session REditorSupport uses (`install.packages("lintr")`).

REditorSupport's LSP will surface `lintr` diagnostics; Raven will continue to surface its own. Both sets will appear in the Problems pane and you can tell them apart by the `source` field (`raven (lint)` for Raven, `lintr` for REditorSupport). See [Coexistence § Language servers](coexistence.md#language-servers-raven-alone-vs-both) for the broader cross-extension model.

## Suppression matrix

Raven recognizes its own `# raven:` primary namespace, the legacy `@lsp-ignore` aliases, and `lintr`'s `# nolint`. All apply to lint diagnostics. The `# raven:` forms — `ignore`, `ignore-next`, `ignore-start`/`ignore-end`, and `ignore-file` (and their `@lsp-ignore` / `@lsp-ignore-next` aliases) — additionally apply to several of Raven's other diagnostics: undefined-variable, invalid-assignment-target, missing-package, and out-of-scope-symbol errors. They do **not** suppress structural syntax parse errors (unbalanced brackets, orphan `else`, etc.), which can only be turned off with `raven.diagnostics.enabled`, nor the dependency-graph diagnostics (missing file, circular dependency, max chain depth exceeded, redundant directive), which are governed only by their own [severity settings](diagnostics.md#cross-file-diagnostics). `# nolint` and `# nolint start/end` apply to lint diagnostics and the `mixed_logical` / `condition_assignment` semantic checks only; they do not suppress any parse-error diagnostics.

A `[code]` selector is **enforced per rule/code** in both tracks: `# raven: ignore[line-length]` silences only the line-length lint, and `# nolint: line_length` is likewise honored per rule (both kebab-case and lintr `snake_case` spellings are accepted). Every form also has an asserting **`expect`** flavor (`# raven: expect…`, `@lsp-expect…`) that reports an [`unused-suppression`](diagnostics.md#suppressing-diagnostics) hint when it matches nothing. See [Directives → Ignore Directives](directives.md#ignore-directives) for the complete syntax and the `expect` semantics.

| Marker | Scope | Origin | Applies to |
|---|---|---|---|
| `# raven: ignore` (trailing) | The line it appears on | Raven (primary) | Lint diagnostics, the `mixed_logical` / `condition_assignment` checks, plus `undefined-variable`, `assign-to-string-literal`, and `package-not-installed` diagnostics (and out-of-scope usages, which carry the `undefined-variable` code). **Not** parse errors, nor the dependency-graph diagnostics |
| `# raven: ignore-next` | The *following* source line | Raven (primary) | Same as `# raven: ignore` |
| `# raven: ignore-start` … `# raven: ignore-end` | Inclusive range between the two markers | Raven (primary) | Same as `# raven: ignore` (lint **and** analyzer diagnostics) |
| `# raven: ignore-file` | Every line in the file | Raven (primary) | Same as `# raven: ignore` (lint **and** analyzer diagnostics) |
| `# raven: ignore[code]` | As above, narrowed to the listed code(s) | Raven (primary) | The selector is enforced per code/rule in every form; comma-separate to list several |
| `# raven: expect…` | Same scope as the matching `ignore…` form | Raven (primary) | Suppresses identically, **and** reports `unused-suppression` (a hint) if it matched nothing |
| `# nolint` (trailing) | The line it appears on | `lintr` convention | Lint diagnostics and the `mixed_logical` / `condition_assignment` semantic checks |
| `# nolint: rule_a, rule_b` | The line it appears on | `lintr` convention | Lint diagnostics and the semantic checks, narrowed to the named rules (honored per rule) |
| `# nolint start` … `# nolint end` | Inclusive range between the two markers | `lintr` convention | Lint diagnostics and the `mixed_logical` / `condition_assignment` semantic checks |
| `# @lsp-ignore` | The line it appears on | Raven (alias of `# raven: ignore`) | Lint diagnostics, the `mixed_logical` / `condition_assignment` checks, plus `undefined-variable`, `assign-to-string-literal`, and `package-not-installed` diagnostics (and out-of-scope usages, which carry the `undefined-variable` code). **Not** parse errors, nor the dependency-graph diagnostics (missing file, circular dependency, max chain depth, redundant directive) |
| `# @lsp-ignore-next` | The *following* source line | Raven (alias of `# raven: ignore-next`) | Same as `# @lsp-ignore` |

Notes:

- A `# nolint` marker inside a string literal (`x <- "# nolint"`) is not parsed as a marker.
- A typo like `# nolinter` or `# @lsp-ignored` is intentionally not recognized — better to surface the lint than to silently swallow it.
- An unterminated `# nolint start` (or `# raven: ignore-start`) suppresses through end of file (matching `lintr`).
- A same-line marker nested inside a commented-code line — `# x <- 1 # nolint` — also works. The fallback only fires when the prefix between the outer `#` and the inner `# nolint` parses as real R code, so the same marker buried in prose (`# this is just talking about nolint # nolint`) is left alone.

## Performance and scope notes

- **R Markdown / Quarto.** Lint rules apply inside R chunk bodies of `.Rmd` / `.qmd` documents — both in the editor and via `raven lint`. Prose, YAML front matter, and non-R chunks are never linted. `# nolint`, `# nolint start` / `# nolint end`, and `# raven: ignore` markers work inside chunk bodies exactly as in plain `.R` files. You can also suppress a whole chunk with the `raven.ignore` chunk option or an in-chunk `# raven: ignore-chunk` directive — see [Code chunks → Suppressing diagnostics in a chunk](chunks.md#suppressing-diagnostics-in-a-chunk). The one exception is [trailing blank lines](#trailing-blank-lines), which describes the file's shape — Markdown, not R — and is disabled for chunk documents.
- **Static, no R subprocess.** Raven's lint rules run against the tree-sitter parse it already maintains for completions and diagnostics. There's no `lintr` install, no `R` process, no startup cost. The `commented_code` rule re-parses each candidate comment body via a thread-local parser pool; every other rule walks only the already-parsed tree.
- **`commented_code` differs subtly from `lintr`.** Both decide whether a comment body "looks like code" by parsing it, but Raven parses with tree-sitter and `lintr` parses with R itself. Edge cases that exercise R-specific syntax (very old `_` assignment, non-ASCII operator overloads, etc.) may be classified differently.
- **Position-aware, but not call-flow-aware.** Raven walks the AST top-down for most rules and does not run R-level data-flow analysis. Rules that would need that (`object_usage_linter`, `cyclocomp_linter`, `seq_linter`) are intentionally out of scope for the native linter — run `lintr` for those (see [above](#filling-the-gaps-with-lintr-itself)).

## See also

- [Diagnostics § Style Lints](diagnostics.md#style-lints) — these rules alongside Raven's other diagnostic categories.
- [Configuration § Linting Settings](configuration.md#linting-settings) — every `raven.linting.*` key.
- [Coexistence § Language servers](coexistence.md#language-servers-raven-alone-vs-both) — running Raven and REditorSupport together.
- [Comparison](comparison.md) — how Raven differs from REditorSupport, Positron/Ark, and RStudio.
