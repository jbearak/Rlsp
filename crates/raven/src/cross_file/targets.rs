//! Static `{targets}` / `{tarchetypes}` declaration and report-link detection.
//!
//! This module models only source-level facts: direct qualified calls, or bare
//! calls whose package is attached and whose callee is not locally shadowed.
//! It never executes R code. Dynamic names, aliases, computed paths, malformed
//! calls, and `tar_map()` values outside the bounded literal table grammar fail
//! closed and contribute no metadata.

use std::collections::{BTreeSet, HashSet};

use tree_sitter::Node;

use super::binding::RuntimeFunctionScope;
use super::source_detect::LibraryCall;
use super::static_path::LazyStaticBindings;
use super::types::{
    TarchetypesDocumentLink, TargetDeclaration, TargetReference, byte_offset_to_utf16_column,
};

#[derive(Debug, Default)]
pub(crate) struct TargetsMetadata {
    pub(crate) declarations: Vec<TargetDeclaration>,
    pub(crate) references: Vec<TargetReference>,
    pub(crate) document_links: Vec<TarchetypesDocumentLink>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CalleeKind {
    Bare,
    Qualified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DetectionMode {
    Full,
    ReferencesOnly,
}

/// Detect target declarations/references and literal report links with the same
/// attachment and local-shadowing discipline as the existing `tar_source()`
/// detector. The caller supplies ordered top-level attachment events from the
/// shared package walk and the same lazy binding table used by sibling detectors.
pub(crate) fn detect_targets_metadata<'tree, 'text>(
    root: Node<'tree>,
    content: &'text str,
    bindings: &mut LazyStaticBindings<'tree, 'text>,
    attaching_calls: &[LibraryCall],
) -> TargetsMetadata {
    let mut output = TargetsMetadata::default();
    visit_node(
        root,
        content,
        bindings,
        attaching_calls,
        RuntimeFunctionScope::Lexical,
        DetectionMode::Full,
        &mut output,
    );
    output.declarations.sort_by(|left, right| {
        (left.line, left.column, &left.name).cmp(&(right.line, right.column, &right.name))
    });
    output.declarations.dedup_by(|left, right| left == right);
    output.references.sort_by(|left, right| {
        (left.line, left.column, &left.name).cmp(&(right.line, right.column, &right.name))
    });
    output.references.dedup();
    output.document_links.sort_by(|left, right| {
        (left.line, left.column, &left.path).cmp(&(right.line, right.column, &right.path))
    });
    output.document_links.dedup();
    output
}

fn visit_node<'tree, 'text>(
    node: Node<'tree>,
    content: &'text str,
    bindings: &mut LazyStaticBindings<'tree, 'text>,
    attaching_calls: &[LibraryCall],
    runtime_scope: RuntimeFunctionScope,
    mode: DetectionMode,
    output: &mut TargetsMetadata,
) {
    if node.kind() == "identifier" {
        return;
    }

    if node.kind() == "call" {
        if let Some(capture) = bindings.capturing_call_kind_at(node) {
            let captured_scope = runtime_scope.for_evaluated_capture_part(node);
            super::binding::visit_evaluated_capture_parts(
                node,
                content,
                capture,
                &mut |evaluated, _frame, _role| {
                    visit_node(
                        evaluated,
                        content,
                        bindings,
                        attaching_calls,
                        captured_scope,
                        mode,
                        output,
                    );
                },
            );
            return;
        }

        if !runtime_scope.is_function_scoped_at(node) {
            if mode == DetectionMode::Full
                && trusted_call(
                    node,
                    "tarchetypes",
                    "tar_plan",
                    content,
                    bindings,
                    attaching_calls,
                )
            {
                collect_tar_plan(node, content, output);
                visit_tar_plan_arguments(
                    node,
                    content,
                    bindings,
                    attaching_calls,
                    runtime_scope,
                    output,
                );
                return;
            }

            if mode == DetectionMode::Full
                && trusted_call(
                    node,
                    "tarchetypes",
                    "tar_map",
                    content,
                    bindings,
                    attaching_calls,
                )
            {
                collect_tar_map_generated(node, content, bindings, attaching_calls, output);
                return;
            }

            if collect_reference_metadata(node, content, bindings, attaching_calls, output) {
                return;
            }
            if mode == DetectionMode::Full
                && collect_declaration_metadata(node, content, bindings, attaching_calls, output)
            {
                visit_children(
                    node,
                    content,
                    bindings,
                    attaching_calls,
                    runtime_scope,
                    DetectionMode::ReferencesOnly,
                    output,
                );
                return;
            }
        }
    }

    visit_children(
        node,
        content,
        bindings,
        attaching_calls,
        runtime_scope,
        mode,
        output,
    );
}

fn visit_children<'tree, 'text>(
    node: Node<'tree>,
    content: &'text str,
    bindings: &mut LazyStaticBindings<'tree, 'text>,
    attaching_calls: &[LibraryCall],
    runtime_scope: RuntimeFunctionScope,
    mode: DetectionMode,
    output: &mut TargetsMetadata,
) {
    let child_scope = if node.kind() == "function_definition" {
        runtime_scope.enter_function()
    } else {
        runtime_scope
    };
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_node(
            child,
            content,
            bindings,
            attaching_calls,
            child_scope,
            mode,
            output,
        );
    }
}

fn visit_tar_plan_arguments<'tree, 'text>(
    call: Node<'tree>,
    content: &'text str,
    bindings: &mut LazyStaticBindings<'tree, 'text>,
    attaching_calls: &[LibraryCall],
    runtime_scope: RuntimeFunctionScope,
    output: &mut TargetsMetadata,
) {
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return;
    };
    let mut cursor = arguments.walk();
    for argument in arguments.children(&mut cursor) {
        if argument.kind() != "argument" {
            continue;
        }
        if let Some(value) = argument.child_by_field_name("value") {
            let mode = if argument.child_by_field_name("name").is_some() {
                DetectionMode::ReferencesOnly
            } else {
                DetectionMode::Full
            };
            visit_node(
                value,
                content,
                bindings,
                attaching_calls,
                runtime_scope,
                mode,
                output,
            );
        }
    }
}

