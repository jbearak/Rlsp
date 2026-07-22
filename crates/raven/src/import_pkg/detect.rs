//! Static detector for phase-one `{import}` calls.

use std::collections::BTreeSet;

use tower_lsp::lsp_types::{Position, Range};
use tree_sitter::{Node, Tree};

use super::{ImportCall, ImportSpec};
use crate::selective_import::{AttachBinding, ImportDestination};
use crate::utf16::byte_offset_to_utf16_column;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallKind {
    From,
    Here,
    Into,
}

/// Detect fully-static double-colon `{import}` calls. Triple-colon calls and a
/// call containing an unsupported dynamic control/source/selection expression
/// are inert as a whole, so their tokens retain ordinary R-reference meaning.
pub fn detect_import_calls(tree: &Tree, content: &str) -> Vec<ImportCall> {
    let mut out = Vec::new();
    visit(tree.root_node(), content, false, &mut out);
    out.sort_by_key(|call| (call.line, call.column));
    out
}

fn visit(node: Node, content: &str, in_function: bool, out: &mut Vec<ImportCall>) {
    if let Some(kind) = import_call_kind(node, content)
        && let Some(call) = parse_call(node, content, kind, in_function)
    {
        out.push(call);
    }
    let child_in_function = in_function || node.kind() == "function_definition";
    for child in node.children(&mut node.walk()) {
        visit(child, content, child_in_function, out);
    }
}

fn import_call_kind(node: Node, content: &str) -> Option<CallKind> {
    if node.kind() != "call" {
        return None;
    }
    let function = node.child_by_field_name("function")?;
    if function.kind() != "namespace_operator" {
        return None;
    }
    // Phase one deliberately excludes import::: forms.
    let operator = function.child_by_field_name("operator")?;
    if &content[operator.byte_range()] != "::" {
        return None;
    }
    let lhs = function.child_by_field_name("lhs")?;
    let rhs = function.child_by_field_name("rhs")?;
    if &content[lhs.byte_range()] != "import" {
        return None;
    }
    match &content[rhs.byte_range()] {
        "from" => Some(CallKind::From),
        "here" => Some(CallKind::Here),
        "into" => Some(CallKind::Into),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct Arg<'a> {
    name: Option<Node<'a>>,
    value: Node<'a>,
}

fn parse_call(node: Node, content: &str, kind: CallKind, in_function: bool) -> Option<ImportCall> {
    let arguments = node.child_by_field_name("arguments")?;
    let args: Vec<_> = arguments
        .children(&mut arguments.walk())
        .filter(|child| child.kind() == "argument")
        .filter_map(|node| {
            Some(Arg {
                name: node.child_by_field_name("name"),
                value: node.child_by_field_name("value")?,
            })
        })
        .collect();
    let formal_matches = match_formals(&args, content, kind)?;

    let character_only = match matched_arg(&args, &formal_matches, ".character_only") {
        Some(arg) => parse_bool(arg.value, content)?,
        None => false,
    };
    // Phase one always uses Raven's configured PackageLibrary tiers; honoring a
    // per-call `.library` would require a separate package-resolution universe.
    if matched_arg(&args, &formal_matches, ".library").is_some() {
        return None;
    }
    if matched_arg(&args, &formal_matches, ".S3")
        .is_some_and(|arg| parse_bool(arg.value, content) != Some(false))
        || matched_arg(&args, &formal_matches, ".chdir")
            .is_some_and(|arg| parse_bool(arg.value, content) != Some(true))
    {
        return None;
    }

    let source_index = formal_matches
        .iter()
        .position(|formal| *formal == Some(".from"))?;
    let into_index = formal_matches
        .iter()
        .position(|formal| *formal == Some(".into"));

    let source_name = if character_only {
        static_string(args[source_index].value, content)?
    } else {
        static_name(args[source_index].value, content)?
    };
    if source_name.contains("://") {
        return None;
    }
    let directory = match matched_arg(&args, &formal_matches, ".directory") {
        Some(arg) => Some(static_string(arg.value, content)?),
        None => None,
    };
    let spec = if is_script_path(&source_name) {
        ImportSpec::LocalScript {
            path: source_name,
            directory,
        }
    } else {
        // `.directory` is meaningful only for a literal script source.
        if directory.is_some() {
            return None;
        }
        ImportSpec::Package(canonical_package_name(&source_name)?)
    };

    let destination = match kind {
        CallKind::Here => ImportDestination::CurrentEnvironment,
        CallKind::From => match matched_arg(&args, &formal_matches, ".into") {
            Some(arg) => parse_destination(arg.value, content)?,
            None => ImportDestination::NamedSearchPath("imports".to_string()),
        },
        CallKind::Into => parse_destination(args[into_index?].value, content)?,
    };

    let except = match matched_arg(&args, &formal_matches, ".except") {
        Some(arg) => static_name_vector(arg.value, content)?,
        None => BTreeSet::new(),
    };
    let explicit_all = match matched_arg(&args, &formal_matches, ".all") {
        Some(arg) => Some(parse_bool(arg.value, content)?),
        None => None,
    };
    let all = explicit_all.unwrap_or(!except.is_empty());

    let mut attach = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        if formal_matches[index].is_some() {
            continue;
        }
        let name = arg.name.and_then(|name| static_name(name, content));
        let exported = if character_only {
            static_string(arg.value, content)?
        } else {
            static_name(arg.value, content)?
        };
        match name {
            Some(local) => attach.push(AttachBinding::Renamed { local, exported }),
            None => attach.push(AttachBinding::Named(exported)),
        }
    }
    if all {
        attach.push(AttachBinding::Wildcard);
    }

    let start = node.start_position();
    let line_text = content.lines().nth(start.row).unwrap_or("");
    let line = start.row as u32;
    let column = byte_offset_to_utf16_column(line_text, start.column);
    let end = node.end_position();
    let end_column = if end.row == start.row {
        byte_offset_to_utf16_column(line_text, end.column)
    } else {
        byte_offset_to_utf16_column(line_text, line_text.len())
    };
    let source_start = args[source_index].value.start_position();
    let source_end = args[source_index].value.end_position();
    let source_line_text = content.lines().nth(source_start.row).unwrap_or("");
    let source_line = source_start.row as u32;
    let source_column = byte_offset_to_utf16_column(source_line_text, source_start.column);
    let source_end_column = if source_end.row == source_start.row {
        byte_offset_to_utf16_column(source_line_text, source_end.column)
    } else {
        byte_offset_to_utf16_column(source_line_text, source_line_text.len())
    };
    Some(ImportCall {
        spec,
        local_resolution: None,
        attach,
        destination,
        excluded_exports: except,
        line,
        column,
        end_column,
        source_line,
        source_column,
        source_end_column,
        function_scoped: in_function,
    })
}

