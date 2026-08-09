# Diagnostics

Raven reports problems in your R code as you type — undefined variables, missing packages, circular dependencies, and scope violations. Diagnostics are cross-file-aware: they reflect the full dependency graph, not just the open buffer.

In the bundled VS Code extension, editor diagnostics are tab scoped. Indexed or sourced files still contribute symbols and dependency information, and documents opened invisibly by another extension remain available to cross-file analysis, but Raven does not give those background documents their own Problems entries. Closing a file's last tab clears its published diagnostics even when another extension keeps the document open in the background; opening a tab for that document republishes its diagnostics. In a diff editor, the modified side owns diagnostics; the original/reference side does not unless it is also shown independently — through its own ordinary tab or another visible editor such as a peek view. Other LSP clients that do not provide Raven with an editor-resource set retain standard `didOpen` scoping, and closing a document clears its published diagnostics.

Diagnostics are deferred until the workspace scan completes (in `auto` backward dependency mode), so cross-file warnings reflect the full project.

Diagnostics fall into two groups. **Correctness diagnostics** — parse errors, undefined variables, package and cross-file issues, assignment-target errors, and semantic warnings — are on by default whenever `raven.diagnostics.enabled` is true, except that standalone JAGS/BUGS and Stan diagnostics are separately opt-in with `raven.diagnostics.jags` and `raven.diagnostics.stan` (both default `"off"`). Most categories expose a severity setting that can raise, lower, or silence them (set to `"off"`); a few (R parse errors, assignment-target errors) have no per-category knob and respond only to the master switch. **Style lints** are subjective formatting rules gated by the tri-state `raven.linting.enabled` switch (default `"auto"` — on when a `.lintr` or `raven.toml` opts in, though the literal home-directory `~/.lintr` is ignored unless the VS Code/LSP-client setting `raven.linting.readHomeLintr` is true or the CLI receives `--config ~/.lintr`, and a `.lintr` is ignored when REditorSupport's `lintr` path is live or you're in Positron; see the [behavior matrix](linting.md#behavior-matrix)). If you're looking for a specific check, scan the categories below before reaching for [Linting](linting.md), which covers the style group in depth.

## Quick Reference

- **Silence one site** — add `# raven: ignore` on the line (or `# raven: ignore-next` on the line above). `# @lsp-ignore` is a permanent alias. See [Suppressing diagnostics](#suppressing-diagnostics)
- **Declare a symbol the analyzer can't see** — use [`# raven: var`, `# raven: func`](directives.md#declaration-directives)
- **Suppress false positives from an uncatalogued NSE helper** — use [`# raven: nse`](non-standard-evaluation.md#when-raven-needs-a-hint) to declare which formals are captured; preferable to a blanket ignore because it keeps checking non-captured arguments
- **Bring a parent file's symbols into scope** — usually nothing to do (auto mode infers relationships). Add `# raven: sourced-by` only when auto-discovery can't see the link. See [Cross-File Awareness](cross-file.md)
- **Account for project startup helpers** — Raven models a workspace-root `.Rprofile` for ordinary script scope. See [`.Rprofile` Startup Prelude](rprofile.md)
- **Turn a category off globally** — set the matching severity to `"off"` (see [Configuration](configuration.md))
- **Opt into model diagnostics** — set `raven.diagnostics.jags` and/or `raven.diagnostics.stan` to `"on"`
- **Disable everything** — set `raven.diagnostics.enabled` to `false`

## Diagnostic codes

Every diagnostic carries a stable, kebab-case `code` (never an opaque number).
Not every code is suppressible: only codes marked **suppressible** below can be
targeted by a `[code]` selector inside a `# raven: ignore[...]` directive.
Writing `# raven: ignore[syntax-error]` or `# raven: ignore[unresolved-source-path]`
is a silent no-op — those codes are deliberately excluded from suppression (parse
errors cannot be suppressed; path-resolution and other dependency-graph diagnostics
are governed only by their severity settings). The suppressible analyzer codes are:

| Code | Diagnostic | Suppressible? |
|---|---|---|
| `undefined-variable` | Undefined / used-before-defined variable (incl. "used before it's available") or an undeclared static `{targets}` target reference | Yes |
| `syntax-error` | Parse errors (the umbrella code; most parse errors carry it directly, but `chained-comparison` is a sub-kind that carries its own code) | No |
| `unresolved-source-path` | A `source()` / forward-directive path (`# raven: source` / `# raven: run` / `# raven: include`) or backward-directive path (`# raven: sourced-by` / `# raven: run-by` / `# raven: included-by`) that does not resolve to a file — missing, outside the workspace, or case-ambiguous (2+ case-insensitive matches); also a missing literal `tar_render()` / `tar_knit()` / single-document `tar_quarto()` path | No |
| `source-path-case-mismatch` | A `source()` / forward-directive **or** backward-directive (`# raven: sourced-by` etc.) path that resolves only by a case difference from the real filename (`templates.r` vs `templates.R`) | No |
| `box-module-not-found` | A static relative `box::use(./module)` target does not resolve to a file module or `__init__.r` / `__init__.R` module | No |
| `box-module-case-mismatch` | A static relative box module target exists only under a different filename case | No |
| `box-export-not-found` | A named/renamed box attachment is absent from a complete module or package export set | Yes |
| `import-module-not-found` | A literal `.R`/`.r` `{import}` script module cannot be resolved | No |
| `import-module-case-mismatch` | A literal `{import}` script path exists only under a different filename case | No |
| `import-export-not-found` | An explicit `{import}` selection is absent from a complete package export snapshot | Yes |
| `assign-to-string-literal` | Assignment to a string literal or other almost-certainly-unintended target | Yes |
| `package-not-installed` | `library()` / `require()` / a static selective import of a package that is not installed (also fires on `pkg::member` / `pkg:::member` when `pkg` is not installed) | Yes |
| `package-outside-active-library` | A package exists in a vanilla system/user library but is unavailable in the active renv project library; child of `package-not-installed` | Yes |
| `namespace-member-not-found` | `pkg::member` where a *complete* package export set has no such exported object (never for `pkg:::member`) | Yes |
| `unused-suppression` | A `# raven: expect[...]` (or, under the global sweep, any suppression) that suppressed nothing — see below | No |

The opt-in style/lint rules contribute their own codes (`line-length`,
`object-name`, …); the full list is in [Linting](linting.md). Both the
kebab-case spelling and lintr's `snake_case` spelling are accepted in a
selector.

## Suppressing diagnostics

Use the `# raven:` directives (and their `@lsp-` aliases) to silence
diagnostics on a line, the next line, a block, a whole file, or an R Markdown
chunk — optionally narrowed to a `[code]`. The full syntax, including the
file/block forms, lives in [Directives → Ignore Directives](directives.md#ignore-directives).

Two flavors:

- **`# raven: ignore`** — silent. It never warns, even if it matched nothing.
- **`# raven: expect`** — asserts the suppression is used. If an `expect`
  directive matches no diagnostic, Raven emits an `unused-suppression` **hint**
  at the directive's line (like Rust's `#[expect]` / TypeScript's
  `@ts-expect-error`), so a now-stale directive can be deleted. Set
  [`raven.diagnostics.reportUnusedSuppressions`](configuration.md) to `true` to
  extend that unused check to *every* ignore directive (Pyright-style).

`unused-suppression` is **hint** severity, so it never gates
`raven check --max-severity error` by default.

## Diagnostic Categories

### Parse Errors

Raven surfaces parse errors from the tree-sitter R grammar whenever the document cannot be parsed as valid R. There's no per-rule severity knob, but the master switch `raven.diagnostics.enabled` suppresses them along with every other diagnostic. Where possible Raven provides a specific, actionable message rather than a generic "R code could not be parsed here":

| Message | Trigger |
|---|---|
| `Unclosed string literal` | An opening `"` or `'` has no matching closing delimiter |
| ``Consecutive pipe `\|>`: expected an expression before this operator.`` | Two pipe operators appear back-to-back without an intervening expression (`x \|> \|> y`) |
| ``Mismatched brackets: `(` opened here; close with `)` not `]`.`` | A bracket opened with `(`, `[`, or `[[` is closed with a non-matching bracket (`c(1, 2]`) |
| ``Unexpected `>`: R has no `=>` operator. For assignment use `<-`; for a pipeline use `\|>` (R 4.1+) or `%>%`.`` | A fat-arrow-style token `=>` appears where R expected an expression — common when porting code from JavaScript or other languages (`x => 1`) |
| `` Unclosed `(`: missing matching `)` `` / `` Unclosed `{`: missing matching `}` `` / `` Unclosed `[`: missing matching `]` `` / `` Unclosed `[[`: missing matching `]]` `` | A delimiter was opened but never closed (`library(`, `function() {`). The diagnostic is anchored on the opening delimiter, spanning to the end of meaningful content on that line. |
| `` Missing opening `{` `` / `` Missing opening `(` `` / `` Missing opening `[` `` / `` Missing opening `[[` `` | A closing delimiter appears with no matching opener (`}` at top level, `)` after a complete expression). A run of stray closers (`}}}`) reports a single diagnostic for the whole run. |
| `In R, 'else' must appear on the same line as the closing '}' of the if block` | `else` placed on its own line after `if (cond) { body }` — R treats the `if` as complete and the `else` becomes an unexpected token |
| ``'else' without a preceding 'if' body: in R, 'else' must follow `if (...) {...}` on the same line`` | `else` appears anywhere R would reject a bare `else` — e.g. `else { 1 }` at the top of a file, `else { … }` inside a `{ … }` block, `(else)`, `else |> f()`, or `else` used as a function argument / assignment RHS |
| ``R does not support chained comparisons: `a < b < c` is a parse error. Combine separate comparisons with `&&` for scalar conditions (`x > 0 && x < 1`) or `&` for vectorized expressions (`x > 0 & x < 1`).`` | A chained comparison such as `0 < x < 1` or `a == b == c` (operators `<`, `<=`, `>`, `>=`, `==`, `!=`) — a parse error in R that tree-sitter accepts silently. One diagnostic per chain, anchored on the second comparison operator (where R's own parse error points). If the chain itself contains a parse error (e.g. it is still being typed, `0 < x <`), Raven reports only that parse error — a complete chain next to a separate parse error is still reported. Carries the code `chained-comparison` (a `syntax-error` sub-kind). Explicitly parenthesized forms like `(0 < x) < 1` are valid R and are never flagged. |
| `R code could not be parsed here` | Tree-sitter detected a parse error that doesn't match any of the specific patterns above |

The `Mismatched brackets` message also covers wrong-closer typos where the user typed an unexpected closer immediately after an unclosed opener (e.g. `f(}` produces a single `` Mismatched brackets: `(` opened here; close with `)` not `}`. `` diagnostic rather than two separate ones).

### JAGS and Stan Parse Errors

Raven can report syntax errors in `.jags`, `.bugs`, `.bug`, and `.stan` files, both in the
editor and from `raven check`. Model-language diagnostics are opt-in: enable
`raven.diagnostics.jags` for JAGS/BUGS or
`raven.diagnostics.stan` for Stan. Both default to `"off"`, and the
`raven.diagnostics.enabled` master switch still takes precedence. Untitled
buffers whose language ID is `jags` or `stan` use the corresponding setting too.
JAGS checking is deliberately syntax-only. Stan also gets the conservative
undeclared-variable pass described below. Raven does not validate dimensions or
distribution signatures, type-check Stan, or run JAGS, `stanc`, R, or any
network process. Other syntactically well-formed semantic errors therefore
remain silent. Syntax findings use the same non-suppressible `syntax-error` code.

Disabling these diagnostic settings does not disable language registration,
parsing, completion, hover, navigation, syntax highlighting, or CLI target
discovery for model files. It only makes their native diagnostic result empty.

Native Stan and JAGS syntax findings share
`raven.diagnostics.maxSyntaxDiagnosticsPerFile` (default `500`). Raven removes
exact duplicate recovery findings, orders the unique findings by source
position, and then keeps the first configured number. Set it to `0` for
unlimited findings. The finite limit bounds retained collector memory and the
editor/CLI payload while traversal continues so cancellation remains
responsive. It does not apply to R diagnostics.

When Tree-sitter recovery provides clear structural evidence, Stan and JAGS
parse findings explain missing closers, missing openers, and mismatches for
`()`, `[]`, and `{}` using the same wording as R. An unclosed-delimiter range
starts at the opener and spans the meaningful code on that line; a stray or
wrong closer is highlighted directly. Stan and JAGS do not use R's special
`[[` / `]]` delimiter semantics.

Raven corroborates these explanations with one bounded, language-aware lexical
scan of the document. Delimiters inside Stan strings, comments, and complete
`#include` paths or inside JAGS comments and `%...%` operators do not affect
pairing; unfinished includes keep the generic fallback. A parser-inserted missing
closer can still be explained when a later closer
cleanly belongs to an enclosing construct. If structural and lexical evidence
disagree, a scan limit is reached, opaque source intersects the recovery, or
more than one delimiter fault is plausible, Raven keeps the generic
`Stan code could not be parsed here` or `JAGS code could not be parsed here`
message rather than guessing.

High-confidence program-structure recovery also gets actionable wording:

- Complete Stan declarations, function definitions, and statements at the top
  level are told that they must appear inside a program block such as
  `functions`, `data`, `parameters`, or `model`. Raven deliberately does not
  guess which one is semantically appropriate.
- Complete top-level JAGS relations and loops are told that they belong inside
  a `data` or `model` block.
- A uniquely missing required JAGS `model` block, an empty/comment-only model,
  and duplicate or out-of-order `var`, `data`, and `model` sections receive
  dedicated explanations anchored on the relevant section.

Incomplete fragments, misspelled block names, balanced malformed expressions,
and otherwise ambiguous recovery remain generic. All explanatory findings keep
ERROR severity and the non-suppressible `syntax-error` code. This native pass
applies to standalone Stan and JAGS documents only; fenced Stan/JAGS chunks in
R Markdown or Quarto are not checked as standalone programs.

JAGS findings come from Raven's in-tree clean-room Tree-sitter grammar. They
cover parser `ERROR` and required `MISSING` nodes, are emitted in stable source
order without duplicate recovery cascades.
The grammar and diagnostic corpus are checked against 806 committed outcomes
from the public JAGS 4.3.2 command-line parse phase. Raven applies this strict
JAGS dialect to `.jags`, `.bugs`, and `.bug`, case-insensitively. Treating those
suffixes as JAGS does not claim general OpenBUGS, WinBUGS, MultiBUGS, or NIMBLE
compatibility.

Full-line, recognized Raven directives are geometry-preserving Raven
extensions and do not create Stan parse errors. Stan's own `#include` remains
part of the Stan grammar; unknown `#` lines are diagnosed. Raven does not check
whether an include target exists. `.stanfunctions` helper files are not checked
as standalone Stan programs.

A recognized trailing `# raven: ignore` / `# @lsp-ignore` marker is masked from
the Stan parser while the code before it remains intact. This masking never
hides a real syntax defect in that code, and syntax findings themselves remain
non-suppressible. For Stan syntax and semantic diagnostic collection,
marker-shaped text inside a string, `//` comment, or `/* ... */` comment never
acts as diagnostic metadata or suppression, including when the string or block
comment spans lines.

### Stan Undeclared Variables

When `raven.diagnostics.stan` is `"on"`, a structurally complete Stan
program gets clear undeclared-variable findings with the existing
`undefined-variable` code and configured
`raven.diagnostics.undefinedVariableSeverity` (warning by default). Setting
that severity to `"off"` suppresses only this semantic pass; Stan syntax
findings remain enabled. The native
pass models declaration order; data → transformed data → parameters →
transformed parameters → model → generated quantities visibility; function
parameters and locals; statement-block locals; and `for` loop variables. It
checks references in dimensions, constraints, initializers, and statements.
Function and distribution names are a separate namespace, so calls, sampling
distributions, prototypes, and a user function passed as a higher-order
argument are not mistaken for variables.

Raven deliberately does **not** diagnose whether a called function or sampling
distribution exists; that requires compiler overload/type resolution. A name
used as a higher-order *value* is a variable expression, however, so an unknown
value such as `reduce_sum(missing_fun, ...)` is diagnosed.

Declaration order follows the pinned compiler: shared type dimensions and
constraints are resolved before any declarator name; the current local name is
visible in its own initializer; and comma declarators become visible from left
to right, so `real a = 1, b = a` is clean while `real a = b, b = 1` reports
`b`. Branch, loop, `while`, nested-block, and `profile` locals are visible only
inside their lexical statement scope.

The completeness boundary is syntactic: the `program` root must contain at
least one direct real `functions`, `data`, `transformed data`, `parameters`,
`transformed parameters`, `model`, or `generated quantities` block. A file with
no such block receives syntax diagnostics only. This keeps standalone assembler
fragments quiet, including files organized by comments such as `//--- data` or
`//--- model`, because other fragments may supply their declarations. Missing
one optional block does *not* suppress the pass: `model { target += from_r; }`
still reports `from_r` because data supplied by R must be declared in Stan.

Raven never descends into `ERROR` or `MISSING` recovery subtrees for semantic
roles. If recovery could have hidden a declaration, unresolved references
visible through that lexical scope fail closed, while sound independent scopes
and blocks continue. If any `#include` occurs anywhere in a file, Raven
suppresses all Stan undeclared-variable findings for that file—whether the
include appears before a program block, after one, or inside a nested lexical
scope. An include can insert declarations, and Raven deliberately does not
implement preprocessing or include resolution.

Local `# raven: var` / `# raven: func` declarations and `# raven: ignore` /
`# raven: ignore-next` suppressions apply to Stan undeclared-variable findings
in both the editor and CLI. This is a narrow host-integration escape hatch; it
does not put Stan in R's scope/cross-file pipeline. A value supplied by R still
reports unless it is declared in Stan or explicitly covered by one of these
directives. Stan syntax findings remain non-suppressible.

Stan semantic findings have a fixed, source-ordered limit of 500 exact unique
diagnostics per file. This bound is separate from
`maxSyntaxDiagnosticsPerFile`; changing the syntax setting cannot change Stan
undeclared-variable output. Traversal continues after saturation so cancellation
remains responsive.

### Undefined Variables

| Diagnostic | Default Severity | Trigger |
|---|---|---|
| Undefined variable | warning | Symbol used that is not defined in scope (local, cross-file, or package) |

Raven checks whether each symbol reference has a visible definition — either in the current file (above the cursor), in a sourced parent/child file (respecting position), or in a loaded package. If not found, it reports an undefined variable diagnostic at the configured severity (default `warning`; see `raven.diagnostics.undefinedVariableSeverity` in [Configuration](configuration.md)).

If the symbol is defined later in the same file at top level, the message also reports that line — e.g. `total_count is used before it is defined (defined on line 7)`. R does not hoist top-level bindings, so the use is still flagged, but the annotation makes it easy to distinguish a forward reference from a missing import or typo.

**What suppresses it:**
- A definition above the usage in the same file
- A definition in a sourced file (via `source()` or directives)
- A package export from a loaded `library()`
- A namespace alias or attached member from a static `box::use()` import
- A declaration directive (`# raven: var`, `# raven: func`)
- A `# raven: ignore` on the line

Raven also recognizes a few call forms that bind a name at runtime, so the bound name resolves without a directive:
- `assign("x", ...)` and write/append-mode `textConnection("x", "w")` bind `x`.
- `load("foo.rda")` binds the conventional object name `foo`.
- `data(foo, bar)` binds each named dataset (`foo`, `bar`) from the call onward, whether given as a bare name or a string; named arguments such as `package=` are ignored.
- `exists("apple")` declares `apple` from the line after the call onward — equivalent to a `# raven: var apple` directive. A user who probes `exists("apple")` is asserting they know the name, so the idiomatic guard `if (!exists("apple")) apple <- default` and any later use of `apple` resolve. The name must be a non-empty string literal (`exists(varname)` and `exists("")` declare nothing); a use *before* the `exists()` call is still flagged, mirroring the directive's next-line visibility.
- `setGeneric("g", ...)` / `setGroupGeneric("g", ...)` bind the generic `g`, so other files in the same package that call it resolve (common in S4-heavy packages like Matrix, whose generics live in a single `R/` file).
- zeallot's `c(a, b) %<-% value` binds each bare symbol on the left (nested `c(...)` destructuring included), and rlang's `x %<~% value` binds its bare-symbol left-hand side; both take effect from the statement onward, like `<-`. Other custom `%op%` operators do not bind.
- Inside an S4 method body (`setMethod("Ops"|"Math"|"Summary"|…, ...)`) or an S3 method bound to a `generic.class` name, the dispatcher-injected specials `.Generic`, `.Method`, and `.Class` are in scope.
- Inside an R6 class method body — a function value within the `public`, `private`, or `active` list arguments of `R6Class(...)` or `R6::R6Class(...)`, or within an unnamed `list(...)` passed positionally among the call's first four arguments (`R6Class(classname, public, private, active)`) — the pronouns `self`, `private`, and `super` are in scope (R6 injects them at construction time). A top-level definition of `R6Class` in the same file shadows the package function and disables this treatment.
- Base R's implicit search-path globals resolve as bare names: `.Autoloaded` (the startup `Autoloads` environment) and `.Random.seed` after a visible prior `set.seed()` call.
- `delayedAssign("x", expr)` binds the promise `x` from the call onward (at top level it becomes a package-internal symbol; inside a function body it stays local, matching R's default `assign.env`). The name must be a string literal.
- rlang's `env_bind_active(current_env(), a = ..., b = ...)` and `env_bind_lazy(current_env(), ...)` bind each *named* argument (`a`, `b`) in the enclosing environment. Only the `current_env()` form is modeled — binding into some other environment is not assumed local.
- `utils::globalVariables(c("a", "b"))` (R's own mechanism for declaring names bound at runtime, used to silence `R CMD check`) makes each listed name resolve package-wide. The bare `.` pronoun is deliberately **not** honored this way — Raven resolves `.` precisely by context (see below), and accepting a blanket `globalVariables(".")` would mask genuine `.`-misuse bugs.
- An active binding installed in a package's `.onLoad`/`.onAttach` hook via `makeActiveBinding("name", fn, env)` — when `env` is the package namespace (e.g. `asNamespace(...)`, `topenv(...)`, or `environment(<a package function>)`) — contributes `name` to the package's internal scope, alongside the existing `assign("name", ..., envir = ns)` and `ns$name <- ...` recognition.

When you are developing an R package, a script anywhere in its source tree (`inst/`, `tools/`, `data-raw/`, `debug/`, …) that calls `devtools::load_all()` / `pkgload::load_all()` (or a bare `load_all()`) is modeled as attaching the package under development: the package's own internal, exported, sysdata, and `.onLoad`/`.onAttach`-bound symbols become visible in that file, mirroring what `load_all()` does at runtime. Genuinely-undefined names not provided by the package still flag.

Windows-only base functions (`shell.exec`, `Sys.junction`, `readRegistry`, the `win*`/clipboard helpers, …) are recognized as builtins even though Raven's builtin list is generated on non-Windows hosts, so platform-guarded code does not draw a false positive.

**Never checked:** Symbols on the RHS of `$` or `@` (member access), function parameters, named-argument labels, and formula variables (`y ~ x`).

#### targets and tarchetypes target names

Raven treats static target names as a namespace separate from ordinary R
bindings. Direct declarations from `tar_target()` and supported tarchetypes
factories, named `tar_plan()` commands, and the bounded literal `tar_map()`
subset can satisfy explicit `tar_read()` / `tar_load()` references in the
pipeline and in R chunks of a document connected by `tar_render()`,
`tar_knit()`, or single-document `tar_quarto()`.

An unresolved reference reports, for example,
`Target 'model' is not declared in the connected targets pipeline`. It carries
`undefined-variable`, uses `raven.diagnostics.undefinedVariableSeverity`, and
honors the normal line/file/chunk suppression forms. A missing literal report
file instead carries the non-suppressible `unresolved-source-path` code and uses
the configured missing-file severity. See
[Cross-file awareness — tarchetypes target factories and report documents](cross-file.md#tarchetypes-target-factories-and-report-documents)
for the static grammar and dynamic non-goals.

#### Call arguments and bracket indices

Raven checks identifiers inside ordinary call arguments and `[` / `[[` indices by default, so real bugs like `paste(undefined_var)`, `df[undefined_var, ]`, and `lst[[typo]]` are flagged. To avoid false positives in [non-standard-evaluation](non-standard-evaluation.md) (NSE) code, it resolves each call's callee to its source and applies a per-call argument policy:

- A standard-eval callee (e.g. `paste`, `print`, `stats::filter`) has its arguments checked.
- A callee with a known NSE policy suppresses only the captured / data-masked / tidy-selected arguments. For example `with(df, col + 1)` and `dplyr::filter(df, col)` still check `df` but suppress `col`; `substitute(expr, env)` suppresses `expr` but checks `env`; `aes(x, y)` and the rlang plural capture helpers suppress every argument.
- Local definitions shadow packages, so `filter <- function(x) x; filter(undefined_var)` checks `undefined_var` (it is not `dplyr::filter`). For a local helper, Raven also infers which formals are captured — if the body calls `substitute()` / `enquo()` / `enexpr()` / `ensym()` on a formal, only the argument bound to that formal is suppressed.
- Non-literal local rebindings shadow packages the same way, mirroring R's call-position function lookup. A simple qualified alias keeps its target's policy: `filter <- stats::filter; filter(df, typo)` checks `typo` (standard-eval), while `filter <- dplyr::filter; filter(df, col)` still suppresses the data-masked `col`. An opaque callable whose policy can't be determined statically (`filter <- get_filter()`, or a bare identifier alias) is suppressed conservatively — its whole call is treated as non-standard rather than inheriting the package verb's policy. An obvious non-function literal (`filter <- 1`) is *not* a shadow: R's lookup skips it in call position, so a later `filter(df, col)` still resolves to the package verb. The last top-level binding of a name wins.
- Tidy-eval **wrapper functions** propagate the data-mask context: a local function that embraces a formal into a call argument (`my_filter <- function(data, cond) filter(data, {{ cond }})`) is itself data-masking, so `my_filter(df, x > 2)` does not flag `x` (while `df` stays checked). The same applies to a body that defuses its `...` through a plural capture helper (`enquos(...)` and friends) or forwards `...` directly into a data-mask position of a covered verb (`function(...) join_by(...)`); the dots-forwarding check resolves the inner verb one level deep through the built-in policy tables, and a local redefinition of the verb disables it. The default expression of a defused formal (e.g. a `values_from = value` pivot-style signature) is quoted rather than evaluated, so it is exempt too. Propagation requires these verified forwarding shapes — a local helper that doesn't forward (`f <- function(x) x`) keeps its arguments checked, exactly as before.
- An **unresolved** callee (not local, not a builtin, not a known export of an in-play package) suppresses its arguments rather than guess — `unknown_fn(typo)` still flags `unknown_fn`, but not `typo`.
- When you are developing an R package, its own exported NSE verbs keep their policy inside the package's own files (any of its `.R`/`.Rmd`/`.Rmarkdown`/`.qmd` source files — `R/`, `tests/`, vignettes, `man/` examples, `inst/`, `data-raw/`, and so on) — even though no `library()` call attaches the package under development. For example, dplyr's own test suite calling `filter(df, x > 1)` does not flag `x`. A genuinely-undefined symbol outside a masked position still flags as usual.

For `[`, base subscripting is standard-eval, so indices are checked unless the indexed object is data.table-like: a known `data.table()` / `as.data.table()` / `fread()` object, or an unresolved object when data.table is detectably in play (a `library(data.table)` call, a `data.table::` reference, or a package `Imports:`/`importFrom`). `[[` is always checked — `DT[[x]]` references `x` as a real variable.

data.table's by-reference converters are also recognized: a statement-level `setDT(x)` flips `x` to a data.table from that line onward (so a later `x[, newcol := val]` no longer flags `newcol`/`val`), `setDF(x)` flips it back to a plain data.frame, and `setattr(x, "class", ...)` sets the class explicitly. The transition is positional — a `[` *above* the converter keeps the object's prior classification.

#### The magrittr dot, pipe placeholders, and exposition

Raven recognizes the special placeholders used by magrittr and dplyr so they are never flagged as undefined, while keeping them tightly scoped so genuine bugs still surface:

- The magrittr dot `.` on the **right** of a `%>%` or `%<>%` pipe (`df %>% nrow(.)`, `df %<>% { .$x }`) is the piped value.
- The **leading** `.` of a magrittr functional sequence (`f <- . %>% step1() %>% step2()`) is the anonymous function's formal. (Only `%>%` builds functional sequences; `%<>%` cannot head one, since it assigns its result back to the left-hand side.)
- The compound-assignment pipe `%<>%` (`x %<>% f()`, defined as `x <- x %>% f()`) feeds its left-hand side into the right-hand call's first argument exactly like `%>%`, so column arguments resolve identically — `df %<>% group_by(ring)` treats `ring` as a column, not an undefined variable.
- `.` inside `dplyr::do(...)` (the current-group data frame) and inside the scoped-verb predicates `all_vars(...)` / `any_vars(...)` (the column under evaluation).
- Every free identifier on the right of the exposition operator `lhs %$% rhs` (semantically `with(lhs, rhs)`), since `rhs` is evaluated in a data mask of `lhs` — e.g. `mtcars %$% cor(cyl, am)` resolves `cyl` and `am` as columns while still checking `mtcars`.
- The native-pipe placeholder `_` on the right of a `|>` pipe (R 4.2+).

These are scoped to the magrittr/`do()`/`all_vars()` contexts only — a bare `.` used as a **native** `|>` placeholder (a common `%>%`→`|>` migration mistake, e.g. `x |> f(.)`) is *not* one of these forms and stays flagged.

#### Shiny deferred-expression scopes

Shiny's deferred-expression helpers — `reactive()`, `observe()`, `observeEvent()`, `eventReactive()`, and the `render*()` family — evaluate their `{ ... }` body later, in a child lexical environment. Raven models each such body as a nested scope rather than suppressing it: identifiers inside are real references and are checked, outer bindings (including server-function parameters like `input`, `output`, `session`) stay visible inside, and a definition made inside the body does not leak back into the surrounding function. So in `server <- function(input, output, session) { output$plot <- renderPlot({ plot(input$x, typo_var) }) }`, only `typo_var` is flagged. This applies to `shiny::`-qualified calls and to bare calls when Shiny is in play (a `library(shiny)` / `require(shiny)` call), even if Shiny export metadata is unavailable; a top-level definition that shadows the name (`renderPlot <- function(...) ...`) disables the Shiny treatment. `isolate({ ... })` evaluates in the current environment, so it is deliberately *not* modeled as a nested scope.

The recognition is heuristic and deliberately errs toward *checking but not isolating* (a missed diagnostic, never a false positive), so a few boundaries apply: the `render*()` family is matched by naming convention (a `render` prefix plus an upper-case letter), which covers the whole render ecosystem (`renderPlotly`, `renderLeaflet`, …) but could also match a same-named eager function from an unrelated package while Shiny is loaded; only the **positional** `{ ... }` body is isolated, so a body passed by its formal name (`renderPlot(expr = { ... })`) — and a block passed to a named eager parameter such as `outputArgs = { ... }` — is not; isolation of *bare* calls triggers only from a within-file `library(shiny)` / `require(shiny)` or a `shiny::` qualifier, so in a package file that pulls Shiny in solely through DESCRIPTION `Imports:` / `importFrom(shiny, …)` the body is still checked but may not be isolated; and only **top-level** definitions are honored as shadows, so a helper redefined *inside* a function (a rare `renderPlot <- function(...) ...` nested in the server) is still treated as Shiny's.

#### foreach iterator scopes

`foreach(...) %do% expr` and `foreach(...) %dopar% expr` — plus the drop-in execution operators other packages register for `foreach()`, doRNG's `%dorng%` and doFuture's `%dofuture%` — expose the named, non-dot arguments of the `foreach()` call as iterator variables inside `expr`. So in `foreach(i = 1:10, j = ys) %do% { i + j + typo }`, `i` and `j` resolve and only `typo` is flagged. Dot-prefixed options such as `.combine`, `.packages`, `.export`, `.noexport`, and `.verbose` are control arguments, not iterators. Raven still checks the iterator value expressions (`foreach(i = missing_vec) %do% i` flags `missing_vec`), the control argument values (`foreach(i = 1:3, .combine = missing_combine) %do% i` flags `missing_combine`), and ordinary symbols in the body. Like the Shiny scopes above, the body is modeled as a nested scope: outer bindings stay visible inside it, and a definition made inside it does not leak back out. Assignments inside an iterator value expression are isolated the same way — foreach evaluates those arguments in its own environment, so in `foreach(i = { x <- 1; 1:3 }) %do% i` the `x` is visible neither in the body nor after the loop (real R leaves `exists("x")` `FALSE`). Recognition is syntax-based — it matches bare `foreach(...)` and namespace-qualified `foreach::foreach(...)` / `foreach:::foreach(...)`, and does not require the foreach package metadata.

Nested compositions joined by the `%:%` operator and `when(...)` filters are modeled too: in `foreach(i = 1:3) %:% when(i %% 2 == 0) %:% foreach(j = 1:3) %do% i + j`, every `foreach()` in the chain contributes iterators. Binding is left-to-right, matching R: each iterator is visible from its own `foreach()` call onward — inside later `when(...)` filters, inside later iterator value expressions (so an inner `foreach(j = seq_len(i))` resolves the outer `i`), and in the executed body — but not in anything to its left. So a reverse reference like `foreach(i = seq_len(j)) %:% foreach(j = 1:3)` still flags `j`, exactly as R raises "object 'j' not found". When two levels share an iterator name, the body resolves it to the innermost (rightmost) `foreach`, and a `when(...)` filter resolves it to the nearest `foreach` on its left. `when(...)` is a filter, not an iterator source, so it contributes no iterators of its own.

Two opt-out settings turn off this descent and restore blanket suppression for highly dynamic or data.table-heavy code (the `undefinedVariableSeverity` master switch still controls severity):

- `raven.diagnostics.undefinedVariableInCallArguments` (default `true`)
- `raven.diagnostics.undefinedVariableInBracketIndices` (default `true`)

**Uncatalogued NSE helpers:** When a function is not in Raven's policy table, use [`# raven: nse`](non-standard-evaluation.md#when-raven-needs-a-hint) to declare its argument-evaluation policy. This is preferable to a blanket ignore because it records a reusable per-function policy — whole-call or per-formal — and keeps checking the non-captured arguments. It is position-aware (applies to calls after the directive line), and the most recent declaration for a function wins. A declaration also **propagates across the `source()` graph** — it governs matching call sites in every connected file, in both directions and transitively (intentionally coarse and file-level), so you can declare a helper's contract once near its `library()`/definition/setup and suppress the false positives at call sites elsewhere; see [cross-file NSE directive propagation](cross-file.md#nse-directive-propagation). When Raven reports an `undefined-variable` inside a call argument whose callee is not a high-confidence standard-eval function (a builtin or base-package export) and is not already governed by an `# raven: nse` directive (own or propagated), Raven can suggest the appropriate `# raven: nse` form — but it deliberately keeps that suggestion **out of the diagnostic message** (a per-finding "declare it with…" suffix is verbose and easily misread as Raven asserting the call *is* NSE). In the editor there is no per-finding hint or code action — the editor diagnostic stays the bare `undefined-variable` message; the NSE suggestion is `raven check` text-footer only, aggregated into one [reframed footer](cli.md#nse-discoverability-footer) in the human-readable `text` output (the `json`/`sarif` formats carry no NSE prose at all). The suggestion is **reserved for package functions Raven cannot analyze**: a bare call to a function you defined in the same file (a top-level `name <- function(...)`) gets no NSE suggestion — Raven already read that function's body and knows whether any formal is NSE-captured, so the plain `undefined-variable` stands on its own. A qualified `pkg::fn(...)` call always keeps the suggestion (it invokes the package export, not a same-named local definition).

**Limitations:** The NSE policy table covers the common, slow-moving surface (base/utils metaprogramming and object-name helpers, default-attached `stats` model-fitting `subset`/`weights` data-masking, `dplyr`/`tidyr` data-masking and tidy-select verbs including attached Bioconductor tidy-omics generics from `plyranges`, `tidySummarizedExperiment`, and `tidySingleCellExperiment`, `tibble`/`targets` constructors and target-name helpers (including the `names` selector of `tar_make()`, `tar_make_future()`, and `tar_make_clustermq()`), common `tarchetypes` target factories plus precise `tar_map()` / `tar_plan()` / report-factory policies, `gt`/`gtsummary` table-column selectors, `recipes` step/role column captures, ggplot2 mapping helpers (`aes`/`vars`/`qplot`), `tidyr::gather` key/value outputs, `tidytext` (`unnest_tokens`/`bind_tf_idf`), `modelr::data_grid`, `drake::readd`, rlang capture helpers, the `plyr::.()` quoting helper (so `ddply(df, .(col), ...)` does not flag `col`) and the `*ply` split-apply verbs (the trailing `...` are data-masked — and so suppressed — only when `.fun` is a data-masking verb such as `summarise`/`mutate`/`transform`, e.g. `ddply(df, .(g), summarise, n = length(unique(team)))`; with an ordinary `.fun` the `...` stay checked), `survival::tmerge` time-dependent terms, a few DSLs) but is not exhaustive, and source resolution depends on package-metadata coverage, so an uncatalogued NSE helper can still produce a false positive. In data.table projects, an unresolved non-data.table object such as `df[typo, ]` may be silently skipped. Use `# raven: nse` as the targeted escape hatch for an uncatalogued NSE helper; fall back to `# nolint`, `# raven: ignore`, or the opt-out settings for broader suppression.

### Package Diagnostics

| Diagnostic | Default Severity | Trigger |
|---|---|---|
| Missing package | warning | `library()`/`require()`, or `pkg::member` / `pkg:::member`, references a package unavailable in the active library paths |
| Package outside active renv library | warning | A referenced package is installed in a vanilla R library that the active renv project removed from `.libPaths()` |
| Namespace member not found | warning | `pkg::member` where `pkg`'s *complete* export set has no such exported object (never for `pkg:::member`) |

### Package names vs. install status

Raven can resolve a package's **export names** from three sources, consulted in order — installed packages (Tier 1), a committed `.raven/packages.json` (Tier 2), or Raven's broad `names.db` metadata (Tier 3) when available — so symbols from `library(pkg)` resolve even when the package isn't installed (for example in CI with no R). That metadata isn't bundled with the binary; install it with `raven packages update` for broad CRAN/Bioconductor coverage. See [Package database](package-database.md). Crucially, knowing a package's exports is kept **separate** from knowing whether it is installed:

- **Export resolution** (suppresses undefined-variable noise) uses all three tiers, in every mode.
- **Install status** (drives the *missing-package* diagnostic) is **Tier 1 only** — it reflects what is actually present in the local library paths, and never the package symbol database. A database that knows `dplyr`'s exports does **not** make `dplyr` count as installed.

#### Per-mode behavior

| | Export resolution | Missing-package ("not installed") |
|---|---|---|
| **Language server (interactive)** | tiers 1→2→3 (prevents an undefined-variable storm when R is absent and Tier 2 metadata or the Tier 3 database covers the package) | Fires when install state is known and the package is absent — regardless of the database. Export metadata stops the symbol storm when coverage exists but never masks the "install this dependency" nudge. |
| **`raven check` (CI)** | tiers 1→2→3 | Generic absence is **suppressed by default** (CI deliberately omits installation). The actionable `package-outside-active-library` subtype remains enabled. Re-enable generic absence with [`--report-uninstalled`](cli.md#missing-package-reporting-in-ci). |

When enabled, `--report-uninstalled` reports `library()` calls **not present in the local library paths** — *not* relative to the Tier 2/Tier 3 export metadata. Reach for it when a `library(X)` call must really succeed at runtime: CI that installs packages (e.g. `renv::restore()`) and wants to catch failures, or CI that **actually runs your R scripts** after `raven check` (e.g. R-package development), where an uninstalled package is a real error. Gate-only CI that never executes the scripts wants the default.

When Raven successfully activates an explicit workspace's `renv/activate.R`, it
also compares the vanilla and active library paths. If a referenced package
exists only in a path removed by activation, Raven reports
`package-outside-active-library` and suggests running `renv::hydrate()` to add
installed packages to the project library. Raven reports this project-level
setup problem at most once per package per document, even when that document
contains many qualified references. Raven may use explicitly declared exports
from that outside installation only to suppress undefined-variable cascades
after a true `library()` / `require()` attachment. This diagnostic-only evidence never makes
the package available for completion, `system.file()`, or package loading.
Packages installed after startup are recognized when the library is rebuilt
(for example with `raven.refreshPackages`); changes made only in an outside
vanilla library are picked up by the next refresh rather than watched
continuously. Suppressing
`package-not-installed` also suppresses this child code. Multi-root editor
sessions fail closed to no subtype because package routing has no unambiguous
single-project renv identity there.

#### Accepted gap

With missing-package off by default in `raven check`, a genuine typo such as `library(dpylr)` — unknown to every tier — is **silent** unless `--report-uninstalled` is passed. This is documented behavior: the default avoids nagging about known-but-uninstalled dependencies in CI. Pass the flag (e.g. in a pipeline that runs `renv::restore()`) to catch packages that failed to install. The language server still flags such a call interactively whenever install state is known.

### Namespace member references (`pkg::member`)

Writing `pkg::member` (or `pkg:::member`) makes Raven aware of `pkg` even without a `library(pkg)` call: it **warms `pkg`'s metadata** into the package cache (export names, datasets) so completion and hover work, but it deliberately does **not** attach `pkg` to bare-name scope — only `library()`/`require()` do that. A `pkg:::member` (internal access) warms the package too but yields no completions and is never member-validated.

The `namespace-member-not-found` diagnostic (`raven.packages.namespaceMemberSeverity`, default `warning`) is **exports-authoritative**: it reports `pkg::member` only when Raven holds a *complete* export set for `pkg` and that set has no such exported object. The message reads `'member' is not an exported object of package 'pkg'`. The completeness signal is per source:

- **Complete** — a static NAMESPACE parse without `exportPattern()`, R's `getNamespaceExports()`, the committed package database, or the embedded base table. Absence is conclusive, so the diagnostic fires.
- **Partial** — exports recovered only from the `INDEX` file (documented topics, not the full export list). The diagnostic stays **silent** to avoid false positives.
- **Unknown** — the package has not been warmed yet (or could not be resolved without R). Silent. Because the member authority is synchronous and never spawns R, the diagnostic appears only **after** background warming republishes diagnostics — including across other open documents that reference the same `pkg::` (issue #503).

Data objects (a package's `lazy_data`, and base-package datasets such as `datasets::mtcars`) are **positive-only**: they can confirm a member is present but never prove one absent, so a `pkg::dataset` reference is never flagged as missing. `pkg:::member` is never validated at all. A `::`-accurate data-member authority is tracked as a follow-up (issue #505). Suppress an individual false positive with `# raven: ignore[namespace-member-not-found]`.

### Cross-File Diagnostics

| Diagnostic | Default Severity | Trigger |
|---|---|---|
| Missing file | warning | `source()` or directive references a file that doesn't exist |
| Source path case mismatch | information / warning (host-derived) | `source()`, forward directive, or backward directive resolves only by a case difference from the real filename |
| Circular dependency | error | Two files source each other (directly or transitively) |
| Max chain depth exceeded | warning | Source chain exceeds configured maximum depth |
| Out-of-scope symbol | warning | Symbol from a sourced file used before the `source()` call |
| Redundant directive | hint | `# raven: source` directive for a file already brought in by an earlier `source()` call |

These dependency-graph diagnostics are **not** suppressible with `# raven: ignore`; turn each off via its severity setting (see [Configuration](configuration.md)). The out-of-scope-symbol diagnostic is the exception — it honors `# raven: ignore` / `# raven: ignore-next` on the offending usage line.

An exact optional source guard — `if (file.exists("path")) source("path")` with the same plain literal path — does not produce `unresolved-source-path` or `source-path-case-mismatch` when the guarded file is absent. The source still enters the dependency graph and lends symbols when it exists. Existing guarded targets outside the workspace remain diagnosable.

#### Source path case mismatch

When a path differs from the real on-disk filename **only by case** — e.g. `source("scripts/templates.r")` when the file is `templates.R` — Raven still resolves the file into the source graph (so the symbols it defines stay visible and you don't get a flood of false `undefined-variable` warnings), and reports the problem **once, at the path's line**, as `source-path-case-mismatch`. This covers both forward references — a `source()` call or forward directive (`# raven: source` / `# raven: run` / `# raven: include`) — **and** backward directives (`# raven: sourced-by` / `# raven: run-by` / `# raven: included-by`). Its severity is host-derived under the default `"auto"` policy:

- **Case-insensitive filesystem** (macOS, typical Windows): **information**. The path works here, but it is a portability hazard — it will not be found on a case-sensitive filesystem such as Linux CI.
- **Case-sensitive filesystem** (Linux/CI): **warning**. For a forward `source()`, R itself would error at `source()` time; Raven resolves the single case-insensitive match anyway so the one actionable warning isn't buried under downstream noise.

For a **backward directive** the message differs: R never *executes* a `# raven: sourced-by` (it is a Raven-only annotation), so the diagnostic does not claim R would error — it reports that the directive's casing doesn't match the file on disk and asks you to fix it. Raven still resolves the relationship to the real file in both filesystem regimes (so no cascade), exactly like the forward case.

Because the severity reflects the host filesystem, the same code can surface as information on a developer's Mac and as a warning in Linux CI — which is intended: CI is exactly where the case-sensitive failure bites. Pin a fixed level or turn it off with [`raven.crossFile.caseMismatchSeverity`](configuration.md) (default `"auto"`). When two on-disk files match the path case-insensitively (only possible on a case-sensitive filesystem) the path is ambiguous, so Raven leaves it unresolved and reports `unresolved-source-path` instead. The match is ASCII-only.

### Assignment Targets

Always on whenever diagnostics are enabled; not configurable per rule. Applies to every assignment operator: `<-`, `<<-`, `=`, `->`, `->>`. For right-arrow operators the target is the right-hand side; for the others it's the left-hand side. Both tiers honor `# raven: ignore` / `# raven: ignore-next` on the affected line.

| Diagnostic | Default Severity | Trigger |
|---|---|---|
| Invalid assignment target | error | Target is a value R rejects outright: a literal (`TRUE`, `FALSE`, `NULL`, any `NA*`, `Inf`, `NaN`, a number including signed `-1`/`+1.5`) or a reserved word (`else`, `in`, `next`, `break`) |
| Suspicious assignment target | warning | Target is something R technically accepts, but the binding is almost always unintended: a string literal (`"foo" <- 1` — R binds the value to a variable named `foo`) or a dots argument (`... <- 1`, `..1 <- 1` — R creates a binding the standard `...` / `..N` accessors can't reach) |

**Not flagged:**
- `"[.Surv" <- function(x, i) …`, `"coef<-.varPower" <- function(…) …`, `"area" <- function(r) …` — a quoted-string target whose assigned value is a function definition is idiomatic R (S3/replacement/operator methods for syntactically invalid names, and old-S-style definitions), semantically identical to the backtick form. Exempt for every assignment spelling, including chained definitions (`"coef<-" <- "coefficients<-" <- function(…) …`) and `.Primitive(…)` values; any other non-function value on a string target is still flagged.
- `"iris" <- <value>` at top level of a package's `data/*.R` file — the canonical form `data()` expects for registering dataset objects (used throughout R-core's `datasets` package). String-target assignments nested inside functions in those files are still flagged.
- `T <- FALSE` / `F <- TRUE` — `T` and `F` are ordinary bindings that default to `TRUE`/`FALSE`; R accepts the assignment. Use the [`T` / `F` symbol](#style-lints) style lint if you want these reported.
- `f(name = value)` — named-argument syntax inside a call, not assignment.
- `function(x = TRUE)` — default values in formal parameters, not assignment.
- `if <- 1`, `for <- 1`, `while <- 1`, `function <- 1`, `repeat <- 1` — tree-sitter reports these as syntax errors directly, so the same code surfaces only one diagnostic.

### Semantic Warnings

Always-on diagnostics that flag likely-wrong code — not style preferences. Active as long as `raven.diagnostics.enabled` is true. Configurable severity via `raven.diagnostics.*`; honor `# raven: ignore` / `# raven: ignore-next` and `# nolint`.

| Diagnostic | Default Severity | Trigger |
|---|---|---|
| Mixed logical operators | warning | `\|` / `\|\|` whose immediate operand is a bare `&` / `&&` (no parentheses), e.g. `a & b \| c`. `&` binds more tightly than `\|` in R, making the grouping easy to mis-read. Stops at call/subset boundaries |
| Condition assignment | warning | `=` used as a binary operator directly inside an `if` / `while` condition (`if (x = 1)`). R rejects this as a syntax error at runtime; tree-sitter-r accepts it silently. Stops at call, parenthesized-expression, and braced-expression boundaries |

**Suppression:** `# raven: ignore` on the line, `# raven: ignore-next` on the line above, or `# nolint` (with optional rule names `mixed_logical`, `condition_assignment`).

**Settings:** `raven.diagnostics.mixedLogicalSeverity` (default `"warning"`), `raven.diagnostics.conditionAssignmentSeverity` (default `"warning"`).

### Style Lints

Native style diagnostics (18 of [`lintr`](https://lintr.r-lib.org/)'s default rules). Implemented in Rust against the tree-sitter AST — no R or `lintr` install required. Gated by the tri-state `raven.linting.enabled` switch (default `"auto"` — on when a `.lintr` or `raven.toml` opts in, except the literal home-directory `~/.lintr` is ignored unless the VS Code/LSP-client setting `raven.linting.readHomeLintr` is true or the CLI receives `--config ~/.lintr`, and a `.lintr` is ignored when REditorSupport's `lintr` path is live or you're in Positron); tune per rule via the `raven.linting.*` severities. All style lint rules default to severity `information`, matching REditorSupport `languageserver`'s mapping for `lintr` style findings. For a user-facing guide — master-switch matrix, quick-start config, `.lintr` migration, gaps vs `lintr`, and how to run `lintr` alongside Raven — see [Linting](linting.md).

| Diagnostic | Default Severity | Trigger |
|---|---|---|
| Line length | information | Line exceeds `raven.linting.lineLength` (default 80 characters, matching `lintr`'s `nchar()`) |
| Trailing whitespace | information | Spaces or tabs at end of line (whitespace inside a multi-line string is part of its value and exempt) |
| Tab character | information | Tab used for indentation (tabs in comments, strings, or between tokens are exempt, matching `lintr::whitespace_linter`) |
| Trailing blank lines | information | Blank lines at end of file, or missing final newline |
| Assignment operator | information | Top-level assignment uses an operator other than the preferred one (`<-` by default; configurable via `raven.linting.assignmentOperator`) |
| Object name | information | Function, variable, or argument name doesn't match the configured named styles or regexes (`snake_case` + `symbols` by default, matching `lintr`; configurable per kind via `raven.linting.objectNameStyle*` and `raven.linting.objectNameRegexes*`) |
| Object length | information | Identifier name exceeds `raven.linting.objectLength` characters (default 30; S3 methods measure only the part after a known generic prefix) |
| Infix spaces | information | Missing space around one of `lintr`'s low-precedence operators (`a+b`, `x<-1`, `a%>%b`, `f(x=1)`); high-precedence operators (`^`, `:`, `::`, `$`, `@`) and unary forms are never linted |
| Commented code | information | A comment (standalone block or end-of-line) whose body parses as R and contains a call, assignment, or operator (`# foo(bar)`, `# x <- 1 + 2`) |
| Quotes | information | String literal (raw strings included) not using the preferred delimiter (`raven.linting.stringDelimiter`; default `"`); literals containing the preferred quote character are exempt |
| Commas | information | Whitespace before `,` (`a , b`) or missing whitespace after `,` (`c(1,2)`). Newline after comma is fine |
| `T` / `F` symbol | information | Bare `T` / `F` used in reference position (use `TRUE` / `FALSE`), with a dedicated message at assignment targets. Skipped for named arguments, formal parameters, `$`/`@` field names, formula terms, subscripted uses, and callees |
| Semicolon | information | `;` separator outside strings/comments (`a; b`, trailing `a;`) |
| Equals NA | information | `x == NA`, `x != NA` (either side), any typed-`NA` variant, or `x %in% NA`. Use `is.na(x)` |
| Vector logic | information | `&` or `\|` in an `if` / `while` / `expect_true()` condition, and `&&` / `\|\|` inside `subset()` / `filter()` arguments. Condition scan stops at call boundaries; bitwise-arithmetic operands are exempt |
| Function left parentheses | information | Whitespace between `function` (or `\`) and `(`, or between a call's function name and its `(` (`blah (1)`) |
| Spaces inside | information | Whitespace immediately inside `(`, `[`, or `[[` (`f( x )`, `df[ 1 ]`, `f( )`). Multi-line wrapping and comma/`= )` neighbors are exempt |
| Indentation | information | Leading whitespace doesn't match the surrounding syntax (braced blocks, multi-line argument lists, continuation lines). Configurable indent unit via `raven.linting.indentationUnit` (default `"auto"` in VS Code, tracking each file's resolved `editor.tabSize`); infix-operator continuation style via `raven.linting.infixContinuationStyle` (`"either"` default, strict `"indented"` or `"aligned"` — see [Linting § Indentation](linting.md#indentation)) |

Lint diagnostics carry the `source` field `raven (lint)` so they're easy to distinguish from cross-file or syntax diagnostics. Named-argument `=` inside function calls is never flagged.

The infix-spaces lint requires at least one space on both sides of `lintr`'s low-precedence operator set: arithmetic (`+`, `-`, `*`, `/`), comparison (`<`, `<=`, `==`, `!=`, ...), logical (`&`, `&&`, `|`, `||`), assignment (`<-`, `<<-`, `:=`, `->`, `->>`, `=` — including named-argument and formal-default `=`), pipe (`|>`, `%>%`, any `%...%`), and binary formula (`y ~ x`). High-precedence operators (`^`/`**`, `:`, `::`, `:::`, `$`, `@`, `?`) and unary `-`, `+`, `!`, `~` are never linted, matching `lintr`. Alignment whitespace (`x   <- 1`) is not flagged, and line-continuation cases (operator at end of line, RHS on the next line) are skipped since the line break supplies the separation.

The commented-code lint groups consecutive standalone comment lines (and checks end-of-line comments individually) and try-parses their bodies as R. A comment is reported when it parses without errors **and** contains at least one call, assignment, binary/unary operator, function definition, or control-flow construct — bare identifiers, literals, juxtaposed prose, and lone `-`/`?` operators are treated as prose. Roxygen lines (`#'`), shebangs, annotation comments (`# TODO:`, `# FIXME:`, `# NOTE:`, `# XXX:`, `# HACK:`, `# BUG:`, `# WARNING:`, `# OPTIMIZE:`), Emacs mode lines (`# -*- ... -*-`), and `# nolint` / `# raven:` / `# @lsp-…` directives are skipped up front.

The object-name lint has independent settings for **functions** (`objectNameStyleFunction`, `objectNameRegexesFunction`), **variables** (`objectNameStyleVariable`, `objectNameRegexesVariable`), and **arguments** (`objectNameStyleArgument`, `objectNameRegexesArgument`). Each style key accepts one named style or an array of styles: `snake_case`, `camelCase`, `dotted.case`, `UPPER_CASE`, `lowercase`, `symbols`, or `any`. Names pass when they match any named style or regex for that kind. Using `any` accepts all names for that kind; an empty style array with regexes is regex-only mode.

> [!NOTE]
> Some names are always accepted regardless of the configured style:
> - Named styles treat an optional leading `.` as decorative; the rest of the name must still match (e.g. `.helper` under `snake_case` is fine, `.myHelper` is not). Custom regexes match the full identifier including the leading dot.
> - Names with the shape `<generic>.<class>` are exempt when `<generic>` is a known base R S3 generic (`print.MyClass`, `as.Date.character`, `` `+.MyClass` ``) or a generic declared in the same file via `UseMethod`. For other generics, use `# nolint` or `# raven: ignore`.
> - Backtick-quoted names are *stripped*, not skipped (`` `myBadName` <- 1 `` lints like `myBadName <- 1`); operator overloads like `` `%+%` `` pass via the default `symbols` style. Non-ASCII identifiers are skipped only when no regexes are configured for the kind; with regexes configured they are checked against the regexes.

**Suppression:** lint diagnostics honor the `lintr` conventions in addition to Raven's own:

- `# nolint` on a line suppresses lints on that line (rule-name filters like `# nolint: line_length` narrow suppression to the named rules).
- `# nolint start` / `# nolint end` brackets a region.
- The standard `# raven: ignore` and `# raven: ignore-next` markers also apply to lint diagnostics.

## Suppression

### Per-Line: `# raven: ignore`

```r
x <- unknown_var # raven: ignore
```

```r
# raven: ignore-next
x <- unknown_var
```

### Per-Symbol: Declaration Directives

```r
load("data.RData")
# raven: var model_fit
# raven: var training_data
x <- model_fit  # No warning
```

See [Directives](directives.md#declaration-directives) for full syntax.

### Per-Category: Configuration

Each diagnostic category has a severity setting that accepts `"error"`, `"warning"`, `"information"` (or its `"info"` alias), `"hint"`, or `"off"`:

```json
"raven.crossFile.missingFileSeverity": "off",
"raven.diagnostics.undefinedVariableSeverity": "off"
```

See [Configuration](configuration.md) for all severity settings.

## Cross-File Behavior

Diagnostics respect the full dependency graph:

```r
# main.R
library(dplyr)
source("analysis.R")
```

```r
# analysis.R
# In auto mode, Raven discovers that main.R sources this file
result <- mutate(df, x = 1)  # No warning: dplyr loaded in parent before source()
```

When a parent file changes (e.g., a `library()` call is added or removed), Raven revalidates diagnostics in dependent files automatically.

## JAGS and Stan

R semantic, lint, package, and cross-file diagnostics are suppressed for JAGS
and Stan. Standalone `.jags`, `.bugs`, and `.bug` programs receive syntax-only
diagnostics. Standalone `.stan` programs receive syntax diagnostics plus the
conservative, fragment-aware undeclared-variable analysis described above; they
do not enter R's scope, package, lint, or cross-file pipelines.

## R Markdown and Quarto

In R Markdown (`.Rmd` / `.Rmarkdown`) and Quarto (`.qmd`) documents, the R code inside chunks is diagnosed as a single R program. Raven analyzes a masked view of the document in which every non-R line — prose, YAML front matter, and non-R fenced blocks (Python, Bash, etc.) — is blanked while R chunk bodies are preserved at their original line and column positions. As a result:

- Syntax errors, undefined variables, and lint findings inside `{r}` (and `{rscript}`) chunks are reported at the document's own coordinates, exactly as they would be in a `.R` file.
- Prose, YAML, markdown links, and non-R chunks never produce diagnostics.
- Symbols defined in one R chunk are in scope in later R chunks (the chunks share a single analysis), so a variable assigned in an early chunk and used in a later one is not flagged as undefined.
- Chunk options that only affect knitr execution (such as `eval=FALSE`) suppress diagnostics for that chunk body — it may hold intentionally incomplete snippets — but language intelligence (completions, semantic tokens, indentation) still works inside it.
- `# nolint` markers and `# raven: ignore` directives work inside chunks just as in plain R.

### Parameterized reports (`params`)

When the YAML front matter declares a top-level `params:` key, Raven treats `params` as a defined symbol for that document — undefined-variable and out-of-scope diagnostics will not flag uses of `params` inside R chunks. Without a `params:` key in the front matter, `params` is treated as any other undefined symbol and is flagged normally.

Code intelligence for individual R chunks is covered in [chunks.md](chunks.md).
