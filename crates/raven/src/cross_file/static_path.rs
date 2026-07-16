//
// cross_file/static_path.rs
//
// Static folding of computed `source()` path expressions (issue #638).
//
// Folds the common "computed path" idioms used in `source()` file arguments
// into a plain path string at analysis time:
//
// - `file.path("a", "b")` with every part statically foldable → `"a/b"`
// - `normalizePath(expr)` → pure syntactic unwrap of `expr` (see caveat below)
// - a local variable bound exactly once, at the top level, before use, to a
//   foldable expression (`repo_root <- normalizePath(file.path("..", ".."))`)
//
// Folding is strict all-or-nothing, mirroring `try_parse_system_file_call` in
// `source_detect.rs`: any component that is not statically foldable makes the
// whole expression fold to `None` (never a partial or guessed path).
//
// CAVEAT — conservative lexical heuristic: `normalizePath()` is peeled
// syntactically; at runtime it returns an absolute, symlink-resolved path.
// Raven's resolver only performs lexical normalization and deliberately never
// canonicalizes symlinks, and this module likewise ignores `setwd()` calls.
// This matches the limitation class of every other static path analysis in
// Raven (a literal `source("x.R")` is equally blind to `setwd()`).
//
// This module is the single folding implementation shared by
// `source_detect.rs` (detection → dependency edges → scope → diagnostics) and
// `file_path_intellisense.rs` (go-to-definition), so the two surfaces cannot
// drift.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

/// Single-assignment variable bindings usable in static path folding,
/// collected once per document from the full AST.
///
/// A name resolves at a use site iff:
/// - exactly one binding form targets it in the whole file
///   (`binding_count == 1`),
/// - that binding is an unconditional top-level `<-`/`=` assignment, and
/// - the assignment starts strictly before the use site (textual
///   "declared, then used" order — deliberately not scope-aware, matching the
///   existing apply-family `VarBinding` precedent in `source_detect.rs`).
///
/// The use-site ordering check runs *before* the memo is consulted; the memo
/// caches only the intrinsic fold of a binding's fixed RHS (which is
/// use-site-independent), so a use-before-assignment site can never poison a
/// later valid use. A separate `visiting` set guards against reference
/// cycles.
pub(crate) struct StaticBindings<'tree, 'text> {
    content: &'text str,
    bindings: HashMap<String, super::binding::Binding<Node<'tree>>>,
    /// Memoized intrinsic fold of each candidate's RHS, keyed by name.
    memo: RefCell<HashMap<String, Option<String>>>,
    /// Names currently being folded (cycle guard).
    visiting: RefCell<HashSet<String>>,
}

impl<'tree, 'text> StaticBindings<'tree, 'text> {
    /// Collect binding facts for the whole document rooted at `root`.
    pub(crate) fn collect(root: Node<'tree>, content: &'text str) -> Self {
        use super::binding::{AssignmentOperator, BindingSite};

        let bindings = super::binding::collect_bindings(root, content, |site| match site {
            BindingSite::Binary {
                target,
                value: Some(value),
                operator: AssignmentOperator::Left | AssignmentOperator::Equals,
                top_level: true,
                ..
            } if target.kind() == "identifier" => Some(value),
            _ => None,
        });
        Self {
            content,
            bindings,
            memo: RefCell::new(HashMap::new()),
            visiting: RefCell::new(HashSet::new()),
        }
    }

    /// Resolve `name` at a use site starting at byte `use_byte`, folding the
    /// binding's RHS recursively. Returns `None` when the name is not a valid
    /// single-assignment candidate, is assigned at or after the use site, its
    /// RHS is not foldable, or resolution would cycle.
    fn resolve(&self, name: &str, use_byte: usize) -> Option<String> {
        let info = self.bindings.get(name)?;
        let rhs = *info.resolved_before(use_byte)?;
        // Use-site gate passed; the RHS fold itself is use-site-independent.
        if let Some(cached) = self.memo.borrow().get(name) {
            return cached.clone();
        }
        if !self.visiting.borrow_mut().insert(name.to_string()) {
            return None; // cycle
        }
        let folded = fold_string_expr(rhs, self.content, self);
        self.visiting.borrow_mut().remove(name);
        self.memo
            .borrow_mut()
            .insert(name.to_string(), folded.clone());
        folded
    }

    /// Whether any binding form in the document targets `name`.
    fn is_bound(&self, name: &str) -> bool {
        self.bindings.contains_key(name)
    }
}