fn pre_dot_formals(kind: CallKind) -> &'static [&'static str] {
    match kind {
        CallKind::From | CallKind::Here => &[".from"],
        CallKind::Into => &[".into"],
    }
}

fn post_dot_formals(kind: CallKind) -> &'static [&'static str] {
    const COMMON: &[&str] = &[
        ".library",
        ".directory",
        ".all",
        ".except",
        ".chdir",
        ".character_only",
        ".S3",
    ];
    const FROM: &[&str] = &[
        ".into",
        ".library",
        ".directory",
        ".all",
        ".except",
        ".chdir",
        ".character_only",
        ".S3",
    ];
    const INTO: &[&str] = &[
        ".from",
        ".library",
        ".directory",
        ".all",
        ".except",
        ".chdir",
        ".character_only",
        ".S3",
    ];
    match kind {
        CallKind::From => FROM,
        CallKind::Here => COMMON,
        CallKind::Into => INTO,
    }
}

/// Reproduce the portions of R argument matching that affect `{import}`:
/// exact named matching for every formal, partial named matching only before
/// `...`, then positional matching only before `...`. Unmatched named and
/// positional arguments belong to the selected-member list.
fn match_formals<'a>(
    args: &[Arg<'a>],
    content: &str,
    kind: CallKind,
) -> Option<Vec<Option<&'static str>>> {
    let pre = pre_dot_formals(kind);
    let post = post_dot_formals(kind);
    let mut matches = vec![None; args.len()];
    let mut claimed = BTreeSet::new();

    for (index, arg) in args.iter().enumerate() {
        let Some(name) = arg.name.and_then(|node| static_name(node, content)) else {
            continue;
        };
        let exact = pre
            .iter()
            .chain(post.iter())
            .copied()
            .find(|formal| *formal == name);
        if let Some(formal) = exact {
            if !claimed.insert(formal) {
                return None;
            }
            matches[index] = Some(formal);
        }
    }

    for (index, arg) in args.iter().enumerate() {
        if matches[index].is_some() {
            continue;
        }
        let Some(name) = arg.name.and_then(|node| static_name(node, content)) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let candidates = pre
            .iter()
            .copied()
            .filter(|formal| !claimed.contains(formal) && formal.starts_with(&name))
            .collect::<Vec<_>>();
        if let [formal] = candidates.as_slice() {
            claimed.insert(*formal);
            matches[index] = Some(*formal);
        }
    }

    for (index, arg) in args.iter().enumerate() {
        if arg.name.is_some() {
            continue;
        }
        if let Some(formal) = pre.iter().copied().find(|formal| !claimed.contains(formal)) {
            claimed.insert(formal);
            matches[index] = Some(formal);
        }
    }

    Some(matches)
}

