//! Shared conservative collection of R binding forms.
//!
//! Static path folding and static package-vector detection attach different
//! payloads to a binding, but a name's binding count must be identical for
//! both consumers. This module owns the syntax walk, mutation invalidators,
//! and `assign()` argument matching; callers supply only the policy that turns
//! a supported binding site into their candidate payload.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

/// Assignment operator at a binary binding site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AssignmentOperator {
    Left,
    Equals,
    SuperLeft,
    Right,
    SuperRight,
}

/// A binding site that may carry a consumer-specific static payload.
///
/// Other binding forms, including replacement/compound assignments,
/// `rm()`/`remove()`, function parameters, and `for` variables, still count
/// as invalidators but are deliberately not offered as candidates.
#[derive(Clone, Copy, Debug)]
pub(crate) enum BindingSite<'tree> {
    Binary {
        node: Node<'tree>,
        target: Node<'tree>,
        value: Option<Node<'tree>>,
        operator: AssignmentOperator,
        top_level: bool,
        value_is_side_effect_free: bool,
        /// Whether a bare `c()` may safely be interpreted with base semantics
        /// at this binding site by package-vector consumers.
        helpers_trusted: bool,
    },
    AssignCall {
        node: Node<'tree>,
        value: Option<Node<'tree>>,
        value_is_side_effect_free: bool,
        /// Whether a bare `c()` may safely be interpreted with base semantics
        /// at this binding site by package-vector consumers.
        helpers_trusted: bool,
    },
}

impl BindingSite<'_> {
    fn start_byte(self) -> usize {
        match self {
            Self::Binary { node, .. } | Self::AssignCall { node, .. } => node.start_byte(),
        }
    }
}

/// Per-name binding facts with a consumer-defined candidate payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct LexicalScope {
    start_byte: usize,
    end_byte: usize,
}

/// Stable identity for the runtime scope executing an evaluated capture part.
///
/// The wrapper anchor is resolved only after `scope.rs` has built the completed
/// real-plus-synthetic [`FunctionScopeTree`](super::scope::FunctionScopeTree), so
/// captures inside Shiny and foreach synthetic frames retain those frames. The
/// lexical scope is kept separately for binding analysis, which intentionally
/// remains independent of `scope.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeFunctionScopeIdentity {
    anchor_byte: usize,
    lexical_scope: Option<LexicalScope>,
}

impl RuntimeFunctionScopeIdentity {
    pub(crate) fn anchor_byte(self) -> usize {
        self.anchor_byte
    }
}

/// How an evaluated subtree's runtime-containing function is determined.
///
/// Ordinary syntax remains [`Lexical`](Self::Lexical). Evaluated capture parts
/// use [`Explicit`](Self::Explicit) because their source coordinates may sit
/// inside function syntax that is only being quoted: `bquote(function()
/// .(x <- 1))` evaluates the assignment in the frame running `bquote`, not in
/// the never-invoked function whose syntax contains it. Explicit identities are
/// wrapper anchors resolved against the completed real-plus-synthetic scope tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeFunctionScope {
    Lexical,
    Explicit(Option<RuntimeFunctionScopeIdentity>),
}

impl RuntimeFunctionScope {
    /// Freeze the runtime scope currently executing `wrapper` for a capture
    /// control or operand. An already-explicit identity comes from an outer
    /// capture and must survive nested caller-relative wrappers.
    pub(crate) fn for_evaluated_capture_part(self, wrapper: Node) -> Self {
        match self {
            Self::Lexical => Self::Explicit(Some(RuntimeFunctionScopeIdentity {
                anchor_byte: wrapper.start_byte(),
                lexical_scope: lexical_scope(wrapper),
            })),
            Self::Explicit(_) => self,
        }
    }

    /// A closure created inside an evaluated operand establishes a fresh
    /// invocation boundary for its formals/defaults/body.
    pub(crate) fn enter_function(self) -> Self {
        Self::Lexical
    }

    pub(crate) fn is_function_scoped_at(self, node: Node) -> bool {
        self.lexical_scope_at(node).is_some()
    }

    fn lexical_scope_at(self, node: Node) -> Option<LexicalScope> {
        match self {
            Self::Lexical => lexical_scope(node),
            Self::Explicit(Some(identity)) => identity.lexical_scope,
            Self::Explicit(None) => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Binding<T> {
    count: u32,
    /// Collector-wide immediate-invalidation generation already reflected in
    /// `count`. Advancing the collector generation is O(1); a binding pays its
    /// pending delta only when next touched or during finalization.
    generation: u32,
    candidate: Option<(T, usize)>,
    safe_eager_offset: Option<usize>,
    first_offset: Option<usize>,
    persistent_uncertainty: bool,
    /// Ordered binding/uncertainty offsets per lexical effect scope. Queries
    /// use binary search, avoiding scans over all mutations.
    shadow_offsets_by_scope: HashMap<Option<LexicalScope>, Vec<usize>>,
    /// Trusted immediate current-frame removals, ordered per lexical scope.
    kill_offsets_by_scope: HashMap<Option<LexicalScope>, Vec<usize>>,
    /// Earliest persistent generic-mutation barrier per lexical scope. These
    /// can invalidate alias assumptions in deferred contexts without making an
    /// unrelated function body shadow a top-level use.
    earliest_persistent_shadow_by_scope: HashMap<Option<LexicalScope>, usize>,
}

impl<T> Default for Binding<T> {
    fn default() -> Self {
        Self {
            count: 0,
            generation: 0,
            candidate: None,
            safe_eager_offset: None,
            first_offset: None,
            persistent_uncertainty: false,
            shadow_offsets_by_scope: HashMap::new(),
            kill_offsets_by_scope: HashMap::new(),
            earliest_persistent_shadow_by_scope: HashMap::new(),
        }
    }
}

impl<T> Binding<T> {
    /// Return the candidate iff this is the name's only binding and it occurs
    /// strictly before `before_byte`.
    #[cfg(test)]
    pub(crate) fn resolved_before(&self, before_byte: usize) -> Option<&T> {
        self.resolved_with_offset_before(before_byte)
            .map(|(candidate, _)| candidate)
    }

    pub(crate) fn resolved_with_offset_before(&self, before_byte: usize) -> Option<(&T, usize)> {
        if self.count != 1 {
            return None;
        }
        let (candidate, offset) = self.candidate.as_ref()?;
        (*offset < before_byte).then_some((candidate, *offset))
    }

    fn has_safe_eager_value_before(&self, before_byte: usize, generation: u32) -> bool {
        self.effective_count(generation) == 1
            && self
                .safe_eager_offset
                .is_some_and(|offset| offset < before_byte)
    }

    fn effective_count(&self, generation: u32) -> u32 {
        self.count
            .saturating_add(generation.saturating_sub(self.generation))
    }

    fn materialize_generation(&mut self, generation: u32) {
        self.count = self.effective_count(generation);
        self.generation = generation;
    }
}

#[derive(Debug)]
struct BindingCollection<T> {
    map: HashMap<String, Binding<T>>,
    immediate_generation: u32,
}

impl<T> Default for BindingCollection<T> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            immediate_generation: 0,
        }
    }
}

/// Collect every statically named binding or invalidation in one AST walk.
///
/// `candidate_for` is called for the first ordinary binary assignment or
/// eligible bare/base `assign()` call seen for each name. Once any binding for a
/// name exists, no later site can be uniquely resolvable, so later sites only
/// update binding facts. Other valid statically named `assign()` calls still
/// count as invalidators. The callback decides whether the first offered site
/// provides a payload; binding counting is independent of that decision. In
/// particular, callers can share exact binding semantics without widening their
/// distinct payload policies.
pub(crate) fn collect_bindings<'tree, T>(
    root: Node<'tree>,
    content: &str,
    mut candidate_for: impl FnMut(BindingSite<'tree>) -> Option<T>,
) -> HashMap<String, Binding<T>> {
    let mut collection = BindingCollection::default();
    let mut visited_effect_nodes = HashSet::new();
    visit_bindings(
        root,
        content,
        &mut collection,
        &mut candidate_for,
        &mut visited_effect_nodes,
        BindingVisitState {
            current_frame_kills_are_definite: true,
            proven_immediate_capture_root: false,
            evaluation_frame: CaptureEvaluationFrame::Caller,
            runtime_function_scope: RuntimeFunctionScope::Lexical,
            known_immediate_context: false,
            eager_assignment_value_root: false,
        },
    );
    // Materializing the collector generation is deliberately deferred until
    // this single O(number of bindings) finalization pass. Repeated immediate
    // invalidations during the walk only advance the generation.
    let persistent_invalidation = collection.map.remove(UNKNOWN_BINDING_KEY).is_some();
    for binding in collection.map.values_mut() {
        binding.materialize_generation(collection.immediate_generation);
        if persistent_invalidation {
            binding.count = binding.count.saturating_add(1);
        }
    }
    collection.map
}

const UNKNOWN_BINDING_KEY: &str = "\0raven-unknown-binding";
const UNKNOWN_HELPER_BINDING_KEY: &str = "\0raven-unknown-helper-binding";
const UNKNOWN_NAMED_BINDING_KEY: &str = "\0raven-unknown-named-binding";
const UNKNOWN_LOADED_BINDING_KEY: &str = "\0raven-unknown-loaded-binding";
const ASSIGN_FORMALS: [&str; 6] = ["x", "value", "pos", "envir", "inherits", "immediate"];
const LOAD_FORMALS: [&str; 3] = ["file", "envir", "verbose"];
const SYS_LOAD_IMAGE_FORMALS: [&str; 2] = ["name", "quiet"];

#[derive(Clone, Copy)]
struct BindingVisitState {
    current_frame_kills_are_definite: bool,
    proven_immediate_capture_root: bool,
    evaluation_frame: CaptureEvaluationFrame,
    runtime_function_scope: RuntimeFunctionScope,
    /// Cached classification for this node. Child classifications are derived
    /// during descent instead of repeatedly walking ancestors.
    known_immediate_context: bool,
    /// This node is the direct value of an immediately evaluated assignment,
    /// modulo transparent braces/parentheses.
    eager_assignment_value_root: bool,
}

fn visit_bindings<'tree, T>(
    node: Node<'tree>,
    content: &str,
    collection: &mut BindingCollection<T>,
    candidate_for: &mut impl FnMut(BindingSite<'tree>) -> Option<T>,
    visited_effect_nodes: &mut HashSet<Node<'tree>>,
    state: BindingVisitState,
) {
    // Conservative capture traversal unions syntax reachable through multiple
    // runtime branches. A mutation node reached by both branches is still one
    // syntactic binding site: count it once while allowing each branch to expose
    // different descendants (notably captured `.()` inside a `..()` operand).
    let first_effect_visit = visited_effect_nodes.insert(node);
    if node.kind() == "call" {
        let capture_is_immediate =
            state.known_immediate_context || state.eager_assignment_value_root;
        let capture = capturing_call_kind(node, content, |name| {
            capture_is_immediate && bare_helper_is_trusted(&collection.map, name)
        });
        if let Some(capture) = capture {
            if first_effect_visit
                && (capture_evaluation_order_has_source_inversion(
                    node,
                    content,
                    capture,
                    state.evaluation_frame,
                ) || capture_invalidation_order_has_source_inversion(
                    node,
                    content,
                    capture,
                    state.evaluation_frame,
                ))
            {
                // Binding queries are keyed by source offsets. When a capture
                // evaluates a syntactically later control before an earlier
                // expression (notably `bquote(..., where = ...)`), those offsets
                // cannot encode the runtime order. Install a persistent barrier
                // before traversing either side: this may reject later static
                // candidates, but it cannot fabricate a binding or dependency
                // edge from the reversed timeline.
                invalidate_persistent_unknown_mutation(collection, node);
            }
            let captured_runtime_scope = state
                .runtime_function_scope
                .for_evaluated_capture_part(node);
            visit_evaluated_capture_parts_for_invalidation(
                node,
                content,
                capture,
                &mut |evaluated, relative_frame, _role, evaluated_kills_are_definite| {
                    let evaluated_frame = relative_frame.relative_to(state.evaluation_frame);
                    visit_bindings(
                        evaluated,
                        content,
                        collection,
                        candidate_for,
                        visited_effect_nodes,
                        BindingVisitState {
                            current_frame_kills_are_definite: state
                                .current_frame_kills_are_definite
                                && evaluated_kills_are_definite,
                            proven_immediate_capture_root: capture_is_immediate,
                            evaluation_frame: evaluated_frame,
                            runtime_function_scope: captured_runtime_scope,
                            known_immediate_context: false,
                            eager_assignment_value_root: false,
                        },
                    );
                },
            );
            return;
        }
    }
    match node.kind() {
        "binary_operator" if first_effect_visit => {
            record_assignment(node, content, collection, candidate_for, state)
        }
        "call" if first_effect_visit => {
            record_mutation_call(node, content, collection, candidate_for, state)
        }
        "function_definition" if first_effect_visit => {
            record_function_params(node, content, collection)
        }
        "for_statement" => {
            // `for (... in NULL)` is provably empty: evaluate no body and add
            // no global mutation barrier, while the iterator remains a binding.
            let provably_empty = node
                .child_by_field_name("sequence")
                .is_some_and(|sequence| sequence.kind() == "null");
            if provably_empty {
                if first_effect_visit && state.evaluation_frame.is_caller_or_global() {
                    record_for_variable(node, content, collection);
                }
                return;
            }
            if first_effect_visit && state.evaluation_frame.is_caller_or_global() {
                // The sequence is evaluated eagerly and the body may execute;
                // either can mutate unrelated bindings through indirect calls.
                invalidate_unknown_mutation_in_context(
                    collection,
                    node,
                    state.known_immediate_context,
                );
                record_for_variable(node, content, collection);
            }
        }
        _ => {}
    }
    let enters_function_execution = node.kind() == "function_definition";
    let transparent_context = matches!(
        node.kind(),
        "braced_expression" | "parenthesized_expression"
    );
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let known_immediate_context =
            node.kind() == "program" || (transparent_context && state.known_immediate_context);
        let eager_assignment_value_root = if transparent_context {
            state.eager_assignment_value_root
        } else {
            state.known_immediate_context && assignment_value_child(node, child, content)
        };
        visit_bindings(
            child,
            content,
            collection,
            candidate_for,
            visited_effect_nodes,
            BindingVisitState {
                current_frame_kills_are_definite: state.current_frame_kills_are_definite,
                proven_immediate_capture_root: !enters_function_execution
                    && state.proven_immediate_capture_root
                    && matches!(
                        node.kind(),
                        "braced_expression" | "parenthesized_expression"
                    ),
                // Evaluating a function definition creates a closure in the
                // surrounding capture frame, but its formals, defaults, and body
                // run only when invoked in the function's own local frame.
                evaluation_frame: if enters_function_execution {
                    CaptureEvaluationFrame::Caller
                } else {
                    state.evaluation_frame
                },
                runtime_function_scope: if enters_function_execution {
                    state.runtime_function_scope.enter_function()
                } else {
                    state.runtime_function_scope
                },
                known_immediate_context,
                eager_assignment_value_root,
            },
        );
    }
}

fn assignment_value_child(parent: Node, child: Node, content: &str) -> bool {
    if parent.kind() != "binary_operator" {
        return false;
    }
    let Some(operator) = parent.child_by_field_name("operator") else {
        return false;
    };
    let value_field = match node_text(operator, content) {
        "<-" | "=" | "<<-" => "rhs",
        "->" | "->>" => "lhs",
        _ => return false,
    };
    parent
        .child_by_field_name(value_field)
        .is_some_and(|value| value.id() == child.id())
}

fn bump_at<'m, T>(
    collection: &'m mut BindingCollection<T>,
    name: &str,
    node: Node,
) -> &'m mut Binding<T> {
    bump_at_in_scope(collection, name, node, lexical_scope(node))
}

fn bump_at_in_runtime_scope<'m, T>(
    collection: &'m mut BindingCollection<T>,
    name: &str,
    node: Node,
    runtime_function_scope: RuntimeFunctionScope,
) -> &'m mut Binding<T> {
    bump_at_in_scope(
        collection,
        name,
        node,
        runtime_function_scope.lexical_scope_at(node),
    )
}

fn bump_at_in_scope<'m, T>(
    collection: &'m mut BindingCollection<T>,
    name: &str,
    node: Node,
    scope: Option<LexicalScope>,
) -> &'m mut Binding<T> {
    let offset = node.start_byte();
    let entry = collection
        .map
        .entry(name.to_string())
        .or_insert_with(|| Binding {
            generation: collection.immediate_generation,
            ..Default::default()
        });
    entry.materialize_generation(collection.immediate_generation);
    entry.count = entry.count.saturating_add(1);
    entry.first_offset = Some(
        entry
            .first_offset
            .map_or(offset, |existing| existing.min(offset)),
    );
    entry
        .shadow_offsets_by_scope
        .entry(scope)
        .or_default()
        .push(offset);
    entry
}

