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

#[cfg(test)]
thread_local! {
    static COLLECTION_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn collection_count_for_current_thread() -> usize {
    COLLECTION_COUNT.with(std::cell::Cell::get)
}

/// Neutral payload collected at one statically named binding site.
///
/// Path and package-vector eligibility are intentionally independent: ordinary
/// top-level assignments may supply a path RHS, while package vectors additionally
/// require a trusted bare `c()` and may also come from eligible `assign()` calls.
/// Keeping both options in one payload lets every artifact consumer share one
/// full-tree binding collection without widening either policy.
#[derive(Debug)]
struct StaticCandidate<'tree> {
    path_rhs: Option<Node<'tree>>,
    package_vector: Option<Vec<String>>,
}

/// Single-assignment facts usable by static path and package-vector consumers,
/// collected once per document from the full AST.
///
/// A candidate resolves at a use site iff exactly one binding form targets its
/// name in the whole file and that binding starts strictly before the use site.
/// Each payload field then applies its own eligibility policy. Path folding also
/// memoizes the intrinsic fold of a fixed RHS after the use-site ordering gate;
/// a separate `visiting` set guards against reference cycles.
pub(crate) struct StaticBindings<'tree, 'text> {
    content: &'text str,
    bindings: HashMap<String, super::binding::Binding<StaticCandidate<'tree>>>,
    /// Memoized intrinsic fold of each candidate's RHS, keyed by name.
    ///
    /// Participating literals are retained with the value so callback-enabled
    /// folds can replay the same traversal result on a memo hit.
    memo: RefCell<HashMap<String, MemoizedFold<'tree>>>,
    /// Names currently being folded (cycle guard).
    visiting: RefCell<HashSet<String>>,
}

#[derive(Clone)]
struct MemoizedFold<'tree> {
    value: Option<String>,
    path_literals: Vec<Node<'tree>>,
}

impl<'tree, 'text> StaticBindings<'tree, 'text> {
    /// Collect binding facts for the whole document rooted at `root`.
    pub(crate) fn collect(root: Node<'tree>, content: &'text str) -> Self {
        use super::binding::{AssignmentOperator, BindingSite};

        #[cfg(test)]
        COLLECTION_COUNT.with(|count| count.set(count.get() + 1));

        let bindings = super::binding::collect_bindings(root, content, |site| {
            let (path_rhs, package_rhs) = match site {
                BindingSite::Binary {
                    target,
                    value,
                    operator: AssignmentOperator::Left | AssignmentOperator::Equals,
                    top_level: true,
                    helpers_trusted,
                    ..
                } if target.kind() == "identifier" => {
                    (value, helpers_trusted.then_some(value).flatten())
                }
                BindingSite::AssignCall {
                    value,
                    helpers_trusted: true,
                    ..
                } => (None, value),
                _ => (None, None),
            };
            let package_vector =
                package_rhs.and_then(|value| extract_package_vector(value, content));
            (path_rhs.is_some() || package_vector.is_some()).then_some(StaticCandidate {
                path_rhs,
                package_vector,
            })
        });
        Self {
            content,
            bindings,
            memo: RefCell::new(HashMap::new()),
            visiting: RefCell::new(HashSet::new()),
        }
    }

