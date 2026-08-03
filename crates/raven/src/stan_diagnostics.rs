//! Conservative undeclared-variable diagnostics for complete Stan programs.
//!
//! The analysis is deliberately smaller than a Stan compiler. It recognizes
//! only identifier children of `variable_expression` as variable references,
//! models block-order and lexical visibility, and declines to guess when
//! preprocessing or parser recovery can make the visible declarations
//! incomplete. Function and distribution names occupy a separate namespace.
//!
//! A file is eligible only when its `program` root has at least one direct,
//! structurally recognized Stan program block. This is the completeness
//! boundary that keeps standalone section fragments quiet. A `preproc_include`
//! anywhere in the tree suppresses the whole semantic pass because an include
//! can contribute a declaration at its insertion point; Raven does not
//! preprocess Stan.

use std::collections::HashSet;

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};
use tree_sitter::Node;

use crate::handlers::DiagCancelToken;
use crate::utf16::byte_offset_to_utf16_column;

/// Fixed per-file bound for Stan semantic findings.
///
/// This is intentionally independent of `maxSyntaxDiagnosticsPerFile`: it
/// bounds pathological semantic result storage and editor/CLI payloads without
/// changing syntax diagnostics. Traversal continues after saturation so
/// cancellation stays responsive. The ordered retention set contains at most
/// this many entries at every point.
pub(crate) const MAX_STAN_SEMANTIC_DIAGNOSTICS: usize = 500;

const PROGRAM_BLOCKS: [&str; 7] = [
    "functions",
    "data",
    "transformed_data",
    "parameters",
    "transformed_parameters",
    "model",
    "generated_quantities",
];

const ORDERED_VARIABLE_BLOCKS: [(&str, bool); 6] = [
    ("data", true),
    ("transformed_data", true),
    ("parameters", true),
    ("transformed_parameters", true),
    ("model", false),
    ("generated_quantities", false),
];

/// Collect native Stan undeclared-variable diagnostics.
///
/// Returns `None` on cancellation and never exposes a partial result. Syntax
/// diagnostics are collected separately by the caller and use their own cap.
/// `should_retain` receives the outward diagnostic plus its private identifier
/// and runs before exact deduplication and the fixed semantic cap, so suppressed
/// or host-declared candidates never consume retained capacity.
pub(crate) fn collect_undefined_variables<F>(
    root: Node<'_>,
    text: &str,
    severity: DiagnosticSeverity,
    cancel: &DiagCancelToken,
    should_retain: F,
) -> Option<Vec<Diagnostic>>
where
    F: Fn(&Diagnostic, &str) -> bool,
{
    let mut budget = CancellationBudget::new(cancel);
    budget.check()?;

    // Includes are extras and can occur below otherwise ordinary nodes, so
    // scan every child (including recovery subtrees) before semantic descent.
    if tree_contains_include(root, &mut budget)? {
        return Some(Vec::new());
    }

    let direct_blocks = direct_program_blocks(root, &mut budget)?;
    if direct_blocks.is_empty() {
        return Some(Vec::new());
    }

    let function_namespace = collect_function_namespace(&direct_blocks, text, &mut budget)?;
    let mut analyzer = Analyzer::new(text, severity, cancel, function_namespace, should_retain);

    if let Some(functions) = direct_blocks
        .iter()
        .copied()
        .find(|node| node.kind() == "functions")
    {
        analyzer.analyze_functions_block(functions)?;
    }

    let mut program_scope = Frame::default();
    for (kind, exports) in ORDERED_VARIABLE_BLOCKS {
        let Some(block) = direct_blocks
            .iter()
            .copied()
            .find(|node| node.kind() == kind)
        else {
            continue;
        };
        analyzer.analyze_program_block(block, &mut program_scope, exports)?;
    }

    analyzer.finish()
}

#[derive(Default)]
struct Frame {
    names: HashSet<String>,
    /// Recovery may have hidden a declaration in this frame. An unresolved
    /// reference visible through it is therefore not safe to report.
    declarations_incomplete: bool,
}

#[derive(Clone)]
struct Finding {
    start_byte: usize,
    end_byte: usize,
    name: String,
    diagnostic: Diagnostic,
}

impl Finding {
    fn key(&self) -> (usize, usize, &str) {
        (self.start_byte, self.end_byte, self.name.as_str())
    }
}

/// A fixed-size, source-ordered exact set.
///
/// Although the semantic walk itself is source ordered for valid programs,
/// binary insertion makes recovery-shaped trees deterministic too. A late
/// earlier candidate can displace the current final entry without allowing
/// storage to grow beyond the fixed bound.
#[derive(Default)]
struct RetainedFindings {
    entries: Vec<Finding>,
}