fn bump_kill_at_in_scope<'m, T>(
    collection: &'m mut BindingCollection<T>,
    name: &str,
    node: Node,
    scope: Option<LexicalScope>,
) -> &'m mut Binding<T> {
    let offset = node.start_byte();
    let entry = bump_at_in_scope(collection, name, node, scope);
    if let Some(offsets) = entry.shadow_offsets_by_scope.get_mut(&scope) {
        offsets.pop();
    }
    entry
        .kill_offsets_by_scope
        .entry(scope)
        .or_default()
        .push(offset);
    entry
}

/// Count a possible current-frame removal without allowing it to restore a
/// base alias. Conservative branch unions use this when the removal is not
/// guaranteed to run: it still invalidates static candidates, but an earlier
/// shadow remains possible in the branch where the removal does not execute.
fn bump_non_restoring_kill_at_in_scope<T>(
    collection: &mut BindingCollection<T>,
    name: &str,
    node: Node,
    scope: Option<LexicalScope>,
) {
    let entry = bump_at_in_scope(collection, name, node, scope);
    if let Some(offsets) = entry.shadow_offsets_by_scope.get_mut(&scope) {
        offsets.pop();
    }
}

fn record_assignment<'tree, T>(
    node: Node<'tree>,
    content: &str,
    collection: &mut BindingCollection<T>,
    candidate_for: &mut impl FnMut(BindingSite<'tree>) -> Option<T>,
    state: BindingVisitState,
) {
    let BindingVisitState {
        evaluation_frame,
        runtime_function_scope,
        proven_immediate_capture_root,
        known_immediate_context,
        ..
    } = state;
    // Grammar fields remain reliable when inline comments appear as named
    // extras inside the binary expression; named-child arity does not.
    let Some(op_node) = node.child_by_field_name("operator") else {
        return;
    };
    let op = node_text(op_node, content);
    let (target_field, value_field, operator) = match op {
        "<-" => ("lhs", "rhs", AssignmentOperator::Left),
        "=" => ("lhs", "rhs", AssignmentOperator::Equals),
        "<<-" => ("lhs", "rhs", AssignmentOperator::SuperLeft),
        "->" => ("rhs", "lhs", AssignmentOperator::Right),
        "->>" => ("rhs", "lhs", AssignmentOperator::SuperRight),
        // Compound assignments mutate their root name but do not expose a
        // statically known resulting value.
        "%<>%" | ":=" => {
            if evaluation_frame == CaptureEvaluationFrame::ExternalOrUnknown {
                return;
            }
            invalidate_unknown_mutation_in_context(collection, node, known_immediate_context);
            if let Some(lhs) = node.child_by_field_name("lhs") {
                match binding_target_name(lhs, content) {
                    Some(BindingTargetName::Known(name)) => {
                        bump_at_in_runtime_scope(collection, &name, node, runtime_function_scope);
                    }
                    Some(BindingTargetName::Unknown) => invalidate_unknown_mutation_in_context(
                        collection,
                        node,
                        known_immediate_context,
                    ),
                    None => {}
                }
            }
            return;
        }
        _ => return,
    };
    let escaping_assignment = matches!(
        operator,
        AssignmentOperator::SuperLeft | AssignmentOperator::SuperRight
    );
    if !escaping_assignment && evaluation_frame == CaptureEvaluationFrame::ExternalOrUnknown {
        return;
    }
    let Some(target) = node.child_by_field_name(target_field) else {
        return;
    };
    let uncertainty_scope = if escaping_assignment {
        None
    } else {
        runtime_function_scope.lexical_scope_at(node)
    };
    let Some(BindingTargetName::Known(name)) = binding_target_name(target, content) else {
        mark_unknown_named_binding_in_scope(collection, node, uncertainty_scope);
        invalidate_unknown_mutation_in_context(collection, node, known_immediate_context);
        return;
    };
    if !matches!(
        target.kind(),
        "identifier" | "string" | "raw_string_literal"
    ) {
        // Replacement assignments evaluate index/target expressions and may
        // dispatch to arbitrary replacement functions before binding the
        // root name.
        invalidate_unknown_mutation_in_context(collection, node, known_immediate_context);
    }
    let value = node.child_by_field_name(value_field);
    let value_is_side_effect_free = value.is_some_and(|value| {
        binding_value_is_side_effect_free(value, content, collection, known_immediate_context)
    });
    if value.is_some() && !value_is_side_effect_free {
        // Ordinary assignment forces its RHS. An identifier may force a
        // delayed/active binding and an arbitrary expression may mutate names
        // unrelated to the assignment target.
        invalidate_unknown_mutation_in_context(collection, node, known_immediate_context);
    }
    let site = BindingSite::Binary {
        node,
        target,
        value,
        operator,
        top_level: known_immediate_context
            || (proven_immediate_capture_root
                && evaluation_frame.is_caller_or_global()
                && !runtime_function_scope.is_function_scoped_at(node)),
        value_is_side_effect_free,
        helpers_trusted: bare_helper_is_trusted(&collection.map, "c"),
    };
    record_site(collection, &name, site, candidate_for);
}