fn matched_arg<'a>(
    args: &'a [Arg<'a>],
    formal_matches: &[Option<&str>],
    wanted: &str,
) -> Option<Arg<'a>> {
    args.iter()
        .copied()
        .zip(formal_matches)
        .find_map(|(arg, formal)| (*formal == Some(wanted)).then_some(arg))
}

fn parse_destination(node: Node, content: &str) -> Option<ImportDestination> {
    let destination = static_string(node, content)?;
    Some(if destination.is_empty() {
        ImportDestination::CurrentEnvironment
    } else {
        ImportDestination::NamedSearchPath(destination)
    })
}

fn parse_bool(node: Node, content: &str) -> Option<bool> {
    match &content[node.byte_range()] {
        "TRUE" => Some(true),
        "FALSE" => Some(false),
        _ => None,
    }
}

fn static_string(node: Node, content: &str) -> Option<String> {
    (node.kind() == "string").then(|| unquote(&content[node.byte_range()]).to_string())
}

fn static_name(node: Node, content: &str) -> Option<String> {
    matches!(node.kind(), "identifier" | "string")
        .then(|| unquote(&content[node.byte_range()]).to_string())
}

fn unquote(raw: &str) -> &str {
    let bytes = raw.as_bytes();
    if bytes.len() >= 2
        && matches!(bytes[0], b'\'' | b'"' | b'`')
        && bytes[0] == bytes[bytes.len() - 1]
    {
        &raw[1..raw.len() - 1]
    } else {
        raw
    }
}

fn static_name_vector(node: Node, content: &str) -> Option<BTreeSet<String>> {
    if let Some(name) = static_string(node, content) {
        return Some([name].into_iter().collect());
    }
    if node.kind() != "call"
        || node
            .child_by_field_name("function")
            .and_then(|function| static_name(function, content))
            .as_deref()
            != Some("c")
    {
        return None;
    }
    let arguments = node.child_by_field_name("arguments")?;
    arguments
        .children(&mut arguments.walk())
        .filter(|child| child.kind() == "argument")
        .map(|arg| static_string(arg.child_by_field_name("value")?, content))
        .collect()
}

fn is_script_path(name: &str) -> bool {
    name.ends_with(".R") || name.ends_with(".r")
}

/// Canonicalize the documented static package-spec surface without attempting
/// to execute its version check. `{import}` ignores whitespace and accepts an
/// optional parenthesized version with `<`, `>`, `<=`, `>=`, `==`, `!=`, or no
/// operator (meaning equality). Raven needs only the package component for its
/// `PackageLibrary`; malformed or computed specs remain inert.
fn canonical_package_name(spec: &str) -> Option<String> {
    let compact: String = spec
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect();
    let package = if let Some(open) = compact.find('(') {
        if open == 0
            || !compact.ends_with(')')
            || compact[..open].contains(')')
            || compact[open + 1..compact.len() - 1]
                .chars()
                .any(|character| matches!(character, '(' | ')'))
        {
            return None;
        }
        let requirement = &compact[open + 1..compact.len() - 1];
        let version = [">=", "<=", "==", "!=", ">", "<"]
            .iter()
            .find_map(|operator| requirement.strip_prefix(operator))
            .unwrap_or(requirement);
        if version.is_empty()
            || !version.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '.' | '-')
            })
        {
            return None;
        }
        &compact[..open]
    } else {
        if compact.contains(')') {
            return None;
        }
        compact.as_str()
    };

    crate::r_subprocess::is_valid_package_name(package).then(|| package.to_string())
}

