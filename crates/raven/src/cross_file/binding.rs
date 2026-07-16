//! Shared conservative collection of R binding forms.
//!
//! Static path folding and static package-vector detection attach different
//! payloads to a binding, but a name's binding count must be identical for
//! both consumers. This module owns the syntax walk, mutation invalidators,
//! and `assign()` argument matching; callers supply only the policy that turns
//! a supported binding site into their candidate payload.

use std::collections::HashMap;

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
#[derive(Debug, Default)]
pub(crate) struct Binding<T> {
    count: u32,
    candidate: Option<(T, usize)>,
    safe_eager_offset: Option<usize>,
    first_offset: Option<usize>,
    persistent_uncertainty: bool,
}

impl<T> Binding<T> {
    /// Return the candidate iff this is the name's only binding and it occurs
    /// strictly before `before_byte`.
    pub(crate) fn resolved_before(&self, before_byte: usize) -> Option<&T> {
        if self.count != 1 {
            return None;
        }
        let (candidate, offset) = self.candidate.as_ref()?;
        (*offset < before_byte).then_some(candidate)
    }

    fn has_safe_eager_value_before(&self, before_byte: usize) -> bool {
        self.count == 1
            && self
                .safe_eager_offset
                .is_some_and(|offset| offset < before_byte)
    }
}

/// Collect every statically named binding or invalidation in one AST walk.
///
/// `candidate_for` is called for ordinary binary assignments and for eligible
/// bare/base `assign()` calls at the top level with a default destination.
/// Other valid statically named `assign()` calls still count as invalidators.
/// The callback decides whether an offered site provides a payload; binding
/// counting is independent of that decision. In particular, callers can share
/// exact binding semantics without widening their distinct payload policies.
pub(crate) fn collect_bindings<'tree, T>(
    root: Node<'tree>,
    content: &str,
    mut candidate_for: impl FnMut(BindingSite<'tree>) -> Option<T>,
) -> HashMap<String, Binding<T>> {
    let mut map = HashMap::new();
    visit_bindings(root, content, &mut map, &mut candidate_for);
    // An escaped backtick target can denote a name whose canonical spelling
    // we cannot recover without evaluating R escapes. Disable every static
    // candidate rather than letting such a target alias an otherwise-folded
    // plain name. The sentinel cannot be an R identifier because names cannot
    // contain NUL.
    if map.remove(UNKNOWN_BINDING_KEY).is_some() {
        invalidate_existing_bindings(&mut map);
    }
    map
}

const UNKNOWN_BINDING_KEY: &str = "\0raven-unknown-binding";
const UNKNOWN_HELPER_BINDING_KEY: &str = "\0raven-unknown-helper-binding";
const UNKNOWN_NAMED_BINDING_KEY: &str = "\0raven-unknown-named-binding";
const ASSIGN_FORMALS: [&str; 6] = ["x", "value", "pos", "envir", "inherits", "immediate"];

fn visit_bindings<'tree, T>(
    node: Node<'tree>,
    content: &str,
    map: &mut HashMap<String, Binding<T>>,
    candidate_for: &mut impl FnMut(BindingSite<'tree>) -> Option<T>,
) {
    match node.kind() {
        "binary_operator" => record_assignment(node, content, map, candidate_for),
        "call" => record_mutation_call(node, content, map, candidate_for),
        "function_definition" => record_function_params(node, content, map),
        "for_statement" => {
            // The sequence is evaluated eagerly and the body may execute;
            // either can mutate unrelated bindings through indirect calls.
            invalidate_unknown_mutation(map, node);
            record_for_variable(node, content, map);
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_bindings(child, content, map, candidate_for);
    }
}

fn bump<'m, T>(map: &'m mut HashMap<String, Binding<T>>, name: &str) -> &'m mut Binding<T> {
    let entry = map.entry(name.to_string()).or_insert_with(|| Binding {
        count: 0,
        candidate: None,
        safe_eager_offset: None,
        first_offset: None,
        persistent_uncertainty: false,
    });
    entry.count = entry.count.saturating_add(1);
    entry
}

fn bump_at<'m, T>(
    map: &'m mut HashMap<String, Binding<T>>,
    name: &str,
    offset: usize,
) -> &'m mut Binding<T> {
    let binding = bump(map, name);
    binding.first_offset = Some(
        binding
            .first_offset
            .map_or(offset, |existing| existing.min(offset)),
    );
    binding
}