impl RetainedFindings {
    fn retain(&mut self, finding: Finding) {
        let key = finding.key();
        let index = self
            .entries
            .partition_point(|existing| existing.key() < key);
        if self
            .entries
            .get(index)
            .is_some_and(|existing| existing.key() == key)
        {
            return;
        }
        if index >= MAX_STAN_SEMANTIC_DIAGNOSTICS {
            return;
        }
        self.entries.insert(index, finding);
        if self.entries.len() > MAX_STAN_SEMANTIC_DIAGNOSTICS {
            self.entries.pop();
        }
    }

    fn into_diagnostics(self) -> Vec<Diagnostic> {
        self.entries
            .into_iter()
            .map(|finding| finding.diagnostic)
            .collect()
    }
}

struct CancellationBudget<'a> {
    cancel: &'a DiagCancelToken,
    visited: usize,
}

impl<'a> CancellationBudget<'a> {
    fn new(cancel: &'a DiagCancelToken) -> Self {
        Self { cancel, visited: 0 }
    }

    /// Check immediately and then once per 64 visited nodes.
    fn check(&mut self) -> Option<()> {
        let should_check = self.visited == 0 || self.visited & 63 == 0;
        self.visited = self.visited.saturating_add(1);
        if should_check && self.cancel.is_cancelled() {
            None
        } else {
            Some(())
        }
    }
}

fn tree_contains_include(node: Node<'_>, budget: &mut CancellationBudget<'_>) -> Option<bool> {
    budget.check()?;
    if node.kind() == "preproc_include" {
        return Some(true);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if tree_contains_include(child, budget)? {
            return Some(true);
        }
    }
    Some(false)
}

fn direct_program_blocks<'tree>(
    root: Node<'tree>,
    budget: &mut CancellationBudget<'_>,
) -> Option<Vec<Node<'tree>>> {
    if root.kind() != "program" {
        return Some(Vec::new());
    }
    let mut blocks = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        budget.check()?;
        // Direct membership is intentional: never recover a block nested below
        // ERROR or arbitrary top-level text.
        if PROGRAM_BLOCKS.contains(&child.kind()) {
            blocks.push(child);
        }
    }
    Some(blocks)
}

#[derive(Default)]
struct FunctionNamespace {
    declared: HashSet<String>,
    /// Exact names that a direct recovery node in `functions` plausibly
    /// started declaring. They suppress only same-name higher-order value
    /// references; unrelated clear misses remain reportable.
    recovered_candidates: HashSet<String>,
}

