//
// box_use/detect.rs
//
// Recognise static `box::use(...)` calls and parse their specs + attach lists.
//

//! `box::use()` call detection.
//!
//! [`detect_box_imports`] walks a tree-sitter tree for `call` nodes whose
//! function is the `namespace_operator` `box::use` (or `box:::use`) and turns
//! each argument into a [`BoxImport`]. Spec/attach parsing is done on the raw
//! text of each argument value (see [`parse_spec`]) rather than on the argument
//! sub-tree, because R operator precedence makes `./mod[a, b]` parse as
//! `. / (mod[a, b])` — the `[` binds tighter than `/`. Parsing the deparsed
//! text sidesteps that entirely and matches box's own conceptual model of a
//! spec as `[alias =] path [attach-list]`.

use tower_lsp::lsp_types::{Position, Range};
use tree_sitter::{Node, Tree};

use super::{BoxAttach, BoxImport, BoxSpec};
use crate::utf16::byte_offset_to_utf16_column;

/// Detect every static `box::use(...)` call in `tree` and return one
/// [`BoxImport`] per argument, in document order.
///
/// Both top-level and function-scoped calls are detected. A call lexically
/// inside a `function_definition` body is marked
/// [`function_scoped`](BoxImport::function_scoped) so scope injection can keep
/// it local-only; arguments with no static value are skipped.
pub fn detect_box_imports(tree: &Tree, content: &str) -> Vec<BoxImport> {
    let mut out = Vec::new();
    visit(tree.root_node(), content, false, &mut out);
    out.sort_by_key(|imp| (imp.line, imp.column));
    out
}

/// Recursively visit `node`. `in_function` is `true` once we have descended
/// through any `function_definition`, so a `box::use()` call anywhere inside a
/// function body — however deeply nested in blocks — is marked function-scoped.
fn visit(node: Node, content: &str, in_function: bool, out: &mut Vec<BoxImport>) {
    if is_box_use_call(node, content) {
        out.extend(parse_use_call_node(node, content, in_function));
    }
    let child_in_function = in_function || node.kind() == "function_definition";
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, content, child_in_function, out);
    }
}

/// True when `node` is a `call` whose function is `box::use` / `box:::use`.
pub(crate) fn is_box_use_call(node: Node, content: &str) -> bool {
    node.kind() == "call"
        && node
            .child_by_field_name("function")
            .is_some_and(|f| is_box_use_function(f, content))
}

/// Parse the arguments of one `box::use(...)` call node into imports.
///
/// Returns an empty vec when `node` is not a well-formed box call. `in_function`
/// marks every produced import function-scoped. Exposed for the export parser's
/// re-export handling (which passes `false`, since re-exports are module-level).
pub(crate) fn parse_use_call_node(node: Node, content: &str, in_function: bool) -> Vec<BoxImport> {
    let mut out = Vec::new();
    if let Some(args) = node.child_by_field_name("arguments") {
        collect_arguments(args, content, in_function, &mut out);
    }
    out
}

/// True for a call-function node spelling `box::use` (or `box:::use`).
///
/// Kept behaviourally identical to the `is_box_use_function` used by the
/// `infix_spaces` lint exemption so the two recognisers cannot drift.
fn is_box_use_function(node: Node, content: &str) -> bool {
    if node.kind() != "namespace_operator" {
        return false;
    }
    let side = |field: &str| {
        node.child_by_field_name(field)
            .map(|n| &content[n.byte_range()])
    };
    side("lhs") == Some("box") && side("rhs") == Some("use")
}

fn collect_arguments(args: Node, content: &str, in_function: bool, out: &mut Vec<BoxImport>) {
    let mut cursor = args.walk();
    for child in args.children(&mut cursor) {
        if child.kind() != "argument" {
            continue;
        }
        let Some(value) = child.child_by_field_name("value") else {
            continue;
        };
        let explicit_alias = child
            .child_by_field_name("name")
            .and_then(|name| bare_name(name, content));
        let (spec, attach) = parse_spec(&content[value.byte_range()]);
        let (line, column) = node_start_utf16(value, content);
        // Range for diagnostics/navigation: covers the whole argument node
        // (spec + attach list) so `pkg[a, b]` highlights fully. Multi-line
        // specs fall back to the start of the value's last line (conservative);
        // single-line specs — the overwhelming case — get an exact end column.
        let end_column = node_end_utf16_column(child, content, line);
        out.push(BoxImport {
            spec,
            local_resolution: None,
            explicit_alias,
            attach,
            line,
            column,
            end_column,
            function_scoped: in_function,
        });
    }
}

