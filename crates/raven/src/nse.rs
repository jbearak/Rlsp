//! Non-standard-evaluation (NSE) argument policies for undefined-variable
//! diagnostics.
//!
//! Issue #398 untangles undefined-variable diagnostics from the old blanket
//! "call-like arguments are NSE" suppression. Instead of suppressing every
//! identifier inside any `call`/`subset`/`subset2` argument list, the
//! diagnostics collector resolves the callee to its source and consults an
//! **argument-policy table** that says, per call, which argument subtrees are
//! captured / data-masked / tidy-selected (and therefore not free-variable
//! references) versus evaluated normally.
//!
//! This module is the **pure** half of that machinery: it has no dependency on
//! tree-sitter or scope resolution. It exposes
//!
//! - `ArgPolicy`, the per-call decision (`Standard`, `WholeCall`, or
//!   `PerFormal`),
//! - `base_policy` / `package_policy`, the built-in `(source, name)` →
//!   policy table, and
//! - `suppressed_arguments`, which maps a policy plus the ordered argument
//!   labels of a concrete call onto a per-argument suppression mask using R's
//!   named-then-positional matching rules.
//!
//! The AST-facing half (extracting the callee, resolving it against scope and
//! the package library, inferring user-defined captured formals, and applying
//! the suppression mask while walking the tree) lives in `handlers.rs`.
//!
//! The table deliberately prefers **per-formal** policies over whole-call
//! suppression so that, for example, `with(df, col + 1)` still checks `df`
//! while suppressing `col`. Whole-call suppression is reserved for forms where
//! every meaningful argument is captured (`aes(...)`, the plural rlang capture
//! helpers). The table is a curated v1 of the common, slow-moving NSE surface
//! (base/utils metaprogramming and object-name helpers, default-attached
//! `stats` model-fitting `subset`/`weights` data-masking, dplyr/tidyr
//! data-masking and tidy-select verbs, `tibble`/`targets` constructors and
//! target-name helpers, `gt`/`gtsummary` table-column selectors, `recipes`
//! step/role column captures, ggplot2 mapping helpers (`aes`/`vars`/`qplot`),
//! `tidytext`/`modelr`/`drake` column- and target-name captures, rlang capture
//! helpers, the plyr `.()` quoting helper and `*ply` split-apply verbs (whose
//! `...` are suppressed only when `.fun` is a data-masking verb — see
//! `is_plyr_split_apply_verb` and the call-site upgrade in `handlers.rs`), and a
//! few established DSLs); it is intentionally extensible rather
//! than exhaustive. A handful of large, uniform export families
//! (`recipes::step_*`, `gt::fmt_*`/`sub_*`/`cells_*`) are matched by name
//! prefix rather than enumerated, since every member shares one empirically
//! verified contract.

/// What to do with the arguments of a resolved call when collecting
/// undefined-variable candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ArgPolicy {
    /// Standard evaluation: every argument is a real expression, so descend
    /// into all of them and collect candidate references.
    Standard,
    /// Whole-call NSE: every argument subtree is captured / data-masked, so
    /// suppress them all. Reserved for forms where per-formal modeling is
    /// unnecessary (e.g. `aes(...)`, `exprs(...)`).
    WholeCall,
    /// Per-formal NSE: suppress only the arguments bound to the named formals
    /// (and, when `captured_dots` is set, the arguments absorbed by `...` /
    /// trailing positionals). Every other argument is checked.
    ///
    /// Owned `String`s (rather than `&'static str`) so the same variant can
    /// carry both the built-in table's literals and the formals inferred from a
    /// user-defined function's parameter list (Phase 2.5).
    PerFormal {
        /// Ordered formal names, used for positional argument matching. A
        /// literal `"..."` marks where `...` sits: positional arguments at or
        /// past that point are absorbed by `...` and governed by
        /// `captured_dots`; formals after `...` can only be matched by name.
        formals: Vec<String>,
        /// Names of formals whose bound argument is captured (suppressed).
        captured: Vec<String>,
        /// Whether arguments absorbed by `...` (or overflowing positionals) are
        /// captured (suppressed).
        captured_dots: bool,
    },
}

impl ArgPolicy {
    /// Construct a [`ArgPolicy::PerFormal`] from string slices (used by the
    /// built-in table; the Phase 2.5 inference constructs the variant directly
    /// from owned formal names).
    pub(crate) fn per_formal(formals: &[&str], captured: &[&str], captured_dots: bool) -> Self {
        ArgPolicy::PerFormal {
            formals: formals.iter().map(|s| s.to_string()).collect(),
            captured: captured.iter().map(|s| s.to_string()).collect(),
            captured_dots,
        }
    }
}

/// Built-in NSE policy for a base-R / builtin callee `name`, or `None` when the
/// builtin evaluates its arguments normally (the common case, e.g. `paste`,
/// `c`, `eval`, `tryCatch`, `stopifnot`, `system.time`).
///
/// Notable entries:
/// - `library` / `require` capture the bare package name so `library(dplyr)`
///   does not flag `dplyr`.
/// - `substitute` / `quote` / `bquote` / `evalq` capture the quoted expression
///   but still check environment arguments.
/// - `with` / `within` / `subset` / `transform` check the data argument and
///   suppress the data-masked expression(s).
/// - object-name helpers (`data`, `rm`, `remove`, `save`) suppress their bare
///   object-name `...` while checking control arguments such as `file`/`list`.
pub(crate) fn base_policy(name: &str) -> Option<ArgPolicy> {
    let policy = match name {
        // Package attachment: the package argument is a bare name, not a value.
        "library" | "require" => {
            ArgPolicy::per_formal(&["package", "help"], &["package", "help"], false)
        }

        // Base metaprogramming: capture the quoted expression, check the rest.
        "substitute" => ArgPolicy::per_formal(&["expr", "env"], &["expr"], false),
        "quote" => ArgPolicy::per_formal(&["expr"], &["expr"], false),
        "bquote" => ArgPolicy::per_formal(&["expr", "where", "splice"], &["expr"], false),
        "expression" => ArgPolicy::WholeCall,
        // alist() = as.list(sys.call())[-1L]: captures every argument unevaluated.
        "alist" => ArgPolicy::WholeCall,
        "evalq" => ArgPolicy::per_formal(&["expr", "envir", "enclos"], &["expr"], false),
        "on.exit" => ArgPolicy::per_formal(&["expr", "add", "after"], &["expr"], false),
        // Native-code interfaces: the first argument is a native routine name
        // or string, not a normal R variable reference; later arguments are
        // evaluated and passed through to the routine.
        ".Call" | ".Call.graphics" | ".External" | ".External2" | ".External.graphics" => {
            ArgPolicy::per_formal(&[".NAME", "...", "PACKAGE"], &[".NAME"], false)
        }
        ".C" | ".Fortran" => ArgPolicy::per_formal(
            &[".NAME", "...", "NAOK", "DUP", "PACKAGE", "ENCODING"],
            &[".NAME"],
            false,
        ),
        // `curve(sin(x), 0, 1)`: the expression is evaluated against an implicit
        // `x`, so capture it but check the numeric range/control arguments.
        "curve" => ArgPolicy::per_formal(&["expr", "from", "to", "n"], &["expr"], false),

        // Data-mask helpers: check the data, suppress the masked expression(s).
        "with" | "within" => ArgPolicy::per_formal(&["data", "expr"], &["expr"], true),
        "subset" => ArgPolicy::per_formal(
            &["x", "subset", "select", "drop"],
            &["subset", "select"],
            false,
        ),
        "transform" => ArgPolicy::per_formal(&["_data", "..."], &[], true),

        // Object-name helpers: suppress bare object names, check controls.
        "data" => ArgPolicy::per_formal(
            &[
                "...",
                "list",
                "package",
                "lib.loc",
                "verbose",
                "envir",
                "overwrite",
            ],
            &[],
            true,
        ),
        "rm" | "remove" => {
            ArgPolicy::per_formal(&["...", "list", "pos", "envir", "inherits"], &[], true)
        }
        "save" => ArgPolicy::per_formal(
            &[
                "...",
                "list",
                "file",
                "ascii",
                "version",
                "envir",
                "compress",
                "compression_level",
                "eval.promises",
                "precheck",
            ],
            &[],
            true,
        ),
        "help" => ArgPolicy::per_formal(
            &[
                "topic",
                "package",
                "lib.loc",
                "verbose",
                "try.all.packages",
                "help_type",
            ],
            &["topic", "package"],
            false,
        ),
        // utils::example captures the bare help `topic` (like `help`) but
        // evaluates `package`/`lib.loc` — verified against R 4.6.0.
        "example" => ArgPolicy::per_formal(
            &["topic", "package", "lib.loc", "character.only"],
            &["topic"],
            false,
        ),
        // utils object/topic-name helpers: capture the bare name, check controls.
        // (All live in utils — see `builtin_nse_home`.) Verified against R 4.6.0.
        "getAnywhere" | "argsAnywhere" => ArgPolicy::per_formal(&["x"], &["x"], false),
        "news" => ArgPolicy::per_formal(
            &["query", "package", "lib.loc", "format", "reader", "db"],
            &["query"],
            false,
        ),
        "demo" => ArgPolicy::per_formal(
            &[
                "topic",
                "package",
                "lib.loc",
                "character.only",
                "verbose",
                "type",
                "echo",
                "ask",
                "encoding",
            ],
            &["topic"],
            false,
        ),
        "fix" => ArgPolicy::per_formal(&["x", "..."], &["x"], false),
        "debugcall" => ArgPolicy::per_formal(&["call", "once"], &["call"], false),
        "undebugcall" => ArgPolicy::per_formal(&["call"], &["call"], false),

        // stats/methods are default-attached, so their NSE helpers must resolve
        // for bare calls too (not only `stats::lm`). Routed here via
        // `builtin_nse_home` (home = "stats" / "methods"). Verified vs R 4.6.0.
        "lm" | "glm" | "loess" | "nls" | "xtabs" | "oneway.test" | "factanal" => {
            return stats_policy(name);
        }
        // methods::hasArg(x): `x` is a symbol checked for presence, never evaluated.
        "hasArg" => ArgPolicy::per_formal(&["name"], &["name"], false),

        _ => return None,
    };
    Some(policy)
}

/// The package each builtin NSE helper in [`base_policy`] is actually exported
/// from. Almost all are `base`; the other default-attached homes are `utils`
/// (`data`, `help`, `example`, `news`, …), `stats` (`lm`, `glm`, …), `methods`
/// (`hasArg`), and `graphics` (`curve`). Lets [`package_policy`] route a
/// `pkg::name` call to the builtin policy only when `pkg` is the true home, so
/// `utils::data` / `stats::lm` resolve while the invalid `base::data` does not.
///
/// Keep in sync with [`base_policy`]: any entry added there that is **not** in
/// `base` must be listed here, or its correctly-qualified form will be missed.
fn builtin_nse_home(name: &str) -> &'static str {
    match name {
        "data" | "help" | "example" | "news" | "getAnywhere" | "argsAnywhere" | "demo" | "fix"
        | "debugcall" | "undebugcall" => "utils",
        "lm" | "glm" | "loess" | "nls" | "xtabs" | "oneway.test" | "factanal" => "stats",
        "hasArg" => "methods",
        "curve" => "graphics",
        _ => "base",
    }
}