fn collect_declaration_metadata<'tree, 'text>(
    call: Node<'tree>,
    content: &'text str,
    bindings: &mut LazyStaticBindings<'tree, 'text>,
    attaching_calls: &[LibraryCall],
    output: &mut TargetsMetadata,
) -> bool {
    if trusted_call(
        call,
        "targets",
        "tar_target",
        content,
        bindings,
        attaching_calls,
    ) {
        if let Some(name) = bound_argument(call, content, &["name", "command", "pattern"], "name")
            .and_then(|node| target_name_declaration(node, content))
        {
            output.declarations.push(name);
        }
        return true;
    }

    for function in TARGET_FACTORY_NAMES {
        if trusted_call(
            call,
            "tarchetypes",
            function,
            content,
            bindings,
            attaching_calls,
        ) {
            let mut has_static_name = false;
            if let Some(name) = bound_argument(call, content, &["name", "..."], "name")
                .and_then(|node| target_name_declaration(node, content))
            {
                has_static_name = true;
                push_factory_declarations(function, name, output);
            }
            if has_static_name && matches!(*function, "tar_render" | "tar_knit" | "tar_quarto") {
                collect_document_link(call, function, content, output);
            }
            return true;
        }
    }

    false
}

/// Collect explicit target reads without allowing nested target constructors to
/// become declarations. Returns whether `call` itself was a recognized read so
/// malformed/dynamic argument expressions are not recursively reinterpreted.
fn collect_reference_metadata<'tree, 'text>(
    call: Node<'tree>,
    content: &'text str,
    bindings: &mut LazyStaticBindings<'tree, 'text>,
    attaching_calls: &[LibraryCall],
    output: &mut TargetsMetadata,
) -> bool {
    if trusted_call(
        call,
        "targets",
        "tar_read",
        content,
        bindings,
        attaching_calls,
    ) {
        if let Some(reference) = bound_argument(
            call,
            content,
            &["name", "branches", "meta", "store"],
            "name",
        )
        .and_then(|node| target_name_reference(node, content))
        {
            output.references.push(reference);
        }
        return true;
    }

    if trusted_call(
        call,
        "targets",
        "tar_load",
        content,
        bindings,
        attaching_calls,
    ) {
        if let Some(names) = bound_argument(
            call,
            content,
            &[
                "names", "branches", "meta", "strict", "silent", "envir", "store",
            ],
            "names",
        ) {
            collect_target_reference_expr(names, content, bindings, &mut output.references);
        }
        return true;
    }

    false
}

const TARGET_FACTORY_NAMES: &[&str] = &[
    "tar_url",
    "tar_file",
    "tar_file_fast",
    "tar_rds",
    "tar_qs",
    "tar_keras",
    "tar_torch",
    "tar_arrow_feather",
    "tar_parquet",
    "tar_fst",
    "tar_fst_dt",
    "tar_fst_tbl",
    "tar_nanoparquet",
    "tar_aws_file",
    "tar_aws_fst",
    "tar_aws_fst_dt",
    "tar_aws_fst_tbl",
    "tar_aws_keras",
    "tar_aws_parquet",
    "tar_aws_qs",
    "tar_aws_rds",
    "tar_aws_torch",
    "tar_file_read",
    "tar_change",
    "tar_force",
    "tar_skip",
    "tar_group_by",
    "tar_group_select",
    "tar_group_count",
    "tar_group_size",
    "tar_combine",
    "tar_render",
    "tar_knit",
    "tar_quarto",
    "tar_map",
];

/// Record the public target set produced by one supported factory. Most
/// factories produce only `name`; these three create one deterministic helper
/// target as part of their documented return value.
fn push_factory_declarations(
    function: &str,
    declaration: TargetDeclaration,
    output: &mut TargetsMetadata,
) {
    let suffix = match function {
        "tar_file_read" => Some("_file"),
        "tar_change" | "tar_force" => Some("_change"),
        _ => None,
    };
    if let Some(suffix) = suffix {
        output.declarations.push(TargetDeclaration {
            name: format!("{}{suffix}", declaration.name),
            line: declaration.line,
            column: declaration.column,
            end_column: declaration.end_column,
        });
    }
    output.declarations.push(declaration);
}

fn collect_tar_plan(call: Node, content: &str, output: &mut TargetsMetadata) {
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return;
    };
    if arguments.has_error() {
        return;
    }
    let mut cursor = arguments.walk();
    for argument in arguments.children(&mut cursor) {
        if argument.kind() != "argument" {
            continue;
        }
        let Some(name_node) = argument.child_by_field_name("name") else {
            continue;
        };
        let Some(name) = super::binding::plain_argument_name(name_node, content) else {
            continue;
        };
        if name.is_empty() || argument.child_by_field_name("value").is_none() {
            continue;
        }
        output
            .declarations
            .push(declaration_at(name.into_owned(), name_node, content));
    }
}

fn collect_document_link(call: Node, function: &str, content: &str, output: &mut TargetsMetadata) {
    let formals: &[&str] = match function {
        "tar_render" | "tar_knit" => &["name", "path"],
        "tar_quarto" => &["name", "path"],
        _ => return,
    };
    let Some(path_node) = bound_argument(call, content, formals, "path") else {
        return;
    };
    let Some(path) = super::binding::extract_plain_string(path_node, content) else {
        return;
    };
    let Some(extension) = path.rsplit_once('.').map(|(_, extension)| extension) else {
        return;
    };
    let extension = extension.to_ascii_lowercase();
    let supported_document = match function {
        "tar_render" | "tar_knit" => matches!(extension.as_str(), "rmd" | "rmarkdown"),
        "tar_quarto" => matches!(extension.as_str(), "qmd" | "rmd" | "rmarkdown"),
        _ => false,
    };
    if !supported_document {
        return;
    }
    let (line, column, end_column) = node_range(path_node, content);
    output.document_links.push(TarchetypesDocumentLink {
        path,
        line,
        column,
        end_column,
    });
}