/// The bare identifier text of an alias `name` node, unquoting a string /
/// backtick literal. `None` for an empty or non-name node.
fn bare_name(node: Node, content: &str) -> Option<String> {
    let raw = &content[node.byte_range()];
    let unquoted = strip_delims(raw);
    if unquoted.is_empty() {
        None
    } else {
        Some(unquoted.to_string())
    }
}

/// Strip a single pair of matching surrounding quotes or backticks.
fn strip_delims(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' || first == b'\'' || first == b'`') && first == last {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Start position of `node` as (0-based line, 0-based UTF-16 column).
fn node_start_utf16(node: Node, content: &str) -> (u32, u32) {
    let start = node.start_position();
    let line_text = content.lines().nth(start.row).unwrap_or("");
    (
        start.row as u32,
        byte_offset_to_utf16_column(line_text, start.column),
    )
}

/// One-past-the-end UTF-16 column of `node`, for a highlight range anchored on
/// `start_line`. When `node` ends on `start_line` this is exact; when it spans
/// onto a later line we clamp to the end of `start_line` so the range stays
/// single-line and well-formed (the provenance model is single-line).
fn node_end_utf16_column(node: Node, content: &str, start_line: u32) -> u32 {
    let end = node.end_position();
    let start_line_text = content.lines().nth(start_line as usize).unwrap_or("");
    if end.row as u32 == start_line {
        byte_offset_to_utf16_column(start_line_text, end.column)
    } else {
        // Multi-line: clamp to the end of the start line.
        byte_offset_to_utf16_column(start_line_text, start_line_text.len())
    }
}

/// One source-side attachment token and its exact document range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttachmentTokenRange {
    pub exported: String,
    pub range: Range,
}

/// Semantic identity of one exact named/renamed attachment token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttachmentTokenIdentity {
    pub local: String,
    pub exported: String,
}

#[derive(Debug, Clone)]
struct ParsedAttach {
    binding: BoxAttach,
    local_span: Option<(usize, usize)>,
    exported_span: Option<(usize, usize)>,
}

/// Parse a single spec's deparsed text into a [`BoxSpec`] and its attach list.
///
/// The spec grammar (after the argument `name =` alias, which is handled by the
/// caller) is `module-path [ attach-list ]`. Delimiters inside matching quotes or
/// backticks are ignored, so statically quoted non-syntactic names remain usable.
pub fn parse_spec(spec_text: &str) -> (BoxSpec, Vec<BoxAttach>) {
    let (spec, attach) = parse_spec_detailed(spec_text);
    (
        spec,
        attach.into_iter().map(|entry| entry.binding).collect(),
    )
}

/// Return exact source-side attachment token ranges for one canonical import.
/// Used by diagnostics so repeated/multiline members highlight their own token
/// without storing syntax coordinates in the reusable selective-import model.
pub(crate) fn attachment_token_ranges(
    tree: &Tree,
    content: &str,
    import: &BoxImport,
) -> Vec<AttachmentTokenRange> {
    let Some(value) = find_import_value(tree.root_node(), content, import) else {
        return Vec::new();
    };
    let raw = &content[value.byte_range()];
    let (_, attach) = parse_spec_detailed(raw);
    attach
        .into_iter()
        .filter_map(|entry| {
            let (start, end) = entry.exported_span?;
            Some(AttachmentTokenRange {
                exported: match entry.binding {
                    BoxAttach::Named(name) => name,
                    BoxAttach::Renamed { exported, .. } => exported,
                    BoxAttach::Wildcard => return None,
                },
                range: Range::new(
                    byte_offset_to_lsp_position(content, value.start_byte() + start),
                    byte_offset_to_lsp_position(content, value.start_byte() + end),
                ),
            })
        })
        .collect()
}