/// Built-in NSE policy for a `package::name` callee, or `None` when that
/// package's function (as far as the v1 table knows) evaluates its arguments
/// normally.
///
/// Keyed by source `(package, name)` rather than bare text so that a local
/// `filter <- function(...)` or `stats::filter` is classified differently from
/// `dplyr::filter` (see the resolver in `handlers.rs`). Coverage is a curated
/// subset of the common data-masking / tidy-select / capture surface.
pub(crate) fn package_policy(package: &str, name: &str) -> Option<ArgPolicy> {
    let policy = match package {
        // Builtin NSE helpers are attached by default but live in different
        // packages (`data`/`help` in utils, `curve` in graphics, `lm`/`glm` in
        // stats, `hasArg` in methods, the rest in base). Resolve a `pkg::name`
        // call to the builtin policy only when `pkg` is the function's true
        // home, so `base::rm` / `utils::data` / `stats::lm` resolve while the
        // invalid `base::data` does not falsely suppress.
        "base" | "utils" | "graphics" | "stats" | "methods"
            if builtin_nse_home(name) == package =>
        {
            base_policy(name)?
        }
        "dplyr" => dplyr_policy(name)?,
        "tidyr" => tidyr_policy(name)?,
        "tibble" => tibble_policy(name)?,
        "ggplot2" => match name {
            "aes" | "vars" => ArgPolicy::WholeCall,
            // qplot(x, y, ..., data, ...): the x/y aesthetics and any further
            // aesthetic mappings absorbed by `...` (e.g. `colour =`, `size =`)
            // are evaluated in the `data` mask, so they are suppressed. `data`
            // and the literal plot controls (`facets`/`xlim`/`main`/…), which
            // sit after `...` and bind by name only, stay checked. Verified
            // against ggplot2 + R 4.6.0.
            "qplot" => ArgPolicy::per_formal(
                &[
                    "x", "y", "...", "data", "facets", "margins", "geom", "xlim", "ylim", "log",
                    "main", "xlab", "ylab", "asp", "stat", "position",
                ],
                &["x", "y"],
                true,
            ),
            _ => return None,
        },
        "rlang" => match name {
            "enquo" | "enexpr" | "ensym" => ArgPolicy::per_formal(&["arg"], &["arg"], false),
            "quo" | "expr" => ArgPolicy::per_formal(&["expr"], &["expr"], false),
            "quos" | "exprs" | "enquos" | "enexprs" | "ensyms" => ArgPolicy::WholeCall,
            _ => return None,
        },
        "data.table" => match name {
            // Column-name NSE; the `v`-suffixed variants take character vectors.
            "setkey" | "setorder" | "setindex" => ArgPolicy::per_formal(&["x", "..."], &[], true),
            _ => return None,
        },
        "targets" => targets_policy(name)?,
        "gt" => gt_policy(name)?,
        "gtsummary" => gtsummary_policy(name)?,
        "recipes" => recipes_policy(name)?,
        "htmltools" => match name {
            "withTags" => ArgPolicy::per_formal(&["code"], &["code"], false),
            _ => return None,
        },
        "grid" => match name {
            "grid.Call" | "grid.Call.graphics" => {
                ArgPolicy::per_formal(&["fnname", "..."], &["fnname"], false)
            }
            _ => return None,
        },
        "dbplyr" => match name {
            // window_order(.data, ...): the ordering columns in `...` are
            // data-masked (a bare undefined symbol there errors "not found");
            // `window_frame`/`sql`/`build_sql` take numerics/strings and stay
            // checked. Verified against dbplyr 2.5.2 + R 4.6.0.
            "window_order" => ArgPolicy::per_formal(&[".data", "..."], &[], true),
            _ => return None,
        },
        "survival" => match name {
            // tmerge(data1, data2, id, ..., tstart, tstop, options):
            // `id`, `tstart`, and `tstop` are data-masked (column expressions
            // evaluated within `data1`) — e.g. tmerge(..., tstart=age, tstop=futime).
            // Every `...` argument is a data-masked time-dependent term — typically
            // a call to the tmerge-only NSE helpers `tdc`/`event`/`cumtdc`/`cumevent`.
            // `options` is a plain control list (e.g. list(tdcstart=...)) and is
            // NOT data-masked, so it stays checked.
            "tmerge" => ArgPolicy::per_formal(
                &["data1", "data2", "id", "...", "tstart", "tstop", "options"],
                &["id", "tstart", "tstop"],
                true,
            ),
            _ => return None,
        },
        "tidytext" => match name {
            // unnest_tokens(tbl, output, input, ...): `output` (the new
            // token column's name) and `input` (the source text column) are
            // bare data-masked symbols. The trailing `...` is forwarded to the
            // tokenizer (e.g. `n` for ngrams) and evaluated, so dots stay
            // checked. Verified against tidytext 0.4.x + R 4.6.0.
            "unnest_tokens" => ArgPolicy::per_formal(
                &[
                    "tbl", "output", "input", "token", "format", "to_lower", "drop", "collapse",
                    "...",
                ],
                &["output", "input"],
                false,
            ),
            // bind_tf_idf(tbl, term, document, n): `term`/`document`/`n` are
            // bare column references evaluated in the `tbl` mask; `tbl` checked.
            "bind_tf_idf" => ArgPolicy::per_formal(
                &["tbl", "term", "document", "n"],
                &["term", "document", "n"],
                false,
            ),
            _ => return None,
        },
        "modelr" => match name {
            // data_grid(data, ..., .model): the `...` `name = expression`
            // pairs are evaluated in the `data` mask (like `tidyr::expand`),
            // so they are suppressed; `data` and `.model` stay checked.
            // Verified against modelr 0.1.x + R 4.6.0.
            "data_grid" => ArgPolicy::per_formal(&["data", "...", ".model"], &[], true),
            _ => return None,
        },
        "drake" => match name {
            // readd(target, ...): `target` is a bare drake target name (the
            // same NSE shape as `targets::tar_read`'s `name`). Everything else
            // (`character_only`, `path`, `cache`, …) is evaluated normally.
            // Verified against drake 7.x + R 4.6.0.
            "readd" => ArgPolicy::per_formal(
                &[
                    "target",
                    "character_only",
                    "path",
                    "search",
                    "cache",
                    "namespace",
                    "verbose",
                    "show_source",
                    "subtargets",
                    "subtarget_list",
                ],
                &["target"],
                false,
            ),
            _ => return None,
        },
        "plyr" => match name {
            // `.()` quotes every argument unevaluated — its body is
            // `structure(as.list(match.call()[-1]), env = .env, class =
            // "quoted")`, so each positional is a bare column name to be
            // re-evaluated later in the split-apply data mask (the
            // `.variables` of `ddply`/`dlply`/`daply`/...), never a free
            // reference. It is the plyr analog of `rlang::quos` / base
            // `alist`, so suppress the whole call. Verified against plyr 1.8.9.
            "." => ArgPolicy::WholeCall,
            // plyr's own data-masking verbs evaluate their `...` in a per-group
            // data mask (formals `.data, ...`), like the dplyr verbs of the same
            // name (`summarize` is an identical alias of `summarise`). Modeled so
            // both direct calls (`summarise(df, x = mean(y))`) AND use as a
            // split-apply `.fun` resolve to a dots-capturing policy. Verified
            // against plyr 1.8.9 + R 4.6.0.
            "summarise" | "summarize" | "mutate" => {
                ArgPolicy::per_formal(&[".data", "..."], &[], true)
            }
            // The `*ply` split-apply verbs (issue #467) forward their `...` to
            // `.fun` — plyr calls `.fun(piece, ...)` per slice. The BASE policy
            // captures nothing (`.data`/`.variables`/`.margins`/`.fun` and the
            // `...` all checked, behaviorally Standard); it carries the
            // empirically verified formal order only so the call site can locate
            // `.fun` and `...`. `handlers.rs` upgrades `captured_dots` to true at
            // a call site when `.fun` resolves to a data-masking verb (the only
            // case in which the forwarded `...` are columns). For `d*ply`,
            // `.variables` is typically a `.()` call whose quoted columns the
            // `WholeCall` arm above already suppresses (#466).
            _ => match plyr_split_apply_formals(name) {
                Some(formals) => ArgPolicy::per_formal(formals, &[], false),
                None => return None,
            },
        },
        _ => return None,
    };
    Some(policy)
}

/// The empirically verified formal order of each plyr `*ply` split-apply verb
/// that forwards `...` directly into `.fun(piece, ...)` — the `a*`/`d*`/`l*`
/// families (12 verbs) — or `None` for any other name. Two families are
/// intentionally absent: `r*ply` (`raply`/`rdply`/`rlply`/`r_ply`) takes
/// `.n, .expr` rather than `.fun`; and `m*ply` (`maply`/`mdply`/`mlply`/`m_ply`)
/// wraps `.fun` in `splat()`, spreading its `...` as ordinary arguments rather
/// than into `.fun`'s data mask (see the `m*ply` note at the end of the match).
/// Both stay standard-eval. This is the single source of truth for
/// [`is_plyr_split_apply_verb`] and the `*ply` arm of [`package_policy`], so the
/// recognition predicate and the formal order it consumes cannot drift. The
/// post-`...` control formals are listed so a named control argument
/// (`.drop = FALSE`, `.parallel = TRUE`) matches its formal and is not absorbed
/// as a captured dot when `.fun` is data-masking. Verified vs plyr 1.8.9 + R 4.6.0.
fn plyr_split_apply_formals(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        // a*ply: `(.data, .margins, .fun, ...)` — `.fun` is the 3rd formal.
        "aaply" => &[
            ".data",
            ".margins",
            ".fun",
            "...",
            ".expand",
            ".progress",
            ".inform",
            ".drop",
            ".parallel",
            ".paropts",
        ],
        "adply" => &[
            ".data",
            ".margins",
            ".fun",
            "...",
            ".expand",
            ".progress",
            ".inform",
            ".parallel",
            ".paropts",
            ".id",
        ],
        "alply" => &[
            ".data",
            ".margins",
            ".fun",
            "...",
            ".expand",
            ".progress",
            ".inform",
            ".parallel",
            ".paropts",
            ".dims",
        ],
        "a_ply" => &[
            ".data",
            ".margins",
            ".fun",
            "...",
            ".expand",
            ".progress",
            ".inform",
            ".print",
            ".parallel",
            ".paropts",
        ],
        // d*ply: `(.data, .variables, .fun, ...)` — `.fun` is the 3rd formal.
        "daply" => &[
            ".data",
            ".variables",
            ".fun",
            "...",
            ".progress",
            ".inform",
            ".drop_i",
            ".drop_o",
            ".parallel",
            ".paropts",
        ],
        "ddply" | "dlply" => &[
            ".data",
            ".variables",
            ".fun",
            "...",
            ".progress",
            ".inform",
            ".drop",
            ".parallel",
            ".paropts",
        ],
        "d_ply" => &[
            ".data",
            ".variables",
            ".fun",
            "...",
            ".progress",
            ".inform",
            ".drop",
            ".print",
            ".parallel",
            ".paropts",
        ],
        // l*ply: `(.data, .fun, ...)` — `.fun` is the 2nd formal.
        "laply" => &[
            ".data",
            ".fun",
            "...",
            ".progress",
            ".inform",
            ".drop",
            ".parallel",
            ".paropts",
        ],
        "ldply" => &[
            ".data",
            ".fun",
            "...",
            ".progress",
            ".inform",
            ".parallel",
            ".paropts",
            ".id",
        ],
        "llply" => &[
            ".data",
            ".fun",
            "...",
            ".progress",
            ".inform",
            ".parallel",
            ".paropts",
        ],
        "l_ply" => &[
            ".data",
            ".fun",
            "...",
            ".progress",
            ".inform",
            ".print",
            ".parallel",
            ".paropts",
        ],
        // The `m*ply` family (`maply`/`mdply`/`mlply`/`m_ply`) is deliberately
        // ABSENT: it wraps `.fun` in `splat()` (`f <- splat(.fun)`), calling
        // `do.call(.fun, c(as.list(row), list(...)))` per row, so the m*ply
        // call's `...` are spread as ordinary arguments — NOT forwarded into
        // `.fun`'s per-group data mask the way `a*`/`d*`/`l*ply` forward
        // `.fun(piece, ...)`. Empirically (plyr 1.8.9 + R 4.6.0)
        // `mdply(df, summarise, z = sum(x))` errors "object 'x' not found",
        // confirming `x` is not a visible column. Modeling m*ply would suppress
        // genuine free-variable references in those `...` (a false negative), so
        // it stays standard-eval — same exclusion rationale as `r*ply`.
        _ => return None,
    })
}

/// True when `name` is one of plyr's 12 `*ply` split-apply verbs that forward
/// their `...` directly into `.fun(piece, ...)` (the `a*`/`d*`/`l*` families).
/// See [`plyr_split_apply_formals`] for the excluded `r*ply` and `m*ply`
/// families and the empirical-verification note. Consumed by `handlers.rs` to
/// gate the call-site `captured_dots` upgrade to plyr `*ply` callees only.
pub(crate) fn is_plyr_split_apply_verb(name: &str) -> bool {
    plyr_split_apply_formals(name).is_some()
}

fn dplyr_policy(name: &str) -> Option<ArgPolicy> {
    let policy = match name {
        "filter" | "slice" => {
            ArgPolicy::per_formal(&[".data", "...", ".by", ".preserve"], &[".by"], true)
        }
        "mutate" => ArgPolicy::per_formal(
            &[".data", "...", ".by", ".keep", ".before", ".after"],
            &[".by", ".before", ".after"],
            true,
        ),
        "transmute" | "select" | "rename" | "distinct" => {
            ArgPolicy::per_formal(&[".data", "..."], &[], true)
        }
        "summarise" | "summarize" | "reframe" => {
            ArgPolicy::per_formal(&[".data", "...", ".by", ".groups"], &[".by"], true)
        }
        "arrange" => ArgPolicy::per_formal(&[".data", "...", ".by_group"], &[], true),
        "group_by" | "rowwise" => {
            ArgPolicy::per_formal(&[".data", "...", ".add", ".drop"], &[], true)
        }
        // `slice_*` row selectors data-mask `order_by` / `by` and take literal
        // controls (`n`, `prop`, `with_ties`); suppress everything past `.data`.
        "slice_max" | "slice_min" | "slice_head" | "slice_tail" | "slice_sample" => {
            ArgPolicy::per_formal(&[".data", "..."], &[], true)
        }
        "relocate" => ArgPolicy::per_formal(
            &[".data", "...", ".before", ".after"],
            &[".before", ".after"],
            true,
        ),
        "count" | "add_count" => ArgPolicy::per_formal(
            &[".data", "...", ".wt", ".sort", ".name", ".drop"],
            &[".wt"],
            true,
        ),
        "pull" => ArgPolicy::per_formal(&[".data", "var", "name", "..."], &["var", "name"], false),
        // Whole-call data-masking helpers, normally nested inside the verbs above.
        "case_when" | "across" | "c_across" | "if_any" | "if_all" => ArgPolicy::WholeCall,
        // join_by(a == b): column-name NSE on both sides of the whole call.
        "join_by" => ArgPolicy::WholeCall,
        // top_n(x, n, wt): n/wt are data-masked; x is the data frame.
        "top_n" => ArgPolicy::per_formal(&["x", "n", "wt"], &["n", "wt"], false),
        // tally/add_tally(x, wt, ...): only `wt` is data-masked.
        "tally" | "add_tally" => {
            ArgPolicy::per_formal(&["x", "wt", "sort", "name"], &["wt"], false)
        }
        "with_groups" => {
            ArgPolicy::per_formal(&[".data", ".groups", ".f", "..."], &[".groups"], true)
        }
        // rename_with: `.cols` is tidy-selected; `.fn` and the trailing dots are
        // evaluated (they are forwarded to `.fn`) — verified against R 4.6.0.
        "rename_with" => {
            ArgPolicy::per_formal(&[".data", ".fn", ".cols", "..."], &[".cols"], false)
        }
        // dplyr re-exports the tibble constructors as identical objects; resolve
        // the dplyr-qualified / dplyr-in-play forms to the same policy.
        "tibble" | "tibble_row" | "data_frame" => return tibble_policy(name),
        _ => return None,
    };
    Some(policy)
}