fn record_mutation_call<'tree, T>(
    node: Node<'tree>,
    content: &str,
    collection: &mut BindingCollection<T>,
    candidate_for: &mut impl FnMut(BindingSite<'tree>) -> Option<T>,
    state: BindingVisitState,
) {
    let BindingVisitState {
        proven_immediate_capture_root,
        evaluation_frame,
        runtime_function_scope,
        known_immediate_context,
        ..
    } = state;
    let Some(function) = node.child_by_field_name("function") else {
        return;
    };
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return;
    };
    if let Some(kind) = assign_call_kind(function, content) {
        if kind == AssignCallKind::UnknownNamespace
            || (kind == AssignCallKind::BareCandidate
                && (!known_immediate_context || !bare_helper_is_trusted(&collection.map, "assign")))
        {
            if evaluation_frame.is_caller_or_global() {
                mark_unknown_named_binding_in_scope(collection, node, None);
                invalidate_unknown_mutation_in_context(collection, node, known_immediate_context);
            }
            return;
        }
        if arguments.has_error() {
            return;
        }
        if has_duplicate_exact_names(arguments, content, &ASSIGN_FORMALS) {
            return;
        }
        if arguments_have_uninterpreted_names(arguments, content) {
            // An escaped tag may decode to `envir`/`pos`, so the mutation may
            // escape the lexical frame. Do not project that uncertainty from an
            // external bquote operand back into the caller.
            if evaluation_frame.is_caller_or_global() {
                mark_unknown_named_binding_in_scope(collection, node, None);
                invalidate_unknown_mutation_in_context(collection, node, known_immediate_context);
            }
            return;
        }
        let actuals_are_side_effect_free = argument_values_are_side_effect_free(
            arguments,
            content,
            collection,
            known_immediate_context,
        );
        if !actuals_are_side_effect_free && evaluation_frame.is_caller_or_global() {
            // assign() may force supplied actuals before discovering a later
            // missing required formal. Preserve those possible side effects
            // even when full formal matching cannot produce a binding. An
            // external bquote operand does not turn those ordinary effects into
            // mutations of the caller frame.
            invalidate_unknown_mutation_in_context(collection, node, known_immediate_context);
        }
        let Some(resolved) = resolve_assign_arguments(
            arguments,
            content,
            bare_helper_is_trusted(&collection.map, "globalenv"),
            bare_helper_is_trusted(&collection.map, ".GlobalEnv"),
        ) else {
            return;
        };
        if evaluation_frame == CaptureEvaluationFrame::ExternalOrUnknown
            && resolved.destination != CaptureEvaluationFrame::Global
        {
            return;
        }
        let Some(name) = extract_plain_string(resolved.name, content) else {
            // A dynamic or escaped `x` may decode/evaluate to any name. A
            // non-default destination may escape the lexical frame, so expose
            // that uncertainty to every lexical scope.
            let effect_scope = if resolved.destination == CaptureEvaluationFrame::Caller {
                runtime_function_scope.lexical_scope_at(node)
            } else {
                None
            };
            mark_unknown_named_binding_in_scope(collection, node, effect_scope);
            invalidate_unknown_mutation_in_context(collection, node, known_immediate_context);
            return;
        };
        if matches!(
            kind,
            AssignCallKind::BareCandidate | AssignCallKind::BaseCandidate
        ) && (resolved.destination == CaptureEvaluationFrame::Caller
            || resolved.destination == CaptureEvaluationFrame::Global)
            && (known_immediate_context
                || (proven_immediate_capture_root
                    && !runtime_function_scope.is_function_scoped_at(node)))
        {
            let helpers_trusted = bare_helper_is_trusted(&collection.map, "c");
            record_site(
                collection,
                &name,
                BindingSite::AssignCall {
                    node,
                    value: resolved.value,
                    value_is_side_effect_free: actuals_are_side_effect_free,
                    helpers_trusted,
                },
                candidate_for,
            );
        } else {
            // A same-named function from another namespace may re-export or
            // delegate to base::assign. It is therefore an invalidator, but
            // never a source of a static candidate. Calls with a non-default
            // destination or non-top-level frame receive the same treatment.
            if resolved.destination == CaptureEvaluationFrame::Caller {
                bump_at_in_runtime_scope(collection, &name, node, runtime_function_scope);
            } else {
                bump_at_in_scope(collection, &name, node, None);
            }
        }
    } else if let Some(loader) = trusted_loader_kind(function, content, collection) {
        if arguments.has_error() || has_duplicate_exact_names(arguments, content, loader.formals())
        {
            return;
        }
        let actuals_are_side_effect_free = argument_values_are_side_effect_free(
            arguments,
            content,
            collection,
            known_immediate_context,
        );
        if !actuals_are_side_effect_free && evaluation_frame.is_caller_or_global() {
            // Destination classification applies only to the names restored by
            // load. Supplied argument expressions are forced in the frame
            // evaluating the call and retain their independent mutation effects.
            invalidate_unknown_mutation_in_context(collection, node, known_immediate_context);
        }
        if arguments_have_uninterpreted_names(arguments, content) {
            // An escaped tag may decode to the destination formal. The restored
            // names can therefore reach the global environment even when the
            // visible spelling cannot be classified.
            mark_unknown_loaded_binding_in_scope(collection, node, None);
            return;
        }
        let Some(destination) = resolve_loader_destination(loader, arguments, content, collection)
        else {
            return;
        };
        let effect_scope = match destination.relative_to(evaluation_frame) {
            LoadDestination::Current => runtime_function_scope.lexical_scope_at(node),
            LoadDestination::Global | LoadDestination::Unknown => None,
            LoadDestination::External => return,
        };
        mark_unknown_loaded_binding_in_scope(collection, node, effect_scope);
    } else if callee_leaf_is(function, content, "rm") || callee_leaf_is(function, content, "remove")
    {
        if evaluation_frame == CaptureEvaluationFrame::ExternalOrUnknown
            && (!namespace_is_base(function, content)
                || !remove_call_has_global_destination(
                    arguments,
                    content,
                    bare_helper_is_trusted(&collection.map, "globalenv"),
                    bare_helper_is_trusted(&collection.map, ".GlobalEnv"),
                ))
        {
            return;
        }
        if function.kind() == "namespace_operator" && !namespace_is_base(function, content) {
            invalidate_unknown_mutation_in_context(collection, node, known_immediate_context);
            return;
        }
        if matches!(
            function.kind(),
            "identifier" | "string" | "raw_string_literal"
        ) {
            let helper = if node_is_plain_name(function, content, "rm") {
                "rm"
            } else {
                "remove"
            };
            if (!known_immediate_context && !proven_immediate_capture_root)
                || !bare_helper_is_trusted(&collection.map, helper)
            {
                invalidate_unknown_mutation_in_context(collection, node, known_immediate_context);
                return;
            }
        }
        record_remove_call(node, arguments, content, collection, state);
    } else if callee_leaf_is(function, content, "delayedAssign")
        || callee_leaf_is(function, content, "makeActiveBinding")
    {
        if evaluation_frame.is_caller_or_global() {
            // Both APIs can replace an ordinary binding with one whose later
            // lookup executes arbitrary code. Treat the call as a barrier even
            // when its target appears statically named. Its destination may escape
            // the lexical frame, so alias uncertainty is global.
            mark_unknown_named_binding_in_scope(collection, node, None);
            invalidate_persistent_unknown_mutation(collection, node);
        }
    } else if callee_leaf_has_uninterpreted_escape(function, content) {
        // Without evaluating R escapes, an escaped callee leaf could be
        // `assign`, `rm`, `remove`, or an active/delayed binding constructor.
        // Use the strongest persistent barrier because future reads/writes
        // may themselves execute arbitrary code.
        if evaluation_frame.is_caller_or_global() {
            mark_unknown_named_binding_in_scope(collection, node, None);
            invalidate_persistent_unknown_mutation(collection, node);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AssignCallKind {
    BareCandidate,
    BaseCandidate,
    UnknownNamespace,
}

/// Classify a statically named `assign` callee.
///
/// Bare and `base`-qualified spellings have known base semantics and may
/// provide candidates when a bare call is immediately evaluated and known
/// unshadowed. Other namespaces have unknown mutation semantics.
fn assign_call_kind(function: Node, content: &str) -> Option<AssignCallKind> {
    match function.kind() {
        "identifier" | "string" | "raw_string_literal" => {
            node_is_plain_name(function, content, "assign").then_some(AssignCallKind::BareCandidate)
        }
        "namespace_operator" => {
            let rhs = function.child_by_field_name("rhs")?;
            if !node_is_plain_name(rhs, content, "assign") {
                return None;
            }
            let lhs = function.child_by_field_name("lhs")?;
            Some(if node_is_plain_name(lhs, content, "base") {
                AssignCallKind::BaseCandidate
            } else {
                AssignCallKind::UnknownNamespace
            })
        }
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoaderKind {
    Load,
    SysLoadImage,
}

impl LoaderKind {
    fn formals(self) -> &'static [&'static str] {
        match self {
            Self::Load => &LOAD_FORMALS,
            Self::SysLoadImage => &SYS_LOAD_IMAGE_FORMALS,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoadDestination {
    Current,
    Global,
    External,
    Unknown,
}

impl LoadDestination {
    fn relative_to(self, frame: CaptureEvaluationFrame) -> Self {
        match (self, frame) {
            (Self::Current, CaptureEvaluationFrame::Caller) => Self::Current,
            (Self::Current, CaptureEvaluationFrame::Global) => Self::Global,
            (Self::Current, CaptureEvaluationFrame::ExternalOrUnknown) => Self::External,
            _ => self,
        }
    }
}

fn trusted_loader_kind<T>(
    function: Node,
    content: &str,
    collection: &BindingCollection<T>,
) -> Option<LoaderKind> {
    let classify = |name: &str| match name {
        "load" => Some(LoaderKind::Load),
        "sys.load.image" => Some(LoaderKind::SysLoadImage),
        _ => None,
    };
    match function.kind() {
        "identifier" | "string" | "raw_string_literal" => {
            let (name, kind) = if node_is_plain_name(function, content, "load") {
                ("load", LoaderKind::Load)
            } else if node_is_plain_name(function, content, "sys.load.image") {
                ("sys.load.image", LoaderKind::SysLoadImage)
            } else {
                return None;
            };
            bare_helper_is_trusted(&collection.map, name).then_some(kind)
        }
        "namespace_operator" if namespace_is_base(function, content) => function
            .child_by_field_name("rhs")
            .and_then(|rhs| plain_identifier_name(rhs, content))
            .and_then(classify),
        _ => None,
    }
}

fn resolve_loader_destination<T>(
    loader: LoaderKind,
    arguments: Node,
    content: &str,
    collection: &BindingCollection<T>,
) -> Option<LoadDestination> {
    let matched =
        match_call_arguments(arguments, content, loader.formals(), CallMatchMode::Strict)?;
    let CallActual::Value(_) = matched[0]? else {
        return None;
    };
    if loader == LoaderKind::SysLoadImage {
        // `sys.load.image(name, quiet)` always restores into the global
        // workspace; its second formal controls messages, not destination.
        return Some(LoadDestination::Global);
    }
    Some(match matched[1] {
        None | Some(CallActual::Missing) => LoadDestination::Current,
        Some(CallActual::Value(value)) => classify_load_environment(value, content, collection),
    })
}

fn classify_load_environment<T>(
    value: Node,
    content: &str,
    collection: &BindingCollection<T>,
) -> LoadDestination {
    if value_is_global_environment(
        value,
        content,
        bare_helper_is_trusted(&collection.map, "globalenv"),
        bare_helper_is_trusted(&collection.map, ".GlobalEnv"),
    ) {
        return LoadDestination::Global;
    }
    if value.kind() != "call" {
        return LoadDestination::Unknown;
    }
    let Some(function) = value.child_by_field_name("function") else {
        return LoadDestination::Unknown;
    };
    let Some(arguments) = value.child_by_field_name("arguments") else {
        return LoadDestination::Unknown;
    };
    if arguments.has_error() || arguments_have_uninterpreted_names(arguments, content) {
        return LoadDestination::Unknown;
    }
    let trusted = |name: &str| match function.kind() {
        "identifier" | "string" | "raw_string_literal" => {
            node_is_plain_name(function, content, name)
                && bare_helper_is_trusted(&collection.map, name)
        }
        "namespace_operator" => {
            namespace_is_base(function, content)
                && function
                    .child_by_field_name("rhs")
                    .is_some_and(|rhs| node_is_plain_name(rhs, content, name))
        }
        _ => false,
    };
    let values = complete_call_argument_values(arguments);
    let no_values = values.as_ref().is_some_and(Vec::is_empty);
    if (trusted("parent.frame") || trusted("environment")) && no_values {
        LoadDestination::Current
    } else if (trusted("new.env") && values.is_some())
        || ((trusted("baseenv") || trusted("emptyenv")) && no_values)
    {
        LoadDestination::External
    } else {
        LoadDestination::Unknown
    }
}

fn namespace_is_base(function: Node, content: &str) -> bool {
    function.kind() == "namespace_operator"
        && function
            .child_by_field_name("lhs")
            .is_some_and(|lhs| node_is_plain_name(lhs, content, "base"))
}

/// Whether the leaf of a bare or namespace-qualified callee has `expected`.
fn callee_leaf_is(function: Node, content: &str, expected: &str) -> bool {
    match function.kind() {
        "identifier" | "string" | "raw_string_literal" => {
            node_is_plain_name(function, content, expected)
        }
        "namespace_operator" => function
            .child_by_field_name("rhs")
            .is_some_and(|rhs| node_is_plain_name(rhs, content, expected)),
        _ => false,
    }
}

fn callee_leaf_has_uninterpreted_escape(function: Node, content: &str) -> bool {
    let leaf = match function.kind() {
        "identifier" | "string" | "raw_string_literal" => function,
        "namespace_operator" => match function.child_by_field_name("rhs") {
            Some(rhs) => rhs,
            None => return false,
        },
        _ => return false,
    };
    matches!(leaf.kind(), "identifier" | "string") && node_text(leaf, content).contains('\\')
}

fn arguments_have_uninterpreted_names(arguments: Node, content: &str) -> bool {
    let mut cursor = arguments.walk();
    arguments.children(&mut cursor).any(|argument| {
        argument.kind() == "argument"
            && argument
                .child_by_field_name("name")
                .is_some_and(|name| plain_argument_name(name, content).is_none())
    })
}

fn has_duplicate_exact_names(arguments: Node, content: &str, recognized: &[&str]) -> bool {
    let mut seen = vec![false; recognized.len()];
    let mut cursor = arguments.walk();
    for argument in arguments.children(&mut cursor) {
        let Some(name) = (argument.kind() == "argument")
            .then(|| argument.child_by_field_name("name"))
            .flatten()
            .and_then(|name| plain_argument_name(name, content))
        else {
            continue;
        };
        let Some(index) = recognized.iter().position(|formal| *formal == name) else {
            continue;
        };
        if seen[index] {
            return true;
        }
        seen[index] = true;
    }
    false
}

/// Compare an identifier, backtick identifier, or plain string name without
/// interpreting R escapes.
fn node_is_plain_name(node: Node, content: &str, expected: &str) -> bool {
    let text = node_text(node, content);
    if text == expected {
        return true;
    }
    if let Some(inner) = text
        .strip_prefix('`')
        .and_then(|text| text.strip_suffix('`'))
    {
        return !inner.contains('\\') && inner == expected;
    }
    extract_plain_string(node, content).is_some_and(|name| name == expected)
}

/// Match a bquote macro call head without treating callable string literals as
/// the special `.` / `..` identifiers. Escape-free backtick identifiers remain
/// canonical aliases of the bare names.
fn bquote_macro_function_is(function: Node, content: &str, expected: &str) -> bool {
    plain_identifier_name(function, content) == Some(expected)
}

fn record_site<'tree, T>(
    collection: &mut BindingCollection<T>,
    name: &str,
    site: BindingSite<'tree>,
    candidate_for: &mut impl FnMut(BindingSite<'tree>) -> Option<T>,
) {
    let payload = (!collection.map.contains_key(name))
        .then(|| candidate_for(site))
        .flatten();
    let (site_node, escaping_scope) = match site {
        BindingSite::Binary {
            node,
            operator: AssignmentOperator::SuperLeft | AssignmentOperator::SuperRight,
            ..
        } => (node, true),
        BindingSite::Binary { node, .. } | BindingSite::AssignCall { node, .. } => (node, false),
    };
    let entry = if escaping_scope {
        bump_at_in_scope(collection, name, site_node, None)
    } else {
        bump_at(collection, name, site_node)
    };
    let safe_eager = match site {
        BindingSite::Binary {
            target,
            top_level: true,
            value_is_side_effect_free: true,
            ..
        } => matches!(
            target.kind(),
            "identifier" | "string" | "raw_string_literal"
        ),
        BindingSite::AssignCall {
            value_is_side_effect_free: true,
            ..
        } => true,
        _ => false,
    };
    if entry.count == 1 && safe_eager {
        entry.safe_eager_offset = Some(site.start_byte());
    }
    if entry.candidate.is_none()
        && let Some(payload) = payload
    {
        entry.candidate = Some((payload, site.start_byte()));
    }
}

#[derive(Clone, Copy)]
pub(crate) enum CallActual<'tree> {
    Value(Node<'tree>),
    Missing,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallMatchMode {
    Strict,
    RecoverIncomplete,
}

/// Match R call actuals by exact name, unique partial name, then position.
/// Recovery mode tolerates parser errors caused by missing closing delimiters;
/// semantic consumers continue to use strict mode.
pub(crate) fn match_call_arguments<'tree>(
    arguments: Node<'tree>,
    content: &str,
    formals: &[&str],
    mode: CallMatchMode,
) -> Option<Vec<Option<CallActual<'tree>>>> {
    if mode == CallMatchMode::Strict && arguments.has_error() {
        return None;
    }
    if mode == CallMatchMode::RecoverIncomplete && contains_explicit_error(arguments) {
        return None;
    }
    let mut slots = Vec::new();
    let mut current_argument = None;
    let mut saw_argument_syntax = false;
    let mut cursor = arguments.walk();
    for child in arguments.children(&mut cursor) {
        match child.kind() {
            "argument" => {
                current_argument = Some(child);
                saw_argument_syntax = true;
            }
            "comma" => {
                slots.push(current_argument.take());
                saw_argument_syntax = true;
            }
            _ => {}
        }
    }
    if saw_argument_syntax {
        slots.push(current_argument);
    }

    let mut named = Vec::new();
    let mut positional = Vec::new();
    for slot in slots {
        let Some(argument) = slot else {
            positional.push(CallActual::Missing);
            continue;
        };
        let actual = argument
            .child_by_field_name("value")
            .map(CallActual::Value)
            .unwrap_or(CallActual::Missing);
        if let Some(name) = argument.child_by_field_name("name") {
            named.push((plain_argument_name(name, content)?, actual));
        } else {
            positional.push(actual);
        }
    }

    let mut matched = vec![None; formals.len()];
    let mut partials = Vec::new();
    for (name, value) in named {
        if let Some(index) = formals.iter().position(|formal| *formal == name) {
            if matched[index].replace(value).is_some() {
                return None;
            }
        } else {
            partials.push((name, value));
        }
    }
    for (name, value) in partials {
        if name.is_empty() {
            return None;
        }
        let mut candidates = formals
            .iter()
            .enumerate()
            .filter(|(index, formal)| {
                matched[*index].is_none() && formal.starts_with(name.as_ref())
            })
            .map(|(index, _)| index);
        let index = candidates.next()?;
        if candidates.next().is_some() {
            return None;
        }
        matched[index] = Some(value);
    }
    let mut next_formal = 0;
    for value in positional {
        while next_formal < formals.len() && matched[next_formal].is_some() {
            next_formal += 1;
        }
        if next_formal == formals.len() {
            return None;
        }
        matched[next_formal] = Some(value);
        next_formal += 1;
    }
    Some(matched)
}

fn contains_explicit_error(node: Node) -> bool {
    if node.kind() == "ERROR" {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor).any(contains_explicit_error)
}

/// Resolve the `x` and `value` formals of `assign()` using R's exact,
/// unambiguous-partial, then positional matching order. Calls whose duplicate
/// or colliding named arguments would error are not bindings.
pub(crate) struct ResolvedAssignArguments<'tree> {
    pub(crate) name: Node<'tree>,
    pub(crate) value: Option<Node<'tree>>,
    /// Destination relative to the frame evaluating the `assign()` call.
    pub(crate) destination: CaptureEvaluationFrame,
}

pub(crate) fn resolve_assign_arguments<'tree>(
    arguments: Node<'tree>,
    content: &str,
    bare_globalenv_trusted: bool,
    dot_global_env_trusted: bool,
) -> Option<ResolvedAssignArguments<'tree>> {
    let matched = match_call_arguments(arguments, content, &ASSIGN_FORMALS, CallMatchMode::Strict)?;

    let CallActual::Value(name) = matched[0]? else {
        return None;
    };
    let value = match matched[1]? {
        CallActual::Value(value) => Some(value),
        CallActual::Missing => return None,
    };
    // At top level, omitted/missing `pos` and `envir` select the current frame.
    // `inherits = FALSE` is destination-equivalent to its default when those
    // formals are absent; accept only the unshadowable FALSE constant, not `F`
    // or another expression that merely evaluates to false.
    let pos_omitted = !matches!(matched[2], Some(CallActual::Value(_)));
    let envir_omitted = !matches!(matched[3], Some(CallActual::Value(_)));
    let envir_is_global = matches!(
        matched[3],
        Some(CallActual::Value(value))
            if value_is_global_environment(
                value,
                content,
                bare_globalenv_trusted,
                dot_global_env_trusted,
            )
    );
    let pos_is_global = matches!(
        matched[2],
        Some(CallActual::Value(value)) if matches!(node_text(value, content), "1" | "1L")
    );
    let inherits_is_default = match matched[4] {
        None | Some(CallActual::Missing) => true,
        Some(CallActual::Value(value)) => node_text(value, content) == "FALSE",
    };
    let destination = if !inherits_is_default {
        CaptureEvaluationFrame::ExternalOrUnknown
    } else if pos_omitted && envir_omitted {
        CaptureEvaluationFrame::Caller
    } else if (pos_is_global && envir_omitted) || (pos_omitted && envir_is_global) {
        CaptureEvaluationFrame::Global
    } else {
        CaptureEvaluationFrame::ExternalOrUnknown
    };
    Some(ResolvedAssignArguments {
        name,
        value,
        destination,
    })
}

fn value_is_global_environment(
    value: Node,
    content: &str,
    bare_globalenv_trusted: bool,
    dot_global_env_trusted: bool,
) -> bool {
    if dot_global_env_trusted
        && plain_identifier_name(value, content).is_some_and(|name| name == ".GlobalEnv")
    {
        return true;
    }
    if value.kind() != "call" {
        return false;
    }
    let Some(function) = value.child_by_field_name("function") else {
        return false;
    };
    let Some(arguments) = value.child_by_field_name("arguments") else {
        return false;
    };
    let trusted_callee = match function.kind() {
        "identifier" | "string" | "raw_string_literal" => {
            bare_globalenv_trusted && node_is_plain_name(function, content, "globalenv")
        }
        "namespace_operator" => {
            function
                .child_by_field_name("lhs")
                .is_some_and(|lhs| node_is_plain_name(lhs, content, "base"))
                && function
                    .child_by_field_name("rhs")
                    .is_some_and(|rhs| node_is_plain_name(rhs, content, "globalenv"))
        }
        _ => false,
    };
    trusted_callee
        && complete_call_argument_values(arguments).is_some_and(|values| values.is_empty())
}

fn remove_call_has_global_destination(
    arguments: Node,
    content: &str,
    bare_globalenv_trusted: bool,
    dot_global_env_trusted: bool,
) -> bool {
    let mut cursor = arguments.walk();
    arguments.children(&mut cursor).any(|argument| {
        if argument.kind() != "argument" {
            return false;
        }
        let Some(name) = argument
            .child_by_field_name("name")
            .and_then(|name| plain_argument_name(name, content))
        else {
            return false;
        };
        let Some(value) = argument.child_by_field_name("value") else {
            return false;
        };
        match name.as_ref() {
            "pos" => matches!(node_text(value, content), "1" | "1L"),
            "envir" => value_is_global_environment(
                value,
                content,
                bare_globalenv_trusted,
                dot_global_env_trusted,
            ),
            _ => false,
        }
    })
}

enum BindingTargetName {
    Known(String),
    Unknown,
}

fn binding_target_name(node: Node, content: &str) -> Option<BindingTargetName> {
    if let Some(root) = replacement_root_identifier(node) {
        return Some(
            plain_identifier_name(root, content)
                .map(|name| BindingTargetName::Known(name.to_string()))
                .unwrap_or(BindingTargetName::Unknown),
        );
    }
    extract_plain_string(node, content)
        .map(BindingTargetName::Known)
        .or_else(|| {
            // A quoted assignment target containing escapes may alias any plain
            // name after R decodes it, just like an escaped backtick identifier.
            (node.kind() == "string").then_some(BindingTargetName::Unknown)
        })
}

/// Return the canonical R name for a plain identifier spelling.
///
/// Escape-free backtick spellings are equivalent to their unquoted name, so
/// `` `p` `` and `p` must share binding-map keys. Escaped backtick spellings
/// return `None`: interpreting those escapes is deliberately outside this
/// syntactic collector, and callers must not use them as static candidates.
pub(crate) fn plain_identifier_name<'a>(node: Node, content: &'a str) -> Option<&'a str> {
    if node.kind() != "identifier" {
        return None;
    }
    let text = node_text(node, content);
    let Some(inner) = text
        .strip_prefix('`')
        .and_then(|text| text.strip_suffix('`'))
    else {
        return Some(text);
    };
    (!inner.contains('\\')).then_some(inner)
}

/// Canonicalize an escape-free R argument tag.
///
/// R accepts identifier, backtick, and quoted tags. Escaped spellings return
/// `None` because decoding their runtime formal name requires R evaluation.
pub(crate) fn plain_argument_name<'a>(node: Node, content: &'a str) -> Option<Cow<'a, str>> {
    plain_identifier_name(node, content)
        .map(Cow::Borrowed)
        .or_else(|| extract_plain_string(node, content).map(Cow::Owned))
}

fn replacement_root_identifier(mut node: Node) -> Option<Node> {
    loop {
        match node.kind() {
            "identifier" => return Some(node),
            "subset" | "subset2" => node = node.child_by_field_name("function")?,
            "extract_operator" => node = node.child_by_field_name("lhs")?,
            "parenthesized_expression" => node = node.named_child(0)?,
            "call" => {
                let arguments = node.child_by_field_name("arguments")?;
                let mut cursor = arguments.walk();
                let first = arguments
                    .children(&mut cursor)
                    .find(|child| child.kind() == "argument")?;
                node = first.child_by_field_name("value")?;
            }
            _ => return None,
        }
    }
}