/// Exact semantic identity for an attachment token, only when `token` is one
/// of the parser-approved named/renamed spans. Nested dynamic expressions are
/// deliberately excluded even when they repeat a static member's spelling.
pub(crate) fn attachment_token_identity(
    value: Node,
    token: Node,
    content: &str,
) -> Option<AttachmentTokenIdentity> {
    if token.start_byte() < value.start_byte() || token.end_byte() > value.end_byte() {
        return None;
    }
    let raw = &content[value.byte_range()];
    let token_span = (
        token.start_byte() - value.start_byte(),
        token.end_byte() - value.start_byte(),
    );
    let (_, attach) = parse_spec_detailed(raw);
    attach.into_iter().find_map(|entry| match entry.binding {
        BoxAttach::Named(name) if entry.exported_span == Some(token_span) => {
            Some(AttachmentTokenIdentity {
                local: name.clone(),
                exported: name,
            })
        }
        BoxAttach::Renamed { local, exported }
            if entry.local_span == Some(token_span) || entry.exported_span == Some(token_span) =>
        {
            Some(AttachmentTokenIdentity { local, exported })
        }
        _ => None,
    })
}

/// Whether `token` is an exact static declaration token in one `box::use()`
/// value. Supported module-path identifiers are structural by containment in the
/// parsed module span; attachment identifiers must match an exact parser span.
pub(crate) fn is_static_declaration_token(value: Node, token: Node, content: &str) -> bool {
    if token.start_byte() < value.start_byte() || token.end_byte() > value.end_byte() {
        return false;
    }
    let raw = &content[value.byte_range()];
    let (spec, _) = parse_spec_detailed(raw);
    if spec.is_supported()
        && let Some((start, end)) = module_byte_span(raw)
    {
        let token_start = token.start_byte() - value.start_byte();
        let token_end = token.end_byte() - value.start_byte();
        if token_start >= start && token_end <= end {
            return true;
        }
    }
    attachment_token_identity(value, token, content).is_some()
}

/// Exact range of the module/package portion of one import argument, excluding
/// its attachment list and surrounding whitespace. Navigation uses this instead
/// of the whole argument so attachment tokens and punctuation never open the
/// module file.
pub(crate) fn module_spec_range(tree: &Tree, content: &str, import: &BoxImport) -> Option<Range> {
    let value = find_import_value(tree.root_node(), content, import)?;
    let raw = &content[value.byte_range()];
    let (relative_start, relative_end) = module_byte_span(raw)?;
    let start = value.start_byte() + relative_start;
    let end = value.start_byte() + relative_end;
    Some(Range::new(
        byte_offset_to_lsp_position(content, start),
        byte_offset_to_lsp_position(content, end),
    ))
}

fn module_byte_span(raw: &str) -> Option<(usize, usize)> {
    let leading = raw.len().saturating_sub(raw.trim_start().len());
    let trimmed = raw.trim();
    let module = find_unquoted(trimmed, '[')
        .map(|open| &trimmed[..open])
        .unwrap_or(trimmed)
        .trim_end();
    (!module.is_empty()).then_some((leading, leading + module.len()))
}

fn find_import_value<'a>(node: Node<'a>, content: &str, import: &BoxImport) -> Option<Node<'a>> {
    if is_box_use_call(node, content)
        && let Some(arguments) = node.child_by_field_name("arguments")
    {
        for argument in arguments.children(&mut arguments.walk()) {
            if argument.kind() != "argument" {
                continue;
            }
            let Some(value) = argument.child_by_field_name("value") else {
                continue;
            };
            let (line, column) = node_start_utf16(value, content);
            if line == import.line && column == import.column {
                return Some(value);
            }
        }
    }
    for child in node.children(&mut node.walk()) {
        if let Some(value) = find_import_value(child, content, import) {
            return Some(value);
        }
    }
    None
}

fn parse_spec_detailed(spec_text: &str) -> (BoxSpec, Vec<ParsedAttach>) {
    let leading = spec_text.len().saturating_sub(spec_text.trim_start().len());
    let trimmed = spec_text.trim();
    match find_unquoted(trimmed, '[') {
        Some(open) => {
            let close = rfind_unquoted(trimmed, ']').filter(|close| *close > open);
            let inner_end = close.unwrap_or(trimmed.len());
            let inner = &trimmed[open + 1..inner_end];
            (
                classify_module(&trimmed[..open]),
                parse_attach_list(inner, leading + open + 1),
            )
        }
        None => (classify_module(trimmed), Vec::new()),
    }
}