/// NSE policy for `tibble` constructors (the `tibble` package; also re-exported
/// by dplyr). The column arguments in `...` are evaluated in a sequential data
/// mask (`tibble(a = 1, b = a * 2)`), so they are suppressed like dplyr verbs;
/// the leading-dot control arguments are checked. Verified against R 4.6.0.
fn tibble_policy(name: &str) -> Option<ArgPolicy> {
    let policy = match name {
        "tibble" => ArgPolicy::per_formal(&["...", ".rows", ".name_repair"], &[], true),
        "tibble_row" => ArgPolicy::per_formal(&["...", ".name_repair"], &[], true),
        // Deprecated alias of `tibble()`; only formal is `...`.
        "data_frame" => ArgPolicy::per_formal(&["..."], &[], true),
        _ => return None,
    };
    Some(policy)
}

/// NSE policy for `stats` model-fitting functions. Each data-masks the design
/// arguments evaluated inside the model frame (`subset`, `weights`, and for
/// `glm` also `etastart`/`mustart`/`offset`); `formula` is deliberately left
/// CHECKED so it traverses raven's separate `~` handling. Verified against
/// R 4.6.0.
fn stats_policy(name: &str) -> Option<ArgPolicy> {
    let policy = match name {
        "lm" => ArgPolicy::per_formal(
            &[
                "formula",
                "data",
                "subset",
                "weights",
                "na.action",
                "method",
                "model",
                "x",
                "y",
                "qr",
                "singular.ok",
                "contrasts",
                "offset",
                "...",
            ],
            &["subset", "weights", "offset"],
            false,
        ),
        "glm" => ArgPolicy::per_formal(
            &[
                "formula",
                "family",
                "data",
                "weights",
                "subset",
                "na.action",
                "start",
                "etastart",
                "mustart",
                "offset",
                "control",
                "model",
                "method",
                "x",
                "y",
                "singular.ok",
                "contrasts",
                "...",
            ],
            &["weights", "subset", "etastart", "mustart", "offset"],
            false,
        ),
        "loess" => ArgPolicy::per_formal(
            &[
                "formula",
                "data",
                "weights",
                "subset",
                "na.action",
                "model",
                "span",
                "enp.target",
                "degree",
                "parametric",
                "drop.square",
                "normalize",
                "family",
                "method",
                "control",
                "...",
            ],
            &["weights", "subset"],
            false,
        ),
        "nls" => ArgPolicy::per_formal(
            &[
                "formula",
                "data",
                "start",
                "control",
                "algorithm",
                "trace",
                "subset",
                "weights",
                "na.action",
                "model",
                "lower",
                "upper",
                "...",
            ],
            &["subset", "weights"],
            false,
        ),
        "xtabs" => ArgPolicy::per_formal(
            &[
                "formula",
                "data",
                "subset",
                "sparse",
                "na.action",
                "na.rm",
                "addNA",
                "exclude",
                "drop.unused.levels",
            ],
            &["subset"],
            false,
        ),
        "oneway.test" => ArgPolicy::per_formal(
            &["formula", "data", "subset", "na.action", "var.equal"],
            &["subset"],
            false,
        ),
        "factanal" => ArgPolicy::per_formal(
            &[
                "x",
                "factors",
                "data",
                "covmat",
                "n.obs",
                "subset",
                "na.action",
                "start",
                "scores",
                "rotation",
                "control",
                "...",
            ],
            &["subset"],
            false,
        ),
        _ => return None,
    };
    Some(policy)
}

/// NSE policy for `targets` helpers that take a bare target name (or tidyselect
/// over target names). `tar_target` captures name/command/pattern; the
/// read/load/introspection family captures the target-name argument(s) only.
/// `tar_pattern`'s `...` are dimension lengths that are evaluated, so dots stay
/// checked. Verified against targets 1.12.0.
fn targets_policy(name: &str) -> Option<ArgPolicy> {
    let policy = match name {
        "tar_target" => ArgPolicy::per_formal(
            &["name", "command", "pattern"],
            &["name", "command", "pattern"],
            false,
        ),
        "tar_make" => ArgPolicy::per_formal(
            &[
                "names",
                "shortcut",
                "reporter",
                "seconds_meta_append",
                "seconds_meta_upload",
                "seconds_reporter",
                "seconds_interval",
                "callr_function",
                "callr_arguments",
                "envir",
                "script",
                "store",
                "garbage_collection",
                "use_crew",
                "terminate_controller",
                "as_job",
            ],
            &["names"],
            false,
        ),
        "tar_make_future" => ArgPolicy::per_formal(
            &[
                "names",
                "shortcut",
                "reporter",
                "seconds_meta_append",
                "seconds_meta_upload",
                "seconds_reporter",
                "seconds_interval",
                "workers",
                "callr_function",
                "callr_arguments",
                "envir",
                "script",
                "store",
                "garbage_collection",
            ],
            &["names"],
            false,
        ),
        "tar_make_clustermq" => ArgPolicy::per_formal(
            &[
                "names",
                "shortcut",
                "reporter",
                "seconds_meta_append",
                "seconds_meta_upload",
                "seconds_reporter",
                "seconds_interval",
                "workers",
                "log_worker",
                "callr_function",
                "callr_arguments",
                "envir",
                "script",
                "store",
                "garbage_collection",
            ],
            &["names"],
            false,
        ),
        "tar_read" => {
            ArgPolicy::per_formal(&["name", "branches", "meta", "store"], &["name"], false)
        }
        "tar_load" => ArgPolicy::per_formal(
            &[
                "names", "branches", "meta", "strict", "silent", "envir", "store",
            ],
            &["names"],
            false,
        ),
        "tar_meta" => ArgPolicy::per_formal(
            &["names", "fields", "targets_only", "complete_only", "store"],
            &["names", "fields"],
            false,
        ),
        "tar_objects" => ArgPolicy::per_formal(&["names", "cloud", "store"], &["names"], false),
        "tar_progress" => {
            ArgPolicy::per_formal(&["names", "fields", "store"], &["names", "fields"], false)
        }
        "tar_branch_names" => ArgPolicy::per_formal(&["name", "index", "store"], &["name"], false),
        "tar_branches" => ArgPolicy::per_formal(
            &["name", "pattern", "script", "store"],
            &["name", "pattern"],
            false,
        ),
        "tar_pattern" => ArgPolicy::per_formal(&["pattern", "...", "seed"], &["pattern"], false),
        "tar_deps" => ArgPolicy::per_formal(&["expr"], &["expr"], false),
        _ => return None,
    };
    Some(policy)
}

/// NSE policy for `gt` table-construction verbs. `gt` selects table columns with
/// tidyselect (`columns =`, also `after`/`hide_columns`/`target_columns`/
/// `spanners`/`groups`) and filters rows with a data mask (`rows =`), so those
/// are suppressed while `data` and the literal formatting controls (`decimals`,
/// `locale`, …) stay checked.
///
/// Two families are matched by prefix because every export shares an identical,
/// empirically verified contract: the `fmt_*` formatters (31 exports) and the
/// `sub_*` value substituters (5 exports) are all `(data, columns, rows, …)`,
/// and the `cells_*` location helpers (14 exports) take only column/row/group
/// selectors or string keywords, so they are whole-call suppressed. The
/// `cols_*` family is NOT uniform (label/width/merge variants differ), so its
/// column-selecting members are enumerated. Verified against gt + R 4.6.0.
fn gt_policy(name: &str) -> Option<ArgPolicy> {
    // fmt_*/sub_*: uniform `(data, columns, rows, …)`. Modeled without `...` and
    // with captured_dots=false so the trailing literal controls (`decimals`,
    // `locale`, …) — which evaluate in the caller's environment — stay checked.
    if name.starts_with("fmt_") || name.starts_with("sub_") {
        return Some(ArgPolicy::per_formal(
            &["data", "columns", "rows"],
            &["columns", "rows"],
            false,
        ));
    }
    // cells_*: location helpers; every argument is a column/row/group selector
    // (or a harmless string keyword), so suppress the whole call.
    if name.starts_with("cells_") {
        return Some(ArgPolicy::WholeCall);
    }
    let policy = match name {
        "cols_hide" | "cols_unhide" | "cols_move_to_start" | "cols_move_to_end" => {
            ArgPolicy::per_formal(&["data", "columns"], &["columns"], false)
        }
        // `after` names the column to move past.
        "cols_move" => {
            ArgPolicy::per_formal(&["data", "columns", "after"], &["columns", "after"], false)
        }
        "cols_label_with" => ArgPolicy::per_formal(&["data", "columns", "fn"], &["columns"], false),
        "cols_align" => ArgPolicy::per_formal(&["data", "align", "columns"], &["columns"], false),
        "cols_merge" => ArgPolicy::per_formal(
            &["data", "columns", "hide_columns", "rows", "pattern"],
            &["columns", "hide_columns", "rows"],
            false,
        ),
        "cols_merge_n_pct" => ArgPolicy::per_formal(
            &["data", "col_n", "col_pct", "rows", "autohide"],
            &["col_n", "col_pct", "rows"],
            false,
        ),
        "cols_merge_range" => ArgPolicy::per_formal(
            &[
                "data",
                "col_begin",
                "col_end",
                "rows",
                "autohide",
                "sep",
                "locale",
            ],
            &["col_begin", "col_end", "rows"],
            false,
        ),
        "cols_merge_uncert" => ArgPolicy::per_formal(
            &["data", "col_val", "col_uncert", "rows", "sep", "autohide"],
            &["col_val", "col_uncert", "rows"],
            false,
        ),
        "tab_spanner" => ArgPolicy::per_formal(
            &[
                "data", "label", "columns", "spanners", "level", "id", "gather", "replace",
            ],
            &["columns", "spanners"],
            false,
        ),
        "data_color" => ArgPolicy::per_formal(
            &["data", "columns", "rows", "direction", "target_columns"],
            &["columns", "rows", "target_columns"],
            false,
        ),
        "summary_rows" => ArgPolicy::per_formal(
            &[
                "data",
                "groups",
                "columns",
                "fns",
                "fmt",
                "side",
                "missing_text",
                "formatter",
                "...",
            ],
            &["groups", "columns"],
            false,
        ),
        "grand_summary_rows" => ArgPolicy::per_formal(
            &[
                "data",
                "columns",
                "fns",
                "fmt",
                "side",
                "missing_text",
                "formatter",
                "...",
            ],
            &["columns"],
            false,
        ),
        "row_group_order" => ArgPolicy::per_formal(&["data", "groups"], &["groups"], false),
        _ => return None,
    };
    Some(policy)
}

/// NSE policy for `gtsummary` table builders. The variable-selection arguments
/// (`by`, `include`, `variable`, `row`, `col`) are tidyselect/data-masked and
/// suppressed; the `label`/`type`/`statistic`/`digits`/`value` arguments take
/// `column ~ "spec"` formula lists whose LHS is handled by raven's separate `~`
/// path, so they are left CHECKED. `modify_header`/`tbl_uvregression` evaluate
/// their selectors in the caller's environment (a bare undefined symbol is
/// looked up, not captured) and so are deliberately omitted. Verified against
/// gtsummary + R 4.6.0.
fn gtsummary_policy(name: &str) -> Option<ArgPolicy> {
    let policy = match name {
        "tbl_summary" => ArgPolicy::per_formal(
            &[
                "data",
                "by",
                "label",
                "statistic",
                "digits",
                "type",
                "value",
                "missing",
                "missing_text",
                "missing_stat",
                "sort",
                "percent",
                "include",
            ],
            &["by", "include"],
            false,
        ),
        "tbl_continuous" => ArgPolicy::per_formal(
            &[
                "data",
                "variable",
                "include",
                "digits",
                "by",
                "statistic",
                "label",
                "value",
            ],
            &["variable", "include", "by"],
            false,
        ),
        "tbl_cross" => ArgPolicy::per_formal(
            &[
                "data",
                "row",
                "col",
                "label",
                "statistic",
                "digits",
                "percent",
                "margin",
                "missing",
                "missing_text",
                "margin_text",
            ],
            &["row", "col"],
            false,
        ),
        _ => return None,
    };
    Some(policy)
}