    /// Resolve `name` at `use_node`, folding the binding's RHS recursively.
    /// Returns `None` when the name is not a valid single-assignment candidate,
    /// is assigned at or after the use, may have been replaced by a relevant
    /// unknown-name load, has an unfodable RHS, or resolution would cycle.
    fn resolve(
        &self,
        name: &str,
        use_node: Node<'tree>,
        on_path_literal: &mut dyn FnMut(Node<'tree>),
    ) -> Option<String> {
        let info = self.bindings.get(name)?;
        let (candidate, candidate_offset) =
            info.resolved_with_offset_before(use_node.start_byte())?;
        if super::binding::unknown_loaded_names_may_affect_candidate(
            &self.bindings,
            candidate_offset,
            use_node,
            helper_use_is_deferred(use_node, self.content),
        ) {
            return None;
        }
        let rhs = candidate.path_rhs?;
        // Use-site gate passed; the RHS fold itself is use-site-independent.
        if let Some(cached) = self.memo.borrow().get(name) {
            for literal in &cached.path_literals {
                on_path_literal(*literal);
            }
            return cached.value.clone();
        }
        if !self.visiting.borrow_mut().insert(name.to_string()) {
            return None; // cycle
        }
        let mut path_literals = Vec::new();
        let mut record_literal = |literal| {
            path_literals.push(literal);
            on_path_literal(literal);
        };
        let value =
            fold_string_expr_with_path_literals(rhs, self.content, self, &mut record_literal);
        self.visiting.borrow_mut().remove(name);
        self.memo.borrow_mut().insert(
            name.to_string(),
            MemoizedFold {
                value: value.clone(),
                path_literals,
            },
        );
        value
    }

    /// Whether any binding form in the document targets `name`.
    pub(crate) fn helper_is_bound_at(&self, name: &str, use_node: Node) -> bool {
        // Keep exact named helper bindings at the established file-wide
        // conservative policy. Only unknown helper uncertainty is ordered: a
        // later immediate unknown assignment cannot retroactively change an
        // earlier top-level helper call.
        let deferred_use = helper_use_is_deferred(use_node, self.content);
        self.bindings.contains_key(name)
            || super::binding::path_helper_may_be_shadowed_at(
                &self.bindings,
                use_node.start_byte(),
                deferred_use,
            )
            || super::binding::unknown_loaded_names_may_shadow_at(
                &self.bindings,
                use_node,
                deferred_use,
            )
    }

    pub(crate) fn named_binding_may_shadow_at(
        &self,
        name: &str,
        use_node: Node,
        deferred_use: bool,
    ) -> bool {
        super::binding::named_binding_may_shadow_lexically_at(
            &self.bindings,
            name,
            use_node,
            deferred_use,
        )
    }

    pub(crate) fn named_alias_may_shadow_at(
        &self,
        name: &str,
        use_node: Node,
        deferred_use: bool,
    ) -> bool {
        super::binding::named_alias_may_shadow_lexically_at(
            &self.bindings,
            name,
            use_node,
            deferred_use,
        )
    }

    /// Resolve a package-vector candidate under the package consumer's distinct
    /// eligibility policy.
    pub(crate) fn resolve_package_vector(&self, name: &str, use_node: Node) -> Option<&[String]> {
        let (candidate, candidate_offset) = self
            .bindings
            .get(name)?
            .resolved_with_offset_before(use_node.start_byte())?;
        if super::binding::unknown_loaded_names_may_affect_candidate(
            &self.bindings,
            candidate_offset,
            use_node,
            !super::binding::is_known_immediate_context(use_node),
        ) {
            return None;
        }
        candidate.package_vector.as_deref()
    }

    /// Whether an inline bare `c()` has provable base semantics at `use_node`.
    pub(crate) fn package_c_is_trusted_at(&self, use_node: Node) -> bool {
        let deferred_use = !super::binding::is_known_immediate_context(use_node);
        !super::binding::helper_may_be_shadowed_at(
            &self.bindings,
            use_node.start_byte(),
            deferred_use,
        ) && !super::binding::unknown_loaded_names_may_shadow_at(
            &self.bindings,
            use_node,
            deferred_use,
        ) && !super::binding::named_binding_may_shadow_at(
            &self.bindings,
            "c",
            use_node.start_byte(),
            deferred_use,
        )
    }
}

/// Walk-scoped lazy cache for capture-helper shadow checks.
///
/// Recursive source/package/scope walkers create one instance at their root and
/// thread it through every nested capture classification. Qualified helpers do
/// not force collection; the first bare base helper collects [`StaticBindings`]
/// once and all later calls in the same walk reuse it.
pub(crate) struct LazyStaticBindings<'tree, 'text> {
    root: Node<'tree>,
    content: &'text str,
    bindings: Option<StaticBindings<'tree, 'text>>,
    /// Capture classification is shared across several artifact walkers. Cache
    /// the ancestor-based immediacy result per capture node so each node pays for
    /// that walk at most once.
    immediate_context_by_node: HashMap<usize, bool>,
}

impl<'tree, 'text> LazyStaticBindings<'tree, 'text> {
    pub(crate) fn new(root: Node<'tree>, content: &'text str) -> Self {
        Self {
            root,
            content,
            bindings: None,
            immediate_context_by_node: HashMap::new(),
        }
    }

    pub(crate) fn get(&mut self) -> &StaticBindings<'tree, 'text> {
        self.bindings
            .get_or_insert_with(|| StaticBindings::collect(self.root, self.content))
    }

    pub(crate) fn capturing_call_kind_at(
        &mut self,
        node: Node,
    ) -> Option<super::binding::CapturingCallKind> {
        let content = self.content;
        let mut deferred_use = None;
        super::binding::capturing_call_kind(node, content, |name| {
            let deferred_use = *deferred_use.get_or_insert_with(|| {
                let immediate = *self
                    .immediate_context_by_node
                    .entry(node.id())
                    .or_insert_with(|| super::binding::is_known_immediate_context(node));
                !immediate
            });
            !self
                .get()
                .named_binding_may_shadow_at(name, node, deferred_use)
        })
    }

    pub(crate) fn resolve_package_vector(
        &mut self,
        name: &str,
        use_node: Node,
    ) -> Option<Vec<String>> {
        self.get()
            .resolve_package_vector(name, use_node)
            .map(<[String]>::to_vec)
    }

    pub(crate) fn package_c_is_trusted_at(&mut self, use_node: Node) -> bool {
        self.get().package_c_is_trusted_at(use_node)
    }

    #[cfg(test)]
    pub(crate) fn is_collected(&self) -> bool {
        self.bindings.is_some()
    }

    #[cfg(test)]
    pub(crate) fn collection_address(&self) -> Option<usize> {
        self.bindings
            .as_ref()
            .map(|bindings| std::ptr::from_ref(bindings).addr())
    }
}

/// Extract a strict bare `c()` of positional plain string literals for the
/// package-vector payload. Helper trust is established by the binding-site
/// policy before this function is called.
fn extract_package_vector(node: Node, content: &str) -> Option<Vec<String>> {
    super::binding::extract_bare_c_plain_strings(node, content)
        .map(|pairs| pairs.into_iter().map(|(package, _)| package).collect())
}

/// Classify a folding-helper call from its enclosing path consumer.
///
/// The helper itself sits below argument/call syntax, so immediacy is decided
/// from the nearest enclosing `source()`/`sys.source()` call or assignment.
/// The shared predicate then permits only top-level brace/parenthesis wrappers.
fn helper_use_is_deferred(node: Node, content: &str) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if parent.kind() == "function_definition" {
            return true;
        }
        if parent.kind() == "call"
            && parent
                .child_by_field_name("function")
                .is_some_and(|function| {
                    matches!(node_text(function, content), "source" | "sys.source")
                })
        {
            return !super::binding::is_known_immediate_context(parent);
        }
        if parent.kind() == "binary_operator"
            && parent
                .child_by_field_name("operator")
                .is_some_and(|operator| {
                    matches!(
                        node_text(operator, content),
                        "<-" | "=" | "<<-" | "->" | "->>"
                    )
                })
        {
            return !super::binding::is_known_immediate_context(parent);
        }
        current = parent;
    }
    true
}