/// Classify a module-path string (no attach list) into a [`BoxSpec`].
fn classify_module(module_str: &str) -> BoxSpec {
    let raw_parts = split_unquoted_ranges(module_str, '/');
    let parts: Vec<String> = raw_parts
        .iter()
        .map(|(start, end)| module_str[*start..*end].trim())
        .map(str::to_string)
        .collect();
    if parts.is_empty() || parts.iter().any(String::is_empty) {
        return BoxSpec::Unsupported(module_str.trim().to_string());
    }

    // Local module: begins with `.` (current dir) or one-or-more `..` (parents).
    if parts[0] == "." || parts[0] == ".." {
        let mut up_levels = 0usize;
        let mut idx = 0usize;
        if parts[0] == "." {
            idx = 1;
        } else {
            while idx < parts.len() && parts[idx] == ".." {
                up_levels += 1;
                idx += 1;
            }
        }
        let components: Option<Vec<String>> = parts[idx..]
            .iter()
            .map(|component| {
                (component != "." && component != "..")
                    .then(|| static_name(component))
                    .flatten()
            })
            .collect();
        let Some(components) = components.filter(|components| !components.is_empty()) else {
            return BoxSpec::Unsupported(module_str.trim().to_string());
        };
        return BoxSpec::LocalModule {
            up_levels,
            components,
        };
    }

    // Bare single static name → installed package.
    if parts.len() == 1 {
        return static_name(&parts[0])
            .map(BoxSpec::Package)
            .unwrap_or_else(|| BoxSpec::Unsupported(module_str.trim().to_string()));
    }

    // Multi-component path not beginning with `./` or `../` → non-local module
    // search path (`foo/bar`). Unsupported, fails conservatively.
    BoxSpec::Unsupported(module_str.trim().to_string())
}

/// Parse the comma-separated inner text of an attach list. `base_offset` is the
/// inner text's byte offset inside the original spec and is retained for ranges.
fn parse_attach_list(inner: &str, base_offset: usize) -> Vec<ParsedAttach> {
    let mut out = Vec::new();
    for (raw_start, raw_end) in split_unquoted_ranges(inner, ',') {
        let raw = &inner[raw_start..raw_end];
        let leading = raw.len().saturating_sub(raw.trim_start().len());
        let entry = raw.trim();
        if entry.is_empty() {
            continue;
        }
        let entry_offset = base_offset + raw_start + leading;
        if entry == "..." {
            out.push(ParsedAttach {
                binding: BoxAttach::Wildcard,
                local_span: None,
                exported_span: None,
            });
            continue;
        }
        if let Some(eq) = find_unquoted(entry, '=') {
            let local_raw = &entry[..eq];
            let exported_raw = &entry[eq + 1..];
            let Some(local) = static_name(local_raw) else {
                continue;
            };
            let Some(exported) = static_name(exported_raw) else {
                continue;
            };
            let local_leading = local_raw.len() - local_raw.trim_start().len();
            let local_trimmed = local_raw.trim();
            let local_start = entry_offset + local_leading;
            let exported_leading = exported_raw.len() - exported_raw.trim_start().len();
            let exported_trimmed = exported_raw.trim();
            let exported_start = entry_offset + eq + 1 + exported_leading;
            out.push(ParsedAttach {
                binding: BoxAttach::Renamed { local, exported },
                local_span: Some((local_start, local_start + local_trimmed.len())),
                exported_span: Some((exported_start, exported_start + exported_trimmed.len())),
            });
            continue;
        }
        if let Some(name) = static_name(entry) {
            let span = Some((entry_offset, entry_offset + entry.len()));
            out.push(ParsedAttach {
                binding: BoxAttach::Named(name),
                local_span: span,
                exported_span: span,
            });
        }
    }
    out
}

/// Parse a static R name. Matching quotes/backticks permit non-syntactic names;
/// unquoted names accept Unicode alphanumerics plus `.` / `_`, but not a leading
/// digit. Dynamic expressions and malformed quoting fail closed.
fn static_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if matches!(first, b'`' | b'\'' | b'"') && first == last {
            let inner = &trimmed[1..trimmed.len() - 1];
            return (!inner.is_empty() && !inner.contains('\n') && !inner.contains('\r'))
                .then(|| inner.to_string());
        }
    }
    if trimmed.is_empty() {
        return None;
    }
    let mut chars = trimmed.chars();
    let first = chars.next()?;
    if first.is_numeric() {
        return None;
    }
    trimmed
        .chars()
        .all(|c| c.is_alphanumeric() || c == '.' || c == '_')
        .then(|| trimmed.to_string())
}