fn collect_target_reference_expr(
    node: Node,
    content: &str,
    bindings: &mut LazyStaticBindings,
    output: &mut Vec<TargetReference>,
) {
    if let Some(reference) = target_name_reference(node, content) {
        output.push(reference);
        return;
    }
    if node.kind() != "call" {
        return;
    }
    let Some(function) = node.child_by_field_name("function") else {
        return;
    };
    if node_text(function, content) != "c"
        || bindings.get().named_binding_may_shadow_at("c", node, false)
    {
        return;
    }
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return;
    };
    let mut collected = Vec::new();
    let mut cursor = arguments.walk();
    for argument in arguments.children(&mut cursor) {
        if argument.kind() != "argument" {
            continue;
        }
        if argument.child_by_field_name("name").is_some() {
            return;
        }
        let Some(value) = argument.child_by_field_name("value") else {
            return;
        };
        let Some(reference) = target_name_reference(value, content) else {
            return;
        };
        collected.push(reference);
    }
    output.extend(collected);
}

fn collect_tar_map_generated<'tree, 'text>(
    call: Node<'tree>,
    content: &'text str,
    bindings: &mut LazyStaticBindings<'tree, 'text>,
    attaching_calls: &[LibraryCall],
    output: &mut TargetsMetadata,
) {
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return;
    };
    if arguments.has_error() {
        return;
    }
    let Some(values_node) = bound_argument(
        call,
        content,
        &[
            "values",
            "...",
            "names",
            "descriptions",
            "unlist",
            "delimiter",
        ],
        "values",
    ) else {
        return;
    };

    // Analyze target-definition objects transactionally. References inside their
    // commands remain useful even when generated names fail closed, but base
    // declarations and document links escape only when every name component is
    // statically reproducible. Runtime `tar_map()` returns renamed targets, never
    // the unsuffixed objects supplied through `...`.
    let mut nested = TargetsMetadata::default();
    let Some(target_objects) = dots_bound_arguments(
        call,
        content,
        &[
            "values",
            "...",
            "names",
            "descriptions",
            "unlist",
            "delimiter",
        ],
    ) else {
        return;
    };
    for value in target_objects {
        visit_node(
            value,
            content,
            bindings,
            attaching_calls,
            RuntimeFunctionScope::Lexical,
            DetectionMode::Full,
            &mut nested,
        );
    }
    output.references.append(&mut nested.references);

    let Some(table) = parse_literal_values_table(values_node, content, bindings) else {
        return;
    };
    let value_names: HashSet<&str> = table.iter().map(|(name, _)| name.as_str()).collect();
    if nested
        .declarations
        .iter()
        .any(|declaration| value_names.contains(declaration.name.as_str()))
    {
        // Upstream rejects target names that collide with values-table columns.
        // Inventing generated names for that invalid pipeline could satisfy reads
        // that runtime tarchetypes never registers.
        return;
    }
    let selected = match bound_argument(
        call,
        content,
        &[
            "values",
            "...",
            "names",
            "descriptions",
            "unlist",
            "delimiter",
        ],
        "names",
    ) {
        Some(node) => match parse_names_selection(node, content, bindings, &table) {
            Some(selected) if !selected.is_empty() => selected,
            _ => return,
        },
        None => table.iter().map(|(name, _)| name.clone()).collect(),
    };
    let delimiter = bound_argument(
        call,
        content,
        &[
            "values",
            "...",
            "names",
            "descriptions",
            "unlist",
            "delimiter",
        ],
        "delimiter",
    )
    .map(|node| super::binding::extract_plain_string(node, content))
    .unwrap_or_else(|| Some("_".to_string()));
    let Some(delimiter) = delimiter.filter(|delimiter| !delimiter.is_empty()) else {
        return;
    };
    let Some(suffixes) = produce_suffixes(&table, &selected, &delimiter) else {
        return;
    };

    let mut generated = Vec::new();
    for base in nested.declarations {
        for suffix in &suffixes {
            let Some(name) = make_r_name(&format!("{}{}{}", base.name, delimiter, suffix)) else {
                // Name repair is locale-sensitive outside ASCII. Commit neither
                // partial declarations nor report links when any mapped name is
                // not reproducible by the static subset.
                return;
            };
            generated.push(TargetDeclaration {
                name,
                line: base.line,
                column: base.column,
                end_column: base.end_column,
            });
        }
    }
    output.document_links.append(&mut nested.document_links);
    output.declarations.extend(generated);
}

fn parse_literal_values_table(
    node: Node,
    content: &str,
    bindings: &mut LazyStaticBindings,
) -> Option<Vec<(String, Vec<String>)>> {
    if node.kind() != "call" {
        return None;
    }
    let function = node.child_by_field_name("function")?;
    let callee = node_text(function, content);
    let trusted = match callee {
        "base::list" | "base::data.frame" | "tibble::tibble" => true,
        "list" | "data.frame" | "tibble" => !bindings
            .get()
            .named_binding_may_shadow_at(callee, node, false),
        _ => false,
    };
    if !trusted {
        return None;
    }
    let arguments = node.child_by_field_name("arguments")?;
    if arguments.has_error() {
        return None;
    }
    let mut columns = Vec::new();
    let mut seen_names = HashSet::new();
    let mut cursor = arguments.walk();
    for argument in arguments.children(&mut cursor) {
        if argument.kind() != "argument" {
            continue;
        }
        let name_node = argument.child_by_field_name("name")?;
        let name = super::binding::plain_argument_name(name_node, content)?.into_owned();
        if is_literal_table_control(callee, &name) || !seen_names.insert(name.clone()) {
            return None;
        }
        let values =
            parse_literal_vector(argument.child_by_field_name("value")?, content, bindings)?;
        if values.is_empty() {
            return None;
        }
        columns.push((name, values));
    }
    let rows = columns.iter().map(|(_, values)| values.len()).max()?;
    if columns
        .iter()
        .any(|(_, values)| values.len() != 1 && values.len() != rows)
    {
        return None;
    }
    for (_, values) in &mut columns {
        if values.len() == 1 && rows > 1 {
            values.resize(rows, values[0].clone());
        }
    }
    Some(columns)
}