/// Statically fold a path-valued expression to a `String`.
///
/// Handles exactly four forms (strict all-or-nothing; anything else → `None`):
/// - a string literal (raw text between the quotes; a literal containing a
///   backslash bails, so folded paths never diverge from R's unescaped
///   runtime string),
/// - a bare identifier resolved through `bindings`,
/// - a call to `file.path(...)` or `normalizePath(...)`,
/// - a parenthesized expression, peeled recursively.
///
/// Braced expressions remain conservative: evaluating a block may run
/// preceding side effects before producing its final value.
pub(crate) fn fold_string_expr<'tree>(
    node: Node<'tree>,
    content: &str,
    bindings: &StaticBindings<'tree, '_>,
) -> Option<String> {
    fold_string_expr_with_path_literals(node, content, bindings, &mut |_| {})
}

/// Fold a path-valued expression and report every accepted string literal that
/// participates in the traversal.
///
/// This is the policy-bearing fold used by both static source detection and
/// computed-path navigation. The callback runs only for plain string literals
/// reached through the same accepted branches as [`fold_string_expr`]; option
/// literals and strings below rejected branches are never traversed. Memoized
/// identifier folds replay their participating literals without changing the
/// existing value memoization or cycle guard.
pub(crate) fn fold_string_expr_with_path_literals<'tree>(
    node: Node<'tree>,
    content: &str,
    bindings: &StaticBindings<'tree, '_>,
    on_path_literal: &mut dyn FnMut(Node<'tree>),
) -> Option<String> {
    match node.kind() {
        "parenthesized_expression" => fold_string_expr_with_path_literals(
            node.named_child(0)?,
            content,
            bindings,
            on_path_literal,
        ),
        "string" => {
            let value = super::binding::extract_plain_string(node, content)?;
            on_path_literal(node);
            Some(value)
        }
        "identifier" => bindings.resolve(
            super::binding::plain_identifier_name(node, content)?,
            node,
            on_path_literal,
        ),
        "call" => {
            let func = node.child_by_field_name("function")?;
            match node_text(func, content) {
                "file.path" if !bindings.helper_is_bound_at("file.path", node) => {
                    fold_file_path_call(node, content, bindings, on_path_literal)
                }
                "normalizePath" if !bindings.helper_is_bound_at("normalizePath", node) => {
                    fold_normalize_path_call(node, content, bindings, on_path_literal)
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
fn fold_file_path_call<'tree>(
    node: Node<'tree>,
    content: &str,
    bindings: &StaticBindings<'tree, '_>,
    on_path_literal: &mut dyn FnMut(Node<'tree>),
) -> Option<String> {
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
            let part = fold_string_expr_with_path_literals(
                value_node,
                content,
                bindings,
                on_path_literal,
            )?;
            parts.push(part);
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// Fold `normalizePath(path, winslash =, mustWork =)` by strictly matching R
/// actuals to those three formals (exact name, unique partial name, then
/// position) and recursing into the matched `path`. `winslash`/`mustWork` values
/// are ignored once present and syntactically complete because they do not
/// change which file the path denotes. Duplicate/ambiguous names, excess
/// positionals, missing actuals, parser errors, and unmatched names all bail.
fn fold_normalize_path_call<'tree>(
    node: Node<'tree>,
    content: &str,
    bindings: &StaticBindings<'tree, '_>,
    on_path_literal: &mut dyn FnMut(Node<'tree>),
) -> Option<String> {
    use super::binding::{CallActual, CallMatchMode, match_call_arguments};

    let arguments = node.child_by_field_name("arguments")?;
    let matched = match_call_arguments(
        arguments,
        content,
        &["path", "winslash", "mustWork"],
        CallMatchMode::Strict,
    )?;
    let CallActual::Value(path) = matched[0]? else {
        return None;
    };
    if matched[1..]
        .iter()
        .any(|actual| matches!(actual, Some(CallActual::Missing)))
    {
        return None;
    }
    fold_string_expr_with_path_literals(path, content, bindings, on_path_literal)
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

    /// Fold the file argument of the LAST source() call in `code`, reporting
    /// accepted path literals through `on_path_literal`.
    fn fold_last_source_arg_with(
        code: &str,
        on_path_literal: &mut dyn FnMut(Node),
    ) -> Option<String> {
        let tree = parse(code);
        let root = tree.root_node();
        let bindings = StaticBindings::collect(root, code);
        let mut result = None;
        fn visit<'tree>(
            node: Node<'tree>,
            code: &str,
            bindings: &StaticBindings<'tree, '_>,
            out: &mut Option<String>,
            on_path_literal: &mut dyn FnMut(Node<'tree>),
        ) {
            if node.kind() == "call"
                && let Some(func) = node.child_by_field_name("function")
                && &code[func.byte_range()] == "source"
                && let Some(args) = node.child_by_field_name("arguments")
                && let Some(value) =
                    super::super::source_detect::source_call_file_value_node(&args, code, false)
            {
                *out = fold_string_expr_with_path_literals(value, code, bindings, on_path_literal);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit(child, code, bindings, out, on_path_literal);
            }
        }
        visit(root, code, &bindings, &mut result, on_path_literal);
        result
    }

    /// Fold the file argument of the LAST source() call in `code`.
    fn fold_last_source_arg(code: &str) -> Option<String> {
        fold_last_source_arg_with(code, &mut |_| {})
    }

    /// Fold the last source argument and collect the exact string nodes visited
    /// by the policy-bearing traversal.
    fn fold_last_source_arg_with_literals(code: &str) -> (Option<String>, Vec<String>) {
        let mut literals = Vec::new();
        let result = fold_last_source_arg_with(code, &mut |literal| {
            literals.push(code[literal.byte_range()].to_string())
        });
        (result, literals)
    }

    #[test]
    fn lazy_static_bindings_reuses_one_capture_cache_per_walk() {
        let code = "quote(x)\nquote(y)\n";
        let tree = parse(code);
        let root = tree.root_node();
        let first = root.named_child(0).unwrap();
        let second = root.named_child(1).unwrap();
        let mut bindings = LazyStaticBindings::new(root, code);

        assert!(bindings.bindings.is_none());
        assert_eq!(
            bindings.capturing_call_kind_at(first),
            Some(super::super::binding::CapturingCallKind::Whole)
        );
        let first_cache = bindings.bindings.as_ref().unwrap() as *const StaticBindings;
        assert_eq!(
            bindings.capturing_call_kind_at(second),
            Some(super::super::binding::CapturingCallKind::Whole)
        );
        let second_cache = bindings.bindings.as_ref().unwrap() as *const StaticBindings;
        assert_eq!(first_cache, second_cache);
    }

    #[test]
    fn shared_candidates_keep_path_and_package_payloads_independent() {
        let code = r#"
path <- "child.R"
libs <- c("dplyr")
assign("assigned", c("tidyr"))
source(path)
"#;
        let tree = parse(code);
        let bindings = StaticBindings::collect(tree.root_node(), code);

        let path = bindings
            .bindings
            .get("path")
            .and_then(|binding| binding.resolved_before(usize::MAX))
            .unwrap();
        assert!(path.path_rhs.is_some());
        assert!(path.package_vector.is_none());

        let libs = bindings
            .bindings
            .get("libs")
            .and_then(|binding| binding.resolved_before(usize::MAX))
            .unwrap();
        assert!(libs.path_rhs.is_some());
        assert_eq!(
            libs.package_vector.as_deref().unwrap(),
            ["dplyr".to_string()]
        );

        let assigned = bindings
            .bindings
            .get("assigned")
            .and_then(|binding| binding.resolved_before(usize::MAX))
            .unwrap();
        assert!(assigned.path_rhs.is_none());
        assert_eq!(
            assigned.package_vector.as_deref().unwrap(),
            ["tidyr".to_string()]
        );
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
    fn folds_parenthesized_path_values() {
        assert_eq!(
            fold_last_source_arg(r#"source((file.path("a", "b.R")))"#),
            Some("a/b.R".to_string())
        );

        let code = r#"
p <- ("child.R")
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), Some("child.R".to_string()));
    }

    #[test]
    fn braced_path_values_remain_conservative() {
        assert_eq!(fold_last_source_arg(r#"source({ "child.R" })"#), None);
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
    fn normalize_path_uses_strict_r_argument_matching() {
        for code in [
            r#"source(normalizePath(pa = file.path("a", "b.R"), win = arbitrary(), must = another()))"#,
            r#"source(normalizePath(file.path("a", "b.R"), arbitrary(), another()))"#,
            r#"source(normalizePath(mustWork = arbitrary(), file.path("a", "b.R"), winslash = another()))"#,
        ] {
            assert_eq!(
                fold_last_source_arg(code),
                Some("a/b.R".to_string()),
                "{code}"
            );
        }

        for code in [
            r#"source(normalizePath(path = "a.R", path = "b.R"))"#,
            r#"source(normalizePath(pa = "a.R", pat = "b.R"))"#,
            r#"source(normalizePath("a.R", "/", FALSE, "extra"))"#,
            r#"source(normalizePath(, "/", FALSE))"#,
            r#"source(normalizePath("a.R", winslash =))"#,
            r#"source(normalizePath("a.R", mustWork =))"#,
            r#"source(normalizePath("a.R", bogus = FALSE))"#,
            r#"source(normalizePath("a.R""))"#,
        ] {
            assert_eq!(fold_last_source_arg(code), None, "{code}");
        }
    }

    #[test]
    fn callback_tracks_only_literals_on_the_shared_fold_path() {
        let code = r#"
source(file.path((normalizePath((file.path(("scripts"), "nested")), winslash = "base-option")), normalizePath(path = (file.path(("helpers.R"))), winslash = "slash-option", mustWork = "work-option"), fsep = "/"))
"#;
        let (folded, literals) = fold_last_source_arg_with_literals(code);

        assert_eq!(folded, Some("scripts/nested/helpers.R".to_string()));
        assert_eq!(literals, vec!["\"scripts\"", "\"nested\"", "\"helpers.R\""]);
    }

    #[test]
    fn callback_replays_variable_literals_on_memo_hits() {
        let code = r#"
root <- file.path("scripts", "nested")
source(file.path(root, root, "helpers.R"))
"#;
        let (folded, literals) = fold_last_source_arg_with_literals(code);

        assert_eq!(
            folded,
            Some("scripts/nested/scripts/nested/helpers.R".to_string())
        );
        assert_eq!(
            literals,
            vec![
                "\"scripts\"",
                "\"nested\"",
                "\"scripts\"",
                "\"nested\"",
                "\"helpers.R\""
            ]
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

        let code = r#"
makeActiveBinding("x", function(value) {
  get("assign", baseenv())("p", "b.R", envir = .GlobalEnv)
}, .GlobalEnv)
p <- "a.R"
x <- 1
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);

        let code = r#"
`make\x41ctiveBinding`("x", function(value) {
  get("assign", baseenv())("p", "b.R", envir = .GlobalEnv)
}, .GlobalEnv)
p <- "a.R"
x <- 1
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);
    }

    #[test]
    fn backtick_identifier_reassignment_uses_the_same_binding_key() {
        let code = r#"
p <- "a.R"
`p` <- "b.R"
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
    fn dynamic_assign_name_invalidates_prior_candidates() {
        for (setup, target) in [
            ("n <- \"p\"\np <- \"a.R\"", "n"),
            ("p <- \"a.R\"", r#""\x70""#),
        ] {
            let code = format!("{setup}\nassign({target}, \"b.R\")\nsource(p)\n");
            assert_eq!(fold_last_source_arg(&code), None, "{target}");
        }

        let code = "n <- \"p\"\nassign(n, \"old.R\")\np <- \"a.R\"\nsource(p)\n";
        assert_eq!(fold_last_source_arg(code), Some("a.R".to_string()));

        for mutation in [
            r#"assign("x" = "p", "value" = "b.R")"#,
            r#"assign(`\x78` = "p", value = "b.R")"#,
            r#"rm("list" = c("p"))"#,
            r#"rm(`l\x69st` = c("p"))"#,
        ] {
            let code = format!("p <- \"a.R\"\n{mutation}\nsource(p)\n");
            assert_eq!(fold_last_source_arg(&code), None, "{mutation}");
        }
    }

    #[test]
    fn escaped_mutation_callee_invalidates_prior_candidates() {
        for remove in [r#"`\x72m`(p)"#, r#""r\x6d"(p)"#, r#"base::`\x72m`(p)"#] {
            let code = format!("p <- \"a.R\"\n{remove}\nsource(p)\n");
            assert_eq!(fold_last_source_arg(&code), None, "{remove}");
        }

        for shadowed in [
            "rm <- function(...) base::assign(\"p\", \"b.R\", envir = .GlobalEnv)\np <- \"a.R\"\nrm(other)",
            "assign <- function(...) base::assign(\"p\", \"b.R\", envir = .GlobalEnv)\np <- \"a.R\"\nassign(\"other\", 1)",
        ] {
            let code = format!("{shadowed}\nsource(p)\n");
            assert_eq!(fold_last_source_arg(&code), None);
        }
    }

    #[test]
    fn unknown_name_mutations_do_not_invalidate_later_candidates() {
        for mutation in [r#"`\x70` <- "old.R""#, r#"rm("\x70")"#] {
            let code = format!("{mutation}\np <- \"a.R\"\nsource(p)\n");
            assert_eq!(
                fold_last_source_arg(&code),
                Some("a.R".to_string()),
                "{mutation}"
            );
        }

        let code = r#"
name <- "c"
assign(name, function(...) "p")
p <- "a.R"
rm(list = c("other"))
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);

        let code = r#"
name <- "file.path"
assign(name, function(...) "wrong.R")
p <- file.path("right.R")
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);
    }

    #[test]
    fn helper_uncertainty_is_position_aware_for_immediate_uses() {
        for helper in ["file.path", "normalizePath"] {
            let source_expr = if helper == "file.path" {
                r#"file.path("right.R")"#
            } else {
                r#"normalizePath("right.R")"#
            };
            let code = format!(
                "source({source_expr})\nname <- \"{helper}\"\nassign(name, function(...) \"wrong.R\")\n"
            );
            assert_eq!(
                fold_last_source_arg(&code),
                Some("right.R".to_string()),
                "{helper}"
            );

            let code = format!(
                "name <- \"{helper}\"\nassign(name, function(...) \"wrong.R\")\nsource({source_expr})\n"
            );
            assert_eq!(fold_last_source_arg(&code), None, "earlier {helper}");

            let code = format!(
                "source({source_expr})\nf <- function() assign(name, function(...) \"wrong.R\")\n"
            );
            assert_eq!(fold_last_source_arg(&code), None, "deferred {helper}");
        }
    }

    #[test]
    fn wrapped_immediate_helper_uses_ignore_later_dynamic_rebinding() {
        for source in [
            r#"(source(file.path("right.R")))"#,
            r#"{ source(file.path("right.R")) }"#,
        ] {
            let code = format!(
                "{source}\nname <- \"file.path\"\nassign(name, function(...) \"wrong.R\")\n"
            );
            assert_eq!(
                fold_last_source_arg(&code),
                Some("right.R".to_string()),
                "{source}"
            );
        }

        for assignment in [
            r#"(p <- file.path("right.R"))"#,
            r#"{ p <- file.path("right.R") }"#,
        ] {
            let tree = parse(assignment);
            let mut file_path_call = None;
            fn find_file_path_call<'tree>(
                node: Node<'tree>,
                code: &str,
                result: &mut Option<Node<'tree>>,
            ) {
                if node.kind() == "call"
                    && node
                        .child_by_field_name("function")
                        .is_some_and(|function| &code[function.byte_range()] == "file.path")
                {
                    *result = Some(node);
                    return;
                }
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    find_file_path_call(child, code, result);
                }
            }
            find_file_path_call(tree.root_node(), assignment, &mut file_path_call);
            assert!(!helper_use_is_deferred(file_path_call.unwrap(), assignment));
        }
    }

    #[test]
    fn proven_non_mutating_constructs_preserve_path_candidates() {
        for intervening in [
            "for (i in NULL) assign(name, \"bad.R\")",
            "rm(list = NULL)",
            "rm(list = base::character())",
            "rm(list = base::character(0))",
            "rm(list = base::character(length = 0))",
            "rm(list = character())",
            "rm(list = character(0))",
            "x <- base::paste0(\"a\", \"b\")",
            "x <- base::paste(\"a\", \"b\")",
            "quote(rm(p))",
            "quote(p <- \"bad.R\")",
            "x <- quote(assign(\"p\", \"bad.R\"))",
        ] {
            let code = format!("p <- \"good.R\"\n{intervening}\nsource(p)\n");
            assert_eq!(
                fold_last_source_arg(&code),
                Some("good.R".to_string()),
                "{intervening}"
            );
        }
    }

    #[test]
    fn untrusted_or_deferred_character_removals_stay_conservative() {
        for setup in [
            "character <- function(...) \"p\"\np <- \"good.R\"\nrm(list = character())",
            "p <- \"good.R\"\nf <- function() rm(list = character())",
        ] {
            let code = format!("{setup}\nsource(p)\n");
            assert_eq!(fold_last_source_arg(&code), None, "{setup}");
        }
    }

    #[test]
    fn transparent_top_level_wrappers_keep_later_candidates_viable() {
        for mutation in ["{ assign(name, \"old.R\") }", "(assign(name, \"old.R\"))"] {
            let code = format!("{mutation}\np <- \"good.R\"\nsource(p)\n");
            assert_eq!(fold_last_source_arg(&code), Some("good.R".to_string()));
        }
    }

    #[test]
    fn deferred_unknown_name_mutation_persistently_invalidates_candidates() {
        let code = r#"
f <- function() assign("\x70", "b.R", envir = .GlobalEnv)
p <- "a.R"
f()
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);

        let code = r#"
f <- function() rm(list = c("other"), envir = .GlobalEnv)
p <- "a.R"
c <- function(...) "p"
f()
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);

        let code = r#"
f <- function() rm(list = c("other",), envir = .GlobalEnv)
p <- "a.R"
c <- function(...) "p"
f()
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);

        let code = r#"
later <- function(x) function() x
g <- later(rm(list = victims, envir = .GlobalEnv))
victims <- "p"
p <- "a.R"
g()
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);

        let code = r#"
`%delay%` <- function(x, y) function() x
n <- "p"
trigger <- assign(n, "b.R", envir = .GlobalEnv) %delay% NULL
p <- "a.R"
trigger()
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);

        let code = r#"
f <- function() assign("ignored", "x")
assign <- function(...) base::assign("p", "b.R", envir = .GlobalEnv)
p <- "a.R"
f()
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);
    }

    #[test]
    fn base_qualified_assign_calls_invalidate_bindings() {
        for assign in [
            "base::assign",
            "base:::assign",
            "\"base\"::assign",
            "base::\"assign\"",
            "base::`assign`",
        ] {
            let code = format!("p <- \"a.R\"\n{assign}(\"p\", \"b.R\")\nsource(p)\n");
            assert_eq!(fold_last_source_arg(&code), None, "{assign}");
        }

        // Another namespace may re-export base::assign, so it invalidates but
        // cannot provide a candidate of its own.
        for assign in ["other::assign", "other:::assign"] {
            let code = format!("p <- \"a.R\"\n{assign}(\"p\", \"b.R\")\nsource(p)\n");
            assert_eq!(fold_last_source_arg(&code), None, "{assign}");
        }
    }

    #[test]
    fn trusted_loaders_invalidate_unknown_names_by_destination_and_scope() {
        for loader in [
            r#"load("state.RData")"#,
            r#"base::load("state.RData")"#,
            r#"base:::load("state.RData")"#,
            r#"sys.load.image("state.RData")"#,
            r#"sys.load.image("state.RData", quiet = TRUE)"#,
            r#"base::sys.load.image("state.RData")"#,
        ] {
            let code = format!("p <- \"good.R\"\n{loader}\nsource(p)\n");
            assert_eq!(fold_last_source_arg(&code), None, "{loader}");
        }

        for external in [
            r#"base::load("state.RData", envir = base::new.env())"#,
            r#"base::load("state.RData", envir = base::baseenv())"#,
            r#"base::load("state.RData", envir = base::emptyenv())"#,
        ] {
            let code = format!("p <- \"good.R\"\n{external}\nsource(p)\n");
            assert_eq!(
                fold_last_source_arg(&code),
                Some("good.R".to_string()),
                "{external}"
            );
        }

        let unknown = "p <- \"good.R\"\nenv <- choose_env()\nbase::load(\"state.RData\", envir = env)\nsource(p)\n";
        assert_eq!(fold_last_source_arg(unknown), None);

        let later_assignment = "base::load(\"state.RData\")\np <- \"good.R\"\nsource(p)\n";
        assert_eq!(
            fold_last_source_arg(later_assignment),
            Some("good.R".to_string())
        );

        let unrelated_function =
            "p <- \"good.R\"\nf <- function() base::load(\"state.RData\")\nsource(p)\n";
        assert_eq!(
            fold_last_source_arg(unrelated_function),
            Some("good.R".to_string())
        );

        let same_function =
            "p <- \"good.R\"\nf <- function() { base::load(\"state.RData\"); source(p) }\n";
        assert_eq!(fold_last_source_arg(same_function), None);

        let explicit_global = "p <- \"good.R\"\nf <- function() base::load(\"state.RData\", envir = .GlobalEnv)\nsource(p)\n";
        assert_eq!(fold_last_source_arg(explicit_global), None);
    }

    #[test]
    fn load_argument_side_effects_and_trust_policy_are_preserved() {
        let side_effect = r#"
p <- "good.R"
base::load({ base::assign("p", "bad.R", envir = .GlobalEnv); "state.RData" },
           envir = base::new.env())
source(p)
"#;
        assert_eq!(fold_last_source_arg(side_effect), None);

        let escaped_destination = r#"
p <- "good.R"
base::load(file = "state.RData", `\x65nvir` = .GlobalEnv)
source(p)
"#;
        assert_eq!(fold_last_source_arg(escaped_destination), None);

        for untrusted in [
            "load <- function(...) NULL\np <- \"good.R\"\nload(\"state.RData\")\nsource(p)\n",
            "p <- \"good.R\"\nother::load(\"state.RData\")\nsource(p)\n",
            "sys.load.image <- function(...) NULL\np <- \"good.R\"\nsys.load.image(\"state.RData\")\nsource(p)\n",
        ] {
            assert_eq!(
                fold_last_source_arg(untrusted),
                Some("good.R".to_string()),
                "{untrusted}"
            );
        }

        for helper in ["file.path", "normalizePath"] {
            let expression = if helper == "file.path" {
                r#"file.path("good.R")"#
            } else {
                r#"normalizePath("good.R")"#
            };
            let code = format!("base::load(\"state.RData\")\np <- {expression}\nsource(p)\n");
            assert_eq!(fold_last_source_arg(&code), None, "{helper}");

            let code = format!(
                "f <- function() base::load(\"state.RData\")\np <- {expression}\nsource(p)\n"
            );
            assert_eq!(
                fold_last_source_arg(&code),
                Some("good.R".to_string()),
                "unrelated {helper}"
            );
        }
    }

    #[test]
    fn evaluated_assign_actuals_invalidate_other_bindings() {
        let code = r#"
delayedAssign("e", {
  get("assign", baseenv())("p", "b.R", envir = .GlobalEnv)
  .GlobalEnv
})
p <- "a.R"
base::assign("other", 1, envir = e)
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);

        let code = r#"
f <- function() base::assign("ignored", c("x"))
c <- function(...) {
  get("assign", baseenv())("p", "b.R", envir = .GlobalEnv)
  "x"
}
p <- "a.R"
f()
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);
    }

    #[test]
    fn evaluated_assignment_rhs_invalidates_other_bindings() {
        let code = r#"
delayedAssign("v", {
  get("assign", baseenv())("p", "b.R", envir = .GlobalEnv)
  1
})
p <- "a.R"
other <- v
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);

        let code = r#"
