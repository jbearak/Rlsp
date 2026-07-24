# Directives

Raven recognizes special comment directives that provide hints the static analyzer cannot infer on its own. Directives cover cross-file relationships, working directory context, symbol declarations, and diagnostic suppression.

## General Syntax

**`# raven:` is the canonical prefix for every directive family** — cross-file relationships, working directory, declarations, and diagnostic suppression alike. The legacy **`@lsp-`** prefix remains a permanent, backward-compatible alias for every directive: `# raven: source utils.R` and `# @lsp-source utils.R` are exactly equivalent, as are `# raven: sourced-by ../main.R` / `# @lsp-sourced-by ../main.R`, `# raven: cd /data` / `# @lsp-cd /data`, and so on. You never need to mix the two; pick `# raven:` for new code.

All directives must appear on their own comment line (starting with `#`, optionally indented). They are not recognized in trailing comments like `x <- 1 # raven: source file.R`. The exceptions are the line-level ignore markers `# raven: ignore` / `# raven: expect` (and their aliases `@lsp-ignore` / `@lsp-expect`), which work as trailing comments.

All directives support optional colon and quotes:
```r
# raven: sourced-by ../main.R
# raven: sourced-by: "../main.R"
# raven: sourced-by: '../main.R'
```

**Header-only directives:** Backward directives and working directory directives must appear in the **file header** — the region of consecutive blank and comment lines at the top of the file, before any code. They are ignored if placed after the first line of code. Forward directives, declaration directives, and ignore directives can appear anywhere.

## Forward Directives

Declare that this file sources another file. Useful when the path is dynamic or conditional:

```r
# raven: source utils/helpers.R
# raven: run utils/helpers.R        # synonym
# raven: include utils/helpers.R    # synonym
# @lsp-source utils/helpers.R       # alias (also @lsp-run / @lsp-include)
```

Symbols from the target file become available after the directive's line. Optional `line=` parameter overrides the call-site:

```r
# raven: source utils/helpers.R line=25
# Symbols from helpers.R become available at line 25 (not at this directive's line)
```

```r
# raven: source utils/helpers.R line=eof
# Symbols become available at end of file (line=end is an accepted synonym)
```

**Path resolution:** Forward directives respect `# raven: cd` (same as `source()` calls).

## Backward Directives

Declare that this file is sourced by another file (header-only):

```r
# raven: sourced-by ../main.R
# raven: run-by ../main.R        # synonym
# raven: included-by ../main.R   # synonym
# @lsp-sourced-by ../main.R      # alias (also @lsp-run-by / @lsp-included-by)
```

Optional parameters:
- `line=N` — 1-based line number in parent where the source() call occurs
- `line=eof` or `line=end` — use scope at end of parent file
- `match="pattern"` — text pattern to locate the source() call in parent

```r
# raven: sourced-by ../main.R line=15
```

```r
# raven: sourced-by ../main.R line=eof
# Sees all symbols defined in main.R, including those from other sourced files
```

```r
# raven: sourced-by ../main.R match="source("
# Searches for "source(" in main.R and uses the first match
```

**Call-site inference:** When neither `line=` nor `match=` is specified, Raven scans the parent for `source()` calls referencing this file. If none found, the configured default (`assumeCallSite`) is used.

**Path resolution:** Backward directives resolve relative to the file's own directory and **ignore** `# raven: cd`.

**Per-file opt-out of auto mode:** If a file has explicit `# raven: sourced-by` directives, auto-inferred backward edges are disabled for that file.

## Working Directory Directives

Set working directory context for path resolution (header-only):

```r
# raven: cd /data/scripts
# raven: working-directory /data/scripts   # synonym
# raven: working-dir /data/scripts         # synonym
# raven: current-directory /data/scripts   # synonym
# raven: current-dir /data/scripts         # synonym
# raven: wd /data/scripts                  # synonym
# @lsp-cd /data/scripts                    # alias (every synonym also has an @lsp- form)
```