/// Constructor controls can change row count or column-name identity and are
/// not values-table columns. The bounded grammar fails closed when any appear
/// instead of guessing their runtime effect.
fn is_literal_table_control(callee: &str, name: &str) -> bool {
    match callee {
        "data.frame" | "base::data.frame" => matches!(
            name,
            "row.names" | "check.rows" | "check.names" | "fix.empty.names" | "stringsAsFactors"
        ),
        "tibble" | "tibble::tibble" => matches!(name, ".rows" | ".name_repair"),
        _ => false,
    }
}

fn parse_literal_vector(
    node: Node,
    content: &str,
    bindings: &mut LazyStaticBindings,
) -> Option<Vec<String>> {
    if let Some(value) = literal_suffix_value(node, content) {
        return Some(vec![value]);
    }
    if node.kind() != "call" {
        return None;
    }
    let function = node.child_by_field_name("function")?;
    let c_shadowed = bindings.get().named_binding_may_shadow_at("c", node, false);
    if node_text(function, content) != "c" || c_shadowed {
        return None;
    }
    let arguments = node.child_by_field_name("arguments")?;
    let mut values = Vec::new();
    let mut cursor = arguments.walk();
    for argument in arguments.children(&mut cursor) {
        if argument.kind() != "argument" {
            continue;
        }
        if argument.child_by_field_name("name").is_some() {
            return None;
        }
        values.push(literal_suffix_value(
            argument.child_by_field_name("value")?,
            content,
        )?);
    }
    Some(values)
}

fn literal_suffix_value(node: Node, content: &str) -> Option<String> {
    match node.kind() {
        "string" => super::binding::extract_plain_string(node, content),
        "integer" => normalize_integer_suffix(node_text(node, content)),
        "float" => normalize_decimal_suffix(node_text(node, content)),
        "true" => Some("TRUE".to_string()),
        "false" => Some("FALSE".to_string()),
        _ => None,
    }
}

/// Match R's character coercion for non-negative integer literals accepted by
/// the bounded `tar_map()` grammar. Decimal leading zeroes and hexadecimal
/// lexical spelling are normalized to the value that `as.character()` sees.
fn normalize_integer_suffix(text: &str) -> Option<String> {
    let text = text
        .strip_suffix('L')
        .or_else(|| text.strip_suffix('l'))
        .unwrap_or(text);
    let value = if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        i32::from_str_radix(hex, 16).ok()?
    } else {
        text.parse::<i32>().ok()?
    };
    Some(value.to_string())
}

/// Conservative scalar subset of R's `as.character(double)` formatting used by
/// `tar_map()`. Exponents, very small/large magnitudes, and more than 15
/// significant digits can switch to R-specific scientific/rounded spellings and
/// therefore fail closed instead of inventing a target name.
fn normalize_decimal_suffix(text: &str) -> Option<String> {
    if text.contains('e') || text.contains('E') {
        return None;
    }
    let significant_digits = text
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .skip_while(|ch| *ch == '0')
        .count();
    if significant_digits > 15 {
        return None;
    }
    let value: f64 = text.parse().ok()?;
    let magnitude = value.abs();
    if !value.is_finite() || (magnitude != 0.0 && !(0.001..1_000_000.0).contains(&magnitude)) {
        return None;
    }
    Some(value.to_string())
}

fn parse_names_selection(
    node: Node,
    content: &str,
    bindings: &mut LazyStaticBindings,
    table: &[(String, Vec<String>)],
) -> Option<Vec<String>> {
    let available: HashSet<&str> = table.iter().map(|(name, _)| name.as_str()).collect();
    if node.kind() == "identifier" {
        let name = super::binding::plain_identifier_name(node, content)?;
        return available.contains(name).then(|| vec![name.to_string()]);
    }
    if node.kind() != "call" {
        return None;
    }
    let function = node.child_by_field_name("function")?;
    match node_text(function, content) {
        "everything"
            if !bindings
                .get()
                .named_binding_may_shadow_at("everything", node, false) =>
        {
            let args = node.child_by_field_name("arguments")?;
            let mut cursor = args.walk();
            if args
                .children(&mut cursor)
                .any(|child| child.kind() == "argument")
            {
                None
            } else {
                Some(table.iter().map(|(name, _)| name.clone()).collect())
            }
        }
        "tidyselect::everything" => {
            let args = node.child_by_field_name("arguments")?;
            let mut cursor = args.walk();
            if args
                .children(&mut cursor)
                .any(|child| child.kind() == "argument")
            {
                None
            } else {
                Some(table.iter().map(|(name, _)| name.clone()).collect())
            }
        }
        "c" if !bindings.get().named_binding_may_shadow_at("c", node, false) => {
            let args = node.child_by_field_name("arguments")?;
            let mut selected = Vec::new();
            let mut seen = HashSet::new();
            let mut cursor = args.walk();
            for argument in args.children(&mut cursor) {
                if argument.kind() != "argument" {
                    continue;
                }
                if argument.child_by_field_name("name").is_some() {
                    return None;
                }
                let value = argument.child_by_field_name("value")?;
                let name = super::binding::plain_identifier_name(value, content)?;
                if !available.contains(name) {
                    return None;
                }
                if seen.insert(name) {
                    selected.push(name.to_string());
                }
            }
            Some(selected)
        }
        _ => None,
    }
}