fn record_remove_call<T>(
    call: Node,
    arguments: Node,
    content: &str,
    collection: &mut BindingCollection<T>,
    state: BindingVisitState,
) {
    let BindingVisitState {
        current_frame_kills_are_definite,
        proven_immediate_capture_root,
        known_immediate_context,
        runtime_function_scope,
        ..
    } = state;
    if arguments.has_error() {
        return;
    }
    const REMOVE_OPTIONS: [&str; 4] = ["list", "pos", "envir", "inherits"];
    if has_duplicate_exact_names(arguments, content, &REMOVE_OPTIONS) {
        return;
    }
    if arguments_have_uninterpreted_names(arguments, content) {
        // The escaped tag may decode to an evaluated formal such as `list`
        // or `envir`; its value can therefore perform arbitrary side effects.
        invalidate_unknown_mutation_in_context(collection, call, known_immediate_context);
        return;
    }
    let current_frame_kill = (known_immediate_context || proven_immediate_capture_root)
        && !argument_has_explicit_value(arguments, content, "pos")
        && !argument_has_explicit_value(arguments, content, "envir");
    let mut cursor = arguments.walk();
    for argument in arguments.children(&mut cursor) {
        if argument.kind() != "argument" {
            continue;
        }
        let named = argument
            .child_by_field_name("name")
            .and_then(|name| plain_argument_name(name, content));
        if named
            .as_deref()
            .is_some_and(|name| matches!(name, "pos" | "envir" | "inherits"))
        {
            if argument
                .child_by_field_name("value")
                .is_some_and(|value| !remove_option_is_side_effect_free(value))
            {
                // These options are evaluated by rm(). A call, block, or
                // other dynamic expression can mutate unrelated bindings
                // before the removal itself runs.
                invalidate_unknown_mutation_in_context(collection, call, known_immediate_context);
            }
            continue;
        }
        let Some(value) = argument.child_by_field_name("value") else {
            continue;
        };
        if named.as_deref() == Some("list") {
            let allow_bare_c = known_immediate_context;
            match classify_remove_list_value(value, content, &collection.map, allow_bare_c) {
                RemoveListValue::Static(names) => {
                    for name in names {
                        record_removal(
                            collection,
                            &name,
                            call,
                            current_frame_kill,
                            current_frame_kills_are_definite,
                            runtime_function_scope,
                        );
                    }
                }
                RemoveListValue::Dynamic => {
                    // A dynamic names expression is evaluated by rm(), so it
                    // can both remove any prior binding and perform arbitrary
                    // side effects such as shadowing a helper.
                    invalidate_unknown_mutation_in_context(
                        collection,
                        call,
                        known_immediate_context,
                    );
                }
                RemoveListValue::Invalid => {
                    // The call cannot execute, so it removes nothing.
                }
            }
            continue;
        }
        if value.kind() == "identifier" {
            if let Some(name) = plain_identifier_name(value, content) {
                record_removal(
                    collection,
                    name,
                    call,
                    current_frame_kill,
                    current_frame_kills_are_definite,
                    runtime_function_scope,
                );
            } else {
                invalidate_unknown_removal_in_context(collection, call, known_immediate_context);
            }
        } else if let Some(name) = extract_plain_string(value, content) {
            record_removal(
                collection,
                &name,
                call,
                current_frame_kill,
                current_frame_kills_are_definite,
                runtime_function_scope,
            );
        } else if value.kind() == "string" {
            invalidate_unknown_removal_in_context(collection, call, known_immediate_context);
        }
    }
}

fn record_removal<T>(
    collection: &mut BindingCollection<T>,
    name: &str,
    call: Node,
    current_frame_kill: bool,
    current_frame_kills_are_definite: bool,
    runtime_function_scope: RuntimeFunctionScope,
) {
    let scope = runtime_function_scope.lexical_scope_at(call);
    if current_frame_kill && current_frame_kills_are_definite {
        bump_kill_at_in_scope(collection, name, call, scope);
    } else if current_frame_kill {
        bump_non_restoring_kill_at_in_scope(collection, name, call, scope);
    } else {
        bump_at_in_scope(collection, name, call, scope);
    }
}

fn argument_has_explicit_value(arguments: Node, content: &str, expected: &str) -> bool {
    let mut cursor = arguments.walk();
    arguments.children(&mut cursor).any(|argument| {
        argument.kind() == "argument"
            && argument
                .child_by_field_name("name")
                .and_then(|name| plain_argument_name(name, content))
                .as_deref()
                == Some(expected)
            && argument.child_by_field_name("value").is_some()
    })
}

fn remove_option_is_side_effect_free(value: Node) -> bool {
    matches!(
        value.kind(),
        "string"
            | "raw_string_literal"
            | "float"
            | "integer"
            | "complex"
            | "true"
            | "false"
            | "null"
            | "na"
            | "inf"
    )
}

fn argument_values_are_side_effect_free<T>(
    arguments: Node,
    content: &str,
    collection: &BindingCollection<T>,
    allow_bare_helpers: bool,
) -> bool {
    let mut cursor = arguments.walk();
    arguments.children(&mut cursor).all(|argument| {
        argument.kind() != "argument"
            || argument.child_by_field_name("value").is_none_or(|value| {
                binding_value_is_side_effect_free(value, content, collection, allow_bare_helpers)
            })
    })
}

fn binding_value_is_side_effect_free<T>(
    value: Node,
    content: &str,
    collection: &BindingCollection<T>,
    allow_bare_helpers: bool,
) -> bool {
    if remove_option_is_side_effect_free(value) || value.kind() == "function_definition" {
        return true;
    }
    if value.kind() == "identifier" {
        return plain_identifier_name(value, content)
            .and_then(|name| collection.map.get(name))
            .is_some_and(|binding| {
                binding.has_safe_eager_value_before(
                    value.start_byte(),
                    collection.immediate_generation,
                )
            });
    }
    if value.kind() != "call" {
        return false;
    }
    if let Some(capture) = capturing_call_kind(value, content, |name| {
        allow_bare_helpers && bare_helper_is_trusted(&collection.map, name)
    }) {
        // Whole-capture wrappers do not force their captured code. Wrappers
        // with evaluated controls or splice syntax stay conservative here: the
        // binding walk handles their precise evaluated portions separately.
        return capture == CapturingCallKind::Whole;
    }
    let Some(function) = value.child_by_field_name("function") else {
        return false;
    };
    let helper = [
        "c",
        "file.path",
        "normalizePath",
        "paste0",
        "paste",
        // Environment accessors/constructors may allocate or return an
        // environment but do not themselves mutate caller/global bindings.
        // Their supplied arguments are still checked recursively below.
        "new.env",
        "baseenv",
        "emptyenv",
        "globalenv",
        "parent.frame",
        "environment",
    ]
    .into_iter()
    .find(|helper| callee_leaf_is(function, content, helper));
    let Some(helper) = helper else {
        return false;
    };
    let requires_explicit_base = matches!(helper, "paste0" | "paste");
    let trusted = if function.kind() == "namespace_operator" {
        namespace_is_base(function, content)
    } else {
        !requires_explicit_base
            && allow_bare_helpers
            && bare_helper_is_trusted(&collection.map, helper)
    };
    if !trusted {
        return false;
    }
    let Some(arguments) = value.child_by_field_name("arguments") else {
        return false;
    };
    if arguments.has_error() || arguments_have_uninterpreted_names(arguments, content) {
        return false;
    }
    argument_values_are_side_effect_free(arguments, content, collection, allow_bare_helpers)
}

fn invalidate_existing_bindings<T>(collection: &mut BindingCollection<T>) {
    collection.immediate_generation = collection.immediate_generation.saturating_add(1);
}

pub(crate) fn helper_may_be_shadowed<T>(map: &HashMap<String, Binding<T>>) -> bool {
    map.contains_key(UNKNOWN_HELPER_BINDING_KEY)
}

pub(crate) fn helper_may_be_shadowed_at<T>(
    map: &HashMap<String, Binding<T>>,
    before_byte: usize,
    deferred_use: bool,
) -> bool {
    map.get(UNKNOWN_HELPER_BINDING_KEY).is_some_and(|binding| {
        deferred_use
            || binding
                .first_offset
                .is_none_or(|offset| offset < before_byte)
    })
}

/// Static path helpers additionally treat deferred/persistent uncertainty as a
/// barrier even when its syntax appears later than an immediate use. Package
/// vector consumers intentionally retain the ordinary position-aware policy.
pub(crate) fn path_helper_may_be_shadowed_at<T>(
    map: &HashMap<String, Binding<T>>,
    before_byte: usize,
    deferred_use: bool,
) -> bool {
    map.get(UNKNOWN_HELPER_BINDING_KEY).is_some_and(|binding| {
        binding.persistent_uncertainty
            || deferred_use
            || binding
                .first_offset
                .is_none_or(|offset| offset < before_byte)
    })
}

pub(crate) fn named_binding_may_shadow_at<T>(
    map: &HashMap<String, Binding<T>>,
    name: &str,
    before_byte: usize,
    deferred_use: bool,
) -> bool {
    map.get(name).is_some_and(|binding| {
        deferred_use
            || binding
                .first_offset
                .is_none_or(|offset| offset < before_byte)
    }) || map.get(UNKNOWN_NAMED_BINDING_KEY).is_some_and(|binding| {
        (deferred_use && binding.persistent_uncertainty)
            || binding
                .first_offset
                .is_none_or(|offset| offset < before_byte)
    })
}

/// Whether `name` may be shadowed along `use_node`'s lexical function-scope
/// chain. Bindings in unrelated function bodies do not affect the use. For a
/// deferred use, relevant-scope bindings are conservative regardless of source
/// order; an immediate use considers only earlier events.
pub(crate) fn unknown_loaded_names_may_affect_candidate<T>(
    map: &HashMap<String, Binding<T>>,
    candidate_offset: usize,
    use_node: Node,
    deferred_use: bool,
) -> bool {
    map.get(UNKNOWN_LOADED_BINDING_KEY).is_some_and(|binding| {
        let relevant = |scope| {
            let Some(shadows) = binding.shadow_offsets_by_scope.get(&scope) else {
                return false;
            };
            let after_candidate = shadows.partition_point(|offset| *offset <= candidate_offset);
            if deferred_use {
                after_candidate < shadows.len()
            } else {
                shadows
                    .get(after_candidate)
                    .is_some_and(|offset| *offset < use_node.start_byte())
            }
        };
        scope_chain_any(use_node, relevant)
    })
}

pub(crate) fn unknown_loaded_names_may_shadow_at<T>(
    map: &HashMap<String, Binding<T>>,
    use_node: Node,
    deferred_use: bool,
) -> bool {
    map.get(UNKNOWN_LOADED_BINDING_KEY).is_some_and(|binding| {
        ordered_shadow_events_may_affect_use(binding, use_node, deferred_use)
    })
}

pub(crate) fn named_binding_may_shadow_lexically_at<T>(
    map: &HashMap<String, Binding<T>>,
    name: &str,
    use_node: Node,
    deferred_use: bool,
) -> bool {
    named_alias_may_shadow_lexically_at(map, name, use_node, deferred_use)
        || map.get(UNKNOWN_HELPER_BINDING_KEY).is_some_and(|binding| {
            ordered_shadow_events_may_affect_use(binding, use_node, deferred_use)
                || earliest_shadow_offsets_may_affect_use(
                    &binding.earliest_persistent_shadow_by_scope,
                    use_node,
                    deferred_use,
                )
        })
}

/// Whether a named value alias may be shadowed without treating ordered
/// immediate helper-only uncertainty as a named rebinding. Capture helpers use
/// [`named_binding_may_shadow_lexically_at`]; source argument aliases retain the
/// established persistent helper barrier for deferred and cross-branch effects.
pub(crate) fn named_alias_may_shadow_lexically_at<T>(
    map: &HashMap<String, Binding<T>>,
    name: &str,
    use_node: Node,
    deferred_use: bool,
) -> bool {
    [name, UNKNOWN_NAMED_BINDING_KEY].into_iter().any(|key| {
        map.get(key).is_some_and(|binding| {
            ordered_shadow_events_may_affect_use(binding, use_node, deferred_use)
        })
    }) || unknown_loaded_names_may_shadow_at(map, use_node, deferred_use)
        || map.get(UNKNOWN_HELPER_BINDING_KEY).is_some_and(|binding| {
            earliest_shadow_offsets_may_affect_use(
                &binding.earliest_persistent_shadow_by_scope,
                use_node,
                deferred_use,
            )
        })
}

fn ordered_shadow_events_may_affect_use<T>(
    binding: &Binding<T>,
    use_node: Node,
    deferred_use: bool,
) -> bool {
    let relevant = |scope| {
        let Some(shadows) = binding.shadow_offsets_by_scope.get(&scope) else {
            return false;
        };
        if deferred_use {
            return !shadows.is_empty();
        }
        let before = shadows.partition_point(|offset| *offset < use_node.start_byte());
        let Some(&last_shadow) = before.checked_sub(1).and_then(|index| shadows.get(index)) else {
            return false;
        };
        let last_kill = binding.kill_offsets_by_scope.get(&scope).and_then(|kills| {
            kills
                .partition_point(|offset| *offset < use_node.start_byte())
                .checked_sub(1)
                .and_then(|index| kills.get(index))
        });
        last_kill.is_none_or(|kill| *kill < last_shadow)
    };
    scope_chain_any(use_node, relevant)
}

fn earliest_shadow_offsets_may_affect_use(
    offsets: &HashMap<Option<LexicalScope>, usize>,
    use_node: Node,
    deferred_use: bool,
) -> bool {
    let relevant = |scope| {
        offsets
            .get(&scope)
            .is_some_and(|offset| deferred_use || *offset < use_node.start_byte())
    };
    scope_chain_any(use_node, relevant)
}

fn scope_chain_any(use_node: Node, relevant: impl Fn(Option<LexicalScope>) -> bool) -> bool {
    if relevant(None) {
        return true;
    }
    let mut current = Some(use_node);
    while let Some(node) = current {
        if node.kind() == "function_definition"
            && relevant(Some(LexicalScope {
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
            }))
        {
            return true;
        }
        current = node.parent();
    }
    false
}

fn lexical_scope(node: Node) -> Option<LexicalScope> {
    let mut current = Some(node);
    while let Some(node) = current {
        if node.kind() == "function_definition" {
            return Some(LexicalScope {
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
            });
        }
        current = node.parent();
    }
    None
}

fn bare_helper_is_trusted<T>(map: &HashMap<String, Binding<T>>, name: &str) -> bool {
    !helper_may_be_shadowed(map) && !map.contains_key(name)
}

fn invalidate_unknown_mutation<T>(collection: &mut BindingCollection<T>, node: Node) {
    invalidate_unknown_mutation_in_context(collection, node, is_known_immediate_context(node));
}

fn invalidate_unknown_mutation_in_context<T>(
    collection: &mut BindingCollection<T>,
    node: Node,
    known_immediate_context: bool,
) {
    if !known_immediate_context {
        // A mutation nested in a function/quoted/deferred context may execute
        // after syntactically later assignments. Retain a persistent barrier
        // so AST visitation order cannot make those later candidates appear
        // safe.
        invalidate_persistent_unknown_mutation(collection, node);
    } else {
        invalidate_existing_bindings(collection);
        // Even though syntactically later ordinary assignments overwrite an
        // unknown top-level target and remain viable candidates, the target
        // may have shadowed helper functions such as bare `c`. Retain this
        // separate marker so later helper-dependent analysis stays dynamic.
        bump_at(collection, UNKNOWN_HELPER_BINDING_KEY, node);
    }
}

fn invalidate_persistent_unknown_mutation<T>(collection: &mut BindingCollection<T>, node: Node) {
    bump_at(collection, UNKNOWN_BINDING_KEY, node);
    // Keep helper uncertainty after the all-candidates sentinel is consumed.
    // Lexical alias queries retain this event's syntactic scope; helper trust
    // itself remains deliberately file-wide and persistent.
    let scope = lexical_scope(node);
    let offset = node.start_byte();
    let helper = bump_at(collection, UNKNOWN_HELPER_BINDING_KEY, node);
    helper.persistent_uncertainty = true;
    helper
        .earliest_persistent_shadow_by_scope
        .entry(scope)
        .and_modify(|earliest| *earliest = (*earliest).min(offset))
        .or_insert(offset);
}

fn mark_unknown_loaded_binding_in_scope<T>(
    collection: &mut BindingCollection<T>,
    node: Node,
    scope: Option<LexicalScope>,
) {
    let binding = bump_at_in_scope(collection, UNKNOWN_LOADED_BINDING_KEY, node, scope);
    binding.persistent_uncertainty = true;
}

fn mark_unknown_named_binding<T>(collection: &mut BindingCollection<T>, node: Node) {
    mark_unknown_named_binding_in_scope(collection, node, lexical_scope(node));
}

fn mark_unknown_named_binding_in_scope<T>(
    collection: &mut BindingCollection<T>,
    node: Node,
    scope: Option<LexicalScope>,
) {
    let binding = bump_at_in_scope(collection, UNKNOWN_NAMED_BINDING_KEY, node, scope);
    binding.persistent_uncertainty = true;
}

