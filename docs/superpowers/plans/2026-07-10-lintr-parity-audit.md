# lintr-parity audit of the native linter (2026-07-10)

A comprehensive differential audit of every Raven lint rule against **real
lintr 3.3.0.1** (installed, run via `Rscript`), triggered by three user-reported
bugs (#599 backticked names, #589 indentation closer, `x^2` infix false
positive). Every finding below was produced by running both tools on the same
snippet — lintr's own testthat suite (a scratchpad clone of r-lib/lintr) was
the primary case source, extended with adversarial cases. ~600 differential
cases were run across 21 rules; indentation additionally has a persisted
112-case corpus.

Ground truth: lintr **3.3.0.1** defaults. Where 3.3.0.1 and lintr dev
(3.3.0.9000) disagree, the choice is noted.

## Fixed in this change (verified parity)

| rule | what was wrong | now |
|---|---|---|
| infix_spaces | `^` required spaces (`x^2` doubly false-flagged, issue #600); `:`/`$`/`@`/`::`/unary ops flagged whitespace lintr ignores; named-arg/formals `=` and `:=` not linted | lints exactly lintr's low-precedence set incl. `EQ_SUB`/`EQ_FORMALS`/`:=`; high-precedence + unary never linted; `box::use()` `/` exempt; `alist(a =)` quirk pinned |
| object_name | backticked names exempted wholesale (issue #599) | lintr's `strip_names` mirrored (backticks/quotes/`%`, trailing `<-`); `symbols` style added (and to the defaults, `c("snake_case", "symbols")`); full `.base_s3_generics` table (216 entries incl. operator generics); same-file `UseMethod` generics; `special_funs` (`.onLoad`…) exempt; string LHS (`"foo" <- 1`) checked |
| object_length | backtick/non-ASCII exempt; leading dot uncounted | strip + longest `<generic>.` prefix removal; counts all chars like `nchar()` |
| indentation | actual-indent-anchored model diverged widely (44/112 corpus cases): `if (cond ||\n …) {` bodies false-flagged, `$`-chains expected 0, `x <-\n  a +\n  b` double-indented, unbraced bodies expected 0, tidy double-indent defs flagged | rewritten to lintr's accumulated indent-change model (block/hanging/double, opener exclusion, wrapped-call suppression, `assignment_as_infix`, unbraced bodies, `else`/`repeat`, consecutive-lint suppression). Corpus: **0 false positives**; residual differences are deliberate leniencies (below) |
| assignment_operator | `->`/`->>`/`%<>%` never flagged; `f({ y = 1 })`-style call-nested `=` flagged though lintr excludes | lintr's operator set + implicit-assignment exclusion (paren-wrapped args still flagged); dedicated `<<-`/`->>`-cascade and `%<>%` messages |
| quotes | `'he said "hi"'` flagged; raw strings exempt wholesale | content rule: a literal containing the preferred quote char is exempt; raw strings checked by the same rule |
| commas | leading-comma continuation, `a[1, , 2]`, `switch(x = , …)` flagged | lintr's three before-comma exemptions |
| semicolon | `` `a;b` ``, `%;%` flagged | `;` inside any leaf token skipped |
| spaces_inside | `f(a, )`/`alist(a = )`/comment-after-`(` flagged; `if ( x )` etc. not covered; `f( )` not flagged | full paren coverage (control flow, params, lambdas); lintr's exemption set; whitespace-only groups flagged both sides |
| t_and_f_symbol | assignment targets silently skipped; formulas/`T[1]`/`T(1)` flagged | variable-name message on targets; formula/subscript/callee carve-outs |
| equals_na | `x %in% NA` missed | flagged (RHS only) |
| vector_logic | `if (info & as.raw(12))` flagged; `filter(x, y && z)` / `expect_true(a & b)` missed | bitwise/string carve-out; subset/filter + expect_true/false halves added |
| function_left_parentheses | call sites (`blah (1)`, `x$foo (1)`, `` `+` (1,1) ``) never checked — half the linter | call-site coverage with lintr's callee set (`$` yes, `@`/strings/computed no) |
| trailing_whitespace | flagged inside multi-line strings | `allow_in_strings = TRUE` default mirrored |
| no_tab | flagged tabs anywhere (comments, strings, mid-line) | indentation tabs only, strings exempt (lintr's `whitespace_linter`) |
| trailing_blank_lines | missing-newline short-circuited the blank-line lints | both reported |
| commented_code | prose with operators flagged (`# use foo(x) instead`, `# something like i + 1` — tree-sitter tolerates juxtaposition R rejects); `# 1-a`, `# ?data.frame` flagged; `# x <- 1,` / `# f() %>%` missed | juxtaposition rejection; binary `-` and unary `-`/`+`/`?` not evidence; dangling `,`/pipe stripped before the parse test |
| line_length | measured UTF-16 units, labeled "characters" | measured in characters (`nchar()` parity) |

Fix for #589 (indentation closer after parenthesized binary expression) had
already landed on main (PR #595, 2026-07-08) but **postdates the last release
(v0.13.0, cut 2026-07-05; installed binary 0.11.4)** — the report of it "still
broken" is release lag. The residual diagnostic on the *operand* line
(`Indentation should be 8 spaces, not 4.`) is real lintr behavior (verified:
lintr reports the identical message on the identical line; styler formats the
continuation one unit deeper).

## Deliberate divergences (kept, documented in docs/linting.md)

- **Indentation leniencies**: Raven *additionally accepts* the
  aligned-argument style where lintr demands block, the block form where lintr
  demands hanging/double, and the operator chain-start column — so the linter
  never fights Raven's own on-type formatter. Raven flags a strict subset of
  lintr on these shapes; primaries match. (Corpus cases 03/13/14/23/29/33/34.)
- **Consecutive-run re-anchoring** (corpus case 40): lintr re-anchors hanging
  runs mid-stream in one edge case; Raven's simpler same-diff suppression
  differs on one line there.
- **Parse errors**: lintr emits an error-level lint on unparseable files;
  Raven's syntax diagnostics handle that separately (corpus case 41).
- **object_name**: hidden S3 methods (`.print.MyClass`) stay exempt (lintr
  flags them); non-ASCII names skipped when no regexes configured (lintr's
  ASCII regexes flag them).
- **commented_code**: end-of-line comments next to code are checked (kept
  from PR #600, matching lintr); block grouping can merge/shift diagnostics
  vs lintr's per-line lints; annotation prefixes (`# TODO:` …) are skipped.
- **trailing_blank_lines**: `.Rmd`/`.qmd` exempt (lintr lints chunk shape).
- **vector_logic**: `if`/`while` conditions nested inside call arguments are
  still checked (matches lintr dev; 3.3.0.1 suppressed them) and `x[[a | b]]`
  is treated as a vector context (lintr only resets at `[`). The subset/filter
  scan stops at nested call boundaries and skips lambdas (matches lintr dev
  and 3.3.0.1's `filter(data, foo(a && b))` behavior; 3.3.0.1's quirk of
  flagging `&&` inside a lambda nested in a call is not reproduced).
- **Message wording** differs throughout (Raven's messages are more specific,
  e.g. per-side infix messages); locations match.
- **semicolon/commas**: Raven anchors diagnostics on the token, lintr on the
  gap — position-style only.

## Known gaps (not addressed here)

- **Config surface**: lintr arguments with no Raven equivalent are the
  `.lintr` loader's "unrecognized construct" bucket, but several are silently
  *dropped inside otherwise-recognized calls*: `assignment_linter(operator =
  c(...), allow_trailing =)`, `trailing_whitespace_linter(allow_empty_lines =,
  allow_in_strings =)`, `line_length_linter(…, ignore_string_bodies =)`,
  `semicolon_linter(allow_compound =)`, `commas_linter(allow_trailing =)`,
  `quotes_linter(delimiter =)` beyond the two styles,
  `indentation_linter(hanging_indent_style =, assignment_as_infix =)`,
  `infix_spaces_linter(exclude_operators =, allow_multiple_spaces = FALSE)`.
  Defaults coincide everywhere, so default-config behavior is unaffected.
- **object_name/object_length**: a replacement call whose *first* argument
  is not the assigned object (`foo(1, badName) <- 1`) is not descended —
  lintr flags any symbol argument there, but real replacement functions take
  the object first, so the shape is pathological.
- **t_and_f_symbol**: lintr in packages also consults NAMESPACE imports for
  generics; Raven has no package-NAMESPACE model in the linter.
- **line_length**: `raven.linting.lineLength` thresholds by character while
  LSP ranges remain UTF-16 — the diagnostic *range* end may differ from the
  threshold column for non-BMP text (display-only).

## Artifacts

- Indentation corpus (112 cases with both tools' verdicts in headers):
  session scratchpad `indent-corpus/` — the divergent cases and root-cause
  taxonomy are additionally covered by unit tests in
  `crates/raven/src/linting/rules/indentation.rs` and
  `crates/raven/src/linting/mod.rs`.
- Rust regression tests pinning every fixed behavior: `linting::tests` (the
  "lintr-parity regressions" block) plus per-rule test modules; each case's
  expected verdict was first confirmed against real lintr 3.3.0.1.