fn produce_suffixes(
    table: &[(String, Vec<String>)],
    selected: &[String],
    delimiter: &str,
) -> Option<Vec<String>> {
    let rows = table.first()?.1.len();
    let mut raw = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut pieces = Vec::new();
        for selected_name in selected {
            let (_, values) = table.iter().find(|(name, _)| name == selected_name)?;
            pieces.push(values.get(row)?.as_str());
        }
        // `tar_map_produce_suffix()` removes quote characters after coercing
        // selected columns to character and before calling `make.unique()`.
        raw.push(pieces.join(delimiter).replace(['\'', '"'], ""));
    }
    Some(make_unique(raw, delimiter))
}

/// Static equivalent of `base::make.unique(x, sep = delimiter)`. All original
/// spellings are reserved before duplicate suffixes are allocated, so a
/// generated `a_1` cannot collide with a later literal `a_1`.
fn make_unique(raw: Vec<String>, delimiter: &str) -> Vec<String> {
    let mut reserved: HashSet<String> = raw.iter().cloned().collect();
    let mut seen = HashSet::new();
    let mut next_suffix = std::collections::HashMap::<String, usize>::new();
    raw.into_iter()
        .map(|name| {
            if seen.insert(name.clone()) {
                return name;
            }
            let next = next_suffix.entry(name.clone()).or_insert(1);
            loop {
                let candidate = format!("{name}{delimiter}{next}");
                *next += 1;
                if reserved.insert(candidate.clone()) {
                    break candidate;
                }
            }
        })
        .collect()
}

/// ASCII subset of `base::make.names()`. Non-ASCII inputs fail closed because
/// R's locale-sensitive letter classification cannot be reproduced reliably by
/// the language server without executing R.
fn make_r_name(name: &str) -> Option<String> {
    if !name.is_ascii() {
        return None;
    }
    let mut out = String::with_capacity(name.len() + 1);
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_') {
            out.push(ch);
        } else {
            out.push('.');
        }
    }
    let invalid_start = out
        .chars()
        .next()
        .is_none_or(|first| !first.is_ascii_alphabetic() && first != '.')
        || (out.starts_with('.')
            && out
                .chars()
                .nth(1)
                .is_some_and(|second| second.is_ascii_digit()));
    if invalid_start {
        out.insert(0, 'X');
    }
    if matches!(
        out.as_str(),
        "if" | "else"
            | "repeat"
            | "while"
            | "function"
            | "for"
            | "in"
            | "next"
            | "break"
            | "TRUE"
            | "FALSE"
            | "NULL"
            | "Inf"
            | "NaN"
            | "NA"
            | "NA_integer_"
            | "NA_real_"
            | "NA_complex_"
            | "NA_character_"
    ) {
        out.push('.');
    }
    Some(out)
}

fn trusted_call(
    call: Node,
    package: &str,
    function: &str,
    content: &str,
    bindings: &mut LazyStaticBindings,
    attaching_calls: &[LibraryCall],
) -> bool {
    match callee_kind(call, package, function, content) {
        Some(CalleeKind::Qualified) => true,
        Some(CalleeKind::Bare) => {
            package_attached_at(package, call, content, attaching_calls)
                && !bindings
                    .get()
                    .named_binding_may_shadow_at(function, call, false)
        }
        None => false,
    }
}

/// Replay the shared top-level attachment events only through this call site.
/// This prevents a later `library()` from retroactively owning earlier bare
/// calls while preserving conditional `p_load()` prerequisites.
fn package_attached_at(
    package: &str,
    call: Node,
    content: &str,
    attaching_calls: &[LibraryCall],
) -> bool {
    let start = call.start_position();
    let line_text = content.lines().nth(start.row).unwrap_or("");
    let position = (
        start.row as u32,
        byte_offset_to_utf16_column(line_text, start.column),
    );
    let mut attached = BTreeSet::new();
    for attaching in attaching_calls {
        if (attaching.line, attaching.column) > position {
            break;
        }
        if !attaching.attaches || attaching.package.is_empty() {
            continue;
        }
        if attaching
            .requires_attached
            .as_ref()
            .is_none_or(|required| attached.contains(required.as_str()))
        {
            attached.insert(attaching.package.as_str());
        }
    }
    attached.contains(package)
}

fn callee_kind(call: Node, package: &str, function: &str, content: &str) -> Option<CalleeKind> {
    let callee = call.child_by_field_name("function")?;
    match callee.kind() {
        "identifier" => (crate::namespace_completion::unquote_package(node_text(callee, content))
            == function)
            .then_some(CalleeKind::Bare),
        "namespace_operator" => {
            let lhs = callee.child_by_field_name("lhs")?;
            let rhs = callee.child_by_field_name("rhs")?;
            (crate::namespace_completion::unquote_package(node_text(lhs, content)) == package
                && crate::namespace_completion::unquote_package(node_text(rhs, content))
                    == function)
                .then_some(CalleeKind::Qualified)
        }
        _ => None,
    }
}

/// Return argument values bound to `...` by exact named-then-positional
/// matching.
///
/// Exact names bind any formal, including controls after dots. Unknown named
/// arguments belong to dots, which is how `tar_map(values, spec = target)` names
/// a target-definition object. Unnamed actuals fill only unmatched formals before
/// dots; every remaining unnamed actual belongs to dots. Duplicate exact formals,
/// malformed actuals, and an explicit `... =` fail closed.
fn dots_bound_arguments<'tree>(
    call: Node<'tree>,
    content: &str,
    formals: &[&str],
) -> Option<Vec<Node<'tree>>> {
    let arguments = call.child_by_field_name("arguments")?;
    if arguments.has_error() {
        return None;
    }
    let dots_index = formals.iter().position(|formal| *formal == "...")?;
    let mut named_bound = HashSet::new();
    let mut positional = Vec::new();
    let mut dots = Vec::new();
    let mut cursor = arguments.walk();
    for argument in arguments.children(&mut cursor) {
        if argument.kind() != "argument" {
            continue;
        }
        let value = argument.child_by_field_name("value")?;
        if let Some(name_node) = argument.child_by_field_name("name") {
            let name = super::binding::plain_argument_name(name_node, content)?;
            if let Some(index) = formals.iter().position(|formal| *formal == name) {
                if index == dots_index || !named_bound.insert(index) {
                    return None;
                }
            } else {
                dots.push(value);
            }
        } else {
            positional.push(value);
        }
    }

    let mut next = 0;
    for value in positional {
        while named_bound.contains(&next) {
            next += 1;
        }
        if next < dots_index {
            next += 1;
        } else {
            dots.push(value);
        }
    }
    Some(dots)
}