f <- function(...) {
  ..1 <- get("assign", baseenv())("p", "bad.R", envir = .GlobalEnv)
}
p <- "good.R"
f(0)
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);

        // Preserve recursive folding when the identifier is a known earlier
        // ordinary candidate rather than a delayed or active binding.
        let code = "part <- \"a.R\"\np <- part\nsource(p)\n";
        assert_eq!(fold_last_source_arg(code), Some("a.R".to_string()));

        let code = r#"
v <- 1
delayedAssign("v", {
  get("assign", baseenv())("p", "b.R", envir = .GlobalEnv)
  1
})
p <- "a.R"
other <- v
source(p)
"#;
        assert_eq!(fold_last_source_arg(code), None);
    }

    #[test]
    fn bquote_splice_result_preserves_only_reachable_binding_candidates() {
        let code = r#"
 p <- "good.R"
 base::bquote(..(function() {}) + .(base::assign("p", "bad.R")), splice = TRUE)
 source(p)
 "#;
        assert_eq!(fold_last_source_arg(code), Some("good.R".to_string()));

        let code = r#"
 p <- "good.R"
 base::bquote(..({ other <- 1; function() {} }) + .(base::assign("p", "bad.R")), splice = TRUE)
 source(p)
 "#;
        assert_eq!(fold_last_source_arg(code), Some("good.R".to_string()));

        // An unknown result may be vector-like, so conservative invalidation
        // must inspect the assignment that would run after a successful splice.
        let code = r#"
 p <- "good.R"
 base::bquote(..(unknown) + .(base::assign("p", "bad.R")), splice = TRUE)
 source(p)
 "#;
        assert_eq!(fold_last_source_arg(code), None);

        for operand in ["1", "list(1)", "c(1)", "base::c(1)"] {
            let code = format!(
                "p <- \"good.R\"\nbase::bquote(..({operand}) + .(base::assign(\"p\", \"bad.R\")), splice = TRUE)\nsource(p)\n"
            );
            assert_eq!(fold_last_source_arg(&code), None, "{operand}");
        }
    }

    #[test]
    fn bquote_static_paths_follow_operand_evaluation_frame() {
        for capture in [
            r#"bquote(.(p <- "bad.R"), where = new.env())"#,
            r#"bquote(where = new.env(), expr = .(p <- "bad.R"))"#,
            r#"bquote(.(rm(p)), where = new.env())"#,
        ] {
            let code = format!("p <- \"good.R\"\n{capture}\nsource(p)\n");
            assert_eq!(
                fold_last_source_arg(&code),
                Some("good.R".to_string()),
                "{capture}"
            );
        }

        for capture in [
            r#"bquote(.(p <- "good.R"))"#,
            r#"bquote(.(p <- "good.R"), where = parent.frame())"#,
            r#"bquote(where = environment(), expr = .(p <- "good.R"))"#,
            r#"bquote(expr = .(p <- "good.R"), where = .GlobalEnv)"#,
            r#"bquote(expr = .(p <- "good.R"), where = globalenv())"#,
            r#"bquote(expr = .(p <- "good.R"), where = base::globalenv())"#,
        ] {
            let code = format!("{capture}\nsource(p)\n");
            assert_eq!(
                fold_last_source_arg(&code),
                Some("good.R".to_string()),
                "{capture}"
            );
        }

        for capture in [
            r#"bquote(.(p <- "bad.R"))"#,
            r#"bquote(.(p <- "bad.R"), where = .GlobalEnv)"#,
            r#"bquote(.(base::assign("p", "bad.R", envir = .GlobalEnv)), where = new.env())"#,
            r#"bquote(where = new.env(), expr = .(p <<- "bad.R"))"#,
        ] {
            let code = format!("p <- \"good.R\"\n{capture}\nsource(p)\n");
            assert_eq!(fold_last_source_arg(&code), None, "{capture}");
        }
    }

    #[test]
    fn bquote_function_syntax_static_bindings_follow_runtime_scope() {
        let top_level = r#"
            bquote(function() .(p <- "good.R"))
            source(p)
        "#;
        assert_eq!(fold_last_source_arg(top_level), Some("good.R".to_string()));

        let outer_function = r#"
            outer <- function() {
                bquote(function() .(p <- "outer.R"))
            }
            source(p)
        "#;
        assert_eq!(fold_last_source_arg(outer_function), None);

        let nested_closure = r#"
            bquote(function() .(function() p <- "nested.R"))
            source(p)
        "#;
        assert_eq!(fold_last_source_arg(nested_closure), None);

        let ordinary = r#"
            ordinary <- function() p <- "ordinary.R"
            source(p)
        "#;
        assert_eq!(fold_last_source_arg(ordinary), None);
    }

    #[test]
    fn external_bquote_function_bindings_are_analyzed_as_deferred_local_execution() {
        let code = r#"
            p <- "good.R"
            bquote(
                .(function(default = { p <- "default.R" }) {
                    p <- "body.R"
                    function() { nested <- p }
                }),
                where = new.env()
            )
            source(p)
        "#;
        assert_eq!(fold_last_source_arg(code), None);

        let code = r#"
            p <- "good.R"
            bquote(.(external <- "unrelated.R"), where = new.env())
            source(p)
        "#;
        assert_eq!(fold_last_source_arg(code), Some("good.R".to_string()));
    }

    #[test]
    fn unknown_bquote_splice_false_branch_invalidates_path_binding() {
        for (capture, expected) in [
            (
                r#"base::bquote(list(..(1, .(base::assign("p", "bad.R")))), splice = flag)"#,
                None,
            ),
            (
                r#"base::bquote(list(..(1, .(base::assign("p", "bad.R")))), splice = TRUE)"#,
                Some("good.R".to_string()),
            ),
            (
                r#"base::bquote(list(..(base::quote(.(base::assign("p", "bad.R"))))), splice = flag)"#,
                None,
            ),
            (
                r#"base::bquote(list(..(base::quote(.(base::assign("p", "bad.R"))))), splice = TRUE)"#,
                Some("good.R".to_string()),
            ),
        ] {
            let code = format!("p <- \"good.R\"\n{capture}\nsource(p)\n");
            assert_eq!(fold_last_source_arg(&code), expected, "{capture}");
        }
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

        for assign in [r#"assign("p")"#, r#"assign("p", value = )"#] {
            let code = format!("p <- \"a.R\"\n{assign}\nsource(p)\n");
            assert_eq!(fold_last_source_arg(&code), Some("a.R".to_string()));
        }
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
        for remove in [
            "rm(p)",
            "remove(\"p\")",
            "rm(list = \"p\")",
            "rm(list = c(\"p\", \"other\"))",
            "remove(list = c(\"other\", \"p\"))",
            "rm(list = base::c(\"p\", \"other\"))",
            "rm(list = `c`(\"other\", \"p\"))",
            "base::rm(p)",
            "base:::remove(\"p\")",
            "other::rm(p)",
            "other:::remove(\"p\")",
        ] {
            let code = format!("p <- \"a.R\"\n{remove}\nsource(p)\n");
            assert_eq!(fold_last_source_arg(&code), None, "{remove}");
        }
    }

    #[test]
    fn malformed_remove_vectors_do_not_invalidate_bindings() {
        for vector in ["c()", "c(\"p\",)", "c(,\"p\")", "c(\"other\",,\"p\")"] {
            let code = format!("p <- \"a.R\"\nrm(list = {vector})\nsource(p)\n");
            assert_eq!(
                fold_last_source_arg(&code),
                Some("a.R".to_string()),
                "{vector}"
            );
        }

        for remove in [
            r#"rm(list = c("p"), list = c("other"))"#,
            "rm(p, pos = 1, pos = 2)",
            r#"rm(p, pos = 1, pos = 2, `\x6cist` = c("p"))"#,
            r#"assign(x = "p", x = "other", `\x76alue` = "b.R")"#,
        ] {
            let code = format!("p <- \"a.R\"\n{remove}\nsource(p)\n");
            assert_eq!(fold_last_source_arg(&code), Some("a.R".to_string()));
        }
    }

    #[test]
    fn dynamic_remove_list_invalidates_prior_candidates_conservatively() {
        for (remove, setup, list) in [
            ("rm", "p <- \"victim\"\nvictim <- 1", "p"),
            ("remove", "p <- \"p\"", "p"),
            ("rm", "victims <- \"p\"\np <- \"a.R\"", "victims"),
        ] {
            let code = format!("{setup}\n{remove}(list = {list})\nsource(p)\n");
            assert_eq!(fold_last_source_arg(&code), None, "{remove}: {setup}");
        }

        let code = "victims <- \"p\"\nrm(list = victims)\np <- \"a.R\"\nsource(p)\n";
        assert_eq!(fold_last_source_arg(code), Some("a.R".to_string()));

        for list in [
            "c <- function(...) \"p\"\nrm(list = c(\"other\"))",
            "`c` <- function(...) \"p\"\nrm(list = c(\"other\"))",
            "rm(list = other::c(\"other\"))",
        ] {
            let code = format!("p <- \"a.R\"\n{list}\nsource(p)\n");
            assert_eq!(fold_last_source_arg(&code), None, "{list}");
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
    fn variadic_parameters_do_not_invalidate_unrelated_bindings() {
        for parameter in ["...", "..1"] {
            let code = format!("p <- \"a.R\"\nf <- function({parameter}) NULL\nsource(p)\n");
            assert_eq!(fold_last_source_arg(&code), Some("a.R".to_string()));
        }
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