fn find_unquoted(text: &str, needle: char) -> Option<usize> {
    split_quoted_scan(text, |index, ch| (ch == needle).then_some(index))
}

fn rfind_unquoted(text: &str, needle: char) -> Option<usize> {
    let mut found = None;
    split_quoted_scan(text, |index, ch| {
        if ch == needle {
            found = Some(index);
        }
        None::<usize>
    });
    found
}

fn split_unquoted_ranges(text: &str, delimiter: char) -> Vec<(usize, usize)> {
    let mut starts = vec![0usize];
    split_quoted_scan(text, |index, ch| {
        if ch == delimiter {
            starts.push(index + ch.len_utf8());
        }
        None::<usize>
    });
    let mut ranges = Vec::with_capacity(starts.len());
    for (index, start) in starts.iter().copied().enumerate() {
        let end = starts
            .get(index + 1)
            .copied()
            .map(|next| next - delimiter.len_utf8())
            .unwrap_or(text.len());
        ranges.push((start, end));
    }
    ranges
}

/// Scan characters outside matching single/double/backtick quotes. Backslash
/// escapes suppress quote termination inside quoted names.
fn split_quoted_scan<T>(text: &str, mut f: impl FnMut(usize, char) -> Option<T>) -> Option<T> {
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if let Some(active) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active {
                quote = None;
            }
            continue;
        }
        if matches!(ch, '`' | '\'' | '"') {
            quote = Some(ch);
            continue;
        }
        if let Some(result) = f(index, ch) {
            return Some(result);
        }
    }
    None
}