Path interpretation:
- Paths starting with `/` are workspace-root-relative (e.g., `/data` → `<workspace>/data`)
- Other paths are file-relative (e.g., `../shared` → parent directory's `shared`)

**What it affects:**

| Directive Type | Uses the `cd` directive? |
|---|---|
| Forward (`source` / `run` / `include`) | **Yes** |
| `source()` calls | **Yes** |
| Backward (`sourced-by` / `run-by` / `included-by`) | **No** |

### Working Directory Inheritance

When a child file uses a backward directive without its own `cd` directive, it inherits the working directory from its parent. Inheritance is transitive. An explicit `cd` directive in the child always takes precedence.

```r
# parent.R
# raven: cd /data/project
source("child.R")
```

```r
# child.R
# raven: run-by ../parent.R
# Inherits /data/project from parent
source("utils.R")  # Resolves to <workspace>/data/project/utils.R
```

## Self-Contained Sourced Files

```r
# raven: self-contained
# raven: standalone          # alias
# @lsp-self-contained        # alias
# @lsp-standalone            # alias
```

Place `# raven: self-contained` in the **header** of a sourced helper, setup,
or library-style file to declare that it does not rely on caller-provided
bindings. It takes no arguments and is **header-only** (it must appear before
any code; a `self-contained` or `standalone` token after code is ignored).

A self-contained file is resolved **in isolation** from the files that
`source()` it - both when computing its own diagnostics and when it is pulled in
as another file's forward `source()` child:

- It does **not** inherit variables, loaded packages, or working directory from
  any caller - no backward parent-prefix walk, and a caller's forward resolution
  feeds it canonical, caller-independent inputs. Its cross-file scope is a pure
  function of the file itself and its own forward `source()` closure, not of who
  sources it.
- It **still contributes** its own definitions *and* its own `library()`-loaded
  packages forward to callers (the normal additive `source()` merge is
  unchanged). A module that loads the packages its callers rely on still works.
- Its own `rm()`/`remove()` of its own symbols still affects what it exports
  (unchanged); it never removes a caller's bindings.

**When to use it.** A self-contained helper/setup file that is sourced by many
others, especially when that hub also sources a deep or broad chain of files.
Declaring it self-contained both **prevents** caller-union over-approximation
(one caller's bindings leaking into the file's analysis on behalf of another)
and can make Raven much faster on deeply nested, high-fan-out source graphs.

**Opt-in and safe-direction.** You vouch that the file is self-contained. If it
actually relies on a binding a caller provides, the worst case is a
false-positive *"undefined variable"* **inside the self-contained file itself** -
never a hidden real bug in a caller. (Mark the *root* setup file that provides
the shared environment as self-contained, not a leaf that consumes it.)

**Interactions.** `self-contained` only suppresses the *caller -> file*
direction. It composes with `# raven: cd` (the file uses its own working
directory), `# raven: nse` / `# raven: func` (directive propagation over
`source()` edges is unaffected), package mode, and per-call `local = TRUE` /
`sys.source()` (which govern how a *caller* sources the file, independent of
the file's own isolation). See [Speeding Up Cross-File Analysis](cross-file-analysis-performance.md)
for the practical guidance and [Cross-File Analysis](cross-file.md) for the
resolution model.

## Declaration Directives

Declare symbols created dynamically that the parser cannot detect. These suppress false-positive "undefined variable" diagnostics for symbols from `eval()`, `assign()`, `load()`, or external data loading.

Declaration directives work in any R file, whether or not it participates in
cross-file chains. In Stan, local `# raven: var` and `# raven: func`
declarations are also honored by the conservative undeclared-variable pass from
the following line onward. This is local only: it does not create R scope or
cross-file relationships for a `.stan` file.

### Variable Declarations

```r
# raven: var myvar
# raven: variable myvar           # synonym
# raven: declare-var myvar        # synonym
# raven: declare-variable myvar   # synonym
# @lsp-var myvar                  # alias (every synonym also has an @lsp- form)
```

Raven also treats an `exists("myvar")` call as an automatic `# raven: var myvar` — a probe like `if (!exists("myvar")) myvar <- default` declares `myvar` without any directive, with the same next-line visibility as the directive: the name resolves from the line *after* the `exists()` call onward, and a use before it is still flagged. See [Undefined Variables](diagnostics.md#undefined-variables) for the exact rules (string-literal name; next-line visibility).

### Function Declarations

```r
# raven: func myfunc
# raven: function myfunc          # synonym
# raven: declare-func myfunc      # synonym
# raven: declare-function myfunc  # synonym
# @lsp-func myfunc                # alias (every synonym also has an @lsp- form)
```

`# raven: func` can also declare a formal list alongside the symbol, which records the order of parameters for use by `# raven: nse` positional matching (see below). You can paste a real signature — `= default` suffixes are dropped, keeping just the formal names:

```r
# raven: func my_func(data, x, y)        # declares existence + formal order
# raven: func my_func(data, x = NULL)    # defaults are stripped → data, x
# raven: func pkg::my_func(data, x)      # qualified form
# raven: func "my data fn"               # quote a name with spaces / special chars
```

The unquoted name accepts a bare `name` or a single `pkg::name` qualifier — including dotted names like `make.names`, since `.` is a normal identifier character in R. Only a name containing characters *outside* `[A-Za-z0-9._]` (names with spaces, replacement functions like `` `dim<-` ``) must use the quoted form. A default whose value itself contains a comma or parentheses (`x = c(1, 2)`) is out of scope — keep formal lists to names and simple literal defaults.

### NSE Declarations

Teach Raven that a function uses non-standard evaluation (NSE), so bare
identifiers in its captured arguments are not flagged as undefined variables.
For examples and guidance on when to use this directive, see
[Non-Standard Evaluation](non-standard-evaluation.md).

```r
# raven: nse my_func              # whole-call: every argument is NSE
# raven: nse my_func(x)           # only the `x` formal is captured
# raven: nse my_func(x, y)        # `x` and `y` are captured
# raven: nse my_func(...)         # arguments absorbed by `...` are captured
# raven: nse pkg::my_func(x, y)   # qualified form
# raven: nse "my data fn"(x)      # quote a name with spaces (called `my data fn`(x))
# @lsp-nse my_func(x)             # alias (optional colon/spacing also accepted)
```

As with `# raven: func`, the callee name accepts a bare `name` or a single
`pkg::name` qualifier unquoted; a name with characters outside `[A-Za-z0-9._]`
(e.g. spaces) must use the quoted form. The NSE policy is consulted only for
ordinary `callee(args)` calls, so it cannot apply to operators (`a %+% b` is not
a call). An operator name contains characters outside `[A-Za-z0-9._]`, so it
would have to use the quoted form (`# raven: nse "%+%"`) to parse at all — and
even then it is inert, because operator calls are not `callee(args)` calls.

A literal `...` in the captured list declares that the arguments a call passes
through the function's `...` are captured (checked formals before `...` are
still verified). Combine it with named formals, e.g. `# raven: nse my_func(x, ...)`.

`# raven: nse` is position-aware: it applies to calls **after** the directive
line, and the most recent declaration for a function wins. It is an
authoritative declaration that overrides Raven's own inference for that callee —
but it is **not** a blanket diagnostic ignore: it declares a reusable
argument-evaluation policy for the named function. An *unqualified* declaration
overrides even a local `name <- function(...)` definition's inferred policy
(R calls the named binding, and you are declaring how it evaluates its
arguments); a *qualified* one yields to a local binding of the bare name, as
below.

A qualified declaration (`pkg::my_func`) matches both `pkg::my_func(...)` calls
and unqualified `my_func(...)` calls when `pkg` is in scope (loaded via
`library()` or package imports) — except when a local binding shadows the bare
name, since R would then invoke the local value rather than `pkg::my_func`.

#### `# raven: nse` is deliberately coarse

`# raven: nse` is a file-level, name-keyed, authoritative override. Apart from
the position-awareness of the directive line itself (it governs calls *after*
it), it is intentionally **not**:

- **Scope-aware.** It applies to every call of the named function in the file,
  including calls inside nested function bodies that locally rebind the name — a
  nested helper sharing a top-level name is still governed by the file-level
  directive, not its inner definition.
- **`library()`-position-aware.** Whether a `pkg::` qualifier's package is "in
  scope" is judged file-wide, so a qualified declaration can govern a bare call
  even if the matching `library(pkg)` appears later in the file.
- **Arity- or signature-aware.** It does not validate the captured formals
  against the callee's real definition. In particular, `# raven: nse f(...)`
  captures a call's trailing arguments even if `f` does not actually declare a
  `...` formal.

This is by design: the directive is an opt-in escape hatch where you take
responsibility for declaring an accurate policy. It **replaces** Raven's own
inference for the named callee rather than merely adding to it, so an inaccurate
or stale directive cuts both ways — one *broader* than the real NSE behavior
hides diagnostics you may have wanted, while one *narrower* than Raven's own
inference (for example declaring a partial capture on a function Raven already
recognizes as data-masking) can re-surface diagnostics it would otherwise have
suppressed.

Named arguments are matched by formal name regardless of order. For
**positional** arguments Raven needs the formal order, which it reads from a
visible local definition or from a paired `# raven: func` declaration:

```r
# raven: func my_func(data, x, y)   # declares existence + formal order
# raven: nse my_func(x, y)          # declares which formals are captured
my_func(df, col_a, col_b)           # col_a, col_b suppressed; df checked
```

Installed-package functions expose only export names (not formals) to the
synchronous diagnostic pass, so positional matching for an installed-package
callee also requires a paired `# raven: func` with formals.

#### `# raven: nse` propagates across the source graph

A `# raven: nse` declaration governs matching call sites in **every file
connected to it through the resolved `source()` graph** — not just the file that
physically contains it. So you can declare a helper's NSE contract next to its
`library()` call, its definition, or in a sourced setup file, and it suppresses
the corresponding false positives wherever the helper is called. Propagation
works in both directions and transitively:

```r
# setup.R
library(arm)
# raven: nse: lmer

# analysis.R
source("setup.R")
lmer(undefined_var)   # `undefined_var` suppressed — lmer is declared NSE in setup.R
```

Cross-file propagation is intentionally **coarse and file-level**: a propagated
declaration ignores its original line in the other file and governs the whole
connected file. Within the file that *physically contains* the directive, the
usual position-aware, latest-wins behavior still applies, and an own directive
takes precedence over one propagated from a connected file. A directive
propagated from another file is consulted *below* the precise built-in policy
tables, so it never coarsens a known verb (e.g. `dplyr::filter`) — it governs
callees that would otherwise be checked as standard-eval (like `lmer`). Two
unconnected files never share NSE directives. The context an NSE directive needs
to resolve — the `library()` call, a `# raven: func` formal-order declaration —
may live in any connected file and may appear before or after the directive.

Propagation reuses the same dependency graph as cross-file scope analysis, so it
honors the `# raven: cd` and workspace-root path-resolution rules and the
`max_chain_depth` limit, and editing a directive in any connected file
revalidates the dependents that rely on it. A propagated **per-formal** directive
needs the callee's formal order from a `# raven: func` declaration in some
connected file; without one it falls back to named-argument-only matching. A
local `name <- function(...)` definition does **not** supply that order — a local
binding *shadows* the propagated directive entirely (R would call the local
value, so the local definition's own inferred policy applies instead), exactly as
a local binding overrides a qualified `# raven: nse pkg::name`.

### Position-Aware Behavior

Declared symbols are available starting from the next line (line N+1):

```r
# raven: var data_from_api
x <- data_from_api  # OK: in scope (next line after directive)
```

```r
x <- data_from_api  # ERROR: used before declaration
# raven: var data_from_api
```

### Cross-File Inheritance

Declarations propagate to sourced child files when declared before the `source()` call:

```r
# parent.R
# raven: var shared_data
source("child.R")  # child.R can use shared_data
```

### Use Cases

```r
# Dynamic assignment
assign(paste0("var_", i), value)
# raven: var var_1
# raven: var var_2

# Loading data from external sources
load("data.RData")
# raven: var model_fit
# raven: var training_data

# eval() with constructed expressions
eval(parse(text = code_string))
# raven: func dynamic_function
```

## Ignore Directives

Raven's primary suppression namespace is **`# raven:`**. It works both in the
editor (LSP) and in `raven check`, and covers **all** diagnostics — the
always-on analyzer diagnostics ([Diagnostics](diagnostics.md)) and the opt-in
[style/lint rules](linting.md) alike. The legacy `@lsp-ignore` /
`@lsp-ignore-next` markers remain permanent aliases for the line and next-line
forms.

For Stan, these directives suppress conservative `undefined-variable`
findings only; native `syntax-error` findings remain non-suppressible. A
trailing `# raven: ignore` preserves and parses the Stan code before the marker,
while standalone `ignore-next`, file, and block forms keep their normal line
targeting.

### Line and next-line

```r
x <- foo # raven: ignore    # Trailing form: suppresses diagnostics on this line
# raven: ignore-next        # Standalone form: suppresses diagnostics on the next line

x <- foo # @lsp-ignore      # Alias of `# raven: ignore`
# @lsp-ignore-next          # Alias of `# raven: ignore-next`
```

A standalone `# raven: ignore` (or `# @lsp-ignore`) on its own line has no
effect — comment lines carry no diagnostics. Use the `-next` form to suppress
the following line, or place the marker as a trailing comment on the line you
want suppressed. The plain `ignore` / `@lsp-ignore` line forms may appear as
trailing comments; the `-next`, file, and block forms must be standalone (on
their own line) — a trailing `x <- 1 # raven: ignore-next` is silently
ignored.

### Per-code selector

An optional `[code]` selector narrows a suppression to one or more diagnostic
codes, e.g. `# raven: ignore[undefined-variable]` or
`x <- foo # @lsp-ignore[undefined-variable]`. List several codes
comma-separated: `# raven: ignore[undefined-variable, line-length]`. The codes
are the kebab-case names listed in [Diagnostics](diagnostics.md) and the lint
rule names in [Linting](linting.md) (both `kebab-case` and lintr's `snake_case`
spelling are accepted). A bare `# raven: ignore` with no selector suppresses
every diagnostic on the target line. The selector is **enforced**: a
`[code]` directive leaves diagnostics with other codes in place.

### File and block forms

```r
# raven: ignore-file                     # suppress the whole file
# raven: ignore-file[undefined-variable] # …only this code

# raven: ignore-start                    # open a block
x <- foo
y <- bar
# raven: ignore-end                      # close it (inclusive of both directive lines)
```

These cover both the analyzer diagnostics and the lint rules. An unterminated
`ignore-start` extends to the end of the file. For lintr compatibility,
`# nolint` / `# nolint start` / `# nolint end` also remain (lint diagnostics
only) — see the [Linting suppression matrix](linting.md).

### `expect` — assert that a suppression is used

Every form above also comes in an **`expect`** flavor: `# raven: expect`,
`# raven: expect-next`, `# raven: expect[code]`, `# raven: expect-file`,
`# raven: expect-start` / `# raven: expect-end`, and the `@lsp-expect` /
`@lsp-expect-next` aliases. `expect` suppresses exactly like `ignore`, but it
also **asserts that it actually suppressed something**: if an `expect`
directive matches no diagnostic, Raven reports an
[`unused-suppression`](diagnostics.md) hint at the directive's line. This
mirrors Rust's `#[expect]` and TypeScript's `@ts-expect-error`: use `ignore`
for a silent suppression, `expect` when you want to be told once the underlying
diagnostic is gone so the now-stale directive can be removed.

```r
result <- compute() # raven: expect[undefined-variable]
# If `compute` later becomes defined/imported, the expect no longer suppresses
# anything and Raven flags it as an unused suppression (a HINT) — your cue to
# delete the directive.
```

A plain `ignore` is never reported as unused by default. Set
[`raven.diagnostics.reportUnusedSuppressions`](configuration.md) to `true` to
extend the unused-suppression sweep to **every** ignore directive
(Pyright-style), not just `expect`.

### Chunk-level suppression (R Markdown / Quarto)

Inside `.Rmd` / `.qmd` documents you can suppress a whole chunk with the knitr
chunk option `raven.ignore` or the in-chunk `# raven: ignore-chunk` directive.
See [Code chunks](chunks.md#suppressing-diagnostics-in-a-chunk).