/// Resolve one formal with exact named matching followed by positional matching
/// before `...`. Unknown named arguments are absorbed by dots; partial matching
/// is deliberately unsupported and therefore fails closed.
fn bound_argument<'tree>(
    call: Node<'tree>,
    content: &str,
    formals: &[&str],
    wanted: &str,
) -> Option<Node<'tree>> {
    let arguments = call.child_by_field_name("arguments")?;
    if arguments.has_error() {
        return None;
    }
    let wanted_index = formals.iter().position(|formal| *formal == wanted)?;
    let dots_index = formals.iter().position(|formal| *formal == "...");
    let mut actuals = Vec::new();
    let mut named_bound = HashSet::new();
    let mut cursor = arguments.walk();
    for argument in arguments.children(&mut cursor) {
        if argument.kind() != "argument" {
            continue;
        }
        let value = argument.child_by_field_name("value")?;
        if let Some(name_node) = argument.child_by_field_name("name") {
            let name = super::binding::plain_argument_name(name_node, content)?;
            if let Some(index) = formals.iter().position(|formal| *formal == name) {
                if !named_bound.insert(index) {
                    return None;
                }
                if index == wanted_index {
                    actuals.push((index, value));
                }
            }
        } else {
            actuals.push((usize::MAX, value));
        }
    }
    if let Some((_, value)) = actuals.iter().find(|(index, _)| *index == wanted_index) {
        return Some(*value);
    }
    let mut next = 0;
    for (index, value) in actuals {
        if index != usize::MAX {
            continue;
        }
        while named_bound.contains(&next) {
            next += 1;
        }
        if dots_index.is_some_and(|dots| next >= dots) || next >= formals.len() {
            return None;
        }
        if next == wanted_index {
            return Some(value);
        }
        next += 1;
    }
    None
}

/// Whether `node` is the statically bound target-name formal of a direct
/// namespace-qualified declaration call. Bare calls are deliberately excluded:
/// deciding whether an attached package or a user binding owns a bare callee
/// requires package-aware resolution outside this syntax-only structural hook.
pub(crate) fn is_qualified_target_declaration_name(node: Node, content: &str) -> bool {
    if node.kind() != "identifier" {
        return false;
    }
    let Some(argument) = node.parent().filter(|parent| parent.kind() == "argument") else {
        return false;
    };
    let Some(arguments) = argument
        .parent()
        .filter(|parent| parent.kind() == "arguments")
    else {
        return false;
    };
    let Some(call) = arguments.parent().filter(|parent| parent.kind() == "call") else {
        return false;
    };
    let is_declaration = callee_kind(call, "targets", "tar_target", content)
        == Some(CalleeKind::Qualified)
        || TARGET_FACTORY_NAMES.iter().any(|function| {
            *function != "tar_map"
                && callee_kind(call, "tarchetypes", function, content)
                    == Some(CalleeKind::Qualified)
        });
    is_declaration
        && bound_argument(call, content, &["name", "..."], "name")
            .is_some_and(|name| name.id() == node.id())
}

fn target_name_declaration(node: Node, content: &str) -> Option<TargetDeclaration> {
    let name = super::binding::plain_identifier_name(node, content)?.to_string();
    Some(declaration_at(name, node, content))
}

fn target_name_reference(node: Node, content: &str) -> Option<TargetReference> {
    let name = super::binding::plain_identifier_name(node, content)?.to_string();
    let (line, column, end_column) = node_range(node, content);
    Some(TargetReference {
        name,
        line,
        column,
        end_column,
    })
}

fn declaration_at(name: String, node: Node, content: &str) -> TargetDeclaration {
    let (line, column, end_column) = node_range(node, content);
    TargetDeclaration {
        name,
        line,
        column,
        end_column,
    }
}

fn node_range(node: Node, content: &str) -> (u32, u32, u32) {
    let start = node.start_position();
    let end = node.end_position();
    let start_line = content.lines().nth(start.row).unwrap_or("");
    let end_line = content.lines().nth(end.row).unwrap_or("");
    (
        start.row as u32,
        byte_offset_to_utf16_column(start_line, start.column),
        byte_offset_to_utf16_column(end_line, end.column),
    )
}