/// NSE policy for `recipes` steps and role helpers. Every `step_*` constructor
/// (98 exports) shares the contract `step_x(recipe, <columns/data-mask in …>,
/// <literal controls>)`: the `...` is suppressed (bare column names, tidyselect
/// helpers, and `step_mutate`'s `name = expr` data masks) while `recipe` stays
/// checked. This is the same minimal model as `dplyr::select`/`transmute`
/// (`[".data", "..."]`, captured_dots).
///
/// TRADEOFF: a named scalar control passed past `...` (e.g. `num_comp = k`) is
/// absorbed by the dots and suppressed, so a genuinely undefined symbol there is
/// not flagged. This is the data-mask tradeoff already accepted for
/// `mutate`/`lm(subset=)`; it suppresses the far more common false positive of
/// flagging every selected column. The role helpers list their `new_role`/
/// `old_role`/`new_type` controls explicitly so those values stay checked.
///
/// `check_*` is NOT prefix-matched: four recipes `check_*` exports
/// (`check_name`/`check_new_data`/`check_options`/`check_type`) are internal
/// helpers with a different first formal, so only the five user-facing column
/// checks are enumerated. Verified against recipes + R 4.6.0.
fn recipes_policy(name: &str) -> Option<ArgPolicy> {
    // All step_* constructors take tidyselect/data-mask columns in `...`.
    if name.starts_with("step_") {
        return Some(ArgPolicy::per_formal(&["recipe", "..."], &[], true));
    }
    let policy = match name {
        "update_role" => {
            ArgPolicy::per_formal(&["recipe", "...", "new_role", "old_role"], &[], true)
        }
        "add_role" => ArgPolicy::per_formal(&["recipe", "...", "new_role", "new_type"], &[], true),
        "remove_role" => ArgPolicy::per_formal(&["recipe", "...", "old_role"], &[], true),
        // The user-facing column checks share the step contract; the remaining
        // `check_*` exports are internal helpers and must stay None.
        "check_class" | "check_cols" | "check_missing" | "check_new_values" | "check_range" => {
            ArgPolicy::per_formal(&["recipe", "..."], &[], true)
        }
        _ => return None,
    };
    Some(policy)
}

fn tidyr_policy(name: &str) -> Option<ArgPolicy> {
    let policy = match name {
        "pivot_longer" => ArgPolicy::per_formal(
            &["data", "cols", "names_to", "values_to", "..."],
            &["cols"],
            false,
        ),
        "pivot_wider" => ArgPolicy::per_formal(
            &["data", "id_cols", "names_from", "values_from", "..."],
            &["id_cols", "names_from", "values_from"],
            false,
        ),
        "unite" => ArgPolicy::per_formal(&["data", "col", "..."], &["col"], true),
        "separate" | "extract" => {
            ArgPolicy::per_formal(&["data", "col", "into", "..."], &["col"], false)
        }
        "unnest" => ArgPolicy::per_formal(&["data", "cols", "..."], &["cols"], true),
        // gather(data, key, value, ..., na.rm, convert, factor_key): `key` and
        // `value` are bare OUTPUT column names captured as symbols, and the
        // `...` columns to gather are tidy-selected. `data` and the literal
        // controls (`na.rm`/`convert`/`factor_key`) stay checked. Verified
        // against tidyr 1.3.x + R 4.6.0.
        "gather" => ArgPolicy::per_formal(
            &[
                "data",
                "key",
                "value",
                "...",
                "na.rm",
                "convert",
                "factor_key",
            ],
            &["key", "value"],
            true,
        ),
        "unnest_wider" | "unnest_longer" => {
            ArgPolicy::per_formal(&["data", "col", "..."], &["col"], false)
        }
        "hoist" => ArgPolicy::per_formal(&[".data", ".col", "..."], &[".col"], true),
        // nest(.data, ..., .by, .key, .names_sep): the `name = selection`
        // pairs in `...` are tidy-selected; `.by` selects grouping columns
        // (data-masked, like dplyr); `.key` is the deprecated bare/string output
        // column name (NSE). `.names_sep` is a literal string and stays checked.
        // Verified against tidyr (`nest()` source) + R 4.6.0.
        "nest" => ArgPolicy::per_formal(
            &[".data", "...", ".by", ".key", ".names_sep"],
            &[".by", ".key"],
            true,
        ),
        "fill" | "drop_na" | "complete" | "expand" => {
            ArgPolicy::per_formal(&["data", "..."], &[], true)
        }
        // chop/unchop/separate_wider_*: `cols` is tidy-selected; data + controls checked.
        // chop(data, ..., cols, by, error_call): `cols`/`by` tidy-select the
        // packed columns; the deprecated positional cols land in `...`. All are
        // data-masked; `data` and `error_call` stay checked. Verified against
        // tidyr (`chop()` source) + R 4.6.0.
        "chop" => ArgPolicy::per_formal(
            &["data", "...", "cols", "by", "error_call"],
            &["cols", "by"],
            true,
        ),
        "unchop" => ArgPolicy::per_formal(
            &["data", "cols", "...", "keep_empty", "ptype", "error_call"],
            &["cols"],
            false,
        ),
        "separate_wider_delim" => ArgPolicy::per_formal(
            &[
                "data",
                "cols",
                "delim",
                "...",
                "names",
                "names_sep",
                "names_repair",
                "too_few",
                "too_many",
                "cols_remove",
            ],
            &["cols"],
            false,
        ),
        // separate_rows(data, a, b, sep=): the column names in `...` are tidy-selected.
        "separate_rows" => ArgPolicy::per_formal(&["data", "...", "sep", "convert"], &[], true),
        // uncount(data, weights, ...): only `weights` is data-masked.
        "uncount" => ArgPolicy::per_formal(
            &["data", "weights", "...", ".remove", ".id"],
            &["weights"],
            false,
        ),
        // pack(.data, new = c(a, b)): the packed column names in `...` are tidy-selected.
        "pack" => ArgPolicy::per_formal(&[".data", "...", ".names_sep", ".error_call"], &[], true),
        "unpack" => ArgPolicy::per_formal(
            &[
                "data",
                "cols",
                "...",
                "names_sep",
                "names_repair",
                "error_call",
            ],
            &["cols"],
            false,
        ),
        "separate_wider_position" => ArgPolicy::per_formal(
            &[
                "data",
                "cols",
                "widths",
                "...",
                "names_sep",
                "names_repair",
                "too_few",
                "too_many",
                "cols_remove",
            ],
            &["cols"],
            false,
        ),
        "separate_wider_regex" => ArgPolicy::per_formal(
            &[
                "data",
                "cols",
                "patterns",
                "...",
                "names_sep",
                "names_repair",
                "too_few",
                "cols_remove",
            ],
            &["cols"],
            false,
        ),
        _ => return None,
    };
    Some(policy)
}

const BIOC_TIDY_OMICS_META_MEMBERS: &[&str] = &["dplyr", "tidyr", "ggplot2"];

/// Core member packages of a known meta-package, or an empty slice. A file that
/// only does `library(tidyverse)` should still resolve `filter` / `mutate` to
/// the member package's NSE policy, so the resolver treats the meta-package as
/// if its members were in play. (Listing only the members whose exports carry
/// NSE policies in this module; other members are harmless to include.)
pub(crate) fn meta_package_members(name: &str) -> &'static [&'static str] {
    match name {
        "tidyverse" => &[
            "dplyr",
            "tidyr",
            "ggplot2",
            "purrr",
            "readr",
            "tibble",
            "stringr",
            "forcats",
            "lubridate",
        ],
        "tidymodels" => &["dplyr", "tidyr", "ggplot2", "purrr", "rlang", "recipes"],
        // Bioconductor tidy-omics packages attach dplyr/tidyr generics and add
        // object-specific S3 methods. Route bare verbs to the generic owner's
        // policy; the packages do not export qualified `pkg::filter` verbs.
        "plyranges" => &["dplyr"],
        "tidySummarizedExperiment" | "tidySingleCellExperiment" => BIOC_TIDY_OMICS_META_MEMBERS,
        _ => &[],
    }
}

/// The set of base/rlang capture helpers whose single expression argument, when
/// it is a direct reference to one of the enclosing function's formals, marks
/// that formal as captured. Used by the Phase 2.5 user-defined-NSE inference in
/// `handlers.rs`.
pub(crate) fn is_capture_helper(name: &str) -> bool {
    matches!(name, "substitute" | "enquo" | "enexpr" | "ensym")
}

/// The rlang plural capture helpers that defuse every element of `...` they
/// receive. A local function body passing its own `...` to one of these defuses
/// the caller's arguments, so the Phase 2.5 / issue #433 inference in
/// `handlers.rs` marks the function's dots captured.
///
/// Deliberately restricted to the unambiguous `en`-prefixed names. `quos` /
/// `exprs` defuse too, but their short names collide with standard-eval
/// exports elsewhere (most prominently `Biobase::exprs`, the ExpressionSet
/// accessor), and this check is context-free — it cannot confirm rlang is in
/// play. When rlang *is* in play, `quos(...)` / `exprs(...)` forwarding is
/// still recognized through their `WholeCall` table policies by the
/// covered-verb dots pass. `syms()` is absent for a different reason: it
/// evaluates its argument (a character vector), it does not defuse.
pub(crate) fn is_plural_capture_helper(name: &str) -> bool {
    matches!(name, "enquos" | "enexprs" | "ensyms")
}

/// True for Shiny deferred-expression helpers whose expression body is
/// evaluated later in a child lexical environment: `reactive`, `observe`,
/// `observeEvent`, `eventReactive`, and the `render*()` family (e.g.
/// `renderPlot`, `renderText`, `renderUI`).
///
/// These are not NSE: identifiers inside the body are real references and must
/// be checked. Recognition lets the undefined-variable machinery (issue #402)
/// descend into a bare deferred body even when Shiny export metadata is
/// unavailable, and model the body as a nested scope so its local definitions
/// do not leak into the surrounding server function. Callers gate this on Shiny
/// being in play (or a `shiny::` qualifier) and on the name not being shadowed
/// by a local definition.
pub(crate) fn is_shiny_deferred_helper(name: &str) -> bool {
    matches!(
        name,
        "reactive" | "observe" | "observeEvent" | "eventReactive"
    ) || is_shiny_render_helper(name)
}

/// True for the Shiny `render*()` family: a `render` prefix followed by an
/// upper-case letter (`renderPlot`, `renderUI`, …), excluding a bare `render`
/// and lower-case continuations that are not part of the convention.
fn is_shiny_render_helper(name: &str) -> bool {
    name.strip_prefix("render")
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| c.is_ascii_uppercase())
}

/// Compute the per-argument suppression mask for a call.
///
/// `arg_labels[i]` is `Some(name)` for a named argument (`name = value`) and
/// `None` for a positional argument, in source order. The returned vector has
/// the same length: `true` means "suppress this argument's value subtree from
/// undefined-variable collection".
///
/// `PerFormal` matching follows R's rules in two passes: named arguments match
/// formals by (exact) name first; remaining positional arguments then fill the
/// still-unmatched formals in order, stopping at `...`. Positional arguments at
/// or past `...`, and named arguments that match no formal, are absorbed by
/// `...` and suppressed only when `captured_dots` is set. (Partial name
/// matching, which real R supports, is intentionally not modeled.)
///
/// `pipe_fed` is true when the call is the right-hand side of a pipe
/// (`df %>% filter(col > 1)` / `df %<>% filter(col > 1)` / `df |> filter(col >
/// 1)`): the pipe supplies the first formal (the data/object argument)
/// implicitly, so it is pre-consumed and the syntactic positional arguments bind
/// starting at the *second* formal.
/// Without this, the data-masked `col > 1` would bind the non-captured `.data`
/// formal and be checked — a false positive on the most common dplyr idiom.
pub(crate) fn suppressed_arguments(
    policy: &ArgPolicy,
    arg_labels: &[Option<&str>],
    pipe_fed: bool,
) -> Vec<bool> {
    match policy {
        ArgPolicy::Standard => vec![false; arg_labels.len()],
        ArgPolicy::WholeCall => vec![true; arg_labels.len()],
        ArgPolicy::PerFormal {
            formals,
            captured,
            captured_dots,
        } => per_formal_mask(formals, captured, *captured_dots, arg_labels, pipe_fed),
    }
}