/// Statically fold a path-valued expression to a `String`.
///
/// Handles exactly three forms (strict all-or-nothing; anything else → `None`):
/// - a string literal (raw text between the quotes; a literal containing a
///   backslash bails, so folded paths never diverge from R's unescaped
///   runtime string),
/// - a bare identifier resolved through `bindings`,
/// - a call to `file.path(...)` or `normalizePath(...)`.
pub(crate) fn fold_string_expr(
    node: Node,
    content: &str,
    bindings: &StaticBindings,
) -> Option<String> {
    match node.kind() {
        "string" => extract_plain_string(node, content),
        "identifier" => bindings.resolve(node_text(node, content), node.start_byte()),
        "call" => {
            let func = node.child_by_field_name("function")?;
            match node_text(func, content) {
                "file.path" if !bindings.is_bound("file.path") => {
                    fold_file_path_call(node, content, bindings)
                }
                "normalizePath" if !bindings.is_bound("normalizePath") => {
                    fold_normalize_path_call(node, content, bindings)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Fold `file.path(part1, part2, ..., fsep = "/")` by folding every
/// positional part and joining with `/`.
///
/// `fsep=` is accepted only as the literal default `"/"`. Any OTHER named
/// argument bails the whole call: in R, `file.path` has only `fsep` as a real
/// formal — every other named argument is swallowed into `...` and becomes a
/// path component (`file.path("a", b = "x")` is `"a/x"`), and reproducing
/// that argument-ordering subtlety statically is not worth the risk of
/// folding a wrong path.
fn fold_file_path_call(node: Node, content: &str, bindings: &StaticBindings) -> Option<String> {
    let args_node = node.child_by_field_name("arguments")?;
    if args_node.has_error() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() != "argument" {
            continue;
        }
        if let Some(name_node) = child.child_by_field_name("name") {
            let name = node_text(name_node, content);
            if name != "fsep" {
                return None;
            }
            let value_node = child.child_by_field_name("value")?;
            let text = node_text(value_node, content);
            if text != "\"/\"" && text != "'/'" {
                return None;
            }
        } else {
            let value_node = child.child_by_field_name("value")?;
            let part = fold_string_expr(value_node, content, bindings)?;
            parts.push(part);
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// Fold `normalizePath(path, winslash =, mustWork =)` by recursing into the
/// `path` argument (named `path=` or first positional). Pure syntactic
/// unwrap: `winslash`/`mustWork` are ignored without inspecting their values
/// (they don't change which file the path denotes); any other named argument
/// bails (`normalizePath` has no `...`, so an unknown name is either a typo
/// or something we can't reason about).
fn fold_normalize_path_call(
    node: Node,
    content: &str,
    bindings: &StaticBindings,
) -> Option<String> {
    let args_node = node.child_by_field_name("arguments")?;
    if args_node.has_error() {
        return None;
    }
    let mut path_node: Option<Node> = None;
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() != "argument" {
            continue;
        }
        if let Some(name_node) = child.child_by_field_name("name") {
            match node_text(name_node, content) {
                "path" => {
                    path_node = Some(child.child_by_field_name("value")?);
                }
                "winslash" | "mustWork" => {}
                _ => return None,
            }
        } else if path_node.is_none() {
            path_node = Some(child.child_by_field_name("value")?);
        }
    }
    fold_string_expr(path_node?, content, bindings)
}

/// Locate the file-argument's value node of a `source()`/`sys.source()`
/// call's `arguments` node: the named `file=` argument if present, else the
/// first positional argument.
///
/// Shared by `source_detect::try_parse_source_call` (detection) and
/// `file_path_intellisense::extract_full_source_call_path` (go-to-definition)
/// so the two surfaces can never disagree on which node is the file argument.
pub(crate) fn source_call_file_value_node<'a>(
    args_node: &Node<'a>,
    content: &str,
) -> Option<Node<'a>> {
    let mut cursor = args_node.walk();
    let children: Vec<_> = args_node.children(&mut cursor).collect();
    for child in &children {
        if child.kind() == "argument"
            && let Some(name_node) = child.child_by_field_name("name")
            && node_text(name_node, content) == "file"
        {
            return child.child_by_field_name("value");
        }
    }
    for child in &children {
        if child.kind() == "argument" && child.child_by_field_name("name").is_none() {
            return child.child_by_field_name("value");
        }
    }
    None
}

/// Extract a plain (escape-free) string literal's contents. A literal
/// containing a backslash is rejected: this module never processes escape
/// sequences, so folding `"a\tb"` raw would silently diverge from the string
/// R actually builds.
fn extract_plain_string(node: Node, content: &str) -> Option<String> {
    let text = node_text(node, content);
    if !((text.starts_with('"') && text.ends_with('"'))
        || (text.starts_with('\'') && text.ends_with('\'')))
    {
        return None;
    }
    let inner = &text[1..text.len() - 1];
    if inner.contains('\\') {
        return None;
    }
    Some(inner.to_string())
}

fn node_text<'a>(node: Node<'a>, content: &'a str) -> &'a str {
    &content[node.byte_range()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::{Parser, Tree};

    fn parse(code: &str) -> Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        parser.parse(code, None).unwrap()
    }

    /// Fold the file argument of the LAST source() call in `code`.
    fn fold_last_source_arg(code: &str) -> Option<String> {
        let tree = parse(code);
        let root = tree.root_node();
        let bindings = StaticBindings::collect(root, code);
        let mut result = None;
        fn visit(node: Node, code: &str, bindings: &StaticBindings, out: &mut Option<String>) {
            if node.kind() == "call"
                && let Some(func) = node.child_by_field_name("function")
                && &code[func.byte_range()] == "source"
                && let Some(args) = node.child_by_field_name("arguments")
                && let Some(value) = source_call_file_value_node(&args, code)
            {
                *out = fold_string_expr(value, code, bindings);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit(child, code, bindings, out);
            }
        }
        visit(root, code, &bindings, &mut result);
        result
    }

    #[test]
    fn folds_literal_file_path() {
        assert_eq!(
            fold_last_source_arg(r#"source(file.path("a", "b.R"))"#),
            Some("a/b.R".to_string())
        );
    }

    #[test]
    fn folds_nested_file_path() {
        assert_eq!(
            fold_last_source_arg(r#"source(file.path(file.path("a", "b"), "c.R"))"#),
            Some("a/b/c.R".to_string())
        );
    }

    #[test]
    fn folds_normalize_path_wrapper() {
        assert_eq!(
            fold_last_source_arg(r#"source(normalizePath(file.path("..", "x.R")))"#),
            Some("../x.R".to_string())
        );
    }

    #[test]
    fn folds_normalize_path_named_path_and_ignored_args() {
        assert_eq!(
            fold_last_source_arg(
                r#"source(normalizePath(mustWork = FALSE, path = file.path("a", "b.R"), winslash = "/"))"#
            ),
            Some("a/b.R".to_string())
        );
    }

    #[test]
    fn bails_on_normalize_path_unknown_named_arg() {
        assert_eq!(
            fold_last_source_arg(r#"source(normalizePath(file.path("a"), bogus = 1))"#),
            None
        );
    }

    #[test]
    fn folds_issue_repro_variable_chain() {
        let code = r#"
repo_root <- normalizePath(file.path("..", ".."))
source(file.path(repo_root, "scripts/helpers.R"))
"#;
        assert_eq!(
            fold_last_source_arg(code),
            Some("../../scripts/helpers.R".to_string())
        );
    }

    #[test]
    fn folds_variable_as_whole_path() {
        let code = r#"
p <- file.path("scripts", "helpers.R")
source(p)
"#;
        assert_eq!(
            fold_last_source_arg(code),
            Some("scripts/helpers.R".to_string())
        );
    }

    #[test]
    fn folds_two_hop_variable_chain() {
        let code = r#"
a <- ".."
b <- file.path(a, "scripts")
source(file.path(b, "helpers.R"))
"#;
        assert_eq!(
            fold_last_source_arg(code),
            Some("../scripts/helpers.R".to_string())
        );
    }

    #[test]
    fn bails_on_reassigned_variable() {
        let code = r#"
p <- "a.R"
p <- "b.R"
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);
    }

    #[test]
    fn bails_on_use_before_assignment() {
        let code = r#"
source(p)
p <- "a.R"
"#;
        assert_eq!(fold_last_source_arg(code), None);
    }

    #[test]
    fn later_valid_use_not_poisoned_by_earlier_invalid_use() {
        // The first source(p) is use-before-assignment (must not fold); the
        // second is valid. A name-keyed result memo that cached the first
        // failure would poison the second — this guards the memo design.
        let code = r#"
source(p)
p <- "a.R"
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), Some("a.R".to_string()));
    }

    #[test]
    fn bails_on_function_scoped_assignment() {
        let code = r#"
f <- function() {
  p <- "dev.R"
}
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);
    }

    #[test]
    fn bails_on_conditional_assignment() {
        let code = r#"
if (FALSE) p <- "dev.R"
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);
    }

    #[test]
    fn bails_on_for_loop_variable() {
        let code = r#"
for (p in c("a.R", "b.R")) {
  source(p)
}
"#;
        assert_eq!(fold_last_source_arg(code), None);
    }

    #[test]
    fn bails_when_assign_call_also_targets_name() {
        let code = r#"
p <- "a.R"
assign("p", "b.R")
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);
    }

    #[test]
    fn erroneous_assign_call_does_not_count_as_a_binding() {
        // R rejects the exact+partial collision before assigning. The shared
        // collector must apply the same argument-matching rule to path and
        // package-vector candidates.
        let code = r#"
p <- "a.R"
assign(x = "p", value = "b.R", val = "c.R")
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), Some("a.R".to_string()));
    }

    #[test]
    fn bails_when_file_path_is_shadowed() {
        let code = r#"
file.path <- function(...) "other.R"
source(file.path("a.R"))
"#;
        assert_eq!(fold_last_source_arg(code), None);
    }

    #[test]
    fn bails_when_normalize_path_is_shadowed() {
        let code = r#"
normalizePath <- function(...) "other.R"
source(normalizePath("a.R"))
"#;
        assert_eq!(fold_last_source_arg(code), None);
    }

    #[test]
    fn bails_after_replacement_assignment_to_binding_root() {
        for code in [
            r#"
p <- "a.R"
p[1] <- "b.R"
source(p)
"#,
            r#"
p <- "a.R"
names(p) <- "path"
source(p)
"#,
        ] {
            assert_eq!(fold_last_source_arg(code), None);
        }
    }

    #[test]
    fn bails_after_remove_call_targets_binding() {
        for remove in ["rm(p)", "remove(\"p\")", "rm(list = \"p\")"] {
            let code = format!("p <- \"a.R\"\n{remove}\nsource(p)\n");
            assert_eq!(fold_last_source_arg(&code), None, "{remove}");
        }
    }

    #[test]
    fn bails_on_reassignment_with_inline_comment() {
        // tree-sitter-r attaches the comment as a named child of the
        // binary_operator; an arity-based collector would skip the rebinding
        // and wrongly fold to "a.R" while R sources "b.R".
        let code = "p <- \"a.R\"\np <- # tweak\n  \"b.R\"\nsource(p)\n";
        assert_eq!(fold_last_source_arg(code), None);
    }

    #[test]
    fn bails_on_quoted_name_reassignment() {
        // `"p" <- "b.R"` rebinds `p` exactly like a bare-identifier LHS.
        let code = r#"
p <- "a.R"
"p" <- "b.R"
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);
    }

    #[test]
    fn bails_on_compound_assignment_pipe() {
        let code = r#"
p <- "a.R"
p %<>% toupper()
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);
    }

    #[test]
    fn bails_when_shadow_assignment_has_inline_comment() {
        // The comment must not hide the shadowing of the folding helper.
        let code =
            "file.path <- # override\n  function(...) \"other.R\"\nsource(file.path(\"a.R\"))\n";
        assert_eq!(fold_last_source_arg(code), None);
    }

    #[test]
    fn bails_on_function_parameter_shadow() {
        let code = r#"
p <- "a.R"
f <- function(p) p
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);
    }

    #[test]
    fn bails_on_named_file_path_component() {
        // In R, file.path("a", b = "x") makes "x" a path component; folding
        // must bail rather than silently drop or misplace it.
        assert_eq!(
            fold_last_source_arg(r#"source(file.path("a", b = "x.R"))"#),
            None
        );
    }

    #[test]
    fn accepts_default_fsep_only() {
        assert_eq!(
            fold_last_source_arg(r#"source(file.path("a", "b.R", fsep = "/"))"#),
            Some("a/b.R".to_string())
        );
        assert_eq!(
            fold_last_source_arg(r#"source(file.path("a", "b.R", fsep = "\\"))"#),
            None
        );
    }

    #[test]
    fn bails_on_empty_file_path() {
        assert_eq!(fold_last_source_arg(r#"source(file.path())"#), None);
    }

    #[test]
    fn bails_on_non_foldable_component() {
        assert_eq!(
            fold_last_source_arg(r#"source(file.path(Sys.getenv("ROOT"), "x.R"))"#),
            None
        );
        assert_eq!(fold_last_source_arg(r#"source(paste0("a", "b.R"))"#), None);
    }

    #[test]
    fn bails_on_escaped_string_literal() {
        assert_eq!(
            fold_last_source_arg(r#"source(file.path("a\tb", "x.R"))"#),
            None
        );
    }

    #[test]
    fn self_referential_binding_hits_cycle_guard() {
        // `p <- file.path(p, "x.R")` references itself inside its own RHS;
        // the byte-order check alone would recurse forever without the
        // visiting-set guard.
        let code = r#"
p <- file.path(p, "x.R")
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);
    }

    #[test]
    fn plain_string_argument_folds() {
        assert_eq!(
            fold_last_source_arg(r#"source("x.R")"#),
            Some("x.R".to_string())
        );
    }
}