fn node_text<'a>(node: Node, content: &'a str) -> &'a str {
    &content[node.byte_range()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(code: &str) -> TargetsMetadata {
        let tree = crate::parser_pool::with_parser(|parser| parser.parse(code, None)).unwrap();
        let mut bindings = LazyStaticBindings::new(tree.root_node(), code);
        detect_targets_metadata(tree.root_node(), code, &mut bindings, &[])
    }

    #[test]
    fn detects_factories_plan_references_and_documents() {
        let code = r#"
            targets::tar_target(upstream, 1)
            tarchetypes::tar_plan(simple = upstream + 1, tarchetypes::tar_rds(stored, simple))
            tarchetypes::tar_render(report, "report.Rmd", params = list(x = stored))
            targets::tar_read(stored)
        "#;
        let metadata = detect(code);
        let names: HashSet<_> = metadata
            .declarations
            .iter()
            .map(|declaration| declaration.name.as_str())
            .collect();
        assert!(names.is_superset(&HashSet::from(["upstream", "simple", "stored", "report"])));
        assert_eq!(metadata.references[0].name, "stored");
        assert_eq!(metadata.document_links[0].path, "report.Rmd");
    }

    #[test]
    fn bare_shadowing_and_dynamic_paths_fail_closed() {
        let code = r#"
            tar_render <- function(...) NULL
            tar_render(not_a_target, "shadowed.Rmd")
            tarchetypes::tar_render(real, dynamic_path)
            targets::tar_target(real_target, 1)
            targets::tar_read(dynamic_name())
        "#;
        let metadata = detect(code);
        assert_eq!(
            metadata
                .declarations
                .iter()
                .map(|declaration| declaration.name.as_str())
                .collect::<Vec<_>>(),
            vec!["real", "real_target"]
        );
        assert!(metadata.document_links.is_empty());
        assert!(metadata.references.is_empty());
    }

    #[test]
    fn tar_map_expands_literal_static_names() {
        let code = r#"
            tarchetypes::tar_map(
                list(region = c("east", "west"), year = c(2025, 2026)),
                targets::tar_target(model, run(region, year)),
                names = c(region, year)
            )
        "#;
        let metadata = detect(code);
        let names: HashSet<_> = metadata
            .declarations
            .iter()
            .map(|declaration| declaration.name.as_str())
            .collect();
        assert!(!names.contains("model"), "{names:?}");
        assert!(names.contains("model_east_2025"), "{names:?}");
        assert!(names.contains("model_west_2026"), "{names:?}");
    }

    #[test]
    fn tar_map_dynamic_values_fail_closed_without_inventing_base_declaration() {
        let code = r#"
            values <- make_values()
            tarchetypes::tar_map(values, targets::tar_target(model, targets::tar_read(upstream)))
        "#;
        let metadata = detect(code);
        assert!(metadata.declarations.is_empty());
        assert_eq!(
            metadata
                .references
                .iter()
                .map(|reference| reference.name.as_str())
                .collect::<Vec<_>>(),
            vec!["upstream"],
            "static reads inside commands remain analyzable when map names fail closed"
        );
    }

    #[test]
    fn tar_map_accepts_named_dots_target_objects() {
        let metadata = detect(
            r#"
                tarchetypes::tar_map(
                    list(id = c("a", "b")),
                    spec = targets::tar_target(model, id)
                )
            "#,
        );
        assert_eq!(
            metadata
                .declarations
                .iter()
                .map(|declaration| declaration.name.as_str())
                .collect::<Vec<_>>(),
            vec!["model_a", "model_b"]
        );
    }

    #[test]
    fn tar_map_rejects_target_column_collisions() {
        let metadata = detect(
            r#"
                tarchetypes::tar_map(
                    list(model = c("a", "b")),
                    targets::tar_target(model, model)
                )
            "#,
        );
        assert!(metadata.declarations.is_empty());
        assert!(metadata.document_links.is_empty());
    }

    #[test]
    fn tar_map_generated_names_and_links_commit_transactionally() {
        let metadata = detect(
            r#"
                tarchetypes::tar_map(
                    list(id = c("ok", "é")),
                    tarchetypes::tar_render(report, "report.Rmd")
                )
            "#,
        );
        assert!(metadata.declarations.is_empty());
        assert!(metadata.document_links.is_empty());
    }

    #[test]
    fn tar_map_bare_everything_respects_shadowing() {
        let shadowed = detect(
            r#"
                everything <- function() id
                tarchetypes::tar_map(
                    list(id = "a", region = "west"),
                    targets::tar_target(model, id),
                    names = everything()
                )
            "#,
        );
        assert!(shadowed.declarations.is_empty());

        let qualified = detect(
            r#"
                tarchetypes::tar_map(
                    list(id = "a", region = "west"),
                    targets::tar_target(model, id),
                    names = tidyselect::everything()
                )
            "#,
        );
        assert_eq!(qualified.declarations[0].name, "model_a_west");
    }

    #[test]
    fn attached_calls_are_detected_but_aliases_and_shadowing_fail_closed() {
        let code = r#"
            library(targets)
            library(tarchetypes)
            tar_rds(stored, command_value)
            tar_load(c(stored, other))
            factory <- tar_render
            factory(alias_target, "alias.Rmd")
            tar_knit <- function(...) NULL
            tar_knit(shadowed, "shadowed.Rmd")
        "#;
        let metadata = crate::cross_file::extract_metadata(code);
        assert_eq!(
            metadata
                .target_declarations
                .iter()
                .map(|declaration| declaration.name.as_str())
                .collect::<Vec<_>>(),
            vec!["stored"]
        );
        assert_eq!(
            metadata
                .target_references
                .iter()
                .map(|reference| reference.name.as_str())
                .collect::<Vec<_>>(),
            vec!["stored", "other"]
        );
        assert!(metadata.tarchetypes_document_links.is_empty());
    }

    #[test]
    fn bare_calls_require_attachment_before_the_call_site() {
        let code = r#"
            tar_rds(too_early, command_value)
            library(tarchetypes)
            tar_rds(owned, command_value)
        "#;
        let metadata = crate::cross_file::extract_metadata(code);
        assert_eq!(
            metadata
                .target_declarations
                .iter()
                .map(|declaration| declaration.name.as_str())
                .collect::<Vec<_>>(),
            vec!["owned"]
        );
    }

    #[test]
    fn factories_record_documented_auxiliary_targets() {
        let metadata = detect(
            r#"
                tarchetypes::tar_file_read(data, path(), read_csv(.x))
                tarchetypes::tar_change(changed, command(), change())
                tarchetypes::tar_force(forced, command(), force())
            "#,
        );
        let names: HashSet<_> = metadata
            .declarations
            .iter()
            .map(|declaration| declaration.name.as_str())
            .collect();
        assert_eq!(
            names,
            HashSet::from([
                "data",
                "data_file",
                "changed",
                "changed_change",
                "forced",
                "forced_change",
            ])
        );
    }

    #[test]
    fn delayed_commands_collect_reads_without_nested_declarations_or_links() {
        let metadata = detect(
            r#"
                targets::tar_target(
                    wrapper,
                    list(
                        targets::tar_read(upstream),
                        tarchetypes::tar_render(ghost, "ghost.Rmd")
                    )
                )
            "#,
        );
        assert_eq!(
            metadata
                .declarations
                .iter()
                .map(|declaration| declaration.name.as_str())
                .collect::<Vec<_>>(),
            vec!["wrapper"]
        );
        assert_eq!(
            metadata
                .references
                .iter()
                .map(|reference| reference.name.as_str())
                .collect::<Vec<_>>(),
            vec!["upstream"]
        );
        assert!(metadata.document_links.is_empty());
    }

    #[test]
    fn malformed_partial_and_dynamic_arguments_fail_closed() {
        let code = r#"
            tarchetypes::tar_plan(broken = )
            tarchetypes::tar_render(na = partial, pa = "partial.Rmd")
            tarchetypes::tar_knit(dynamic_name(), "dynamic-name.Rmd")
            tarchetypes::tar_quarto(quarto_target, dynamic_path)
            targets::tar_read()
            targets::tar_load(c(valid, dynamic_name()))
        "#;
        let metadata = detect(code);
        assert_eq!(
            metadata
                .declarations
                .iter()
                .map(|declaration| declaration.name.as_str())
                .collect::<Vec<_>>(),
            vec!["quarto_target"],
            "a dynamic path must not erase an independently static target declaration"
        );
        assert!(metadata.references.is_empty());
        assert!(metadata.document_links.is_empty());
    }

    #[test]
    fn report_factories_link_only_supported_literal_documents() {
        let code = r#"
            tarchetypes::tar_knit(knit_report, "report.Rmarkdown")
            tarchetypes::tar_render(render_report, "report.Rmd")
            tarchetypes::tar_render(not_a_document, "data.csv")
            tarchetypes::tar_quarto(quarto_report, "report.qmd")
            tarchetypes::tar_quarto(project_report, "quarto-project")
        "#;
        let metadata = detect(code);
        assert_eq!(
            metadata
                .document_links
                .iter()
                .map(|link| link.path.as_str())
                .collect::<Vec<_>>(),
            vec!["report.Rmarkdown", "report.Rmd", "report.qmd"]
        );
    }

    #[test]
    fn tar_map_matches_make_unique_and_ascii_make_names() {
        let code = r#"
            tarchetypes::tar_map(
                list(id = c("a", "a", "a_1", "a-b", "O'Brien")),
                targets::tar_target(model, run(id)),
                names = id
            )
        "#;
        let metadata = detect(code);
        let names: HashSet<_> = metadata
            .declarations
            .iter()
            .map(|declaration| declaration.name.as_str())
            .collect();
        assert_eq!(
            names,
            HashSet::from([
                "model_a",
                "model_a_2",
                "model_a_1",
                "model_a.b",
                "model_OBrien",
            ])
        );
    }

    #[test]
    fn tar_map_deduplicates_selection_and_rejects_constructor_controls() {
        let duplicate_selection = detect(
            r#"
                tarchetypes::tar_map(
                    list(id = c("a", "b")),
                    targets::tar_target(model, run(id)),
                    names = c(id, id)
                )
            "#,
        );
        assert_eq!(
            duplicate_selection
                .declarations
                .iter()
                .map(|declaration| declaration.name.as_str())
                .collect::<Vec<_>>(),
            vec!["model_a", "model_b"]
        );

        for code in [
            r#"tarchetypes::tar_map(
                data.frame(id = c("a", "b"), check.names = FALSE),
                targets::tar_target(model, run(id))
            )"#,
            r#"tarchetypes::tar_map(
                tibble::tibble(id = c("a", "b"), .name_repair = "minimal"),
                targets::tar_target(model, run(id))
            )"#,
        ] {
            assert!(
                detect(code).declarations.is_empty(),
                "constructor controls must fail closed: {code}"
            );
        }
    }

    #[test]
    fn tar_map_normalizes_decimal_and_hex_integer_spellings() {
        let metadata = detect(
            r#"
                tarchetypes::tar_map(
                    list(id = c(01L, 0x0AL)),
                    targets::tar_target(model, run(id))
                )
            "#,
        );
        assert_eq!(
            metadata
                .declarations
                .iter()
                .map(|declaration| declaration.name.as_str())
                .collect::<Vec<_>>(),
            vec!["model_1", "model_10"]
        );
    }

    #[test]
    fn tar_map_recycles_scalars_and_rejects_ambiguous_static_inputs() {
        let static_code = r#"
            tarchetypes::tar_map(
                list(region = c("east", "west"), year = 2025L),
                targets::tar_target(model, run(region, year))
            )
        "#;
        let static_metadata = detect(static_code);
        let static_names: HashSet<_> = static_metadata
            .declarations
            .iter()
            .map(|declaration| declaration.name.as_str())
            .collect();
        assert!(static_names.contains("model_east_2025"));
        assert!(static_names.contains("model_west_2025"));

        for code in [
            r#"tarchetypes::tar_map(
                list(x = c("a", "b"), y = c(1L, 2L, 3L)),
                targets::tar_target(model, run(x, y))
            )"#,
            r#"tarchetypes::tar_map(
                list(x = c("a", "b")),
                targets::tar_target(model, run(x)),
                names = dynamic_selection()
            )"#,
            r#"tarchetypes::tar_map(
                list(x = c("a", "b")),
                targets::tar_target(model, run(x)),
                delimiter = dynamic_delimiter
            )"#,
            r#"tarchetypes::tar_map(
                list(x = c("a", "b")),
                targets::tar_target(model, run(x)),
                names = c()
            )"#,
        ] {
            let metadata = detect(code);
            assert!(
                metadata.declarations.is_empty(),
                "dynamic or ambiguous tar_map input must fail closed: {code}"
            );
        }
    }
}
