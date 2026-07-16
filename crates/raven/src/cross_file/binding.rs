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
    },
    AssignCall {
        node: Node<'tree>,
        value: Option<Node<'tree>>,
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
}

/// Collect every statically named binding or invalidation in one AST walk.
///
/// `candidate_for` is called only for ordinary binary assignments and valid
/// statically named `assign()` calls. It decides whether the site provides a
/// payload; binding counting is independent of that decision. In particular,
/// callers can share exact binding semantics without widening their distinct
/// payload policies.
pub(crate) fn collect_bindings<'tree, T>(
    root: Node<'tree>,
    content: &str,
    mut candidate_for: impl FnMut(BindingSite<'tree>) -> Option<T>,
) -> HashMap<String, Binding<T>> {
    let mut map = HashMap::new();
    visit_bindings(root, content, &mut map, &mut candidate_for);
    map
}

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
        "for_statement" => record_for_variable(node, content, map),
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
    });
    entry.count = entry.count.saturating_add(1);
    entry
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
            if let Some(lhs) = node.child_by_field_name("lhs")
                && let Some(name) = binding_target_name(lhs, content)
            {
                bump(map, &name);
            }
            return;
        }
        _ => return,
    };
    let Some(target) = node.child_by_field_name(target_field) else {
        return;
    };
    let Some(name) = binding_target_name(target, content) else {
        return;
    };
    let site = BindingSite::Binary {
        node,
        target,
        value: node.child_by_field_name(value_field),
        operator,
        top_level: node
            .parent()
            .is_some_and(|parent| parent.kind() == "program"),
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
    match node_text(function, content) {
        "assign" => {
            let Some((name_node, value)) = resolve_assign_arguments(arguments, content) else {
                return;
            };
            let Some(name) = extract_plain_string(name_node, content) else {
                return;
            };
            record_site(
                map,
                &name,
                BindingSite::AssignCall { node, value },
                candidate_for,
            );
        }
        "rm" | "remove" => record_remove_call(arguments, content, map),
        _ => {}
    }
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
    let entry = bump(map, name);
    if entry.candidate.is_none()
        && let Some(payload) = payload
    {
        entry.candidate = Some((payload, site.start_byte()));
    }
}

/// Resolve the `x` and `value` formals of `assign()` using R's exact,
/// unambiguous-partial, then positional matching order. Calls whose duplicate
/// or colliding named arguments would error are not bindings.
fn resolve_assign_arguments<'tree>(
    arguments: Node<'tree>,
    content: &str,
) -> Option<(Node<'tree>, Option<Node<'tree>>)> {
    #[derive(Clone, Copy)]
    enum Actual<'tree> {
        Value(Node<'tree>),
        Missing,
    }

    if arguments.has_error() {
        return None;
    }
    const FORMALS: [&str; 6] = ["x", "value", "pos", "envir", "inherits", "immediate"];
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
            named.push((node_text(name, content), actual));
        } else {
            positional.push(actual);
        }
    }

    // R matches named actuals in two passes: exact names first, then unique
    // partial names among the still-unmatched formals. `assign()` has no
    // `...`, so an unknown/ambiguous name, duplicate match, or extra actual
    // makes the call error before it can bind `x`.
    let mut matched: [Option<Actual<'tree>>; FORMALS.len()] = [None; FORMALS.len()];
    let mut partials = Vec::new();
    for (name, value) in named {
        if let Some(index) = FORMALS.iter().position(|formal| *formal == name) {
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
        let matches: Vec<_> = FORMALS
            .iter()
            .enumerate()
            .filter(|(index, formal)| matched[*index].is_none() && formal.starts_with(name))
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
        while next_formal < FORMALS.len() && matched[next_formal].is_some() {
            next_formal += 1;
        }
        if next_formal == FORMALS.len() {
            return None;
        }
        matched[next_formal] = Some(value);
        next_formal += 1;
    }

    let Actual::Value(name) = matched[0]? else {
        return None;
    };
    let value = match matched[1] {
        Some(Actual::Value(value)) => Some(value),
        Some(Actual::Missing) | None => None,
    };
    Some((name, value))
}

fn binding_target_name(node: Node, content: &str) -> Option<String> {
    if let Some(root) = replacement_root_identifier(node) {
        return Some(node_text(root, content).to_string());
    }
    extract_plain_string(node, content)
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

fn record_remove_call<T>(arguments: Node, content: &str, map: &mut HashMap<String, Binding<T>>) {
    let mut cursor = arguments.walk();
    for argument in arguments.children(&mut cursor) {
        if argument.kind() != "argument" {
            continue;
        }
        let named = argument
            .child_by_field_name("name")
            .map(|name| node_text(name, content));
        if named.is_some_and(|name| matches!(name, "pos" | "envir" | "inherits")) {
            continue;
        }
        let Some(value) = argument.child_by_field_name("value") else {
            continue;
        };
        if value.kind() == "identifier" {
            bump(map, node_text(value, content));
        } else if let Some(name) = extract_plain_string(value, content) {
            bump(map, &name);
        }
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
            bump(map, node_text(name, content));
        }
    }
}

fn record_for_variable<T>(node: Node, content: &str, map: &mut HashMap<String, Binding<T>>) {
    if let Some(variable) = node.child_by_field_name("variable")
        && variable.kind() == "identifier"
    {
        bump(map, node_text(variable, content));
    }
}

fn extract_plain_string(node: Node, content: &str) -> Option<String> {
    let text = node_text(node, content);
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