/// True only for identifier tokens belonging to a call that the detector
/// consumed completely. Dynamic/inert calls deliberately return false.
pub(crate) fn is_static_declaration_token(token: Node, content: &str) -> bool {
    let mut current = token;
    while let Some(parent) = current.parent() {
        if let Some(kind) = import_call_kind(parent, content) {
            return parse_call(parent, content, kind, false).is_some();
        }
        current = parent;
    }
    false
}

#[derive(Debug)]
pub(crate) struct AttachmentTokenRange {
    pub local: String,
    pub exported: String,
    pub local_range: Option<Range>,
    pub range: Range,
}

/// Exact source-side token ranges for explicit selected/renamed members in one
/// consumed static call. Wildcards and control arguments have no member token.
pub(crate) fn attachment_token_ranges(
    tree: &Tree,
    content: &str,
    import: &ImportCall,
) -> Vec<AttachmentTokenRange> {
    fn find<'a>(node: Node<'a>, content: &str, import: &ImportCall) -> Option<Node<'a>> {
        if let Some(kind) = import_call_kind(node, content) {
            let start = node.start_position();
            let line_text = content.lines().nth(start.row).unwrap_or("");
            if start.row as u32 == import.line
                && byte_offset_to_utf16_column(line_text, start.column) == import.column
                && parse_call(node, content, kind, false).is_some()
            {
                return Some(node);
            }
        }
        node.children(&mut node.walk())
            .find_map(|child| find(child, content, import))
    }

    let Some(call) = find(tree.root_node(), content, import) else {
        return Vec::new();
    };
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let Some(kind) = import_call_kind(call, content) else {
        return Vec::new();
    };
    let args: Vec<_> = arguments
        .children(&mut arguments.walk())
        .filter(|argument| argument.kind() == "argument")
        .filter_map(|argument| {
            Some(Arg {
                name: argument.child_by_field_name("name"),
                value: argument.child_by_field_name("value")?,
            })
        })
        .collect();
    let Some(formal_matches) = match_formals(&args, content, kind) else {
        return Vec::new();
    };
    let node_range = |node: Node| {
        let start = node.start_position();
        let end = node.end_position();
        let start_line = content.lines().nth(start.row).unwrap_or("");
        let end_line = content.lines().nth(end.row).unwrap_or("");
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
    };

    let mut ranges = Vec::new();
    for (argument, formal) in args.iter().zip(formal_matches) {
        if formal.is_some() {
            continue;
        }
        let value = argument.value;
        let value_range = node_range(value);
        if value_range.start.line == import.source_line
            && value_range.start.character == import.source_column
        {
            continue;
        }
        let Some(exported) = static_name(value, content) else {
            continue;
        };
        let named_local = argument.name.and_then(|name| static_name(name, content));
        let local = import.attach.iter().find_map(|attach| match attach {
            AttachBinding::Named(name)
                if named_local.is_none() && name.as_str() == exported.as_str() =>
            {
                Some(name.clone())
            }
            AttachBinding::Renamed {
                local,
                exported: attached_exported,
            } if named_local.as_deref() == Some(local.as_str())
                && attached_exported.as_str() == exported.as_str() =>
            {
                Some(local.clone())
            }
            AttachBinding::Wildcard => None,
            _ => None,
        });
        let Some(local) = local else {
            continue;
        };
        ranges.push(AttachmentTokenRange {
            local,
            exported,
            local_range: argument.name.map(node_range),
            range: value_range,
        });
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn detect(code: &str) -> Vec<ImportCall> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(code, None).unwrap();
        detect_import_calls(&tree, code)
    }

    #[test]
    fn detects_forms_alias_all_except_and_destinations() {
        let calls = detect(
            "import::from(dplyr, keep = filter, .all = TRUE, .except = c('lag'))\n\
             import::here('mod.R', local = exported)\n\
             import::into('tools', select, .from = dplyr)\n",
        );
        assert_eq!(calls.len(), 3);
        assert!(
            matches!(calls[0].destination, ImportDestination::NamedSearchPath(ref n) if n == "imports")
        );
        assert!(calls[0].attach.contains(&AttachBinding::Wildcard));
        assert!(calls[0].excluded_exports.contains("lag"));
        assert!(matches!(calls[1].spec, ImportSpec::LocalScript { .. }));
        assert!(matches!(
            calls[1].destination,
            ImportDestination::CurrentEnvironment
        ));
        assert!(
            matches!(calls[2].destination, ImportDestination::NamedSearchPath(ref n) if n == "tools")
        );
    }

    #[test]
    fn except_implies_all_but_false_overrides_and_into_requires_named_from() {
        assert!(
            detect("import::from(x, .except = 'a')")[0]
                .attach
                .contains(&AttachBinding::Wildcard)
        );
        assert!(
            !detect("import::from(x, b, .except = 'a', .all = FALSE)")[0]
                .attach
                .contains(&AttachBinding::Wildcard)
        );
        assert!(detect("import::into('tools', x, dplyr)").is_empty());
        assert!(matches!(
            detect("import::into(.into = 'tools', x, .from = dplyr)")[0].destination,
            ImportDestination::NamedSearchPath(ref name) if name == "tools"
        ));
        assert!(matches!(
            detect("import::from(dplyr, x, .into = '')")[0].destination,
            ImportDestination::CurrentEnvironment
        ));
    }

    #[test]
    fn dynamic_and_triple_colon_forms_are_inert() {
        assert!(detect("import:::from(dplyr, filter)").is_empty());
        assert!(detect("import::from(pkg(), filter)").is_empty());
        assert!(detect("import::from(dplyr, get(name))").is_empty());
        assert!(detect("import::from(dplyr, filter, .except = exclusions)").is_empty());
        assert!(detect("import::from(dplyr, filter, .library = custom)").is_empty());
        assert!(detect("import::from(dplyr, filter, .S3 = TRUE)").is_empty());
        assert!(detect("import::from('mod.R', x, .chdir = FALSE)").is_empty());
        assert!(detect("import::from(dplyr, x, .all = T)").is_empty());
        assert!(detect("import::from('https://example.test/mod.R', x)").is_empty());
        assert!(detect("import::from(dplyr, x, .all = TRUE, .all = FALSE)").is_empty());
    }

    #[test]
    fn argument_matching_is_call_kind_aware() {
        let partial_source = &detect("import::from(.f = dplyr, filter)")[0];
        assert!(matches!(partial_source.spec, ImportSpec::Package(ref name) if name == "dplyr"));
        assert_eq!(
            partial_source.attach,
            vec![AttachBinding::Named("filter".to_string())]
        );

        let partial_destination = &detect("import::into(.i = 'tools', x, .from = dplyr)")[0];
        assert!(matches!(
            partial_destination.destination,
            ImportDestination::NamedSearchPath(ref name) if name == "tools"
        ));

        let here_alias = &detect("import::here(dplyr, .into = exported)")[0];
        assert_eq!(
            here_alias.attach,
            vec![AttachBinding::Renamed {
                local: ".into".to_string(),
                exported: "exported".to_string(),
            }]
        );
    }

    #[test]
    fn literal_character_only_calls_are_static_but_symbols_are_not() {
        let call =
            &detect("import::from(\"dplyr\", \"filter\", .character_only = TRUE, .into = '')")[0];
        assert!(matches!(call.spec, ImportSpec::Package(ref name) if name == "dplyr"));
        assert_eq!(
            call.attach,
            vec![AttachBinding::Named("filter".to_string())]
        );
        assert!(detect("import::from(pkg_name, \"filter\", .character_only = TRUE)").is_empty());
        assert!(detect("import::from(\"dplyr\", member, .character_only = TRUE)").is_empty());
    }

    #[test]
    fn version_qualified_package_specs_use_the_canonical_package_name() {
        let call = &detect("import::from('parallel (>= 3.2.0)', makeCluster)")[0];
        assert!(matches!(call.spec, ImportSpec::Package(ref name) if name == "parallel"));
        let equality = &detect("import::from('parallel(3.2.0)', makeCluster)")[0];
        assert!(matches!(equality.spec, ImportSpec::Package(ref name) if name == "parallel"));
        assert!(detect("import::from('parallel (=> 3.2.0)', makeCluster)").is_empty());
        assert!(detect("import::from('parallel (>=)', makeCluster)").is_empty());
    }

    #[test]
    fn dotted_aliases_are_members_not_control_arguments() {
        let call = &detect("import::here('mod.R', .local = .exported)")[0];
        assert_eq!(
            call.attach,
            vec![AttachBinding::Renamed {
                local: ".local".to_string(),
                exported: ".exported".to_string(),
            }]
        );
    }
}