fn invalidate_unknown_removal_in_context<T>(
    collection: &mut BindingCollection<T>,
    node: Node,
    known_immediate_context: bool,
) {
    if known_immediate_context {
        invalidate_existing_bindings(collection);
    } else {
        bump_at(collection, UNKNOWN_BINDING_KEY, node);
    }
}

/// A statically trusted call whose arguments are captured rather than all
/// evaluated normally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CapturingCallKind {
    /// Every supplied argument is captured and no subtree is evaluated.
    Whole,
    /// `substitute(expr, env)`: `expr` is captured, `env` is evaluated.
    Substitute,
    /// `bquote(expr, where, splice)`: controls, `.()`, and enabled `..()` are evaluated.
    /// Bare vector constructors are trusted only when the binding collector proves
    /// their base meanings at this call site.
    Bquote {
        bare_c_trusted: bool,
        bare_list_trusted: bool,
        bare_parent_frame_trusted: bool,
        bare_environment_trusted: bool,
        bare_globalenv_trusted: bool,
        dot_global_env_trusted: bool,
    },
    /// A namespace-qualified rlang capture helper, retaining the exact helper
    /// identity so traversal can enforce its real formal contract.
    Tidy(TidyCaptureKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TidyCaptureKind {
    Expr,
    Quo,
    Enquo,
    Enexpr,
    Exprs,
    Quos,
    Enquos,
    Enexprs,
}

/// Environment in which one evaluated capture subtree runs, relative to the
/// environment evaluating the capture wrapper itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureEvaluationFrame {
    /// The wrapper's caller frame. This is also where `where` and a dynamic
    /// `splice` control are evaluated.
    Caller,
    /// The process global environment, proven by an unshadowed standard form.
    Global,
    /// An environment that is external to, or cannot be proven identical to,
    /// the wrapper's caller or the global environment.
    ExternalOrUnknown,
}

/// Runtime role of an evaluated capture subtree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureEvaluationRole {
    /// `where`/`env` is forced in the wrapper's caller frame and selects the
    /// environment for later operand evaluation.
    EnvironmentControl,
    /// A dynamic `splice` control is forced in the caller frame and selects
    /// which bquote traversal branch executes.
    SpliceControl,
    /// An unquote or splice operand evaluated in the selected environment.
    Operand,
    /// Another evaluated control supported by a non-bquote capture helper.
    OtherControl,
}

impl CaptureEvaluationFrame {
    /// Compose a nested capture's relative frame with the frame in which its
    /// wrapper is being evaluated.
    pub(crate) fn relative_to(self, parent: Self) -> Self {
        match self {
            Self::Caller => parent,
            Self::Global => Self::Global,
            Self::ExternalOrUnknown => Self::ExternalOrUnknown,
        }
    }

    pub(crate) fn is_caller_or_global(self) -> bool {
        matches!(self, Self::Caller | Self::Global)
    }
}

/// Classify capture wrappers without trusting arbitrary leaf names.
///
/// Exact `base::` and `rlang::` qualifiers have known semantics. Bare base
/// wrappers are accepted only when the caller can prove that spelling is safe
/// at this use site; bare rlang helper names are never sufficient to prove that
/// the attached function actually comes from rlang. Other namespaces are not
/// trusted merely because their member has a familiar leaf name.
pub(crate) fn capturing_call_kind(
    node: Node,
    content: &str,
    mut bare_base_is_trusted: impl FnMut(&str) -> bool,
) -> Option<CapturingCallKind> {
    if node.kind() != "call" {
        return None;
    }
    let function = node.child_by_field_name("function")?;
    match function.kind() {
        "identifier" | "string" | "raw_string_literal" => {
            let name = ["quote", "expression", "substitute", "bquote"]
                .into_iter()
                .find(|name| node_is_plain_name(function, content, name))?;
            if !bare_base_is_trusted(name) {
                return None;
            }
            Some(base_capture_kind(name, &mut bare_base_is_trusted))
        }
        "namespace_operator" => {
            let lhs = function.child_by_field_name("lhs")?;
            let rhs = function.child_by_field_name("rhs")?;
            if node_is_plain_name(lhs, content, "base") {
                let name = ["quote", "expression", "substitute", "bquote"]
                    .into_iter()
                    .find(|name| node_is_plain_name(rhs, content, name))?;
                return Some(base_capture_kind(name, &mut bare_base_is_trusted));
            }
            if node_is_plain_name(lhs, content, "rlang") {
                let kind = [
                    ("expr", TidyCaptureKind::Expr),
                    ("quo", TidyCaptureKind::Quo),
                    ("enquo", TidyCaptureKind::Enquo),
                    ("enexpr", TidyCaptureKind::Enexpr),
                    ("exprs", TidyCaptureKind::Exprs),
                    ("quos", TidyCaptureKind::Quos),
                    ("enquos", TidyCaptureKind::Enquos),
                    ("enexprs", TidyCaptureKind::Enexprs),
                ]
                .into_iter()
                .find_map(|(name, kind)| node_is_plain_name(rhs, content, name).then_some(kind))?;
                return Some(CapturingCallKind::Tidy(kind));
            }
            None
        }
        _ => None,
    }
}