fn byte_offset_to_lsp_position(content: &str, offset: usize) -> Position {
    let offset = offset.min(content.len());
    let line_start = content[..offset].rfind('\n').map_or(0, |index| index + 1);
    let line = content[..line_start]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count() as u32;
    Position::new(
        line,
        byte_offset_to_utf16_column(&content[line_start..offset], offset - line_start),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse(code: &str) -> Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        parser.parse(code, None).unwrap()
    }

    fn imports(code: &str) -> Vec<BoxImport> {
        detect_box_imports(&parse(code), code)
    }

    #[test]
    fn parse_spec_package_and_alias() {
        let (spec, attach) = parse_spec("dplyr");
        assert_eq!(spec, BoxSpec::Package("dplyr".into()));
        assert!(attach.is_empty());

        // Dotted package name.
        let (spec, _) = parse_spec("data.table");
        assert_eq!(spec, BoxSpec::Package("data.table".into()));
    }

    #[test]
    fn parse_spec_local_modules() {
        assert_eq!(
            parse_spec("./foo").0,
            BoxSpec::LocalModule {
                up_levels: 0,
                components: vec!["foo".into()]
            }
        );
        assert_eq!(
            parse_spec("../lib/util").0,
            BoxSpec::LocalModule {
                up_levels: 1,
                components: vec!["lib".into(), "util".into()]
            }
        );
        assert_eq!(
            parse_spec("../../a/b").0,
            BoxSpec::LocalModule {
                up_levels: 2,
                components: vec!["a".into(), "b".into()]
            }
        );
    }

    #[test]
    fn parse_spec_unsupported_nonlocal_paths() {
        assert!(matches!(parse_spec("foo/bar").0, BoxSpec::Unsupported(_)));
        assert!(matches!(parse_spec("./").0, BoxSpec::Unsupported(_)));
        assert!(matches!(parse_spec("../").0, BoxSpec::Unsupported(_)));
    }

    #[test]
    fn parse_spec_attach_lists() {
        let (_, attach) = parse_spec("dplyr[filter, select]");
        assert_eq!(
            attach,
            vec![
                BoxAttach::Named("filter".into()),
                BoxAttach::Named("select".into())
            ]
        );

        let (_, attach) = parse_spec("dplyr[f = filter, ...]");
        assert_eq!(
            attach,
            vec![
                BoxAttach::Renamed {
                    local: "f".into(),
                    exported: "filter".into()
                },
                BoxAttach::Wildcard
            ]
        );

        // Trailing comma tolerated.
        let (_, attach) = parse_spec("dplyr[a, b,]");
        assert_eq!(attach.len(), 2);
    }

    #[test]
    fn parse_spec_accepts_static_backtick_and_unicode_names() {
        let (_, attach) = parse_spec("magrittr[`%>%`, local = `σ`, naïve]");
        assert_eq!(
            attach,
            vec![
                BoxAttach::Named("%>%".into()),
                BoxAttach::Renamed {
                    local: "local".into(),
                    exported: "σ".into(),
                },
                BoxAttach::Named("naïve".into()),
            ]
        );
        assert_eq!(
            parse_spec("./`my module`").0,
            BoxSpec::LocalModule {
                up_levels: 0,
                components: vec!["my module".into()],
            }
        );
    }

    #[test]
    fn attachment_ranges_are_member_specific_and_multiline() {
        let code = "box::use(\n  ./mod[\n    local = `σ`,\n    missing\n  ]\n)";
        let tree = parse(code);
        let import = imports(code).pop().expect("import");
        assert_eq!(
            module_spec_range(&tree, code, &import),
            Some(Range::new(Position::new(1, 2), Position::new(1, 7)))
        );
        let ranges = attachment_token_ranges(&tree, code, &import);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].exported, "σ");
        assert_eq!(
            ranges[0].range,
            Range::new(Position::new(2, 12), Position::new(2, 15))
        );
        assert_eq!(ranges[1].exported, "missing");
        assert_eq!(
            ranges[1].range,
            Range::new(Position::new(3, 4), Position::new(3, 11))
        );
    }

    #[test]
    fn parse_spec_local_with_attach() {
        let (spec, attach) = parse_spec("./foo[bar]");
        assert_eq!(
            spec,
            BoxSpec::LocalModule {
                up_levels: 0,
                components: vec!["foo".into()]
            }
        );
        assert_eq!(attach, vec![BoxAttach::Named("bar".into())]);
    }

    #[test]
    fn detects_bare_package_use() {
        let got = imports("box::use(dplyr)");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].spec, BoxSpec::Package("dplyr".into()));
        assert_eq!(got[0].explicit_alias, None);
    }

    #[test]
    fn detects_alias_and_attach() {
        let got = imports("box::use(dr = dplyr, dplyr[filter, select])");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].explicit_alias.as_deref(), Some("dr"));
        assert_eq!(got[0].spec, BoxSpec::Package("dplyr".into()));
        assert_eq!(got[1].explicit_alias, None);
        assert_eq!(got[1].attach.len(), 2);
    }

    #[test]
    fn detects_local_module_with_attach_across_precedence() {
        // `./mod[a, b]` parses (in R) as `. / (mod[a, b])`; the text-based
        // spec parser must still recover the local path and attach list.
        let got = imports("box::use(./mod/helpers[a, b])");
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].spec,
            BoxSpec::LocalModule {
                up_levels: 0,
                components: vec!["mod".into(), "helpers".into()]
            }
        );
        assert_eq!(got[0].attach.len(), 2);
    }

    #[test]
    fn detects_multiline_with_trailing_comma() {
        let code = "box::use(\n  dplyr,\n  ./helpers[a],\n)";
        let got = imports(code);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].spec, BoxSpec::Package("dplyr".into()));
        assert!(matches!(got[1].spec, BoxSpec::LocalModule { .. }));
    }

    #[test]
    fn detects_triple_colon_form() {
        let got = imports("box:::use(dplyr)");
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn function_scoped_use_is_detected() {
        let code = "f <- function() {\n  box::use(dplyr)\n}";
        let got = imports(code);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].line, 1);
        assert!(got[0].function_scoped);
    }

    #[test]
    fn top_level_use_is_not_function_scoped() {
        let got = imports("box::use(dplyr)");
        assert_eq!(got.len(), 1);
        assert!(!got[0].function_scoped);
    }

    #[test]
    fn nested_block_inside_function_is_function_scoped() {
        let code = "f <- function() {\n  if (TRUE) {\n    box::use(dplyr)\n  }\n}";
        let got = imports(code);
        assert_eq!(got.len(), 1);
        assert!(got[0].function_scoped);
    }

    #[test]
    fn end_column_covers_spec_and_attach() {
        // `dplyr[filter]` starts at col 9, ends at col 22 (one past `]`).
        let got = imports("box::use(dplyr[filter])");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].column, 9);
        assert_eq!(got[0].end_column, 22);
        assert!(got[0].end_column > got[0].column);
    }

    #[test]
    fn non_box_calls_are_ignored() {
        assert!(imports("library(dplyr)").is_empty());
        assert!(imports("modules::use(foo)").is_empty());
        assert!(imports("use(dplyr)").is_empty());
    }
}