fn collect_function_namespace(
    blocks: &[Node<'_>],
    text: &str,
    budget: &mut CancellationBudget<'_>,
) -> Option<FunctionNamespace> {
    let mut namespace = FunctionNamespace::default();
    let Some(functions) = blocks
        .iter()
        .copied()
        .find(|node| node.kind() == "functions")
    else {
        return Some(namespace);
    };

    let mut cursor = functions.walk();
    for child in functions.named_children(&mut cursor) {
        budget.check()?;
        if is_recovery_node(child) {
            for name in recovered_function_names(child, text) {
                insert_function_name(&mut namespace.recovered_candidates, name);
            }
            continue;
        }
        if child.kind() != "function_definition" {
            continue;
        }
        let Some(declarator) = named_child_of_kind(child, "function_declarator") else {
            continue;
        };
        let Some(name) = declarator.child_by_field_name("name") else {
            continue;
        };
        if let Some(value) = identifier_text(name, text) {
            insert_function_name(&mut namespace.declared, value);
        }
    }
    Some(namespace)
}

/// Insert a declared function and the compiler-provided unnormalized alias for
/// a user-defined probability function.
///
/// Stan makes `name_lupdf` available when `name_lpdf` is declared, and likewise
/// makes `name_lupmf` available for `name_lpmf`. Those aliases can appear as
/// higher-order function values, where the grammar represents them as ordinary
/// variable expressions rather than call names.
fn insert_function_name(namespace: &mut HashSet<String>, name: &str) {
    namespace.insert(name.to_string());
    if let Some(base) = name.strip_suffix("_lpdf") {
        namespace.insert(format!("{base}_lupdf"));
    } else if let Some(base) = name.strip_suffix("_lpmf") {
        namespace.insert(format!("{base}_lupmf"));
    }
}

/// Recover only the narrow function-name shapes exposed directly by a broken
/// declaration in the `functions` block. Never collect arbitrary identifiers
/// nested in the recovery subtree: those may be expressions rather than names.
fn recovered_function_names<'a>(error: Node<'_>, text: &'a str) -> Vec<&'a str> {
    if error.kind() != "ERROR" {
        return Vec::new();
    }

    let mut names = Vec::new();
    let mut saw_return_type = false;
    let mut cursor = error.walk();
    for child in error.named_children(&mut cursor) {
        if is_semantic_extra(child) {
            continue;
        }
        if child.kind() == "function_declarator" {
            if let Some(name) = child.child_by_field_name("name")
                && let Some(value) = identifier_text(name, text)
            {
                names.push(value);
            }
            saw_return_type = false;
        } else if child.kind() == "return_type" {
            saw_return_type = true;
        } else if saw_return_type && child.kind() == "identifier" {
            if let Some(value) = identifier_text(child, text) {
                names.push(value);
            }
        } else {
            // Only direct identifiers in a flattened declaration run after a
            // direct return type are plausible names. A structural child ends
            // that run; nested expression identifiers are never inspected.
            saw_return_type = false;
        }
    }
    names
}

struct Analyzer<'a, F>
where
    F: Fn(&Diagnostic, &str) -> bool,
{
    text: &'a str,
    line_starts: Vec<usize>,
    severity: DiagnosticSeverity,
    cancellation: CancellationBudget<'a>,
    function_namespace: FunctionNamespace,
    findings: RetainedFindings,
    should_retain: F,
}

impl<'a, F> Analyzer<'a, F>
where
    F: Fn(&Diagnostic, &str) -> bool,
{
    fn new(
        text: &'a str,
        severity: DiagnosticSeverity,
        cancel: &'a DiagCancelToken,
        function_namespace: FunctionNamespace,
        should_retain: F,
    ) -> Self {
        Self {
            text,
            line_starts: std::iter::once(0)
                .chain(text.match_indices('\n').map(|(index, _)| index + 1))
                .collect(),
            severity,
            cancellation: CancellationBudget::new(cancel),
            function_namespace,
            findings: RetainedFindings::default(),
            should_retain,
        }
    }

    fn check_cancel(&mut self) -> Option<()> {
        self.cancellation.check()
    }

    fn finish(mut self) -> Option<Vec<Diagnostic>> {
        // Check after the full walk as well, including after cap saturation.
        if self.cancellation.cancel.is_cancelled() {
            return None;
        }
        Some(std::mem::take(&mut self.findings).into_diagnostics())
    }

    fn analyze_functions_block(&mut self, functions: Node<'_>) -> Option<()> {
        let mut cursor = functions.walk();
        for child in functions.named_children(&mut cursor) {
            self.check_cancel()?;
            if child.kind() == "ERROR" || child.is_missing() {
                continue;
            }
            if child.kind() == "function_definition" {
                self.analyze_function(child)?;
            }
        }
        Some(())
    }

    fn analyze_function(&mut self, function: Node<'_>) -> Option<()> {
        // A recovered function can expose a body-shaped node that is not a
        // real Stan function body (for example `real f(real x) = 1;`). Do not
        // infer semantic roles inside that uncertain unit.
        if function.has_error() {
            return Some(());
        }
        let Some(declarator) = named_child_of_kind(function, "function_declarator") else {
            return Some(());
        };
        let Some(body) = function.child_by_field_name("body") else {
            return Some(());
        };
        let mut scopes = vec![Frame::default()];

        if let Some(parameters) = named_child_of_kind(declarator, "parameter_list") {
            let mut cursor = parameters.walk();
            for parameter in parameters.named_children(&mut cursor) {
                self.check_cancel()?;
                if parameter.kind() != "parameter_declaration" {
                    if is_recovery_node(parameter) {
                        scopes[0].declarations_incomplete = true;
                    }
                    continue;
                }
                if parameter.has_error() {
                    scopes[0].declarations_incomplete = true;
                    continue;
                }
                if let Some(name) = parameter.child_by_field_name("parameter")
                    && let Some(value) = identifier_text(name, self.text)
                {
                    scopes[0].names.insert(value.to_string());
                }
            }
        }

        if is_recovery_node(body) {
            return Some(());
        }
        self.analyze_node(body, &mut scopes)
    }

    fn analyze_program_block(
        &mut self,
        block: Node<'_>,
        program_scope: &mut Frame,
        exports: bool,
    ) -> Option<()> {
        let outer = Frame {
            names: program_scope.names.clone(),
            declarations_incomplete: program_scope.declarations_incomplete,
        };
        let mut scopes = vec![outer, Frame::default()];
        self.analyze_sequence(block, &mut scopes)?;
        let local = scopes.pop().expect("program block frame");
        if exports {
            program_scope.names.extend(local.names);
            program_scope.declarations_incomplete |= local.declarations_incomplete;
        }
        Some(())
    }

    /// Analyze a statement/declaration sequence without treating the
    /// container's aggregate `has_error()` as a reason to discard clean sibling
    /// units. A malformed direct declaration taints only the current frame.
    fn analyze_sequence(&mut self, container: Node<'_>, scopes: &mut Vec<Frame>) -> Option<()> {
        let mut cursor = container.walk();
        for child in container.named_children(&mut cursor) {
            self.check_cancel()?;
            if is_recovery_node(child) {
                scopes
                    .last_mut()
                    .expect("active scope")
                    .declarations_incomplete = true;
                continue;
            }
            if child.has_error() {
                if is_declaration(child) {
                    scopes
                        .last_mut()
                        .expect("active scope")
                        .declarations_incomplete = true;
                }
                // The direct unit is uncertain. Later clean siblings remain
                // eligible, subject to any declaration taint above.
                continue;
            }
            self.analyze_node(child, scopes)?;
        }
        Some(())
    }

    fn analyze_node(&mut self, node: Node<'_>, scopes: &mut Vec<Frame>) -> Option<()> {
        self.check_cancel()?;
        if is_recovery_node(node) {
            return Some(());
        }
        match node.kind() {
            "variable_expression" => self.analyze_reference(node, scopes),
            "var_decl" | "top_var_decl" | "top_var_decl_no_assign" => {
                self.analyze_declaration(node, scopes)
            }
            "block_statement" => {
                scopes.push(Frame::default());
                self.analyze_sequence(node, scopes)?;
                scopes.pop();
                Some(())
            }
            // `profile` owns a lexical statement list directly rather than a
            // `block_statement` child in tree-sitter-stan.
            "profile_statement" => {
                scopes.push(Frame::default());
                self.analyze_sequence(node, scopes)?;
                scopes.pop();
                Some(())
            }
            "for_statement" => self.analyze_for(node, scopes),
            "if_statement" | "while_statement" => self.analyze_conditional_body(node, scopes),
            _ => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    self.analyze_node(child, scopes)?;
                }
                Some(())
            }
        }
    }

    fn analyze_declaration(
        &mut self,
        declaration: Node<'_>,
        scopes: &mut Vec<Frame>,
    ) -> Option<()> {
        if declaration.has_error() {
            scopes
                .last_mut()
                .expect("active scope")
                .declarations_incomplete = true;
            return Some(());
        }

        let mut cursor = declaration.walk();
        let names: Vec<_> = declaration
            .children_by_field_name("name", &mut cursor)
            .collect();
        let name_ids: HashSet<_> = names.iter().map(Node::id).collect();

        // Shared type dimensions/constraints precede the first declarator and
        // are checked before any name. Thereafter the grammar is flat: `name,
        // initializer?, name, initializer?, ...`. Stan makes the current name
        // visible in its own initializer, while later comma names remain out
        // of scope, so bind each name as its field is encountered.
        let mut cursor = declaration.walk();
        for child in declaration.named_children(&mut cursor) {
            if name_ids.contains(&child.id()) {
                self.bind_identifier(child, scopes);
            } else {
                self.analyze_node(child, scopes)?;
            }
        }
        Some(())
    }

    fn bind_identifier(&self, identifier: Node<'_>, scopes: &mut [Frame]) {
        if let Some(value) = identifier_text(identifier, self.text) {
            scopes
                .last_mut()
                .expect("active scope")
                .names
                .insert(value.to_string());
        }
    }

    fn analyze_for(&mut self, statement: Node<'_>, scopes: &mut Vec<Frame>) -> Option<()> {
        let Some(loopvar) = statement.child_by_field_name("loopvar") else {
            return Some(());
        };
        let mut cursor = statement.walk();
        let children: Vec<_> = statement
            .named_children(&mut cursor)
            .filter(|child| !is_semantic_extra(*child))
            .collect();
        let Some(loopvar_index) = children.iter().position(|child| child.id() == loopvar.id())
        else {
            return Some(());
        };

        let Some((body, range_parts)) = children
            .get(loopvar_index + 1..)
            .and_then(<[_]>::split_last)
        else {
            return Some(());
        };
        if range_parts.is_empty() {
            return Some(());
        }

        // Stan's grammar exposes a colon range as separate lower/upper named
        // children. Every range component is evaluated before the loop
        // variable is bound; the final semantic child is always the body.
        for range_part in range_parts {
            if !range_part.has_error() {
                self.analyze_node(*range_part, scopes)?;
            }
        }

        scopes.push(Frame::default());
        if let Some(value) = identifier_text(loopvar, self.text) {
            scopes
                .last_mut()
                .expect("loop scope")
                .names
                .insert(value.to_string());
        }
        if !body.has_error() {
            self.analyze_node(*body, scopes)?;
        }
        scopes.pop();
        Some(())
    }

    fn analyze_conditional_body(
        &mut self,
        statement: Node<'_>,
        scopes: &mut Vec<Frame>,
    ) -> Option<()> {
        let mut cursor = statement.walk();
        let mut children = statement
            .named_children(&mut cursor)
            .filter(|child| !is_semantic_extra(*child));
        let Some(condition) = children.next() else {
            return Some(());
        };
        if !condition.has_error() {
            self.analyze_node(condition, scopes)?;
        }
        // Each unbraced body gets a lexical frame too, so a declaration cannot
        // leak through an if/else or while boundary.
        for body in children {
            scopes.push(Frame::default());
            if !body.has_error() {
                self.analyze_node(body, scopes)?;
            }
            scopes.pop();
        }
        Some(())
    }

    fn analyze_reference(&mut self, expression: Node<'_>, scopes: &[Frame]) -> Option<()> {
        let Some(identifier) = expression.named_child(0) else {
            return Some(());
        };
        let Some(name) = identifier_text(identifier, self.text) else {
            return Some(());
        };

        if scopes.iter().rev().any(|scope| scope.names.contains(name))
            || self.function_namespace.declared.contains(name)
            || self.function_namespace.recovered_candidates.contains(name)
            || crate::stan_builtins::callable(name).is_some()
            || is_implicit_stan_variable(name)
        {
            return Some(());
        }
        // If recovery could have hidden a declaration in any visible frame,
        // the reference is not a clear violation and must fail closed.
        if scopes.iter().any(|scope| scope.declarations_incomplete) {
            return Some(());
        }

        let range = self.identifier_range(identifier);
        let diagnostic = Diagnostic {
            range,
            severity: Some(self.severity),
            code: Some(NumberOrString::String(
                crate::diagnostic_code::UNDEFINED_VARIABLE.to_string(),
            )),
            message: format!("{name} is not defined"),
            ..Default::default()
        };
        // Keep the identifier as private analysis state. `Diagnostic.data` is
        // part of Raven's public LSP and CLI JSON payload, so using it as an
        // internal suppression carrier would change those interfaces.
        if !(self.should_retain)(&diagnostic, name) {
            return Some(());
        }
        self.findings.retain(Finding {
            start_byte: identifier.start_byte(),
            end_byte: identifier.end_byte(),
            name: name.to_string(),
            diagnostic,
        });
        Some(())
    }

    fn identifier_range(&self, identifier: Node<'_>) -> Range {
        let start = identifier.start_position();
        let end = identifier.end_position();
        let start_line = self.line(start.row);
        let end_line = self.line(end.row);
        Range::new(
            Position::new(
                start.row as u32,
                byte_offset_to_utf16_column(start_line, start.column),
            ),
            Position::new(
                end.row as u32,
                byte_offset_to_utf16_column(end_line, end.column),
            ),
        )
    }

    fn line(&self, row: usize) -> &str {
        let Some(&start) = self.line_starts.get(row) else {
            return "";
        };
        let end = self
            .line_starts
            .get(row + 1)
            .map_or(self.text.len(), |next| next.saturating_sub(1));
        self.text.get(start..end).unwrap_or("")
    }
}