fn per_formal_mask(
    formals: &[String],
    captured: &[String],
    captured_dots: bool,
    arg_labels: &[Option<&str>],
    pipe_fed: bool,
) -> Vec<bool> {
    let is_captured = |formal: &str| captured.iter().any(|c| c == formal);
    let mut mask = vec![false; arg_labels.len()];
    // Which formal indices have already been bound by a named argument; a
    // positional argument cannot reuse them.
    let mut consumed = vec![false; formals.len()];
    // A pipe supplies the leading data/object formal, so treat it as bound
    // before matching the syntactic arguments (unless that formal is `...`).
    if pipe_fed && formals.first().is_some_and(|f| f != "...") {
        consumed[0] = true;
    }

    // Pass 1: named arguments match formals by exact name.
    for (i, label) in arg_labels.iter().enumerate() {
        let Some(name) = label else { continue };
        match formals.iter().position(|f| f != "..." && f == name) {
            Some(fi) => {
                consumed[fi] = true;
                mask[i] = is_captured(name);
            }
            // Named argument that matches no formal is absorbed by `...`.
            None => mask[i] = captured_dots,
        }
    }

    // Pass 2: positional arguments fill remaining formals in order, stopping at
    // `...`. A cursor that has reached `...` (or run off the end) routes every
    // further positional argument through the dots policy.
    //
    // Running off the end is treated like reaching `...` ON PURPOSE: the
    // `# raven: nse` directive is not arity-aware (see `docs/directives.md`), so
    // when the user declared dots-capture (`captured_dots`) a trailing argument
    // beyond the resolved formal list is suppressed even if the callee does not
    // actually have a `...` formal. With no dots-capture declared, `captured_dots`
    // is false and the overflow argument stays checked. (This overflow rule only
    // chooses whether to suppress; whether the directive's policy is itself
    // broader or narrower than Raven's inference is decided earlier, in
    // `resolve_call_arg_policy`.)
    let mut cursor = 0usize;
    for (i, label) in arg_labels.iter().enumerate() {
        if label.is_some() {
            continue;
        }
        loop {
            match formals.get(cursor).map(String::as_str) {
                Some("...") => {
                    mask[i] = captured_dots;
                    break;
                }
                Some(formal) => {
                    let fi = cursor;
                    cursor += 1;
                    if consumed[fi] {
                        continue;
                    }
                    mask[i] = is_captured(formal);
                    break;
                }
                None => {
                    mask[i] = captured_dots;
                    break;
                }
            }
        }
    }

    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience: build the `arg_labels` slice from a list of optional names.
    fn labels(names: &[Option<&'static str>]) -> Vec<Option<&'static str>> {
        names.to_vec()
    }

    /// Issue #402: the recognized Shiny deferred-expression helpers, including
    /// the `render*()` family by prefix, but not unrelated or `render`-only names.
    #[test]
    fn shiny_deferred_helper_recognition() {
        for name in [
            "reactive",
            "observe",
            "observeEvent",
            "eventReactive",
            "renderPlot",
            "renderText",
            "renderUI",
            "renderDataTable",
        ] {
            assert!(
                is_shiny_deferred_helper(name),
                "{name} should be recognized"
            );
        }
        for name in [
            "render",
            "rendering",
            "isolate",
            "req",
            "reactiveValues",
            "plot",
        ] {
            assert!(
                !is_shiny_deferred_helper(name),
                "{name} should not be recognized"
            );
        }
    }

    #[test]
    fn standard_suppresses_nothing() {
        let mask = suppressed_arguments(
            &ArgPolicy::Standard,
            &labels(&[None, Some("x"), None]),
            false,
        );
        assert_eq!(mask, vec![false, false, false]);
    }

    #[test]
    fn whole_call_suppresses_everything() {
        let mask = suppressed_arguments(&ArgPolicy::WholeCall, &labels(&[None, Some("x")]), false);
        assert_eq!(mask, vec![true, true]);
    }

    #[test]
    fn substitute_suppresses_expr_checks_env_positionally() {
        // substitute(expr, env): first positional -> expr (suppressed),
        // second positional -> env (checked).
        let p = base_policy("substitute").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, None]), false);
        assert_eq!(mask, vec![true, false]);
    }

    #[test]
    fn substitute_named_env_is_checked() {
        // substitute(expr, env = typo_env): expr positional suppressed,
        // env named is checked.
        let p = base_policy("substitute").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, Some("env")]), false);
        assert_eq!(mask, vec![true, false]);
    }

    #[test]
    fn with_checks_data_suppresses_expr() {
        // with(df, col + 1): data checked, expr suppressed.
        let p = base_policy("with").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, None]), false);
        assert_eq!(mask, vec![false, true]);
    }

    #[test]
    fn subset_checks_x_and_drop_suppresses_subset_and_select() {
        // subset(x, subset, select, drop)
        let p = base_policy("subset").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, None, None, None]), false);
        assert_eq!(mask, vec![false, true, true, false]);
    }

    #[test]
    fn library_suppresses_bare_package_name() {
        let p = base_policy("library").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None]), false);
        assert_eq!(mask, vec![true]);
    }

    #[test]
    fn rm_suppresses_dots_checks_named_list() {
        // rm(x, y, list = z): x, y -> dots (suppressed); list -> checked.
        let p = base_policy("rm").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, None, Some("list")]), false);
        assert_eq!(mask, vec![true, true, false]);
    }

    #[test]
    fn save_suppresses_objects_checks_file() {
        // save(x, y, file = "out.rda")
        let p = base_policy("save").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, None, Some("file")]), false);
        assert_eq!(mask, vec![true, true, false]);
    }

    #[test]
    fn dplyr_filter_checks_data_suppresses_dots_and_by() {
        // filter(.data, cond, .by = grp): .data checked, cond -> dots
        // (suppressed), .by suppressed, .preserve would be checked.
        let p = package_policy("dplyr", "filter").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, None, Some(".by")]), false);
        assert_eq!(mask, vec![false, true, true]);
    }

    #[test]
    fn dplyr_filter_preserve_is_checked() {
        let p = package_policy("dplyr", "filter").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, None, Some(".preserve")]), false);
        assert_eq!(mask, vec![false, true, false]);
    }

    #[test]
    fn dplyr_mutate_checks_keep_suppresses_before_after() {
        // mutate(.data, new = expr, .keep = "all", .before = col)
        let p = package_policy("dplyr", "mutate").unwrap();
        let mask = suppressed_arguments(
            &p,
            &labels(&[None, Some("new"), Some(".keep"), Some(".before")]),
            false,
        );
        // .data positional checked; `new` is a label whose VALUE is data-masked
        // (named arg matching no formal -> dots -> suppressed); .keep checked;
        // .before suppressed.
        assert_eq!(mask, vec![false, true, false, true]);
    }

    #[test]
    fn pipe_fed_filter_suppresses_first_masked_column() {
        // `df %>% filter(col > 1)`: the pipe supplies `.data`, so the single
        // syntactic positional binds `...` and is suppressed (not `.data`).
        let p = package_policy("dplyr", "filter").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None]), true);
        assert_eq!(mask, vec![true]);
    }

    #[test]
    fn dplyr_slice_max_suppresses_order_by_and_controls() {
        // df %>% slice_max(x, n = 5): pipe supplies .data; x and n bind `...`.
        let p = package_policy("dplyr", "slice_max").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, Some("n")]), true);
        assert_eq!(mask, vec![true, true]);
    }

    #[test]
    fn dplyr_if_any_is_whole_call() {
        assert_eq!(
            package_policy("dplyr", "if_any"),
            Some(ArgPolicy::WholeCall)
        );
        assert_eq!(
            package_policy("dplyr", "c_across"),
            Some(ArgPolicy::WholeCall)
        );
    }

    #[test]
    fn pipe_fed_select_suppresses_all_columns() {
        // `df %>% select(colA, colB)`: both positionals bind `...`.
        let p = package_policy("dplyr", "select").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, None]), true);
        assert_eq!(mask, vec![true, true]);
    }

    #[test]
    fn pivot_longer_suppresses_cols_checks_names_to() {
        // pivot_longer(data, cols, names_to = "k")
        let p = package_policy("tidyr", "pivot_longer").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, None, Some("names_to")]), false);
        assert_eq!(mask, vec![false, true, false]);
    }

    #[test]
    fn aes_is_whole_call() {
        let p = package_policy("ggplot2", "aes").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, None]), false);
        assert_eq!(mask, vec![true, true]);
    }

    #[test]
    fn ggplot2_qplot_suppresses_aesthetics_checks_data_and_controls() {
        // qplot(mpg, wt, colour = cyl, data = mtcars, main = "t"): x/y
        // positionals and the unmatched aesthetic `colour` are data-masked
        // (suppressed); `data` and the literal control `main` stay checked.
        let p = package_policy("ggplot2", "qplot").unwrap();
        let mask = suppressed_arguments(
            &p,
            &labels(&[None, None, Some("colour"), Some("data"), Some("main")]),
            false,
        );
        assert_eq!(mask, vec![true, true, true, false, false]);
    }

    #[test]
    fn tidyr_gather_captures_key_value_and_columns_checks_data() {
        // gather(df, mykey, myval, a, b, na.rm = TRUE): data checked;
        // key/value bare output names suppressed; the gathered columns (dots)
        // suppressed; `na.rm` checked.
        let p = package_policy("tidyr", "gather").unwrap();
        let mask = suppressed_arguments(
            &p,
            &labels(&[None, None, None, None, None, Some("na.rm")]),
            false,
        );
        assert_eq!(mask, vec![false, true, true, true, true, false]);
    }

    #[test]
    fn tidytext_unnest_tokens_captures_output_input_checks_dots() {
        // df %>% unnest_tokens(word, text, token = "ngrams", n = 2): pipe
        // supplies `tbl`; output/input bare columns suppressed; `token` and the
        // tokenizer dot `n` stay checked (forwarded, not masked).
        let p = package_policy("tidytext", "unnest_tokens").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, None, Some("token"), Some("n")]), true);
        assert_eq!(mask, vec![true, true, false, false]);
    }

    #[test]
    fn tidytext_bind_tf_idf_captures_term_document_n() {
        // bind_tf_idf(d, term, document, n): tbl checked; the three column
        // references suppressed.
        let p = package_policy("tidytext", "bind_tf_idf").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, None, None, None]), false);
        assert_eq!(mask, vec![false, true, true, true]);
    }

    #[test]
    fn modelr_data_grid_suppresses_dots_checks_data_and_model() {
        // data_grid(df, a = seq_range(a, 5), .model = mod): data checked; the
        // `...` expression suppressed (data mask); `.model` checked.
        let p = package_policy("modelr", "data_grid").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, Some("a"), Some(".model")]), false);
        assert_eq!(mask, vec![false, true, false]);
    }

    #[test]
    fn drake_readd_captures_target_only() {
        // readd(my_target, character_only = FALSE): target suppressed; control checked.
        let p = package_policy("drake", "readd").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, Some("character_only")]), false);
        assert_eq!(mask, vec![true, false]);
    }

    #[test]
    fn htmltools_with_tags_captures_tag_expression() {
        let p = package_policy("htmltools", "withTags").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None]), false);
        assert_eq!(mask, vec![true]);
    }

    #[test]
    fn rlang_enquo_captures_single_arg() {
        let p = package_policy("rlang", "enquo").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None]), false);
        assert_eq!(mask, vec![true]);
    }

    #[test]
    fn tar_target_captures_name_and_command() {
        let p = package_policy("targets", "tar_target").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, None]), false);
        assert_eq!(mask, vec![true, true]);
    }

    #[test]
    fn meta_packages_expand_to_members() {
        assert!(meta_package_members("tidyverse").contains(&"dplyr"));
        assert!(meta_package_members("tidyverse").contains(&"ggplot2"));
        assert!(meta_package_members("tidymodels").contains(&"dplyr"));
        // recipes carries NSE policies, so it must be a tidymodels member for a
        // bare `step_*` to resolve under `library(tidymodels)` alone.
        assert!(meta_package_members("tidymodels").contains(&"recipes"));
        // Bioconductor tidy-omics packages attach dplyr/tidyr generics whose S3
        // methods provide the object-specific data masks. The packages do not
        // export `pkg::filter` / `pkg::pivot_longer`; only bare calls should route
        // to the attached generic's NSE policy.
        assert_eq!(meta_package_members("plyranges"), &["dplyr"]);
        for package in ["tidySummarizedExperiment", "tidySingleCellExperiment"] {
            assert_eq!(meta_package_members(package), BIOC_TIDY_OMICS_META_MEMBERS);
        }
        assert!(meta_package_members("tidybulk").is_empty());
        assert!(meta_package_members("dplyr").is_empty());
    }

    #[test]
    fn bioc_tidy_omics_qualified_verbs_are_not_policy_aliases() {
        // Current Bioconductor releases attach dplyr/tidyr generics and register
        // S3 methods; they do not export qualified `pkg::filter` /
        // `pkg::pivot_longer` aliases. Keep those namespace-qualified spellings
        // standard-eval instead of inventing package-local policies.
        for (package, function) in [
            ("plyranges", "filter"),
            ("tidySummarizedExperiment", "filter"),
            ("tidySingleCellExperiment", "mutate"),
            ("tidySummarizedExperiment", "pivot_longer"),
            ("tidybulk", "filter"),
        ] {
            assert_eq!(package_policy(package, function), None);
        }
    }

    #[test]
    fn curve_captures_expr_checks_range() {
        // curve(sin(t), 0, 1): expr captured, from/to checked.
        let p = base_policy("curve").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, None, None]), false);
        assert_eq!(mask, vec![true, false, false]);
    }

    #[test]
    fn standard_eval_builtins_have_no_policy() {
        for name in ["paste", "c", "eval", "tryCatch", "stopifnot", "system.time"] {
            assert!(
                base_policy(name).is_none(),
                "{name} should be standard-eval"
            );
        }
    }
    #[test]
    fn native_interfaces_capture_routine_name_only() {
        for name in [
            ".Call",
            ".Call.graphics",
            ".C",
            ".Fortran",
            ".External",
            ".External2",
            ".External.graphics",
        ] {
            let policy = base_policy(name).unwrap_or_else(|| panic!("{name} should have a policy"));
            // .Call(C_symbol, evaluated_arg, PACKAGE = "pkg"): the native
            // routine name is special; regular arguments remain standard-eval.
            let mask =
                suppressed_arguments(&policy, &labels(&[None, None, Some("PACKAGE")]), false);
            assert_eq!(mask, vec![true, false, false], "{name}");
            assert_eq!(package_policy("base", name), base_policy(name));
        }
    }

    #[test]
    fn grid_native_wrappers_capture_routine_name_only() {
        for name in ["grid.Call", "grid.Call.graphics"] {
            let policy = package_policy("grid", name)
                .unwrap_or_else(|| panic!("{name} should have a policy"));
            let mask = suppressed_arguments(&policy, &labels(&[None, None]), false);
            assert_eq!(mask, vec![true, false], "{name}");
        }
    }

    #[test]
    fn unknown_package_function_has_no_policy() {
        assert!(package_policy("dplyr", "coalesce").is_none());
        assert!(package_policy("stats", "filter").is_none());
        assert!(package_policy("nonexistent", "filter").is_none());
    }

    #[test]
    fn base_namespace_delegates_to_base_policy() {
        // `base::rm`, `base::substitute`, etc. are the same functions as their
        // bare forms, so a namespace-qualified base call must resolve to the
        // identical NSE policy — not be silently downgraded to standard-eval by
        // `resolve_call_arg_policy`'s namespace-qualified branch.
        for name in [
            "rm",
            "remove",
            "substitute",
            "quote",
            "with",
            "subset",
            "transform",
            "library",
            "save",
        ] {
            assert_eq!(
                package_policy("base", name),
                base_policy(name),
                "package_policy(\"base\", {name:?}) should delegate to base_policy"
            );
        }
        // A base function with no NSE policy stays standard-eval (both `None`).
        assert_eq!(package_policy("base", "paste"), None);
        assert_eq!(package_policy("base", "paste"), base_policy("paste"));
    }

    #[test]
    fn builtin_nse_qualified_routing_respects_home_package() {
        // `data`/`help` live in utils and `curve` in graphics, not base — so the
        // correct qualified form must resolve to the builtin policy, while
        // `base::data` (invalid R) must not falsely suppress.
        assert_eq!(package_policy("utils", "data"), base_policy("data"));
        assert_eq!(package_policy("utils", "help"), base_policy("help"));
        assert_eq!(package_policy("graphics", "curve"), base_policy("curve"));
        assert!(package_policy("utils", "data").is_some());
        assert!(package_policy("graphics", "curve").is_some());

        // Wrong home -> no policy (the resolver then treats it as standard-eval).
        assert_eq!(package_policy("base", "data"), None);
        assert_eq!(package_policy("base", "help"), None);
        assert_eq!(package_policy("base", "curve"), None);
        assert_eq!(package_policy("graphics", "data"), None);

        // `subset` genuinely lives in base, so `base::subset` resolves and
        // `utils::subset` does not — the contrast that makes the routing real.
        assert_eq!(package_policy("base", "subset"), base_policy("subset"));
        assert!(package_policy("base", "subset").is_some());
        assert_eq!(package_policy("utils", "subset"), None);
    }

    #[test]
    fn example_captures_topic_but_checks_package_unlike_help() {
        // Verified against R 4.6.0: utils::example captures the bare `topic`
        // (NSE) but *evaluates* `package` (unlike help, which also captures
        // package). vignette/citation evaluate their arguments entirely, so
        // they are deliberately standard-eval (no policy).
        let p = base_policy("example").expect("example has an NSE policy");
        let mask = suppressed_arguments(&p, &labels(&[None, Some("package")]), false);
        assert_eq!(mask, vec![true, false], "topic captured, package checked");

        // Routed under its true home (utils), not base.
        assert_eq!(package_policy("utils", "example"), base_policy("example"));
        assert_eq!(package_policy("base", "example"), None);

        // vignette/citation are standard-eval: no NSE policy at any spelling.
        assert_eq!(base_policy("vignette"), None);
        assert_eq!(base_policy("citation"), None);
        assert_eq!(package_policy("utils", "vignette"), None);
        assert_eq!(package_policy("utils", "citation"), None);
    }

    // ---- Attached/ecosystem-package sweep additions (2026-06-06) ----

    #[test]
    fn base_alist_is_whole_call() {
        // alist() = as.list(sys.call())[-1L]: pure capture of every argument.
        assert_eq!(base_policy("alist"), Some(ArgPolicy::WholeCall));
        assert_eq!(package_policy("base", "alist"), Some(ArgPolicy::WholeCall));
    }

    #[test]
    fn utils_name_helpers_capture_object_routed_to_utils() {
        // getAnywhere(my_fn) / news(query = ...) etc. capture the bare object or
        // query; everything else (package, lib.loc, ...) is checked.
        let p = base_policy("getAnywhere").unwrap();
        assert_eq!(
            suppressed_arguments(&p, &labels(&[None]), false),
            vec![true]
        );
        let p = base_policy("news").unwrap();
        // news(query = Version > 2, package = "foo"): query suppressed, package checked.
        let mask = suppressed_arguments(&p, &labels(&[Some("query"), Some("package")]), false);
        assert_eq!(mask, vec![true, false]);
        let p = base_policy("demo").unwrap();
        assert_eq!(
            suppressed_arguments(&p, &labels(&[None]), false),
            vec![true]
        );
        // All seven route under the utils home, not base.
        for n in [
            "news",
            "getAnywhere",
            "argsAnywhere",
            "demo",
            "fix",
            "debugcall",
            "undebugcall",
        ] {
            assert_eq!(package_policy("utils", n), base_policy(n), "utils::{n}");
            assert_eq!(
                package_policy("base", n),
                None,
                "base::{n} must not resolve"
            );
        }
    }

    #[test]
    fn methods_has_arg_captures_name() {
        // hasArg(x): x is a symbol, never evaluated.
        let p = package_policy("methods", "hasArg").unwrap();
        assert_eq!(
            suppressed_arguments(&p, &labels(&[None]), false),
            vec![true]
        );
    }

    #[test]
    fn stats_model_fitters_suppress_subset_weights_check_formula_and_data() {
        // lm(fml, data = d, subset = grp, weights = w): formula + data checked,
        // subset + weights suppressed (data-masked). formula stays CHECKED so it
        // traverses raven's separate `~` handling.
        let p = package_policy("stats", "lm").unwrap();
        let mask = suppressed_arguments(
            &p,
            &labels(&[None, Some("data"), Some("subset"), Some("weights")]),
            false,
        );
        assert_eq!(mask, vec![false, false, true, true]);
        // glm additionally masks etastart/mustart/offset.
        let p = package_policy("stats", "glm").unwrap();
        let mask = suppressed_arguments(
            &p,
            &labels(&[None, Some("data"), Some("etastart"), Some("offset")]),
            false,
        );
        assert_eq!(mask, vec![false, false, true, true]);
        // xtabs masks only subset.
        let p = package_policy("stats", "xtabs").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, Some("subset")]), false);
        assert_eq!(mask, vec![false, true]);
        assert!(package_policy("stats", "loess").is_some());
        assert!(package_policy("stats", "nls").is_some());
        // stats is default-attached: a BARE `lm(...)` (no library(stats)) must
        // resolve via base_policy / step 3, not require stats to be in-play.
        assert!(base_policy("lm").is_some());
        assert!(base_policy("glm").is_some());
        assert!(base_policy("hasArg").is_some());
        // The invalid `base::lm` / `base::hasArg` must NOT resolve.
        assert_eq!(package_policy("base", "lm"), None);
        assert_eq!(package_policy("base", "hasArg"), None);
    }

    #[test]
    fn dplyr_sweep_additions() {
        assert_eq!(
            package_policy("dplyr", "join_by"),
            Some(ArgPolicy::WholeCall)
        );
        // top_n(df, 5, wt_col): x checked, n + wt suppressed.
        let p = package_policy("dplyr", "top_n").unwrap();
        assert_eq!(
            suppressed_arguments(&p, &labels(&[None, None, None]), false),
            vec![false, true, true]
        );
        // tally(df, wt = w): x checked, wt suppressed.
        let p = package_policy("dplyr", "tally").unwrap();
        assert_eq!(
            suppressed_arguments(&p, &labels(&[None, Some("wt")]), false),
            vec![false, true]
        );
        // rename_with(df, toupper, cols, extra): .cols suppressed; .fn and the
        // trailing dots are CHECKED (the corrected dots_captured=false).
        let p = package_policy("dplyr", "rename_with").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, None, None, None]), false);
        assert_eq!(mask, vec![false, false, true, false]);
    }

    #[test]
    fn tidyr_nest_captures_by_and_key_checks_names_sep() {
        // nest(df, x, .by = g, .key = k, .names_sep = "_"): `.data` is supplied
        // positionally (df checked); the bare `x` is a packed column (dots,
        // captured); `.by` (grouping columns) and `.key` (bare output name) are
        // captured; `.names_sep` is a literal string and stays checked.
        let p = package_policy("tidyr", "nest").unwrap();
        let mask = suppressed_arguments(
            &p,
            &labels(&[None, None, Some(".by"), Some(".key"), Some(".names_sep")]),
            false,
        );
        assert_eq!(mask, vec![false, true, true, true, false]);
    }

    #[test]
    fn tidyr_chop_captures_named_cols_and_by() {
        // chop(df, cols = c(a, b), by = g): data checked; the `cols` and `by`
        // tidy-selections are captured.
        let p = package_policy("tidyr", "chop").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, Some("cols"), Some("by")]), false);
        assert_eq!(mask, vec![false, true, true]);
        // chop(df, x, y): the deprecated positional cols land in `...` and are
        // captured; `data` checked.
        let mask = suppressed_arguments(&p, &labels(&[None, None, None]), false);
        assert_eq!(mask, vec![false, true, true]);
    }

    #[test]
    fn tidyr_sweep_additions() {
        // chop(df, c(a, b)): cols suppressed, data checked.
        let p = package_policy("tidyr", "chop").unwrap();
        assert_eq!(
            suppressed_arguments(&p, &labels(&[None, None]), false),
            vec![false, true]
        );
        // uncount(df, n): weights suppressed.
        let p = package_policy("tidyr", "uncount").unwrap();
        assert_eq!(
            suppressed_arguments(&p, &labels(&[None, None]), false),
            vec![false, true]
        );
        // separate_rows(df, a, b): the dots (column names) are suppressed.
        let p = package_policy("tidyr", "separate_rows").unwrap();
        assert_eq!(
            suppressed_arguments(&p, &labels(&[None, None, None]), false),
            vec![false, true, true]
        );
        assert!(package_policy("tidyr", "unchop").is_some());
        assert!(package_policy("tidyr", "separate_wider_delim").is_some());
    }

    #[test]
    fn tibble_constructors_capture_columns_including_dplyr_reexport() {
        // tibble(a = 1, b = a * 2): both columns suppressed (data-mask dots).
        let p = package_policy("tibble", "tibble").unwrap();
        assert_eq!(
            suppressed_arguments(&p, &labels(&[Some("a"), Some("b")]), false),
            vec![true, true]
        );
        // tibble(a = 1, .name_repair = x): .name_repair is a control -> checked.
        let mask = suppressed_arguments(&p, &labels(&[Some("a"), Some(".name_repair")]), false);
        assert_eq!(mask, vec![true, false]);
        assert!(package_policy("tibble", "tibble_row").is_some());
        // dplyr re-exports tibble/tibble_row (identical objects); the qualified
        // dplyr::tibble form must resolve to the same policy.
        assert_eq!(
            package_policy("dplyr", "tibble"),
            package_policy("tibble", "tibble")
        );
        assert_eq!(
            package_policy("dplyr", "tibble_row"),
            package_policy("tibble", "tibble_row")
        );
    }

    #[test]
    fn targets_selector_policies_capture_target_names() {
        for name in ["tar_make", "tar_make_future", "tar_make_clustermq"] {
            let p = package_policy("targets", name).unwrap();
            assert_eq!(
                suppressed_arguments(&p, &labels(&[None]), false),
                vec![true],
                "targets::{name} positional names"
            );
            assert_eq!(
                suppressed_arguments(&p, &labels(&[Some("names")]), false),
                vec![true],
                "targets::{name} named names"
            );
        }
        let p = package_policy("targets", "tar_make").unwrap();
        assert_eq!(
            suppressed_arguments(
                &p,
                &labels(&[None, Some("shortcut"), Some("reporter")]),
                false
            ),
            vec![true, false, false]
        );
        for (name, control) in [
            ("tar_make_future", "workers"),
            ("tar_make_clustermq", "log_worker"),
        ] {
            let p = package_policy("targets", name).unwrap();
            assert_eq!(
                suppressed_arguments(&p, &labels(&[None, Some(control)]), false),
                vec![true, false],
                "targets::{name} evaluates {control}"
            );
        }

        // tar_read(my_target): name suppressed.
        let p = package_policy("targets", "tar_read").unwrap();
        assert_eq!(
            suppressed_arguments(&p, &labels(&[None]), false),
            vec![true]
        );
        // tar_read(my_target, branches = b): name suppressed, branches checked.
        let mask = suppressed_arguments(&p, &labels(&[None, Some("branches")]), false);
        assert_eq!(mask, vec![true, false]);
        // tar_load(everything()): names suppressed.
        let p = package_policy("targets", "tar_load").unwrap();
        assert_eq!(
            suppressed_arguments(&p, &labels(&[None]), false),
            vec![true]
        );
        // tar_pattern(map(x), y): pattern suppressed; trailing dots CHECKED
        // (corrected dots_captured=false).
        let p = package_policy("targets", "tar_pattern").unwrap();
        assert_eq!(
            suppressed_arguments(&p, &labels(&[None, None]), false),
            vec![true, false]
        );
        // The existing tar_target entry is untouched.
        assert!(package_policy("targets", "tar_target").is_some());
        for n in [
            "tar_meta",
            "tar_objects",
            "tar_progress",
            "tar_branch_names",
            "tar_branches",
            "tar_deps",
        ] {
            assert!(package_policy("targets", n).is_some(), "targets::{n}");
        }
    }

    #[test]
    fn tier3_data_masker_additions() {
        // stats subset-maskers (same shape as lm/glm/xtabs), default-attached so
        // bare calls resolve via base_policy.
        let p = package_policy("stats", "oneway.test").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, Some("data"), Some("subset")]), false);
        assert_eq!(mask, vec![false, false, true]);
        assert!(base_policy("oneway.test").is_some());
        assert!(package_policy("stats", "factanal").is_some());
        assert_eq!(package_policy("base", "oneway.test"), None);

        // tidyr tidy-select `cols`/dots (siblings of chop/separate_wider_delim).
        let p = package_policy("tidyr", "unpack").unwrap();
        assert_eq!(
            suppressed_arguments(&p, &labels(&[None, None]), false),
            vec![false, true]
        );
        let p = package_policy("tidyr", "pack").unwrap();
        assert_eq!(
            suppressed_arguments(&p, &labels(&[None, None]), false),
            vec![false, true]
        );
        assert!(package_policy("tidyr", "separate_wider_position").is_some());
        assert!(package_policy("tidyr", "separate_wider_regex").is_some());

        // tibble::data_frame (deprecated alias of tibble) + dplyr re-export.
        let p = package_policy("tibble", "data_frame").unwrap();
        assert_eq!(
            suppressed_arguments(&p, &labels(&[Some("a"), Some("b")]), false),
            vec![true, true]
        );
        assert_eq!(
            package_policy("dplyr", "data_frame"),
            package_policy("tibble", "data_frame")
        );
    }

    #[test]
    fn capture_helpers_recognized() {
        for name in ["substitute", "enquo", "enexpr", "ensym"] {
            assert!(is_capture_helper(name));
        }
        for name in ["quote", "expr", "paste", "with"] {
            assert!(!is_capture_helper(name));
        }
    }

    #[test]
    fn plural_capture_helpers_recognized() {
        for name in ["enquos", "enexprs", "ensyms"] {
            assert!(is_plural_capture_helper(name));
        }
        for name in ["quos", "exprs", "syms", "enquo", "substitute"] {
            assert!(!is_plural_capture_helper(name));
        }
    }

    #[test]
    fn per_formal_named_arg_for_captured_formal_is_suppressed() {
        // f(env = typo_env, expr = col) for substitute-like (expr captured):
        // expr named -> suppressed; env named -> checked.
        let p = ArgPolicy::per_formal(&["expr", "env"], &["expr"], false);
        let mask = suppressed_arguments(&p, &labels(&[Some("env"), Some("expr")]), false);
        assert_eq!(mask, vec![false, true]);
    }

    #[test]
    fn positional_overflow_without_dots_routes_through_dots_policy() {
        // Only one formal, no `...`, captured_dots = false: extra positionals
        // are not suppressed.
        let p = ArgPolicy::per_formal(&["x"], &[], false);
        let mask = suppressed_arguments(&p, &labels(&[None, None, None]), false);
        assert_eq!(mask, vec![false, false, false]);
    }

    #[test]
    fn named_then_positional_skips_consumed_formal() {
        // g(b = 1, 2) with formals [a, b], a captured: `b` named -> checked;
        // positional `2` fills `a` (skipping consumed `b`) -> suppressed.
        let p = ArgPolicy::per_formal(&["a", "b"], &["a"], false);
        let mask = suppressed_arguments(&p, &labels(&[Some("b"), None]), false);
        assert_eq!(mask, vec![false, true]);
    }

    // --- Table-lock tests -------------------------------------------------
    // The mask-derivation logic above is exhaustively tested on a few
    // representative arms. The tests below instead pin the *variant shape*
    // (None / WholeCall / PerFormal) of every remaining `base_policy` and
    // `package_policy` arm so that a typo or accidental drop in those large
    // match tables — which would silently fall through to "evaluated normally"
    // — fails CI rather than regressing diagnostics quietly. They deliberately
    // do not re-derive masks; that is the job of the tests above.

    /// Coarse variant of a policy lookup: `None` (the table returned `None`,
    /// i.e. arguments are evaluated normally) vs the two NSE shapes.
    #[derive(Debug, PartialEq, Eq)]
    enum Shape {
        None,
        WholeCall,
        PerFormal,
    }

    fn shape(policy: Option<ArgPolicy>) -> Shape {
        match policy {
            std::option::Option::None => Shape::None,
            Some(ArgPolicy::Standard) => Shape::None,
            Some(ArgPolicy::WholeCall) => Shape::WholeCall,
            Some(ArgPolicy::PerFormal { .. }) => Shape::PerFormal,
        }
    }

    #[test]
    fn base_policy_arm_shapes() {
        use Shape::*;
        let cases: &[(&str, Shape)] = &[
            ("library", PerFormal),
            ("require", PerFormal),
            ("substitute", PerFormal),
            ("quote", PerFormal),
            ("bquote", PerFormal),
            ("expression", WholeCall),
            ("evalq", PerFormal),
            ("on.exit", PerFormal),
            ("curve", PerFormal),
            ("with", PerFormal),
            ("within", PerFormal),
            ("subset", PerFormal),
            ("transform", PerFormal),
            ("data", PerFormal),
            ("rm", PerFormal),
            ("remove", PerFormal),
            ("save", PerFormal),
            ("help", PerFormal),
            // Standard-eval base functions must stay None.
            ("paste", None),
            ("c", None),
            ("eval", None),
            ("tryCatch", None),
        ];
        for (name, want) in cases {
            assert_eq!(shape(base_policy(name)), *want, "base_policy({name:?})");
        }
    }

    #[test]
    fn dplyr_policy_arm_shapes() {
        use Shape::*;
        let cases: &[(&str, Shape)] = &[
            ("filter", PerFormal),
            ("slice", PerFormal),
            ("mutate", PerFormal),
            ("transmute", PerFormal),
            ("select", PerFormal),
            ("rename", PerFormal),
            ("distinct", PerFormal),
            ("summarise", PerFormal),
            ("summarize", PerFormal),
            ("reframe", PerFormal),
            ("arrange", PerFormal),
            ("group_by", PerFormal),
            ("rowwise", PerFormal),
            ("slice_max", PerFormal),
            ("slice_min", PerFormal),
            ("slice_head", PerFormal),
            ("slice_tail", PerFormal),
            ("slice_sample", PerFormal),
            ("relocate", PerFormal),
            ("count", PerFormal),
            ("add_count", PerFormal),
            ("pull", PerFormal),
            ("case_when", WholeCall),
            ("across", WholeCall),
            ("c_across", WholeCall),
            ("if_any", WholeCall),
            ("if_all", WholeCall),
            // Not in the curated table -> evaluated normally.
            ("coalesce", None),
            ("n", None),
        ];
        for (name, want) in cases {
            assert_eq!(
                shape(package_policy("dplyr", name)),
                *want,
                "package_policy(dplyr, {name:?})"
            );
        }
    }

    #[test]
    fn tidyr_policy_arm_shapes() {
        use Shape::*;
        let cases: &[(&str, Shape)] = &[
            ("pivot_longer", PerFormal),
            ("pivot_wider", PerFormal),
            ("unite", PerFormal),
            ("separate", PerFormal),
            ("extract", PerFormal),
            ("unnest", PerFormal),
            ("unnest_wider", PerFormal),
            ("unnest_longer", PerFormal),
            ("hoist", PerFormal),
            ("nest", PerFormal),
            ("fill", PerFormal),
            ("drop_na", PerFormal),
            ("complete", PerFormal),
            ("expand", PerFormal),
            ("gather", PerFormal),
            ("replace_na", None),
        ];
        for (name, want) in cases {
            assert_eq!(
                shape(package_policy("tidyr", name)),
                *want,
                "package_policy(tidyr, {name:?})"
            );
        }
    }

    #[test]
    fn other_package_policy_arm_shapes() {
        use Shape::*;
        let cases: &[(&str, &str, Shape)] = &[
            ("ggplot2", "aes", WholeCall),
            ("ggplot2", "vars", WholeCall),
            ("ggplot2", "qplot", PerFormal),
            ("ggplot2", "geom_point", None),
            ("rlang", "enquo", PerFormal),
            ("rlang", "enexpr", PerFormal),
            ("rlang", "ensym", PerFormal),
            ("rlang", "quo", PerFormal),
            ("rlang", "expr", PerFormal),
            ("rlang", "quos", WholeCall),
            ("rlang", "exprs", WholeCall),
            ("rlang", "enquos", WholeCall),
            ("rlang", "enexprs", WholeCall),
            ("rlang", "ensyms", WholeCall),
            ("rlang", "abort", None),
            ("data.table", "setkey", PerFormal),
            ("data.table", "setorder", PerFormal),
            ("data.table", "setindex", PerFormal),
            ("data.table", "fread", None),
            ("targets", "tar_target", PerFormal),
            ("targets", "tar_make", PerFormal),
            ("targets", "tar_make_future", PerFormal),
            ("targets", "tar_make_clustermq", PerFormal),
            ("targets", "tar_read", PerFormal),
            ("targets", "tar_load", PerFormal),
            ("targets", "tar_delete", None),
            ("stats", "lm", PerFormal),
            ("stats", "glm", PerFormal),
            ("stats", "filter", None),
            ("methods", "hasArg", PerFormal),
            ("methods", "setClass", None),
            ("tibble", "tibble", PerFormal),
            ("tibble", "tribble", None),
            ("dplyr", "join_by", WholeCall),
            ("dplyr", "tibble", PerFormal),
            ("tidytext", "unnest_tokens", PerFormal),
            ("tidytext", "bind_tf_idf", PerFormal),
            ("tidytext", "cast_dtm", None),
            ("modelr", "data_grid", PerFormal),
            ("modelr", "add_predictions", None),
            ("drake", "readd", PerFormal),
            ("drake", "loadd", None),
            // plyr `.()` is a plural quoting helper -> whole-call capture; the
            // *ply verbs (issue #467) carry a base per-formal policy whose
            // `...`-suppression is decided per call site by `.fun`.
            ("plyr", ".", WholeCall),
            ("plyr", "ddply", PerFormal),
            ("plyr", "llply", PerFormal),
            ("plyr", "summarise", PerFormal),
            ("plyr", "mutate", PerFormal),
            // r*ply takes `.n, .expr` (no `.fun`) -> deliberately unmodeled.
            ("plyr", "rdply", None),
            // Unknown package -> always None.
            ("nonesuch", "filter", None),
        ];
        for (pkg, name, want) in cases {
            assert_eq!(
                shape(package_policy(pkg, name)),
                *want,
                "package_policy({pkg:?}, {name:?})"
            );
        }
    }

    /// plyr's `.()` quotes every argument unevaluated (`. <- function(...,
    /// .env) structure(as.list(match.call()[-1]), ...)`), so it is a plural
    /// quoting helper like `rlang::quos` / base `alist` — every argument is a
    /// captured variable name, never a free reference. Modeled as `WholeCall`
    /// so `ddply(df, .(iso, year), f)` does not flag `iso`/`year`. (The plyr
    /// `*ply` verbs now carry a base per-formal policy of their own — issue
    /// #467 — whose `...`-suppression is decided per call site by `.fun`;
    /// independently of that, the nested `.()` call suppresses its own quoted
    /// columns.)
    #[test]
    fn plyr_dot_is_whole_call_quoting_helper() {
        assert_eq!(package_policy("plyr", "."), Some(ArgPolicy::WholeCall));
        // The `*ply` verbs are now modeled (issue #467) with a base per-formal
        // policy; their `...`-suppression is decided per call site by `.fun`.
        assert!(matches!(
            package_policy("plyr", "ddply"),
            Some(ArgPolicy::PerFormal { .. })
        ));
        assert!(matches!(
            package_policy("plyr", "llply"),
            Some(ArgPolicy::PerFormal { .. })
        ));
    }

    /// plyr's `summarise`/`summarize`/`mutate` evaluate their `...` in a data
    /// mask (formals `.data, ...`), so they capture their dots like the dplyr
    /// verbs of the same name. Modeled so both direct calls
    /// (`summarise(df, x = mean(y))`) and use as a split-apply `.fun` resolve
    /// correctly. `summarize` is an identical alias of `summarise`. Verified
    /// against plyr 1.8.9 + R 4.6.0.
    #[test]
    fn plyr_data_masking_verbs_capture_dots() {
        for name in ["summarise", "summarize", "mutate"] {
            match package_policy("plyr", name) {
                Some(ArgPolicy::PerFormal { captured_dots, .. }) => {
                    assert!(captured_dots, "plyr::{name} must capture its dots");
                }
                other => panic!("plyr::{name} should be PerFormal, got {other:?}"),
            }
        }
    }

    /// The 12 plyr `*ply` split-apply verbs that forward `...` into
    /// `.fun(piece, ...)` (the `a*`/`d*`/`l*` families; `r*ply` takes
    /// `.n, .expr` and `m*ply` splats `.fun`, so both are excluded) are modeled
    /// with a base per-formal policy that captures nothing: `.data`,
    /// `.variables`/`.margins`, and `.fun` stay checked, and the trailing `...`
    /// stay checked unless the call-site `.fun` resolves to a data-masking verb
    /// (the upgrade in `handlers.rs`). The formals carry `.fun` and `...`.
    /// Verified against plyr 1.8.9 + R 4.6.0.
    #[test]
    fn plyr_split_apply_verbs_base_policy() {
        for name in [
            "aaply", "adply", "alply", "a_ply", "daply", "ddply", "dlply", "d_ply", "laply",
            "ldply", "llply", "l_ply",
        ] {
            match package_policy("plyr", name) {
                Some(ArgPolicy::PerFormal {
                    formals,
                    captured,
                    captured_dots,
                }) => {
                    assert!(
                        captured.is_empty(),
                        "{name}: base policy captures nothing, got {captured:?}"
                    );
                    assert!(!captured_dots, "{name}: base policy must not capture dots");
                    assert!(
                        formals.iter().any(|f| f == ".fun"),
                        "{name}: formals must include .fun, got {formals:?}"
                    );
                    assert!(
                        formals.iter().any(|f| f == "..."),
                        "{name}: formals must include ..., got {formals:?}"
                    );
                }
                other => panic!("plyr::{name} should be PerFormal, got {other:?}"),
            }
        }
    }

    /// `.fun`'s positional index per family, pinning the empirically verified
    /// signatures: `a*ply`/`d*ply` are `(.data, .margins|.variables, .fun, ...)`
    /// (index 2); `l*ply` is `(.data, .fun, ...)` (index 1). `m*ply` is excluded
    /// (splat — see `is_plyr_split_apply_verb_recognition`).
    #[test]
    fn plyr_split_apply_fun_formal_position() {
        let fun_index = |verb: &str| match package_policy("plyr", verb) {
            Some(ArgPolicy::PerFormal { formals, .. }) => formals.iter().position(|f| f == ".fun"),
            _ => None,
        };
        for v in [
            "aaply", "adply", "alply", "a_ply", "daply", "ddply", "dlply", "d_ply",
        ] {
            assert_eq!(fun_index(v), Some(2), "{v}: .fun at index 2");
        }
        for v in ["laply", "ldply", "llply", "l_ply"] {
            assert_eq!(fun_index(v), Some(1), "{v}: .fun at index 1");
        }
    }

    /// `is_plyr_split_apply_verb` recognizes exactly the 12 `*ply` verbs that
    /// forward `...` directly into `.fun(piece, ...)` (the `a*`/`d*`/`l*`
    /// families) — never the `r*ply` family (no `.fun`), never the `m*ply`
    /// family (which wraps `.fun` in `splat()`, so its `...` are spread as
    /// ordinary args, not data-masked — verified against plyr 1.8.9), and never
    /// the `.()` quoting helper or the data-masking verbs themselves.
    #[test]
    fn is_plyr_split_apply_verb_recognition() {
        for name in [
            "aaply", "adply", "alply", "a_ply", "daply", "ddply", "dlply", "d_ply", "laply",
            "ldply", "llply", "l_ply",
        ] {
            assert!(
                is_plyr_split_apply_verb(name),
                "{name} should be a split-apply verb"
            );
        }
        for name in [
            // m*ply splats `.fun` -> `...` are not data-masked (issue #467 f/u).
            "maply",
            "mdply",
            "mlply",
            "m_ply",
            // r*ply takes `.n, .expr` (no `.fun`).
            "raply",
            "rdply",
            "rlply",
            "r_ply",
            // Not split-apply verbs at all.
            ".",
            "summarise",
            "mutate",
            "ddplyr",
            "ply",
            "",
        ] {
            assert!(
                !is_plyr_split_apply_verb(name),
                "{name:?} must not be a split-apply verb"
            );
        }
    }

    // --- §5 attached-package sweep: gt / gtsummary / recipes / dbplyr -----

    #[test]
    fn gt_fmt_suppresses_columns_and_rows_checks_decimals() {
        // fmt_number(data, columns = aa, rows = aa > 1, decimals = n): columns
        // and rows are NSE; `decimals` evaluates in the caller's env, so it is
        // checked (absorbed by dots, captured_dots=false).
        let p = package_policy("gt", "fmt_number").unwrap();
        let mask = suppressed_arguments(
            &p,
            &labels(&[None, Some("columns"), Some("rows"), Some("decimals")]),
            false,
        );
        assert_eq!(mask, vec![false, true, true, false]);
    }

    #[test]
    fn gt_data_color_suppresses_target_columns() {
        // data_color(data, columns = aa, target_columns = bb): both column
        // selectors are suppressed; `data` checked.
        let p = package_policy("gt", "data_color").unwrap();
        let mask = suppressed_arguments(
            &p,
            &labels(&[None, Some("columns"), Some("target_columns")]),
            false,
        );
        assert_eq!(mask, vec![false, true, true]);
    }

    #[test]
    fn gtsummary_tbl_summary_suppresses_by_include_checks_label() {
        // tbl_summary(data, by = grp, label = age ~ "Age", include = c(age)):
        // by/include are tidyselect (suppressed); `label` is a `~` formula list
        // left checked for raven's formula path; `data` checked.
        let p = package_policy("gtsummary", "tbl_summary").unwrap();
        let mask = suppressed_arguments(
            &p,
            &labels(&[None, Some("by"), Some("label"), Some("include")]),
            false,
        );
        assert_eq!(mask, vec![false, true, false, true]);
        // (positional order: data, by, label) -> by suppressed, label checked.
        let mask = suppressed_arguments(&p, &labels(&[None, None, None]), false);
        assert_eq!(mask, vec![false, true, false]);
    }

    #[test]
    fn recipes_step_suppresses_columns_checks_recipe() {
        // step_center(rec, aa, bb): recipe checked, selected columns suppressed.
        let p = package_policy("recipes", "step_center").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, None, None]), false);
        assert_eq!(mask, vec![false, true, true]);
        // Prefix match also covers steps not individually enumerated.
        assert!(package_policy("recipes", "step_zzz_future").is_some());
        // Pipe-fed: `rec %>% step_center(aa)` -> the single positional is a
        // selected column, suppressed (the pipe supplies `recipe`).
        let mask = suppressed_arguments(&p, &labels(&[None]), true);
        assert_eq!(mask, vec![true]);
    }

    #[test]
    fn recipes_update_role_checks_new_role() {
        // update_role(rec, aa, new_role = "predictor"): the selected column is
        // suppressed; `new_role` binds its named formal and stays checked.
        let p = package_policy("recipes", "update_role").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, None, Some("new_role")]), false);
        assert_eq!(mask, vec![false, true, false]);
    }

    #[test]
    fn dbplyr_window_order_suppresses_order_columns() {
        // window_order(.data, aa): the ordering column is data-masked.
        let p = package_policy("dbplyr", "window_order").unwrap();
        let mask = suppressed_arguments(&p, &labels(&[None, None]), false);
        assert_eq!(mask, vec![false, true]);
    }

    #[test]
    fn survival_tmerge_suppresses_id_and_dots_keeps_data() {
        // tmerge(data1, data2, id = idcol, death = event(t, s), x = tdc(t, v)):
        // data1/data2 stay checked; `id` and every `...` term are suppressed.
        let p = package_policy("survival", "tmerge").unwrap();
        let mask = suppressed_arguments(
            &p,
            &labels(&[None, None, Some("id"), Some("death"), Some("x")]),
            false,
        );
        assert_eq!(mask, vec![false, false, true, true, true]);
    }

    #[test]
    fn survival_tmerge_suppresses_tstart_tstop_flags_options() {
        // tstart and tstop are data-masked (column expressions in data1), so
        // they must be suppressed. `options` is a plain control list — flagged.
        let p = package_policy("survival", "tmerge").unwrap();
        let mask = suppressed_arguments(
            &p,
            &labels(&[
                None,
                None,
                Some("id"),
                Some("ev"),
                Some("tstart"),
                Some("tstop"),
                Some("options"),
            ]),
            false,
        );
        // data1, data2: checked; id: masked; ev: dots-captured;
        // tstart, tstop: data-masked → suppressed; options: control → flagged.
        assert_eq!(mask, vec![false, false, true, true, true, true, false]);
    }

    #[test]
    fn survival_non_tmerge_has_no_policy() {
        assert!(package_policy("survival", "coxph").is_none());
    }

    #[test]
    fn section5_package_policy_arm_shapes() {
        use Shape::*;
        let cases: &[(&str, &str, Shape)] = &[
            // gt: fmt_*/sub_* prefix, cells_* whole-call, enumerated cols_*.
            ("gt", "fmt_number", PerFormal),
            ("gt", "fmt_currency", PerFormal),
            ("gt", "sub_missing", PerFormal),
            ("gt", "cells_body", WholeCall),
            ("gt", "cells_title", WholeCall),
            ("gt", "cols_hide", PerFormal),
            ("gt", "cols_move", PerFormal),
            ("gt", "cols_merge", PerFormal),
            ("gt", "tab_spanner", PerFormal),
            ("gt", "data_color", PerFormal),
            ("gt", "summary_rows", PerFormal),
            ("gt", "row_group_order", PerFormal),
            // tab_style nests cells_* helpers (which carry their own policy);
            // its own args are evaluated -> None.
            ("gt", "tab_style", None),
            ("gt", "gt", None),
            // gtsummary: only the tidyselect builders.
            ("gtsummary", "tbl_summary", PerFormal),
            ("gtsummary", "tbl_continuous", PerFormal),
            ("gtsummary", "tbl_cross", PerFormal),
            ("gtsummary", "modify_header", None),
            ("gtsummary", "add_p", None),
            // recipes: step_* prefix, enumerated role + user-facing checks.
            ("recipes", "step_center", PerFormal),
            ("recipes", "step_dummy", PerFormal),
            ("recipes", "step_mutate", PerFormal),
            ("recipes", "update_role", PerFormal),
            ("recipes", "add_role", PerFormal),
            ("recipes", "remove_role", PerFormal),
            ("recipes", "check_missing", PerFormal),
            ("recipes", "check_range", PerFormal),
            // Internal check_* helpers and non-step exports stay None.
            ("recipes", "check_type", None),
            ("recipes", "check_options", None),
            ("recipes", "recipe", None),
            ("recipes", "bake", None),
            // dbplyr: only window_order; SQL/string helpers stay checked.
            ("dbplyr", "window_order", PerFormal),
            ("dbplyr", "window_frame", None),
            ("dbplyr", "sql", None),
            // Surveyed-but-no-additions families (standard-eval / lexical scope).
            ("shiny", "reactive", None),
            ("shiny", "observe", None),
            ("shiny", "renderPlot", None),
            ("shiny", "isolate", None),
            ("parsnip", "fit", None),
            ("parsnip", "set_engine", None),
            ("DBI", "dbGetQuery", None),
            ("DBI", "dbExecute", None),
        ];
        for (pkg, name, want) in cases {
            assert_eq!(
                shape(package_policy(pkg, name)),
                *want,
                "package_policy({pkg:?}, {name:?})"
            );
        }
    }
}