fn record_assignment<'tree, T>(
    node: Node<'tree>,
    content: &str,
    map: &mut HashMap<String, Binding<T>>,
    candidate_for: &mut impl FnMut(BindingSite<'tree>) -> Option<T>,
) {
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
            invalidate_unknown_mutation(map, node);
            if let Some(lhs) = node.child_by_field_name("lhs") {
                match binding_target_name(lhs, content) {
                    Some(BindingTargetName::Known(name)) => {
                        bump_at(map, &name, node.start_byte());
                    }
                    Some(BindingTargetName::Unknown) => invalidate_unknown_mutation(map, node),
                    None => {}
                }
            }
            return;
        }
        _ => return,
    };
    let Some(target) = node.child_by_field_name(target_field) else {
        return;
    };
    let name = match binding_target_name(target, content) {
        Some(BindingTargetName::Known(name)) => name,
        Some(BindingTargetName::Unknown) => {
            mark_unknown_named_binding(map, node);
            invalidate_unknown_mutation(map, node);
            return;
        }
        None => {
            mark_unknown_named_binding(map, node);
            invalidate_unknown_mutation(map, node);
            return;
        }
    };
    if !matches!(
        target.kind(),
        "identifier" | "string" | "raw_string_literal"
    ) {
        // Replacement assignments evaluate index/target expressions and may
        // dispatch to arbitrary replacement functions before binding the
        // root name.
        invalidate_unknown_mutation(map, node);
    }
    let value = node.child_by_field_name(value_field);
    let value_is_side_effect_free = value.is_some_and(|value| {
        binding_value_is_side_effect_free(value, content, map, is_known_immediate_context(node))
    });
    if value.is_some() && !value_is_side_effect_free {
        // Ordinary assignment forces its RHS. An identifier may force a
        // delayed/active binding and an arbitrary expression may mutate names
        // unrelated to the assignment target.
        invalidate_unknown_mutation(map, node);
    }
    let site = BindingSite::Binary {
        node,
        target,
        value,
        operator,
        top_level: node
            .parent()
            .is_some_and(|parent| parent.kind() == "program"),
        value_is_side_effect_free,
        helpers_trusted: !helper_may_be_shadowed(map) && !map.contains_key("c"),
    };
    record_site(map, &name, site, candidate_for);
}