fn base_capture_kind(
    name: &str,
    bare_base_is_trusted: &mut impl FnMut(&str) -> bool,
) -> CapturingCallKind {
    match name {
        "substitute" => CapturingCallKind::Substitute,
        "bquote" => CapturingCallKind::Bquote {
            bare_c_trusted: bare_base_is_trusted("c"),
            bare_list_trusted: bare_base_is_trusted("list"),
            bare_parent_frame_trusted: bare_base_is_trusted("parent.frame"),
            bare_environment_trusted: bare_base_is_trusted("environment"),
            bare_globalenv_trusted: bare_base_is_trusted("globalenv"),
            dot_global_env_trusted: bare_base_is_trusted(".GlobalEnv"),
        },
        "quote" | "expression" => CapturingCallKind::Whole,
        _ => unreachable!("caller restricts base capture names"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureTraversalPolicy {
    /// Emit only subtrees that the trusted call is proven to evaluate.
    PositiveResults,
    /// Inspect uncertain values when doing so prevents unsafe binding reuse.
    ConservativeInvalidation,
}

/// Visit only the portions of a trusted capture wrapper that are proven to
/// evaluate. A call whose actuals fail strict R formal matching evaluates none
/// of them because argument matching errors before forcing any promise.
pub(crate) fn visit_evaluated_capture_parts<'tree>(
    call: Node<'tree>,
    content: &str,
    kind: CapturingCallKind,
    visit: &mut impl FnMut(Node<'tree>, CaptureEvaluationFrame, CaptureEvaluationRole),
) {
    visit_evaluated_capture_parts_with_policy(
        call,
        content,
        kind,
        CaptureTraversalPolicy::PositiveResults,
        &mut |node, frame, role, _| visit(node, frame, role),
    );
}

/// Whether the proven evaluated-root stream moves backward in source
/// coordinates, without filtering roots by effect kind. Removal timelines use
/// this broader predicate because a runtime-earlier `rm(x)` must precede even a
/// plain identifier use in a syntactically earlier captured expression.
pub(crate) fn capture_runtime_order_has_source_inversion(
    call: Node,
    content: &str,
    kind: CapturingCallKind,
) -> bool {
    let mut previous = None;
    let mut inverted = false;
    visit_evaluated_capture_parts(call, content, kind, &mut |root, _frame, _role| {
        if previous.is_some_and(|offset| root.start_byte() < offset) {
            inverted = true;
        }
        previous = Some(root.start_byte());
    });
    inverted
}

/// Whether the proven runtime order of evaluated roots moves backward in source
/// coordinates. Source-position timelines cannot represent such a capture
/// exactly; binding and scope consumers use this signal for conservative
/// fallbacks rather than fabricating effects in the wrong order.
pub(crate) fn capture_evaluation_order_has_source_inversion(
    call: Node,
    content: &str,
    kind: CapturingCallKind,
    parent_frame: CaptureEvaluationFrame,
) -> bool {
    capture_order_has_source_inversion(
        call,
        content,
        kind,
        parent_frame,
        CaptureTraversalPolicy::PositiveResults,
    )
}

/// Conservative invalidation can visit effects from multiple possible branches
/// after positive traversal has stopped. Detect source-order inversion across
/// the single combined stream of every emitted root, including transitions
/// between definite controls and branch-dependent effects, so a syntactically
/// later removal cannot cancel a binding that may execute later at runtime.
fn capture_invalidation_order_has_source_inversion(
    call: Node,
    content: &str,
    kind: CapturingCallKind,
    parent_frame: CaptureEvaluationFrame,
) -> bool {
    capture_order_has_source_inversion(
        call,
        content,
        kind,
        parent_frame,
        CaptureTraversalPolicy::ConservativeInvalidation,
    )
}

fn capture_order_has_source_inversion(
    call: Node,
    content: &str,
    kind: CapturingCallKind,
    parent_frame: CaptureEvaluationFrame,
    policy: CaptureTraversalPolicy,
) -> bool {
    let mut previous = None;
    let mut inverted = false;
    visit_evaluated_capture_parts_with_policy(
        call,
        content,
        kind,
        policy,
        &mut |root, relative_frame, role, _kills_are_definite| {
            let frame = relative_frame.relative_to(parent_frame);
            if frame.is_caller_or_global() {
                if role != CaptureEvaluationRole::SpliceControl
                    && !capture_root_may_affect_bindings(root, content)
                {
                    return;
                }
            } else if !external_capture_root_has_explicit_binding_escape(root, content) {
                return;
            }
            if previous.is_some_and(|offset| root.start_byte() < offset) {
                inverted = true;
            }
            previous = Some(root.start_byte());
        },
    );
    inverted
}

fn capture_root_may_affect_bindings(node: Node, content: &str) -> bool {
    if node.kind() == "function_definition" {
        return false;
    }
    if node.kind() == "call" {
        let pure_environment_call = node
            .child_by_field_name("function")
            .is_some_and(|function| {
                let Some(arguments) = node.child_by_field_name("arguments") else {
                    return false;
                };
                if callee_leaf_is(function, content, "parent.frame") {
                    let Some(matched) =
                        match_call_arguments(arguments, content, &["n"], CallMatchMode::Strict)
                    else {
                        return false;
                    };
                    return match matched[0] {
                        None | Some(CallActual::Missing) => true,
                        Some(CallActual::Value(value)) => {
                            matches!(node_text(value, content), "1" | "1L")
                        }
                    };
                }
                ["environment", "globalenv", "new.env"]
                    .into_iter()
                    .any(|name| callee_leaf_is(function, content, name))
                    && complete_call_argument_values(arguments)
                        .is_some_and(|values| values.is_empty())
            });
        if pure_environment_call {
            return false;
        }
        return true;
    }
    if matches!(
        node.kind(),
        "binary_operator" | "for_statement" | "repeat_statement" | "while_statement"
    ) {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| capture_root_may_affect_bindings(child, content))
}

fn external_capture_root_has_explicit_binding_escape(node: Node, content: &str) -> bool {
    if node.kind() == "function_definition" {
        return false;
    }
    if node.kind() == "binary_operator"
        && node
            .child_by_field_name("operator")
            .is_some_and(|operator| matches!(node_text(operator, content), "<<-" | "->>"))
    {
        return true;
    }
    if node.kind() == "call"
        && let Some(function) = node.child_by_field_name("function")
        && (callee_leaf_is(function, content, "assign")
            || callee_leaf_is(function, content, "rm")
            || callee_leaf_is(function, content, "remove"))
        && node
            .child_by_field_name("arguments")
            .is_some_and(|arguments| arguments_explicitly_target_global(arguments, content))
    {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| external_capture_root_has_explicit_binding_escape(child, content))
}

pub(crate) fn arguments_explicitly_target_global(arguments: Node, content: &str) -> bool {
    let mut cursor = arguments.walk();
    arguments.children(&mut cursor).any(|argument| {
        if argument.kind() != "argument" {
            return false;
        }
        let Some(name) = argument
            .child_by_field_name("name")
            .and_then(|name| plain_argument_name(name, content))
        else {
            return false;
        };
        let Some(value) = argument.child_by_field_name("value") else {
            return false;
        };
        match name.as_ref() {
            "pos" => matches!(node_text(value, content), "1" | "1L"),
            "envir" => matches!(
                node_text(value, content).trim(),
                ".GlobalEnv" | "globalenv()" | "base::globalenv()"
            ),
            _ => false,
        }
    })
}

fn visit_evaluated_capture_parts_for_invalidation<'tree>(
    call: Node<'tree>,
    content: &str,
    kind: CapturingCallKind,
    visit: &mut impl FnMut(Node<'tree>, CaptureEvaluationFrame, CaptureEvaluationRole, bool),
) {
    visit_evaluated_capture_parts_with_policy(
        call,
        content,
        kind,
        CaptureTraversalPolicy::ConservativeInvalidation,
        visit,
    );
}

fn visit_evaluated_capture_parts_with_policy<'tree>(
    call: Node<'tree>,
    content: &str,
    kind: CapturingCallKind,
    policy: CaptureTraversalPolicy,
    visit: &mut impl FnMut(Node<'tree>, CaptureEvaluationFrame, CaptureEvaluationRole, bool),
) {
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return;
    };
    match kind {
        CapturingCallKind::Whole => {}
        CapturingCallKind::Substitute => {
            let Some(matched) =
                match_call_arguments(arguments, content, &["expr", "env"], CallMatchMode::Strict)
            else {
                if policy == CaptureTraversalPolicy::ConservativeInvalidation {
                    visit_all_argument_values(arguments, &mut |value| {
                        visit(
                            value,
                            CaptureEvaluationFrame::Caller,
                            CaptureEvaluationRole::OtherControl,
                            false,
                        )
                    });
                }
                return;
            };
            if let Some(CallActual::Value(env)) = matched[1] {
                visit(
                    env,
                    CaptureEvaluationFrame::Caller,
                    CaptureEvaluationRole::EnvironmentControl,
                    true,
                );
            }
        }
        CapturingCallKind::Bquote {
            bare_c_trusted,
            bare_list_trusted,
            bare_parent_frame_trusted,
            bare_environment_trusted,
            bare_globalenv_trusted,
            dot_global_env_trusted,
        } => {
            let Some(matched) = match_call_arguments(
                arguments,
                content,
                &["expr", "where", "splice"],
                CallMatchMode::Strict,
            ) else {
                if policy == CaptureTraversalPolicy::ConservativeInvalidation {
                    visit_all_argument_values(arguments, &mut |value| {
                        visit(
                            value,
                            CaptureEvaluationFrame::Caller,
                            CaptureEvaluationRole::OtherControl,
                            false,
                        )
                    });
                }
                return;
            };
            let operand_frame = classify_bquote_where_frame(
                matched[1],
                content,
                BquoteWhereHelperTrust {
                    bare_parent_frame: bare_parent_frame_trusted,
                    bare_environment: bare_environment_trusted,
                    bare_globalenv: bare_globalenv_trusted,
                    dot_global_env: dot_global_env_trusted,
                },
            );
            let splice = classify_bquote_splice(matched[2], content);
            let dot_dot_mode = match (splice, policy) {
                (BquoteSplice::False, _) => BquoteDotDotMode::OrdinaryCall,
                (BquoteSplice::True, _) => BquoteDotDotMode::EnabledSplice,
                (BquoteSplice::Unknown, CaptureTraversalPolicy::ConservativeInvalidation) => {
                    BquoteDotDotMode::ConservativeSplice
                }
                (BquoteSplice::Unknown, CaptureTraversalPolicy::PositiveResults) => {
                    BquoteDotDotMode::UnknownSplice
                }
            };

            // base::bquote forces `where` before traversing `expr`. Preserve that
            // order for walkers that can consume it directly. Source-coordinate
            // consumers separately detect inversions and fall back conservatively.
            if let Some(CallActual::Value(where_value)) = matched[1] {
                visit(
                    where_value,
                    CaptureEvaluationFrame::Caller,
                    CaptureEvaluationRole::EnvironmentControl,
                    true,
                );
            }
            if let Some(CallActual::Value(expr)) = matched[0] {
                let splice_value = match (splice, matched[2]) {
                    (BquoteSplice::Unknown, Some(CallActual::Value(value))) => Some(value),
                    (BquoteSplice::True | BquoteSplice::False, _)
                    | (_, None | Some(CallActual::Missing)) => None,
                };
                let mut state = BquoteTraversalState {
                    splice_forced: false,
                    current_frame_kills_are_definite: true,
                };
                visit_bquote_splices(
                    expr,
                    content,
                    BquoteTraversalContext {
                        dot_dot_mode,
                        policy,
                        operand_frame,
                        vector_helpers: BquoteVectorHelperTrust {
                            bare_c: bare_c_trusted,
                            bare_list: bare_list_trusted,
                        },
                    },
                    splice_value,
                    false,
                    &mut state,
                    visit,
                );
            }
        }
        CapturingCallKind::Tidy(kind) => {
            visit_tidy_capture_parts(arguments, content, kind, &mut |node| {
                visit(
                    node,
                    CaptureEvaluationFrame::Caller,
                    CaptureEvaluationRole::OtherControl,
                    true,
                )
            });
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BquoteSplice {
    True,
    False,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BquoteDotDotMode {
    /// With `splice = FALSE`, `..()` is an ordinary recursively traversed call.
    OrdinaryCall,
    /// With proven `splice = TRUE`, `..()` is a splice macro whose malformed
    /// direct uses can definitely abort traversal.
    EnabledSplice,
    /// Conservative invalidation inspects the possible splice branch without
    /// treating its errors as definite: `splice = FALSE` may still run instead.
    ConservativeSplice,
    /// Positive-result walkers suppress uncertain `..()` effects rather than inventing them.
    UnknownSplice,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BquoteVectorHelperTrust {
    bare_c: bool,
    bare_list: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BquoteWhereHelperTrust {
    bare_parent_frame: bool,
    bare_environment: bool,
    bare_globalenv: bool,
    dot_global_env: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BquoteTraversalContext {
    dot_dot_mode: BquoteDotDotMode,
    policy: CaptureTraversalPolicy,
    operand_frame: CaptureEvaluationFrame,
    vector_helpers: BquoteVectorHelperTrust,
}

struct BquoteTraversalState {
    splice_forced: bool,
    current_frame_kills_are_definite: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BquoteSpliceResult {
    VectorLike,
    NonVector,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BquoteTraversalOutcome {
    Complete,
    /// Runtime definitely aborts before any remaining head or tail traversal.
    Aborted,
    /// Positive traversal cannot prove that runtime reaches any remaining syntax.
    Uncertain,
}

impl BquoteTraversalOutcome {
    fn stopped(self) -> bool {
        self != Self::Complete
    }
}

fn classify_bquote_where_frame(
    actual: Option<CallActual>,
    content: &str,
    helpers: BquoteWhereHelperTrust,
) -> CaptureEvaluationFrame {
    let Some(CallActual::Value(value)) = actual else {
        return CaptureEvaluationFrame::Caller;
    };
    classify_bquote_where_value(value, content, helpers)
}

fn classify_bquote_where_value(
    mut value: Node,
    content: &str,
    helpers: BquoteWhereHelperTrust,
) -> CaptureEvaluationFrame {
    loop {
        match value.kind() {
            "parenthesized_expression" => {
                let Some(inner) = value.named_child(0) else {
                    return CaptureEvaluationFrame::ExternalOrUnknown;
                };
                value = inner;
            }
            "braced_expression" => {
                let Some(last_index) = value
                    .named_child_count()
                    .checked_sub(1)
                    .and_then(|index| u32::try_from(index).ok())
                else {
                    return CaptureEvaluationFrame::ExternalOrUnknown;
                };
                let Some(last) = value.named_child(last_index) else {
                    return CaptureEvaluationFrame::ExternalOrUnknown;
                };
                value = last;
            }
            _ => break,
        }
    }

    if node_is_plain_name(value, content, ".GlobalEnv") && helpers.dot_global_env {
        return CaptureEvaluationFrame::Global;
    }
    if value.kind() != "call" {
        return CaptureEvaluationFrame::ExternalOrUnknown;
    }
    let Some(function) = value.child_by_field_name("function") else {
        return CaptureEvaluationFrame::ExternalOrUnknown;
    };
    let Some(arguments) = value.child_by_field_name("arguments") else {
        return CaptureEvaluationFrame::ExternalOrUnknown;
    };

    let bare_or_base = |name: &str, bare_trusted: bool| match function.kind() {
        "identifier" | "string" | "raw_string_literal" => {
            bare_trusted && node_is_plain_name(function, content, name)
        }
        "namespace_operator" => {
            function
                .child_by_field_name("lhs")
                .is_some_and(|lhs| node_is_plain_name(lhs, content, "base"))
                && function
                    .child_by_field_name("rhs")
                    .is_some_and(|rhs| node_is_plain_name(rhs, content, name))
        }
        _ => false,
    };

    if bare_or_base("parent.frame", helpers.bare_parent_frame) {
        let Some(matched) = match_call_arguments(arguments, content, &["n"], CallMatchMode::Strict)
        else {
            return CaptureEvaluationFrame::ExternalOrUnknown;
        };
        return match matched[0] {
            None | Some(CallActual::Missing) => CaptureEvaluationFrame::Caller,
            Some(CallActual::Value(n)) if matches!(node_text(n, content), "1" | "1L") => {
                CaptureEvaluationFrame::Caller
            }
            Some(CallActual::Value(_)) => CaptureEvaluationFrame::ExternalOrUnknown,
        };
    }
    if bare_or_base("environment", helpers.bare_environment) {
        let Some(matched) =
            match_call_arguments(arguments, content, &["fun"], CallMatchMode::Strict)
        else {
            return CaptureEvaluationFrame::ExternalOrUnknown;
        };
        return if matches!(matched[0], None | Some(CallActual::Missing)) {
            CaptureEvaluationFrame::Caller
        } else {
            CaptureEvaluationFrame::ExternalOrUnknown
        };
    }
    if bare_or_base("globalenv", helpers.bare_globalenv)
        && complete_call_argument_values(arguments).is_some_and(|values| values.is_empty())
    {
        return CaptureEvaluationFrame::Global;
    }
    if function.kind() == "namespace_operator"
        && function
            .child_by_field_name("lhs")
            .is_some_and(|lhs| node_is_plain_name(lhs, content, "rlang"))
        && function
            .child_by_field_name("rhs")
            .is_some_and(|rhs| node_is_plain_name(rhs, content, "current_env"))
        && complete_call_argument_values(arguments).is_some_and(|values| values.is_empty())
    {
        return CaptureEvaluationFrame::Caller;
    }

    CaptureEvaluationFrame::ExternalOrUnknown
}

fn classify_bquote_splice(actual: Option<CallActual>, content: &str) -> BquoteSplice {
    match actual {
        None | Some(CallActual::Missing) => BquoteSplice::False,
        Some(CallActual::Value(value)) => match node_text(value, content) {
            "TRUE" => BquoteSplice::True,
            "FALSE" => BquoteSplice::False,
            _ => BquoteSplice::Unknown,
        },
    }
}

fn visit_all_argument_values<'tree>(arguments: Node<'tree>, visit: &mut impl FnMut(Node<'tree>)) {
    let mut cursor = arguments.walk();
    for argument in arguments.children(&mut cursor) {
        if let Some(value) = (argument.kind() == "argument")
            .then(|| argument.child_by_field_name("value"))
            .flatten()
        {
            visit(value);
        }
    }
}

/// Classify the value produced by a direct enabled `..()` operand after its
/// evaluation has completed. `base::bquote` calls `is.vector()` before walking
/// either the preceding call head or the remaining tail, so only a guaranteed
/// vector-like result permits positive traversal to continue.
fn classify_bquote_splice_result(
    node: Node,
    content: &str,
    helpers: BquoteVectorHelperTrust,
) -> BquoteSpliceResult {
    match node.kind() {
        "parenthesized_expression" => node
            .named_child(0)
            .map_or(BquoteSpliceResult::Unknown, |value| {
                classify_bquote_splice_result(value, content, helpers)
            }),
        "braced_expression" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .filter(|child| child.kind() != "comment")
                .last()
                .map_or(BquoteSpliceResult::Unknown, |value| {
                    classify_bquote_splice_result(value, content, helpers)
                })
        }
        "binary_operator" => {
            let Some(operator) = node.child_by_field_name("operator") else {
                return BquoteSpliceResult::Unknown;
            };
            let value_field = match node_text(operator, content) {
                "<-" | "=" | "<<-" => "rhs",
                "->" | "->>" => "lhs",
                _ => return BquoteSpliceResult::Unknown,
            };
            node.child_by_field_name(value_field)
                .map_or(BquoteSpliceResult::Unknown, |value| {
                    classify_bquote_splice_result(value, content, helpers)
                })
        }
        "string" | "raw_string_literal" | "float" | "integer" | "complex" | "true" | "false"
        | "null" | "na" | "inf" => BquoteSpliceResult::VectorLike,
        "function_definition" => BquoteSpliceResult::NonVector,
        "call" => classify_bquote_vector_constructor_result(node, content, helpers),
        _ => BquoteSpliceResult::Unknown,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BquoteVectorConstructor {
    C,
    List,
}

fn classify_bquote_vector_constructor_result(
    call: Node,
    content: &str,
    helpers: BquoteVectorHelperTrust,
) -> BquoteSpliceResult {
    let Some(function) = call.child_by_field_name("function") else {
        return BquoteSpliceResult::Unknown;
    };
    let constructor = match function.kind() {
        "identifier" | "string" | "raw_string_literal" => {
            if node_is_plain_name(function, content, "list") && helpers.bare_list {
                Some(BquoteVectorConstructor::List)
            } else if node_is_plain_name(function, content, "c") && helpers.bare_c {
                Some(BquoteVectorConstructor::C)
            } else {
                None
            }
        }
        "namespace_operator" if namespace_is_base(function, content) => {
            let Some(member) = function.child_by_field_name("rhs") else {
                return BquoteSpliceResult::Unknown;
            };
            if node_is_plain_name(member, content, "list") {
                Some(BquoteVectorConstructor::List)
            } else if node_is_plain_name(member, content, "c") {
                Some(BquoteVectorConstructor::C)
            } else {
                None
            }
        }
        _ => None,
    };
    let Some(constructor) = constructor else {
        return BquoteSpliceResult::Unknown;
    };
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return BquoteSpliceResult::Unknown;
    };
    let Some(values) = complete_call_argument_values(arguments) else {
        return BquoteSpliceResult::Unknown;
    };
    match constructor {
        // list() always returns a list vector after its actuals evaluate.
        BquoteVectorConstructor::List => BquoteSpliceResult::VectorLike,
        // c() is an S3 generic. Restrict the guarantee to operands already known
        // to be unclassed vector-like values, avoiding dispatch through an
        // unknown classed argument.
        BquoteVectorConstructor::C => {
            if values.into_iter().all(|value| {
                classify_bquote_splice_result(value, content, helpers)
                    == BquoteSpliceResult::VectorLike
            }) {
                BquoteSpliceResult::VectorLike
            } else {
                BquoteSpliceResult::Unknown
            }
        }
    }
}

fn complete_call_argument_values(arguments: Node) -> Option<Vec<Node>> {
    if arguments.has_error() {
        return None;
    }
    let mut values = Vec::new();
    let mut comma_count = 0usize;
    let mut cursor = arguments.walk();
    for child in arguments.children(&mut cursor) {
        match child.kind() {
            "comma" => comma_count += 1,
            "argument" => values.push(child.child_by_field_name("value")?),
            _ => {}
        }
    }
    if values.is_empty() {
        (comma_count == 0).then_some(values)
    } else {
        (comma_count + 1 == values.len()).then_some(values)
    }
}

fn visit_bquote_splices<'tree>(
    node: Node<'tree>,
    content: &str,
    context: BquoteTraversalContext,
    splice_value: Option<Node<'tree>>,
    inside_call: bool,
    state: &mut BquoteTraversalState,
    visit: &mut impl FnMut(Node<'tree>, CaptureEvaluationFrame, CaptureEvaluationRole, bool),
) -> BquoteTraversalOutcome {
    let dot_dot_mode = context.dot_dot_mode;
    if node.kind() == "call"
        && let Some(function) = node.child_by_field_name("function")
    {
        if bquote_macro_function_is(function, content, ".") {
            // `unquote()` evaluates exactly `e[[2L]]`; later actuals are inert.
            // A missing first actual errors immediately, so callers must stop
            // traversing syntax that base::bquote never reaches.
            let Some(value) = first_call_actual_value(node) else {
                return BquoteTraversalOutcome::Aborted;
            };
            visit(
                value,
                context.operand_frame,
                CaptureEvaluationRole::Operand,
                state.current_frame_kills_are_definite,
            );
            return BquoteTraversalOutcome::Complete;
        }
        if bquote_macro_function_is(function, content, "..") {
            match dot_dot_mode {
                BquoteDotDotMode::OrdinaryCall => {
                    // With splicing disabled, `..()` has no macro meaning. Walk
                    // it normally so nested `.()` calls still unquote.
                }
                BquoteDotDotMode::EnabledSplice => {
                    force_bquote_splice(splice_value, state, visit);
                    // A root splice errors before forcing its operand. A nested
                    // splice evaluates exactly `macro[[2L]]`; extra actuals are
                    // never evaluated by base::bquote. A missing first actual is
                    // likewise a definite abort.
                    if !inside_call {
                        return BquoteTraversalOutcome::Aborted;
                    }
                    let Some(value) = first_call_actual_value(node) else {
                        return BquoteTraversalOutcome::Aborted;
                    };
                    visit(
                        value,
                        context.operand_frame,
                        CaptureEvaluationRole::Operand,
                        state.current_frame_kills_are_definite,
                    );
                    return BquoteTraversalOutcome::Complete;
                }
                BquoteDotDotMode::ConservativeSplice => {
                    // Inspect the possible enabled branch for invalidation, but
                    // do not propagate its errors: the unknown control may select
                    // ordinary-call traversal instead.
                    force_bquote_splice(splice_value, state, visit);
                    if inside_call {
                        state.current_frame_kills_are_definite = false;
                        if let Some(value) = first_call_actual_value(node) {
                            visit(
                                value,
                                context.operand_frame,
                                CaptureEvaluationRole::Operand,
                                false,
                            );
                        }
                    }
                    return BquoteTraversalOutcome::Complete;
                }
                BquoteDotDotMode::UnknownSplice => {
                    // The control itself is forced, but positive-result walkers
                    // cannot prove that runtime reaches either this subtree or
                    // any remaining enclosing call elements.
                    force_bquote_splice(splice_value, state, visit);
                    return BquoteTraversalOutcome::Uncertain;
                }
            }
        }
    }

    let is_call = matches!(
        node.kind(),
        "call" | "binary_operator" | "parenthesized_expression" | "braced_expression"
    );
    if is_call {
        // For every non-`.()` call, base::bquote tests `splice` before walking
        // the call's elements. R promises memoize this first force.
        force_bquote_splice(splice_value, state, visit);
        let elements = bquote_call_elements(node);
        return visit_bquote_call_elements(&elements, content, context, splice_value, state, visit);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let outcome = visit_bquote_splices(
            child,
            content,
            context,
            splice_value,
            inside_call,
            state,
            visit,
        );
        if outcome.stopped() {
            return outcome;
        }
    }
    BquoteTraversalOutcome::Complete
}

/// Reproduce base::bquote's `unquote.list` order for one call's elements.
///
/// The first direct possible `..()` operand is considered before the preceding
/// elements are recursively unquoted; the tail then repeats the same search.
/// With unknown splicing, neither the enabled branch's operand-first effects nor
/// the disabled branch's prefix-first effects are definite, so positive traversal
/// stops before both. Definite errors likewise propagate outward so neither the
/// remaining list tail nor an enclosing unquote traversal fabricates effects.
fn visit_bquote_call_elements<'tree>(
    elements: &[Node<'tree>],
    content: &str,
    context: BquoteTraversalContext,
    splice_value: Option<Node<'tree>>,
    state: &mut BquoteTraversalState,
    visit: &mut impl FnMut(Node<'tree>, CaptureEvaluationFrame, CaptureEvaluationRole, bool),
) -> BquoteTraversalOutcome {
    let BquoteTraversalContext {
        dot_dot_mode,
        policy,
        vector_helpers,
        ..
    } = context;
    if matches!(
        dot_dot_mode,
        BquoteDotDotMode::EnabledSplice
            | BquoteDotDotMode::ConservativeSplice
            | BquoteDotDotMode::UnknownSplice
    ) && let Some(index) = elements
        .iter()
        .position(|element| is_bquote_dot_dot_call(*element, content))
    {
        if dot_dot_mode == BquoteDotDotMode::UnknownSplice {
            // The enabled branch evaluates the direct splice operand before the
            // prefix, while the disabled branch traverses the prefix first and
            // treats `..()` as an ordinary call. No positive effect in the
            // operand, prefix, or tail is therefore proven under both controls.
            return BquoteTraversalOutcome::Uncertain;
        }
        if dot_dot_mode == BquoteDotDotMode::ConservativeSplice {
            // The operand, prefix, and tail belong to a union of the possible
            // enabled and disabled branches. From this point onward a removal is
            // not guaranteed across every branch and therefore cannot restore a
            // base alias shadowed on another branch.
            state.current_frame_kills_are_definite = false;
        }
        if let Some(value) = first_call_actual_value(elements[index]) {
            // unquote.list evaluates the operand before inspecting its result.
            // Keep those effects even when the subsequent is.vector() gate stops
            // traversal before both the preceding head and remaining tail.
            visit(
                value,
                context.operand_frame,
                CaptureEvaluationRole::Operand,
                state.current_frame_kills_are_definite,
            );
            if matches!(
                dot_dot_mode,
                BquoteDotDotMode::EnabledSplice | BquoteDotDotMode::ConservativeSplice
            ) {
                match classify_bquote_splice_result(value, content, vector_helpers) {
                    BquoteSpliceResult::VectorLike => {}
                    BquoteSpliceResult::NonVector
                        if dot_dot_mode == BquoteDotDotMode::EnabledSplice =>
                    {
                        return BquoteTraversalOutcome::Aborted;
                    }
                    BquoteSpliceResult::NonVector => {
                        // The possible enabled branch aborts, but the possible
                        // disabled branch still traverses `..()` ordinarily.
                    }
                    BquoteSpliceResult::Unknown
                        if policy == CaptureTraversalPolicy::PositiveResults =>
                    {
                        return BquoteTraversalOutcome::Uncertain;
                    }
                    BquoteSpliceResult::Unknown => {
                        // The value may be vector-like, in which case runtime
                        // reaches both branches below. Invalidation must inspect
                        // them so it cannot preserve an unsound static candidate.
                    }
                }
            }
        } else if dot_dot_mode == BquoteDotDotMode::EnabledSplice {
            // unquote.list evaluates the direct splice before recursively
            // unquoting its preceding elements. A malformed splice therefore
            // aborts before both that prefix and the remaining tail.
            return BquoteTraversalOutcome::Aborted;
        }
        for element in &elements[..index] {
            let outcome =
                visit_bquote_splices(*element, content, context, splice_value, true, state, visit);
            if outcome.stopped() {
                return outcome;
            }
        }
        if dot_dot_mode == BquoteDotDotMode::ConservativeSplice {
            visit_bquote_actuals_as_ordinary_call(
                elements[index],
                content,
                context,
                splice_value,
                state,
                visit,
            );
        }
        return visit_bquote_call_elements(
            &elements[index + 1..],
            content,
            context,
            splice_value,
            state,
            visit,
        );
    }

    for element in elements {
        let outcome =
            visit_bquote_splices(*element, content, context, splice_value, true, state, visit);
        if outcome.stopped() {
            return outcome;
        }
    }
    BquoteTraversalOutcome::Complete
}

fn bquote_call_elements(node: Node) -> Vec<Node> {
    if node.kind() == "call" {
        let mut elements = Vec::new();
        if let Some(function) = node.child_by_field_name("function") {
            elements.push(function);
        }
        if let Some(arguments) = node.child_by_field_name("arguments") {
            let mut cursor = arguments.walk();
            elements.extend(arguments.children(&mut cursor).filter_map(|argument| {
                (argument.kind() == "argument")
                    .then(|| argument.child_by_field_name("value"))
                    .flatten()
            }));
        }
        return elements;
    }

    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn is_bquote_dot_dot_call(node: Node, content: &str) -> bool {
    node.kind() == "call"
        && node
            .child_by_field_name("function")
            .is_some_and(|function| bquote_macro_function_is(function, content, ".."))
}

fn first_call_actual_value(call: Node) -> Option<Node> {
    let arguments = call.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    for child in arguments.children(&mut cursor) {
        match child.kind() {
            "argument" => return child.child_by_field_name("value"),
            "comma" => return None,
            _ => {}
        }
    }
    None
}

/// Traverse every direct `..()` actual under the possible `splice = FALSE`
/// branch. The first actual also needs this pass: evaluating it for the possible
/// TRUE branch can capture syntax that disabled-splice bquote traversal reaches,
/// such as `base::quote(.(assign(...)))`.
///
/// A failure in this branch stops only its remaining actuals. The caller still
/// continues with syntax reachable through the enabled branch. The binding
/// collector deduplicates mutation nodes reached by both branch traversals.
fn visit_bquote_actuals_as_ordinary_call<'tree>(
    call: Node<'tree>,
    content: &str,
    context: BquoteTraversalContext,
    splice_value: Option<Node<'tree>>,
    state: &mut BquoteTraversalState,
    visit: &mut impl FnMut(Node<'tree>, CaptureEvaluationFrame, CaptureEvaluationRole, bool),
) {
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return;
    };
    let ordinary_context = BquoteTraversalContext {
        dot_dot_mode: BquoteDotDotMode::OrdinaryCall,
        ..context
    };
    let mut cursor = arguments.walk();
    for child in arguments.children(&mut cursor) {
        let Some(value) = (child.kind() == "argument")
            .then(|| child.child_by_field_name("value"))
            .flatten()
        else {
            continue;
        };
        if visit_bquote_splices(
            value,
            content,
            ordinary_context,
            splice_value,
            true,
            state,
            visit,
        )
        .stopped()
        {
            break;
        }
    }
}

fn force_bquote_splice<'tree>(
    splice_value: Option<Node<'tree>>,
    state: &mut BquoteTraversalState,
    visit: &mut impl FnMut(Node<'tree>, CaptureEvaluationFrame, CaptureEvaluationRole, bool),
) {
    if state.splice_forced {
        return;
    }
    state.splice_forced = true;
    if let Some(splice_value) = splice_value {
        // The splice control is forced before either branch is selected.
        visit(
            splice_value,
            CaptureEvaluationFrame::Caller,
            CaptureEvaluationRole::SpliceControl,
            true,
        );
    }
}

fn visit_tidy_capture_parts<'tree>(
    arguments: Node<'tree>,
    content: &str,
    kind: TidyCaptureKind,
    visit: &mut impl FnMut(Node<'tree>),
) {
    let singular_formal = match kind {
        TidyCaptureKind::Expr | TidyCaptureKind::Quo => Some("expr"),
        TidyCaptureKind::Enquo | TidyCaptureKind::Enexpr => Some("arg"),
        TidyCaptureKind::Exprs
        | TidyCaptureKind::Quos
        | TidyCaptureKind::Enquos
        | TidyCaptureKind::Enexprs => None,
    };
    if let Some(formal) = singular_formal {
        let Some(matched) =
            match_call_arguments(arguments, content, &[formal], CallMatchMode::Strict)
        else {
            // Formal matching errors before the helper body can force an
            // unquote operand. In particular, `rlang::expr(!!x, unused = 2)`
            // must emit no positive or invalidation effects from `x`.
            return;
        };
        if let Some(CallActual::Value(captured)) = matched[0] {
            visit_tidy_splices(captured, content, visit);
        }
        return;
    }

    let controls: &[&str] = match kind {
        TidyCaptureKind::Exprs | TidyCaptureKind::Quos => {
            &[".named", ".ignore_empty", ".unquote_names"]
        }
        TidyCaptureKind::Enquos | TidyCaptureKind::Enexprs => &[
            ".named",
            ".ignore_empty",
            ".ignore_null",
            ".unquote_names",
            ".homonyms",
            ".check_assign",
        ],
        _ => unreachable!("singular tidy helpers returned above"),
    };
    let Some((captured, control_values)) =
        split_tidy_variadic_arguments(arguments, content, controls)
    else {
        return;
    };

    // These helpers force their control formals while setting up dots capture;
    // the expressions in `...` remain captured except for `!!`/`!!!` operands.
    for control in control_values.into_iter().flatten() {
        visit(control);
    }
    for value in captured {
        visit_tidy_splices(value, content, visit);
    }
}

fn split_tidy_variadic_arguments<'tree>(
    arguments: Node<'tree>,
    content: &str,
    controls: &[&str],
) -> Option<(Vec<Node<'tree>>, Vec<Option<Node<'tree>>>)> {
    if arguments.has_error() {
        return None;
    }
    let mut captured = Vec::new();
    let mut control_values = vec![None; controls.len()];
    let mut cursor = arguments.walk();
    for argument in arguments.children(&mut cursor) {
        if argument.kind() != "argument" {
            continue;
        }
        let value = argument.child_by_field_name("value")?;
        let Some(name_node) = argument.child_by_field_name("name") else {
            captured.push(value);
            continue;
        };
        let name = plain_argument_name(name_node, content)?;
        if let Some(index) = controls.iter().position(|control| *control == name) {
            if control_values[index].replace(value).is_some() {
                return None;
            }
        } else {
            // Unknown and partially matching names belong to `...`: all control
            // formals occur after dots and therefore require exact matching.
            captured.push(value);
        }
    }
    Some((captured, control_values))
}

fn visit_tidy_splices<'tree>(
    node: Node<'tree>,
    content: &str,
    visit: &mut impl FnMut(Node<'tree>),
) {
    if let Some(operand) = tidy_unquote_operand(node, content) {
        visit(operand);
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_tidy_splices(child, content, visit);
    }
}

fn tidy_unquote_operand<'tree>(node: Node<'tree>, content: &str) -> Option<Node<'tree>> {
    if node.kind() != "unary_operator" || unary_operator_text(node, content) != Some("!") {
        return None;
    }
    let second = node.child_by_field_name("rhs")?;
    if second.kind() != "unary_operator" || unary_operator_text(second, content) != Some("!") {
        return None;
    }
    let third_or_operand = second.child_by_field_name("rhs")?;
    if third_or_operand.kind() == "unary_operator"
        && unary_operator_text(third_or_operand, content) == Some("!")
    {
        third_or_operand.child_by_field_name("rhs")
    } else {
        Some(third_or_operand)
    }
}

fn unary_operator_text<'a>(node: Node, content: &'a str) -> Option<&'a str> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| !child.is_named())
        .map(|operator| node_text(operator, content))
}

pub(crate) fn is_known_immediate_context(node: Node) -> bool {
    // Direct top-level expressions and transparent brace/parenthesis wrappers
    // execute in source order. Do not climb through calls, conditionals,
    // formulas, functions, or other potentially lazy/non-evaluating syntax.
    let mut current = node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "braced_expression" | "parenthesized_expression" => current = parent,
            "program" => return true,
            _ => return false,
        }
    }
    false
}

enum RemoveListValue {
    Static(Vec<String>),
    Dynamic,
    Invalid,
}

/// Classify `rm(list = ...)` without conflating precise, dynamic, and
/// non-executing expressions.
fn classify_remove_list_value<T>(
    node: Node,
    content: &str,
    map: &HashMap<String, Binding<T>>,
    allow_bare_c: bool,
) -> RemoveListValue {
    if node.kind() == "null" {
        return RemoveListValue::Static(Vec::new());
    }
    if let Some(name) = extract_plain_string(node, content) {
        return RemoveListValue::Static(vec![name]);
    }
    if node.kind() != "call" {
        return RemoveListValue::Dynamic;
    }
    let Some(function) = node.child_by_field_name("function") else {
        return RemoveListValue::Invalid;
    };
    if callee_leaf_is(function, content, "character") {
        let trusted = if function.kind() == "namespace_operator" {
            namespace_is_base(function, content)
        } else {
            allow_bare_c && bare_helper_is_trusted(map, "character")
        };
        if !trusted {
            return RemoveListValue::Dynamic;
        }
        let Some(arguments) = node.child_by_field_name("arguments") else {
            return RemoveListValue::Invalid;
        };
        if arguments.has_error() {
            return RemoveListValue::Invalid;
        }
        let mut values = Vec::new();
        let mut cursor = arguments.walk();
        for argument in arguments.children(&mut cursor) {
            if argument.kind() != "argument" {
                continue;
            }
            let name = argument
                .child_by_field_name("name")
                .and_then(|name| plain_argument_name(name, content));
            if name.as_deref().is_some_and(|name| name != "length") {
                return RemoveListValue::Dynamic;
            }
            values.push(argument.child_by_field_name("value"));
        }
        return match values.as_slice() {
            [] => RemoveListValue::Static(Vec::new()),
            [Some(value)] if matches!(node_text(*value, content), "0" | "0L") => {
                RemoveListValue::Static(Vec::new())
            }
            _ => RemoveListValue::Dynamic,
        };
    }
    let bare_c = match function.kind() {
        "identifier" | "string" | "raw_string_literal" => {
            if !node_is_plain_name(function, content, "c") || !bare_helper_is_trusted(map, "c") {
                return RemoveListValue::Dynamic;
            }
            true
        }
        "namespace_operator" => {
            let explicit_base_c = function
                .child_by_field_name("rhs")
                .is_some_and(|rhs| node_is_plain_name(rhs, content, "c"))
                && function
                    .child_by_field_name("lhs")
                    .is_some_and(|lhs| node_is_plain_name(lhs, content, "base"));
            if !explicit_base_c {
                return RemoveListValue::Dynamic;
            }
            false
        }
        _ => return RemoveListValue::Dynamic,
    };
    if bare_c && !allow_bare_c {
        return RemoveListValue::Dynamic;
    }
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return RemoveListValue::Invalid;
    };
    if arguments.has_error() {
        return RemoveListValue::Invalid;
    }
    let mut names = Vec::new();
    let mut argument_count = 0usize;
    let mut comma_count = 0usize;
    let mut dynamic = false;
    let mut cursor = arguments.walk();
    for argument in arguments.children(&mut cursor) {
        if argument.kind() == "comma" {
            comma_count += 1;
            continue;
        }
        if argument.kind() != "argument" {
            continue;
        }
        argument_count += 1;
        let Some(value) = argument.child_by_field_name("value") else {
            return RemoveListValue::Invalid;
        };
        if argument.child_by_field_name("name").is_some() {
            dynamic = true;
        }
        if let Some(name) = extract_plain_string(value, content) {
            names.push(name);
        } else {
            dynamic = true;
        }
    }
    if argument_count == 0 && comma_count == 0 {
        return RemoveListValue::Static(Vec::new());
    }
    if comma_count + 1 != argument_count {
        return if dynamic {
            // R may evaluate an earlier dynamic actual before encountering a
            // later missing argument. Preserve its possible side effects even
            // though the c() call itself ultimately errors.
            RemoveListValue::Dynamic
        } else {
            RemoveListValue::Invalid
        };
    }
    if dynamic {
        RemoveListValue::Dynamic
    } else {
        RemoveListValue::Static(names)
    }
}

fn record_function_params<T>(node: Node, content: &str, collection: &mut BindingCollection<T>) {
    let Some(parameters) = node.child_by_field_name("parameters") else {
        return;
    };
    let mut cursor = parameters.walk();
    for parameter in parameters.children(&mut cursor) {
        if parameter.kind() == "parameter"
            && let Some(name) = parameter.child_by_field_name("name")
        {
            match name.kind() {
                "identifier" => {
                    if let Some(name_text) = plain_identifier_name(name, content) {
                        bump_at(collection, name_text, name);
                    } else {
                        // An escaped parameter can decode to any ordinary name,
                        // but shadows only within this function's lexical scope.
                        bump_at(collection, UNKNOWN_BINDING_KEY, name);
                        mark_unknown_named_binding(collection, name);
                    }
                }
                // Variadic parameter spellings cannot alias an ordinary name
                // and contain no backtick escapes to interpret.
                "dots" | "dot_dot_i" => {
                    bump_at(collection, node_text(name, content), name);
                }
                _ => {}
            }
        }
    }
}

fn record_for_variable<T>(node: Node, content: &str, collection: &mut BindingCollection<T>) {
    if let Some(variable) = node.child_by_field_name("variable")
        && variable.kind() == "identifier"
    {
        if let Some(name) = plain_identifier_name(variable, content) {
            bump_at(collection, name, variable);
        } else {
            invalidate_unknown_mutation(collection, node);
        }
    }
}

/// Extract a strict bare `c()` of positional plain string literals.
///
/// Returns each decoded string together with its literal node. Empty vectors,
/// named or missing arguments, malformed syntax, non-string values, and string
/// literals requiring escape interpretation are rejected.
pub(crate) fn extract_bare_c_plain_strings<'tree>(
    node: Node<'tree>,
    content: &str,
) -> Option<Vec<(String, Node<'tree>)>> {
    if node.kind() != "call"
        || node
            .child_by_field_name("function")
            .is_none_or(|function| node_text(function, content) != "c")
    {
        return None;
    }
    let arguments = node.child_by_field_name("arguments")?;
    if arguments.has_error() {
        return None;
    }
    let mut strings = Vec::new();
    let mut comma_count = 0usize;
    let mut cursor = arguments.walk();
    for child in arguments.children(&mut cursor) {
        if child.kind() == "comma" {
            comma_count += 1;
            continue;
        }
        if child.kind() != "argument" {
            continue;
        }
        if child.child_by_field_name("name").is_some() {
            return None;
        }
        let value = child.child_by_field_name("value")?;
        if value.kind() != "string" {
            return None;
        }
        strings.push((extract_plain_string(value, content)?, value));
    }
    if strings.is_empty() || comma_count + 1 != strings.len() {
        None
    } else {
        Some(strings)
    }
}

pub(crate) fn extract_plain_string(node: Node, content: &str) -> Option<String> {
    let text = node_text(node, content);
    if let Some(raw) = crate::config_file::lintr_loader::parse_r_raw_string_literal(text) {
        return Some(raw);
    }
    if !((text.starts_with('"') && text.ends_with('"'))
        || (text.starts_with('\'') && text.ends_with('\'')))
    {
        return None;
    }
    let inner = &text[1..text.len() - 1];
    (!inner.contains('\\')).then(|| inner.to_string())
}

fn node_text<'a>(node: Node, content: &'a str) -> &'a str {
    &content[node.byte_range()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Tree;

    fn parse_r(code: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        parser.parse(code, None).unwrap()
    }

    #[test]
    fn shadow_offsets_remain_partitioned_by_lexical_scope() {
        let tree = parse_r("x <- 1");
        let assignment = tree.root_node().named_child(0).unwrap();
        let mut collection = BindingCollection::<()>::default();

        for _ in 0..10_000 {
            bump_at(&mut collection, "x", assignment);
        }

        let binding = collection.map.get("x").unwrap();
        assert_eq!(binding.shadow_offsets_by_scope.len(), 1);
        assert_eq!(
            binding.shadow_offsets_by_scope.get(&None).unwrap().len(),
            10_000
        );
    }

    #[test]
    fn immediate_invalidation_uses_constant_time_generation_barrier() {
        let tree = parse_r("x <- 1");
        let assignment = tree.root_node().named_child(0).unwrap();
        let mut collection = BindingCollection::<()>::default();
        bump_at(&mut collection, "x", assignment);

        for _ in 0..10_000 {
            invalidate_existing_bindings(&mut collection);
        }

        let binding = collection.map.get("x").unwrap();
        assert_eq!(binding.count, 1, "barriers must not scan or mutate entries");
        assert_eq!(binding.generation, 0);
        assert_eq!(collection.immediate_generation, 10_000);
        assert_eq!(
            binding.effective_count(collection.immediate_generation),
            10_001
        );
    }

    #[test]
    fn plain_argument_names_borrow_identifiers_and_own_literals() {
        let code = r#"f(identifier = 1, `backtick` = 2, "quoted" = 3, r"(raw)" = 4)"#;
        let tree = parse_r(code);
        let call = tree.root_node().named_child(0).unwrap();
        let arguments = call.child_by_field_name("arguments").unwrap();
        let mut names = Vec::new();
        for argument in arguments
            .children(&mut arguments.walk())
            .filter(|child| child.kind() == "argument")
        {
            let name = argument.child_by_field_name("name").unwrap();
            names.push(plain_argument_name(name, code).unwrap());
        }
        assert!(matches!(names[0], Cow::Borrowed("identifier")));
        assert!(matches!(names[1], Cow::Borrowed("backtick")));
        assert!(matches!(names[2], Cow::Owned(ref name) if name == "quoted"));
        assert!(matches!(names[3], Cow::Owned(ref name) if name == "raw"));
    }

    #[test]
    fn assign_global_environment_requires_a_canonical_name_or_call() {
        let destination = |code: &str| {
            let tree = parse_r(code);
            let call = tree.root_node().named_child(0).unwrap();
            let arguments = call.child_by_field_name("arguments").unwrap();
            resolve_assign_arguments(arguments, code, true, true)
                .map(|resolved| resolved.destination)
        };

        for code in [
            r#"assign("x", 1, envir = .GlobalEnv)"#,
            r#"assign("x", 1, envir = `.GlobalEnv`)"#,
            r#"assign("x", 1, envir = globalenv())"#,
            r#"assign("x", 1, envir = base::globalenv())"#,
        ] {
            assert_eq!(
                destination(code),
                Some(CaptureEvaluationFrame::Global),
                "{code}"
            );
        }
        for code in [
            r#"assign("x", 1, envir = ".GlobalEnv")"#,
            r#"assign("x", 1, envir = '.GlobalEnv')"#,
            r#"assign("x", 1, envir = r"(.GlobalEnv)")"#,
        ] {
            assert_eq!(
                destination(code),
                Some(CaptureEvaluationFrame::ExternalOrUnknown),
                "{code}"
            );
        }
    }

    #[test]
    fn strict_bare_c_plain_strings_preserve_nodes_and_reject_dynamic_shapes() {
        let code = "c(\"alpha\", # keep node positions\n  'beta')";
        let tree = parse_r(code);
        let call = tree.root_node().named_child(0).unwrap();
        let pairs = extract_bare_c_plain_strings(call, code).unwrap();
        assert_eq!(
            pairs
                .iter()
                .map(|(string, _)| string.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
        assert_eq!(
            pairs
                .iter()
                .map(|(_, node)| &code[node.byte_range()])
                .collect::<Vec<_>>(),
            vec!["\"alpha\"", "'beta'"]
        );

        for code in [
            "c()",
            "c(\"alpha\",)",
            "c(,\"alpha\")",
            "c(\"alpha\",,\"beta\")",
            "c(name = \"alpha\")",
            "c(\"alpha\", dynamic)",
            r#"c("alpha\\nbeta")"#,
            "base::c(\"alpha\")",
            "c(\"alpha\"",
        ] {
            let tree = parse_r(code);
            let call = tree.root_node().named_child(0).unwrap();
            assert!(extract_bare_c_plain_strings(call, code).is_none(), "{code}");
        }
    }

    fn binding_names(code: &str) -> std::collections::HashSet<String> {
        let tree = parse_r(code);
        collect_bindings(tree.root_node(), code, |_| Some(()))
            .into_keys()
            .filter(|name| !name.starts_with('\0'))
            .collect()
    }

    #[test]
    fn root_bquote_dot_dot_skips_operand_but_keeps_where_effects() {
        let names = binding_names(
            r#"bquote(..(operand <- 1), where = { before_error <- 1; parent.frame() }, splice = TRUE)"#,
        );
        assert!(!names.contains("operand"), "root ..() operand must not run");
        assert!(
            names.contains("before_error"),
            "where is forced before the root splice error"
        );
    }

    #[test]
    fn bquote_operand_bindings_follow_where_frame() {
        let code = r#"bquote(.(p <- "good.R"), where = parent.frame())"#;
        let tree = parse_r(code);
        let call = tree.root_node().named_child(0).unwrap();
        let kind = capturing_call_kind(call, code, |_| true).unwrap();
        assert!(!capture_evaluation_order_has_source_inversion(
            call,
            code,
            kind,
            CaptureEvaluationFrame::Caller,
        ));
        assert!(!capture_invalidation_order_has_source_inversion(
            call,
            code,
            kind,
            CaptureEvaluationFrame::Caller,
        ));

        for code in [
            r#"bquote(.(local <- 1), where = new.env())"#,
            r#"bquote(where = new.env(), expr = .(local <- 1))"#,
        ] {
            assert!(!binding_names(code).contains("local"), "{code}");
        }

        for code in [
            r#"bquote(.(local <- 1))"#,
            r#"bquote(.(local <- 1), where = parent.frame())"#,
            r#"bquote(.(local <- 1), where = parent.frame(n = 1L))"#,
            r#"bquote(.(local <- 1), where = environment())"#,
            r#"bquote(expr = .(local <- 1), where = .GlobalEnv)"#,
            r#"bquote(expr = .(local <- 1), where = globalenv())"#,
            r#"bquote(expr = .(local <- 1), where = base::globalenv())"#,
            r#"bquote(where = rlang::current_env(), expr = .(local <- 1))"#,
        ] {
            assert!(binding_names(code).contains("local"), "{code}");
        }

        let names = binding_names(
            r#"bquote(
                where = { control <- 1; new.env() },
                expr = .({ escaped <<- 1; base::assign("global", 1, envir = .GlobalEnv) })
            )"#,
        );
        assert!(names.contains("control"), "where control stays in caller");
        assert!(
            names.contains("escaped"),
            "superassignment keeps escaping effect"
        );
        assert!(
            names.contains("global"),
            "explicit global assign stays visible"
        );
    }

    #[test]
    fn external_bquote_function_execution_uses_function_local_frame() {
        let names = binding_names(
            r#"
            bquote(
                .(function(
                    default = { default_local <- 1 }
                ) {
                    body_local <- 1
                    function() { nested_local <- 1 }
                    escaped <<- 1
                }),
                where = new.env()
            )
            "#,
        );
        for name in ["default_local", "body_local", "nested_local", "escaped"] {
            assert!(names.contains(name), "{name}: {names:?}");
        }
        assert!(
            !binding_names(r#"bquote(.(external_local <- 1), where = new.env())"#)
                .contains("external_local")
        );
    }

    #[test]
    fn bquote_macros_require_identifier_call_heads() {
        for code in [
            r#"bquote("."(literal_dot <- 1))"#,
            r#"bquote(list(".."(literal_dot_dot <- 1)), splice = TRUE)"#,
        ] {
            assert!(binding_names(code).is_empty(), "{code}");
        }

        for (code, expected) in [
            (r#"bquote(`.`(backtick_dot <- 1))"#, "backtick_dot"),
            (
                r#"bquote(list(`..`(backtick_dot_dot <- 1)), splice = TRUE)"#,
                "backtick_dot_dot",
            ),
        ] {
            assert!(binding_names(code).contains(expected), "{code}");
        }
    }

    #[test]
    fn disabled_bquote_dot_dot_traverses_nested_dot_unquotes() {
        for code in [
            r#"bquote(..(.(bound <- 1)))"#,
            r#"bquote(list(..(list(.(bound <- 1)))), splice = FALSE)"#,
        ] {
            assert!(binding_names(code).contains("bound"), "{code}");
        }
    }

    #[test]
    fn bquote_macros_evaluate_only_their_first_actual() {
        for code in [
            r#"bquote(.(first <- 1, extra <- 2))"#,
            r#"bquote(list(..(first <- 1, extra <- 2)), splice = TRUE)"#,
            r#"bquote(list(.(list(.(first <- 1)), extra <- 2)))"#,
        ] {
            let names = binding_names(code);
            assert!(names.contains("first"), "{code}: {names:?}");
            assert!(!names.contains("extra"), "{code}: {names:?}");
        }
    }

    #[test]
    fn conservative_bquote_splice_unions_false_branch_actuals_without_double_counting() {
        let code = r#"bquote(list(..(first <- 1, .(extra <- 2))), splice = flag)"#;
        let tree = parse_r(code);
        let bindings = collect_bindings(tree.root_node(), code, |_| Some(()));

        let first = bindings.get("first").expect("first actual is visited");
        assert!(first.candidate.is_some(), "the candidate is offered once");
        assert_eq!(
            first
                .shadow_offsets_by_scope
                .values()
                .map(Vec::len)
                .sum::<usize>(),
            1,
            "the first actual must record one syntactic binding site"
        );
        assert_eq!(
            first.count, 2,
            "one binding plus the cross-certainty inversion barrier"
        );
        assert!(first.resolved_before(usize::MAX).is_none());
        assert!(
            bindings.contains_key("extra"),
            "the possible false branch must traverse nested .() in extra actuals"
        );
    }

    #[test]
    fn direct_bquote_splice_order_invalidates_a_later_source_candidate() {
        let code = r#"bquote(list(.(rm(x)), ..(x <- 1)), splice = TRUE)"#;
        let tree = parse_r(code);
        let bindings = collect_bindings(tree.root_node(), code, |_| Some(()));
        assert!(
            bindings
                .get("x")
                .is_some_and(|binding| binding.resolved_before(usize::MAX).is_none()),
            "runtime-order inversion must prevent a unique static candidate"
        );
    }

    #[test]
    fn bquote_splice_result_gates_later_binding_traversal() {
        let names = binding_names(
            r#"base::bquote(..({ operand <- 1; function() {} }) + .(tail <- 1), splice = TRUE)"#,
        );
        assert!(names.contains("operand"), "{names:?}");
        assert!(!names.contains("tail"), "{names:?}");

        // Conservative invalidation must inspect the tail when the operand may
        // be vector-like, otherwise a later mutation could leave an unsound
        // static candidate alive.
        let names =
            binding_names(r#"base::bquote(..(unknown) + .(uncertain_tail <- 1), splice = TRUE)"#);
        assert!(names.contains("uncertain_tail"), "{names:?}");

        for operand in ["1", r#"list(1)"#, r#"c(1)"#, r#"base::c(1)"#] {
            let code = format!("base::bquote(..({operand}) + .(vector_tail <- 1), splice = TRUE)");
            let names = binding_names(&code);
            assert!(names.contains("vector_tail"), "{code}: {names:?}");
        }
    }

    #[test]
    fn shadowed_bquote_vector_helpers_are_not_trusted() {
        for helper in ["c", "list"] {
            let code = format!(
                "{helper} <- function(...) function() {{}}\nbase::bquote(..({helper}(1)) + .(tail <- 1), splice = TRUE)"
            );
            let names = binding_names(&code);
            // The conservative binding walk still inspects an uncertain tail;
            // helper shadowing must merely prevent a fabricated vector proof.
            assert!(names.contains("tail"), "{code}: {names:?}");
        }
    }

    #[test]
    fn malformed_bquote_unquotes_stop_binding_traversal() {
        let names = binding_names(
            r#"bquote(list(.(head <- 1), ..(), .(tail <- 1)), where = { where_effect <- 1; parent.frame() }, splice = TRUE)"#,
        );
        assert!(names.contains("where_effect"), "{names:?}");
        assert!(!names.contains("head"), "{names:?}");
        assert!(!names.contains("tail"), "{names:?}");

        let names = binding_names(r#"bquote(list(.(before <- 1), .(), .(after <- 1)))"#);
        assert!(names.contains("before"), "{names:?}");
        assert!(!names.contains("after"), "{names:?}");
    }

    #[test]
    fn nested_bquote_abort_propagates_to_enclosing_unquote_list() {
        for code in [
            r#"bquote(list(list(.(head <- 1), ..(), .(inner_tail <- 1)), .(outer_tail <- 1)), splice = TRUE)"#,
            r#"bquote(list(.(head <- 1), list(.(), .(inner_tail <- 1)), .(outer_tail <- 1)))"#,
        ] {
            let names = binding_names(code);
            if code.contains("..()") {
                assert!(!names.contains("head"), "{code}: {names:?}");
            } else {
                assert!(names.contains("head"), "{code}: {names:?}");
            }
            assert!(!names.contains("inner_tail"), "{code}: {names:?}");
            assert!(!names.contains("outer_tail"), "{code}: {names:?}");
        }
    }

    #[test]
    fn record_site_offers_only_the_first_same_name_binding_as_a_candidate() {
        let code = "x <- 1\nx <- 2\n";
        let tree = parse_r(code);
        let mut offered = 0;
        let bindings = collect_bindings(tree.root_node(), code, |_| {
            offered += 1;
            Some(())
        });
        assert_eq!(offered, 1);
        assert_eq!(bindings.get("x").unwrap().count, 2);
    }

    #[test]
    fn malformed_singular_rlang_capture_has_no_operand_effects() {
        for helper in ["expr", "quo", "enquo", "enexpr"] {
            let code = format!("rlang::{helper}(!!(captured <- 1), unused = 2)");
            assert!(
                !binding_names(&code).contains("captured"),
                "{helper} must reject the call before traversing its operand"
            );
        }
    }

    #[test]
    fn variadic_rlang_controls_are_validated_before_unquotes() {
        let malformed = r#"rlang::exprs(!!(captured <- 1), .named = FALSE, .named = TRUE)"#;
        assert!(!binding_names(malformed).contains("captured"));

        for (helper, control) in [
            ("exprs", ".named = FALSE"),
            ("quos", ".ignore_empty = \"trailing\""),
            ("enquos", ".check_assign = FALSE"),
            ("enexprs", ".ignore_null = \"none\""),
        ] {
            let code = format!("rlang::{helper}({control}, !!(captured <- 1))");
            assert!(binding_names(&code).contains("captured"), "{helper}");
        }
    }
}