fn named_child_of_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

fn is_recovery_node(node: Node<'_>) -> bool {
    node.kind() == "ERROR" || node.is_missing()
}

fn is_declaration(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "var_decl" | "top_var_decl" | "top_var_decl_no_assign"
    )
}

fn is_semantic_extra(node: Node<'_>) -> bool {
    matches!(node.kind(), "comment" | "preproc_include")
}

fn node_text<'a>(node: Node<'_>, text: &'a str) -> Option<&'a str> {
    text.get(node.byte_range())
}

/// Text of a concrete, non-recovered identifier.
///
/// Tree-sitter represents some required recovery points as zero-width
/// `MISSING identifier` nodes. Treating their empty slice as a real name would
/// manufacture `<empty> is not defined` diagnostics at the recovery cursor.
fn identifier_text<'a>(node: Node<'_>, text: &'a str) -> Option<&'a str> {
    if node.kind() != "identifier"
        || node.is_missing()
        || node.has_error()
        || node.start_byte() >= node.end_byte()
    {
        return None;
    }
    let value = node_text(node, text)?;
    (!value.is_empty()).then_some(value)
}

fn is_implicit_stan_variable(name: &str) -> bool {
    // These are statement-level compiler variables, not ordinary declarations.
    // Current grammar versions parse their special uses structurally, but keep
    // the namespace explicit so a future variable-expression shape cannot turn
    // them into false positives.
    matches!(name, "target" | "jacobian")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stan;

    fn findings(source: &str) -> Vec<Diagnostic> {
        let tree = stan::parse(source).expect("Stan parse");
        collect_undefined_variables(
            tree.root_node(),
            source,
            DiagnosticSeverity::WARNING,
            &DiagCancelToken::never(),
            |_, _| true,
        )
        .expect("not cancelled")
    }

    fn names(source: &str) -> Vec<String> {
        findings(source)
            .into_iter()
            .map(|finding| finding.message.replace(" is not defined", ""))
            .collect()
    }

    #[test]
    fn comma_declarations_bind_each_declarator_left_to_right() {
        assert!(
            findings("model { real a = a; }").is_empty(),
            "stanc makes the current local visible in its initializer"
        );
        assert_eq!(names("model { real a = b, b = 1; }"), ["b"]);
        assert!(findings("model { real a = 1, b = a, c = b; }").is_empty());
        assert!(
            findings("model { real duplicate = 1, duplicate = duplicate; }").is_empty(),
            "an already-bound duplicate must not create a bogus undefined use"
        );
    }

    #[test]
    fn block_transition_matrix_and_lexical_statement_scopes() {
        let cases: &[(&str, &str, &[&str])] = &[
            (
                "all exporting transitions",
                "data { int N; } transformed data { real td = N; } parameters { real<lower=td> p; } transformed parameters { real tp = p + td; } model { target += tp; } generated quantities { real gq = tp + p + td + N; }",
                &[],
            ),
            (
                "model locals do not export",
                "model { real model_local = 1; } generated quantities { real gq = model_local; }",
                &["model_local"],
            ),
            (
                "later declarations are not visible earlier",
                "transformed data { real earlier = later; real later = 1; } model {}",
                &["later"],
            ),
            (
                "if locals stay inside",
                "model { if (1) { real branch_local = 1; print(branch_local); } print(branch_local); }",
                &["branch_local"],
            ),
            (
                "while locals stay inside",
                "model { while (0) { real while_local = 1; print(while_local); } print(while_local); }",
                &["while_local"],
            ),
            (
                "profile locals stay inside",
                "model { profile(\"work\") { real profile_local = 1; print(profile_local); } print(profile_local); }",
                &["profile_local"],
            ),
        ];
        for (label, source, expected) in cases {
            assert_eq!(names(source), *expected, "{label}");
        }
    }

    #[test]
    fn recovered_functions_never_create_empty_or_unsafe_callable_findings() {
        let recovered_body = "functions { real hidden(real x) = 1; } model {}";
        assert!(
            findings(recovered_body).is_empty(),
            "a recovered unbraced function body is syntax-only"
        );

        let recovered_name = "functions { real hidden ??? } data { int N; array[N] real x; } model { target += reduce_sum(hidden, x, 1) + unrelated; }";
        assert_eq!(names(recovered_name), ["unrelated"]);
        assert!(
            findings(recovered_name)
                .iter()
                .all(|finding| !finding.message.starts_with(" is not defined")),
            "zero-width missing identifiers must never become findings"
        );

        let clear_higher_order =
            "data { int N; array[N] real x; } model { target += reduce_sum(missing_fun, x, 1); }";
        assert_eq!(names(clear_higher_order), ["missing_fun"]);
    }

    #[test]
    fn one_functions_recovery_node_can_supply_multiple_callable_candidates() {
        let source = "functions { real f ??? real g ??? } data { int N; array[N] real x; } model { target += reduce_sum(f, x, 1) + reduce_sum(g, x, 1) + unrelated; }";
        let tree = stan::parse(source).expect("Stan parse");
        let functions = tree.root_node().named_child(0).expect("functions block");
        let mut cursor = functions.walk();
        let direct_errors: Vec<_> = functions
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "ERROR")
            .collect();
        assert_eq!(
            direct_errors.len(),
            1,
            "fixture must exercise two declarations in one ERROR: {}",
            tree.root_node().to_sexp()
        );
        assert_eq!(
            names(source),
            ["unrelated"],
            "{}",
            tree.root_node().to_sexp()
        );
    }

    #[test]
    fn fragments_without_real_blocks_are_ineligible() {
        for marker in [
            "functions",
            "data",
            "transformed_data",
            "parameters",
            "transformed_parameters",
            "model",
            "generated_quantities",
        ] {
            let source = format!("//--- {marker}\nreal x = external_value;\n");
            assert!(findings(&source).is_empty(), "{marker}");
        }
        assert!(findings("real x = external_value;\n").is_empty());
    }

    #[test]
    fn block_order_and_optional_blocks_control_visibility() {
        let source = "data { int N; }\nparameters { vector[N] beta; }\nmodel { target += N + beta[1] + missing; }\ngenerated quantities { real y = beta[1] + missing_gq; }\n";
        assert_eq!(names(source), ["missing", "missing_gq"]);
        assert_eq!(
            names("model { target += from_r; }\n"),
            ["from_r"],
            "missing optional data does not imply fragment scope"
        );
        assert_eq!(
            names("model { target += later; } generated quantities { real later = 1; }\n"),
            ["later"]
        );
    }

    #[test]
    fn declaration_type_parts_precede_names_while_initializers_see_current_name() {
        let source = "transformed data { array[n] real x; real<lower=bound> y = init; real self = self; array[dim_self] real dim_self; real<lower=constraint_self> constraint_self; int n = 1; real bound = 0; real init = 0; } model {}\n";
        assert_eq!(
            names(source),
            ["n", "bound", "init", "dim_self", "constraint_self"]
        );
    }

    #[test]
    fn functions_locals_nested_blocks_and_loops_have_lexical_scope() {
        let source = r#"functions {
  real helper(real x) {
    real y = x;
    { real z = y; print(z); }
    print(z);
    for (n in 1:limit) { real q = n; print(q); }
    print(n);
    return y + missing_fn;
  }
}
data { int limit; }
model { real local = limit; { real nested = local; print(nested); } print(nested); }
"#;
        assert_eq!(names(source), ["z", "limit", "n", "missing_fn", "nested"]);
    }

    #[test]
    fn for_range_precedes_loopvar_even_with_comment_extras() {
        let source = "model { for (n /* comment */ in 1:n) print(n); print(n); }\n";
        assert_eq!(names(source), ["n", "n"]);
    }

    #[test]
    fn call_distribution_declaration_and_loop_roles_are_not_references() {
        let source = r#"functions {
  real external(real x);
  real user_fun(real x) { return x; }
  real higher_order(real x) { return x; }
}
parameters { real theta; }
model {
  theta ~ mystery_distribution(0, 1);
  target += mystery_call(theta);
  for (loopvar in 1:2) print(loopvar);
  target += higher_order(user_fun, exp, external, theta);
}
"#;
        assert!(findings(source).is_empty());
    }

    #[test]
    fn user_probability_functions_supply_unnormalized_higher_order_aliases() {
        let source = r#"functions {
  real foo_lpdf(array[] real slice, int start, int end) {
    return normal_lpdf(slice | 0, 1);
  }
  real bar_lpmf(array[] int slice, int start, int end) {
    return poisson_lpmf(slice | 1);
  }
}
data {
  array[2] real y;
  array[2] int n;
}
model {
  target += reduce_sum(foo_lupdf, y, 1);
  target += reduce_sum(bar_lupmf, n, 1);
  target += reduce_sum(missing_lupdf, y, 1);
}
"#;
        assert_eq!(names(source), ["missing_lupdf"]);
    }

    #[test]
    fn includes_fail_closed_for_the_whole_file() {
        for (label, source) in [
            (
                "before blocks",
                "#include declarations.stan\nmodel { target += unknown; }\n",
            ),
            (
                "after blocks",
                "model { target += unknown; }\n#include declarations.stan\n",
            ),
            (
                "nested lexical scope",
                "model { if (1) { #include declarations.stan\n target += unknown; } }\n",
            ),
        ] {
            assert!(findings(source).is_empty(), "{label}");
        }
    }

    #[test]
    fn recovery_taints_only_the_scope_that_may_have_lost_a_declaration() {
        let source = "transformed data { real known = 1; real broken = ; print(hidden); }\nmodel { target += model_unknown; }\ngenerated quantities { real y = gq_unknown; }\n";
        let observed = names(source);
        assert!(
            !observed.contains(&"hidden".to_string()),
            "same recovered declaration scope must fail closed: {observed:?}"
        );
        assert!(
            !observed.contains(&"model_unknown".to_string()),
            "exported transformed-data taint can hide a declaration: {observed:?}"
        );
        assert!(
            !observed.contains(&"gq_unknown".to_string()),
            "exported taint remains visible downstream: {observed:?}"
        );

        let independent =
            "model { target += ; }\ngenerated quantities { real y = clean_unknown; }\n";
        assert_eq!(names(independent), ["clean_unknown"]);
    }

    #[test]
    fn findings_are_ordered_deduplicated_utf16_and_bounded() {
        let mut source = String::from("model {\n  print(\"😀\");\n");
        for index in 0..525 {
            source.push_str(&format!("  target += missing_{index};\n"));
        }
        source.push_str("}\n");
        let results = findings(&source);
        assert_eq!(results.len(), MAX_STAN_SEMANTIC_DIAGNOSTICS);
        assert_eq!(results[0].message, "missing_0 is not defined");
        assert!(
            results.iter().all(|finding| finding.data.is_none()),
            "internal identifier state must not leak into LSP diagnostics"
        );
        assert_eq!(results[0].range.start, Position::new(2, 12));
        assert_eq!(results[499].message, "missing_499 is not defined");
        assert!(results.windows(2).all(|pair| {
            (
                pair[0].range.start.line,
                pair[0].range.start.character,
                pair[0].range.end.line,
                pair[0].range.end.character,
            ) < (
                pair[1].range.start.line,
                pair[1].range.start.character,
                pair[1].range.end.line,
                pair[1].range.end.character,
            )
        }));
    }

    #[test]
    fn retained_findings_exactly_deduplicate_the_same_candidate() {
        let diagnostic = Diagnostic {
            range: Range::new(Position::new(1, 2), Position::new(1, 9)),
            severity: Some(DiagnosticSeverity::WARNING),
            code: Some(NumberOrString::String(
                crate::diagnostic_code::UNDEFINED_VARIABLE.to_string(),
            )),
            message: "missing is not defined".to_string(),
            ..Default::default()
        };
        let finding = Finding {
            start_byte: 10,
            end_byte: 17,
            name: "missing".to_string(),
            diagnostic,
        };
        let mut retained = RetainedFindings::default();
        retained.retain(finding.clone());
        retained.retain(finding);
        assert_eq!(retained.into_diagnostics().len(), 1);
    }

    #[test]
    fn retained_findings_displace_the_last_entry_for_a_late_earlier_candidate() {
        let make = |start: usize| Finding {
            start_byte: start,
            end_byte: start + 1,
            name: format!("n{start}"),
            diagnostic: Diagnostic::default(),
        };
        let mut retained = RetainedFindings::default();
        for start in (1..=MAX_STAN_SEMANTIC_DIAGNOSTICS).rev() {
            retained.retain(make(start));
        }

        retained.retain(make(0));

        let keys: Vec<_> = retained
            .entries
            .iter()
            .map(|finding| finding.start_byte)
            .collect();
        assert_eq!(keys.len(), MAX_STAN_SEMANTIC_DIAGNOSTICS);
        assert_eq!(keys[0], 0, "the late earlier candidate must be retained");
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            !keys.contains(&MAX_STAN_SEMANTIC_DIAGNOSTICS),
            "the previous final entry must be displaced"
        );
    }

    #[test]
    fn traversal_continues_after_saturation_and_late_cancellation_discards_results() {
        let mut source = String::from("transformed data {\n");
        for index in 0..600 {
            source.push_str(&format!("  real value_{index} = missing_{index};\n"));
        }
        source.push_str("}\nmodel {}\n");
        let tree = stan::parse(&source).expect("Stan parse");
        let transformed_data = tree
            .root_node()
            .named_child(0)
            .expect("transformed data block");
        let token = tokio_util::sync::CancellationToken::new();
        let cancel = DiagCancelToken::from_token(token.clone());
        let mut analyzer = Analyzer::new(
            &source,
            DiagnosticSeverity::WARNING,
            &cancel,
            FunctionNamespace::default(),
            |_, _| true,
        );
        let mut program_scope = Frame::default();
        analyzer
            .analyze_program_block(transformed_data, &mut program_scope, true)
            .expect("not yet cancelled");
        assert_eq!(
            analyzer.findings.entries.len(),
            MAX_STAN_SEMANTIC_DIAGNOSTICS
        );
        assert!(
            program_scope.names.contains("value_599"),
            "binding after the 500th finding proves traversal did not stop at saturation"
        );
        token.cancel();
        assert!(
            analyzer.finish().is_none(),
            "late cancellation must fail closed"
        );
    }

    #[test]
    fn practical_deep_lexical_nesting_is_stack_safe() {
        const DEPTH: usize = 256;
        let mut source = String::from("model { ");
        source.push_str(&"{ ".repeat(DEPTH));
        source.push_str("target += deepest_missing; ");
        source.push_str(&"} ".repeat(DEPTH));
        source.push('}');
        assert_eq!(names(&source), ["deepest_missing"]);
    }

    #[test]
    fn cancellation_returns_no_partial_vector() {
        let source = "model { target += missing; }\n";
        let tree = stan::parse(source).unwrap();
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        assert!(
            collect_undefined_variables(
                tree.root_node(),
                source,
                DiagnosticSeverity::WARNING,
                &DiagCancelToken::from_token(token),
                |_, _| true,
            )
            .is_none()
        );
    }
}