fn record_mutation_call<'tree, T>(
    node: Node<'tree>,
    content: &str,
    map: &mut HashMap<String, Binding<T>>,
    candidate_for: &mut impl FnMut(BindingSite<'tree>) -> Option<T>,
) {
    let Some(function) = node.child_by_field_name("function") else {
        return;
    };
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return;
    };
    if let Some(kind) = assign_call_kind(function, content) {
        if kind == AssignCallKind::UnknownNamespace
            || (kind == AssignCallKind::BareCandidate
                && (!is_known_immediate_context(node) || !bare_helper_is_trusted(map, "assign")))
        {
            mark_unknown_named_binding(map, node);
            invalidate_unknown_mutation(map, node);
            return;
        }
        if arguments.has_error() {
            return;
        }
        if has_duplicate_exact_names(arguments, content, &ASSIGN_FORMALS) {
            return;
        }
        if arguments_have_uninterpreted_names(arguments, content) {
            mark_unknown_named_binding(map, node);
            invalidate_unknown_mutation(map, node);
            return;
        }
        let actuals_are_side_effect_free = argument_values_are_side_effect_free(
            arguments,
            content,
            map,
            is_known_immediate_context(node),
        );
        if !actuals_are_side_effect_free {
            // assign() may force supplied actuals before discovering a later
            // missing required formal. Preserve those possible side effects
            // even when full formal matching cannot produce a binding.
            invalidate_unknown_mutation(map, node);
        }
        let Some(resolved) = resolve_assign_arguments(arguments, content) else {
            return;
        };
        let Some(name) = extract_plain_string(resolved.name, content) else {
            // A dynamic or escaped `x` may decode/evaluate to any name. It
            // invalidates prior bindings at top level; in a deferred context
            // it becomes persistent because runtime order is unknown.
            mark_unknown_named_binding(map, node);
            invalidate_unknown_mutation(map, node);
            return;
        };
        if matches!(
            kind,
            AssignCallKind::BareCandidate | AssignCallKind::BaseCandidate
        ) && resolved.default_destination
            && node
                .parent()
                .is_some_and(|parent| parent.kind() == "program")
        {
            let helpers_trusted = !helper_may_be_shadowed(map) && !map.contains_key("c");
            record_site(
                map,
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
            bump_at(map, &name, node.start_byte());
        }
    } else if callee_leaf_is(function, content, "rm") || callee_leaf_is(function, content, "remove")
    {
        if function.kind() == "namespace_operator" && !namespace_is_base(function, content) {
            invalidate_unknown_mutation(map, node);
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
            if !is_known_immediate_context(node) || !bare_helper_is_trusted(map, helper) {
                invalidate_unknown_mutation(map, node);
                return;
            }
        }
        record_remove_call(node, arguments, content, map);
    } else if callee_leaf_is(function, content, "delayedAssign")
        || callee_leaf_is(function, content, "makeActiveBinding")
    {
        // Both APIs can replace an ordinary binding with one whose later
        // lookup executes arbitrary code. Treat the call as a barrier even
        // when its target appears statically named.
        mark_unknown_named_binding(map, node);
        invalidate_persistent_unknown_mutation(map, node);
    } else if callee_leaf_has_uninterpreted_escape(function, content) {
        // Without evaluating R escapes, an escaped callee leaf could be
        // `assign`, `rm`, `remove`, or an active/delayed binding constructor.
        // Use the strongest persistent barrier because future reads/writes
        // may themselves execute arbitrary code.
        mark_unknown_named_binding(map, node);
        invalidate_persistent_unknown_mutation(map, node);
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

fn record_site<'tree, T>(
    map: &mut HashMap<String, Binding<T>>,
    name: &str,
    site: BindingSite<'tree>,
    candidate_for: &mut impl FnMut(BindingSite<'tree>) -> Option<T>,
) {
    let payload = map
        .get(name)
        .is_none_or(|binding| binding.candidate.is_none())
        .then(|| candidate_for(site))
        .flatten();
    let entry = bump_at(map, name, site.start_byte());
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

/// Resolve the `x` and `value` formals of `assign()` using R's exact,
/// unambiguous-partial, then positional matching order. Calls whose duplicate
/// or colliding named arguments would error are not bindings.
struct ResolvedAssignArguments<'tree> {
    name: Node<'tree>,
    value: Option<Node<'tree>>,
    default_destination: bool,
}

fn resolve_assign_arguments<'tree>(
    arguments: Node<'tree>,
    content: &str,
) -> Option<ResolvedAssignArguments<'tree>> {
    #[derive(Clone, Copy)]
    enum Actual<'tree> {
        Value(Node<'tree>),
        Missing,
    }

    if arguments.has_error() {
        return None;
    }
    let mut named = Vec::new();
    let mut positional = Vec::new();

    // tree-sitter represents empty positional actuals as adjacent `comma`
    // children with no intervening `argument` node. Reconstruct argument
    // slots first so those gaps are retained.
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

    for slot in slots {
        let Some(argument) = slot else {
            positional.push(Actual::Missing);
            continue;
        };
        // Empty positional actuals still reserve a formal slot in R. Dropping
        // them would shift later actuals left and could hide an extra-argument
        // error (for example a seventh actual after four empty slots).
        let actual = argument
            .child_by_field_name("value")
            .map(Actual::Value)
            .unwrap_or(Actual::Missing);
        if let Some(name) = argument.child_by_field_name("name") {
            let name = plain_argument_name(name, content)?;
            named.push((name, actual));
        } else {
            positional.push(actual);
        }
    }

    // R matches named actuals in two passes: exact names first, then unique
    // partial names among the still-unmatched formals. `assign()` has no
    // `...`, so an unknown/ambiguous name, duplicate match, or extra actual
    // makes the call error before it can bind `x`.
    let mut matched: [Option<Actual<'tree>>; ASSIGN_FORMALS.len()] = [None; ASSIGN_FORMALS.len()];
    let mut partials = Vec::new();
    for (name, value) in named {
        if let Some(index) = ASSIGN_FORMALS.iter().position(|formal| *formal == name) {
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
        let matches: Vec<_> = ASSIGN_FORMALS
            .iter()
            .enumerate()
            .filter(|(index, formal)| matched[*index].is_none() && formal.starts_with(&name))
            .map(|(index, _)| index)
            .collect();
        let [index] = matches.as_slice() else {
            return None;
        };
        matched[*index] = Some(value);
    }

    // Positional actuals fill the remaining formals from left to right.
    let mut next_formal = 0;
    for value in positional {
        while next_formal < ASSIGN_FORMALS.len() && matched[next_formal].is_some() {
            next_formal += 1;
        }
        if next_formal == ASSIGN_FORMALS.len() {
            return None;
        }
        matched[next_formal] = Some(value);
        next_formal += 1;
    }

    let Actual::Value(name) = matched[0]? else {
        return None;
    };
    let value = match matched[1]? {
        Actual::Value(value) => Some(value),
        Actual::Missing => return None,
    };
    // At top level, omitted/missing `pos`, `envir`, and `inherits` use the
    // current global frame. Any explicit destination expression is dynamic
    // for our purposes and can only invalidate an existing candidate.
    let default_destination = matched[2..=4]
        .iter()
        .all(|actual| !matches!(actual, Some(Actual::Value(_))));
    Some(ResolvedAssignArguments {
        name,
        value,
        default_destination,
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

fn identifier_binding_key<'a>(node: Node, content: &'a str) -> &'a str {
    plain_identifier_name(node, content).unwrap_or(UNKNOWN_BINDING_KEY)
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
pub(crate) fn plain_argument_name(node: Node, content: &str) -> Option<String> {
    plain_identifier_name(node, content)
        .map(str::to_string)
        .or_else(|| extract_plain_string(node, content))
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
    map: &mut HashMap<String, Binding<T>>,
) {
    if arguments.has_error() {
        return;
    }
    if remove_has_duplicate_option_names(arguments, content) {
        return;
    }
    if arguments_have_uninterpreted_names(arguments, content) {
        // The escaped tag may decode to an evaluated formal such as `list`
        // or `envir`; its value can therefore perform arbitrary side effects.
        invalidate_unknown_mutation(map, call);
        return;
    }
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
                invalidate_unknown_mutation(map, call);
            }
            continue;
        }
        let Some(value) = argument.child_by_field_name("value") else {
            continue;
        };
        if named.as_deref() == Some("list") {
            let allow_bare_c = is_known_immediate_context(call);
            match classify_remove_list_value(value, content, map, allow_bare_c) {
                RemoveListValue::Static(names) => {
                    for name in names {
                        bump_at(map, &name, call.start_byte());
                    }
                }
                RemoveListValue::Dynamic => {
                    // A dynamic names expression is evaluated by rm(), so it
                    // can both remove any prior binding and perform arbitrary
                    // side effects such as shadowing a helper.
                    invalidate_unknown_mutation(map, call);
                }
                RemoveListValue::Invalid => {
                    // The call cannot execute, so it removes nothing.
                }
            }
            continue;
        }
        if value.kind() == "identifier" {
            if let Some(name) = plain_identifier_name(value, content) {
                bump_at(map, name, call.start_byte());
            } else {
                invalidate_unknown_removal(map, call);
            }
        } else if let Some(name) = extract_plain_string(value, content) {
            bump_at(map, &name, call.start_byte());
        } else if value.kind() == "string" {
            invalidate_unknown_removal(map, call);
        }
    }
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
    map: &HashMap<String, Binding<T>>,
    allow_bare_helpers: bool,
) -> bool {
    let mut cursor = arguments.walk();
    arguments.children(&mut cursor).all(|argument| {
        argument.kind() != "argument"
            || argument.child_by_field_name("value").is_none_or(|value| {
                binding_value_is_side_effect_free(value, content, map, allow_bare_helpers)
            })
    })
}

fn binding_value_is_side_effect_free<T>(
    value: Node,
    content: &str,
    map: &HashMap<String, Binding<T>>,
    allow_bare_helpers: bool,
) -> bool {
    if remove_option_is_side_effect_free(value) || value.kind() == "function_definition" {
        return true;
    }
    if value.kind() == "identifier" {
        return plain_identifier_name(value, content)
            .and_then(|name| map.get(name))
            .is_some_and(|binding| binding.has_safe_eager_value_before(value.start_byte()));
    }
    if value.kind() != "call" {
        return false;
    }
    let Some(function) = value.child_by_field_name("function") else {
        return false;
    };
    let helper = ["c", "file.path", "normalizePath"]
        .into_iter()
        .find(|helper| callee_leaf_is(function, content, helper));
    let Some(helper) = helper else {
        return false;
    };
    let trusted = if function.kind() == "namespace_operator" {
        namespace_is_base(function, content)
    } else {
        allow_bare_helpers && bare_helper_is_trusted(map, helper)
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
    let mut cursor = arguments.walk();
    arguments.children(&mut cursor).all(|argument| {
        argument.kind() != "argument"
            || argument.child_by_field_name("value").is_none_or(|value| {
                binding_value_is_side_effect_free(value, content, map, allow_bare_helpers)
            })
    })
}

fn remove_has_duplicate_option_names(arguments: Node, content: &str) -> bool {
    const OPTIONS: [&str; 4] = ["list", "pos", "envir", "inherits"];
    let mut seen = [false; OPTIONS.len()];
    let mut cursor = arguments.walk();
    for argument in arguments.children(&mut cursor) {
        let Some(name) = (argument.kind() == "argument")
            .then(|| argument.child_by_field_name("name"))
            .flatten()
            .and_then(|name| plain_argument_name(name, content))
        else {
            continue;
        };
        let Some(index) = OPTIONS.iter().position(|option| *option == name) else {
            continue;
        };
        if seen[index] {
            return true;
        }
        seen[index] = true;
    }
    false
}

fn invalidate_existing_bindings<T>(map: &mut HashMap<String, Binding<T>>) {
    for binding in map.values_mut() {
        binding.count = binding.count.saturating_add(1);
    }
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

fn bare_helper_is_trusted<T>(map: &HashMap<String, Binding<T>>, name: &str) -> bool {
    !helper_may_be_shadowed(map) && !map.contains_key(name)
}

fn invalidate_unknown_mutation<T>(map: &mut HashMap<String, Binding<T>>, node: Node) {
    if !is_known_immediate_context(node) {
        // A mutation nested in a function/quoted/deferred context may execute
        // after syntactically later assignments. Retain a persistent barrier
        // so AST visitation order cannot make those later candidates appear
        // safe.
        invalidate_persistent_unknown_mutation(map, node);
    } else {
        invalidate_existing_bindings(map);
        // Even though syntactically later ordinary assignments overwrite an
        // unknown top-level target and remain viable candidates, the target
        // may have shadowed helper functions such as bare `c`. Retain this
        // separate marker so later helper-dependent analysis stays dynamic.
        bump_at(map, UNKNOWN_HELPER_BINDING_KEY, node.start_byte());
    }
}

fn invalidate_persistent_unknown_mutation<T>(map: &mut HashMap<String, Binding<T>>, node: Node) {
    bump_at(map, UNKNOWN_BINDING_KEY, node.start_byte());
    // The unknown target can also be a helper name. Keep that uncertainty
    // after the all-candidates sentinel is consumed during finalization.
    let helper = bump_at(map, UNKNOWN_HELPER_BINDING_KEY, node.start_byte());
    helper.persistent_uncertainty = true;
}

fn mark_unknown_named_binding<T>(map: &mut HashMap<String, Binding<T>>, node: Node) {
    let binding = bump_at(map, UNKNOWN_NAMED_BINDING_KEY, node.start_byte());
    binding.persistent_uncertainty = true;
}

fn invalidate_unknown_removal<T>(map: &mut HashMap<String, Binding<T>>, node: Node) {
    if is_known_immediate_context(node) {
        invalidate_existing_bindings(map);
    } else {
        bump_at(map, UNKNOWN_BINDING_KEY, node.start_byte());
    }
}

pub(crate) fn is_known_immediate_context(node: Node) -> bool {
    // Only a direct top-level expression is guaranteed to execute in source
    // order. Calls and user-defined infix operators receive lazy promises;
    // formulas and other enclosing syntax may not evaluate their operands at
    // all. Treat every nested mutation as persistent rather than attempting
    // an inevitably incomplete eager-context allowlist.
    node.parent()
        .is_some_and(|parent| parent.kind() == "program")
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
    if let Some(name) = extract_plain_string(node, content) {
        return RemoveListValue::Static(vec![name]);
    }
    if node.kind() != "call" {
        return RemoveListValue::Dynamic;
    }
    let Some(function) = node.child_by_field_name("function") else {
        return RemoveListValue::Invalid;
    };
    let bare_c = match function.kind() {
        "identifier" | "string" | "raw_string_literal" => {
            if !node_is_plain_name(function, content, "c")
                || map.contains_key("c")
                || map.contains_key(UNKNOWN_HELPER_BINDING_KEY)
            {
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

fn record_function_params<T>(node: Node, content: &str, map: &mut HashMap<String, Binding<T>>) {
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
                    bump_at(
                        map,
                        identifier_binding_key(name, content),
                        name.start_byte(),
                    );
                }
                // Variadic parameter spellings cannot alias an ordinary name
                // and contain no backtick escapes to interpret.
                "dots" | "dot_dot_i" => {
                    bump_at(map, node_text(name, content), name.start_byte());
                }
                _ => {}
            }
        }
    }
}

fn record_for_variable<T>(node: Node, content: &str, map: &mut HashMap<String, Binding<T>>) {
    if let Some(variable) = node.child_by_field_name("variable")
        && variable.kind() == "identifier"
    {
        if let Some(name) = plain_identifier_name(variable, content) {
            bump_at(map, name, variable.start_byte());
        } else {
            invalidate_unknown_mutation(map, node);
        }
    }
}

fn extract_plain_string(node: Node, content: &str) -> Option<String> {
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
