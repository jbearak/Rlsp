//! Stan-specific parsing preparation.
//!
//! Raven directives are valid editor metadata but are not Stan comments:
//! Stan reserves `#` for preprocessor directives such as `#include`. Before
//! parsing, recognized full-line Raven directives—and a recognized trailing
//! same-line suppression comment—are replaced byte-for-byte with ASCII spaces.
//! Newlines, code before a trailing marker, and a leading UTF-8 BOM are
//! retained, so every tree-sitter byte/point coordinate still addresses the
//! original document.

use std::borrow::Cow;

use tree_sitter::{Parser, Tree};

/// Parse a Stan document after applying Raven's geometry-preserving extension
/// mask. The returned analysis text must be retained alongside the tree.
pub(crate) fn parse(text: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_stan::LANGUAGE_STAN.into())
        .ok()?;
    parser.parse(text, None)
}

/// Mask recognized Raven directive regions without changing byte length or
/// line boundaries. Returns `None` when no mask is needed.
pub(crate) fn mask_raven_directives(text: &str) -> Option<String> {
    let eligibility_view = directive_eligibility_view(text);
    let mut masked = None::<Vec<u8>>;
    let mut offset = 0usize;
    let mut in_header = true;

    for segment in text.split_inclusive('\n') {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let eligible_segment = &eligibility_view[offset..offset + segment.len()];
        let eligible_line = eligible_segment
            .strip_suffix('\n')
            .unwrap_or(eligible_segment);
        let eligible_line = eligible_line.strip_suffix('\r').unwrap_or(eligible_line);
        let scan_line = if offset == 0 {
            eligible_line
                .strip_prefix('\u{feff}')
                .unwrap_or(eligible_line)
        } else {
            eligible_line
        };

        if crate::cross_file::directive::is_recognized_full_line_directive(scan_line, in_header) {
            let bytes = masked.get_or_insert_with(|| text.as_bytes().to_vec());
            let bom_len = if offset == 0 && line.starts_with('\u{feff}') {
                '\u{feff}'.len_utf8()
            } else {
                0
            };
            for byte in &mut bytes[offset + bom_len..offset + line.len()] {
                *byte = b' ';
            }
        } else if let Some(comment) = same_line_suppression_comment(scan_line) {
            let bytes = masked.get_or_insert_with(|| text.as_bytes().to_vec());
            let bom_len = if offset == 0 && line.starts_with('\u{feff}') {
                '\u{feff}'.len_utf8()
            } else {
                0
            };
            let comment_start = scan_line.len() - comment.len();
            for byte in &mut bytes[offset + bom_len + comment_start..offset + line.len()] {
                *byte = b' ';
            }
        }

        if in_header {
            let trimmed = scan_line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                in_header = false;
            }
        }
        offset += segment.len();
    }

    masked.map(|bytes| {
        String::from_utf8(bytes).expect("Stan directive masking preserves valid UTF-8")
    })
}

#[derive(Clone, Copy)]
enum DirectiveLexState {
    Code,
    DoubleString,
    LineComment,
    BlockComment,
}

/// Geometry-preserving source view for Stan diagnostic directive decisions.
///
/// A single persistent lexical scan replaces only hashes inside Stan strings
/// or comments with ASCII spaces. Code-state hashes remain byte-identical.
/// Diagnostic masking and diagnostic-metadata consumers may therefore apply
/// the canonical directive regexes to this view without any raw-regex bypass.
/// Line comments reset at newline; block comments and double-quoted strings
/// persist across lines; backslash escapes are honored in strings; apostrophe
/// remains Stan's transpose operator.
pub(crate) fn directive_eligibility_view(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut view = None::<Vec<u8>>;
    let mut index = 0usize;
    let mut state = DirectiveLexState::Code;

    while index < bytes.len() {
        match state {
            DirectiveLexState::Code => match bytes[index] {
                b'"' => state = DirectiveLexState::DoubleString,
                b'/' if bytes.get(index + 1) == Some(&b'/') => {
                    state = DirectiveLexState::LineComment;
                    index += 2;
                    continue;
                }
                b'/' if bytes.get(index + 1) == Some(&b'*') => {
                    state = DirectiveLexState::BlockComment;
                    index += 2;
                    continue;
                }
                _ => {}
            },
            DirectiveLexState::DoubleString => match bytes[index] {
                b'#' => mask_ineligible_hash(&mut view, bytes, index),
                b'\\' => {
                    if bytes.get(index + 1) == Some(&b'#') {
                        mask_ineligible_hash(&mut view, bytes, index + 1);
                    }
                    index = index.saturating_add(2);
                    continue;
                }
                b'"' => state = DirectiveLexState::Code,
                _ => {}
            },
            DirectiveLexState::LineComment => match bytes[index] {
                b'#' => mask_ineligible_hash(&mut view, bytes, index),
                b'\n' => state = DirectiveLexState::Code,
                _ => {}
            },
            DirectiveLexState::BlockComment => {
                if bytes[index] == b'#' {
                    mask_ineligible_hash(&mut view, bytes, index);
                }
                if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    state = DirectiveLexState::Code;
                    index += 2;
                    continue;
                }
            }
        }
        index += 1;
    }

    view.map_or(Cow::Borrowed(text), |bytes| {
        Cow::Owned(String::from_utf8(bytes).expect("Stan eligibility view preserves UTF-8"))
    })
}

fn mask_ineligible_hash(view: &mut Option<Vec<u8>>, source: &[u8], index: usize) {
    view.get_or_insert_with(|| source.to_vec())[index] = b' ';
}

/// Suffix beginning at an eligible, exact Raven line-suppression marker.
///
/// `line` must come from [`directive_eligibility_view`], which has already
/// removed every hash hidden by a string or comment. Scanning every remaining
/// hash preserves an earlier `#include` or unknown hash construct and returns
/// only the hash whose full suffix matches a same-line marker.
pub(crate) fn same_line_suppression_comment(line: &str) -> Option<&str> {
    line.match_indices('#')
        .map(|(index, _)| &line[index..])
        .find(|comment| crate::cross_file::directive::is_recognized_same_line_suppression(comment))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directive_metadata(source: &str) -> crate::cross_file::CrossFileMetadata {
        let view = directive_eligibility_view(source);
        crate::cross_file::directive::parse_directives_with_same_line_scanner(
            &view,
            same_line_suppression_comment,
        )
    }

    #[test]
    fn masks_only_recognized_raven_directives_and_preserves_geometry() {
        let source = "\u{feff}# raven: cd models\r\ndata { int N; }\nmodel { target += missing; } # raven: ignore[undefined-variable]\n# raven: source shared.R\n#include helper.stanfunctions\n# unknown\n";
        let masked = mask_raven_directives(source).expect("recognized directives must mask");
        assert_eq!(masked.len(), source.len());
        assert_eq!(masked.lines().count(), source.lines().count());
        assert!(masked.starts_with('\u{feff}'));
        assert!(!masked.contains("raven:"));
        assert!(masked.contains("model { target += missing; }"));
        assert!(masked.contains("#include helper.stanfunctions"));
        assert!(masked.contains("# unknown"));
    }

    #[test]
    fn header_only_directive_after_code_is_not_masked() {
        let source = "model {}\n# raven: cd elsewhere\n";
        assert!(mask_raven_directives(source).is_none());
    }

    #[test]
    fn only_same_line_ignore_may_trail_stan_code() {
        assert!(
            mask_raven_directives("model {} # raven: ignore-next\n").is_none(),
            "ignore-next is standalone-only and must not be silently accepted after code"
        );
        let source = "model { target += missing; } # raven: ignore\n";
        let masked = mask_raven_directives(source).expect("same-line ignore mask");
        assert!(masked.starts_with("model { target += missing; }"));
        assert!(!masked.contains("raven:"));
    }

    #[test]
    fn masks_only_the_actual_marker_after_include_or_unknown_hash_syntax() {
        for prefix in ["#include declarations.stan ", "# made-up syntax "] {
            let source = format!("{prefix}# raven: ignore\n");
            let masked = mask_raven_directives(&source).expect("trailing marker mask");
            assert!(masked.starts_with(prefix), "{masked:?}");
            assert_eq!(masked.len(), source.len());
            assert!(!masked.contains("raven:"));
        }
    }

    #[test]
    fn stan_marker_scan_treats_apostrophe_as_transpose_and_skips_strings() {
        let transpose =
            "model { target += sum(x') + missing; } # raven: ignore[undefined-variable]\n";
        let masked = mask_raven_directives(transpose).expect("transpose trailing marker mask");
        assert!(masked.starts_with("model { target += sum(x') + missing; } "));
        assert!(!masked.contains("raven:"));

        let in_string = "model { print(\"# raven: ignore\"); target += missing; }\n";
        assert!(
            mask_raven_directives(in_string).is_none(),
            "marker-shaped text inside a Stan string is source, not metadata"
        );

        let escaped_quote = "model { print(\"escaped \\\" # raven: ignore\"); } # raven: ignore\n";
        let masked = mask_raven_directives(escaped_quote).expect("real trailing marker mask");
        assert!(masked.contains("# raven: ignore\");"));
        assert!(!masked.ends_with("# raven: ignore\n"));
    }

    #[test]
    fn stan_marker_scan_ignores_hashes_and_quotes_inside_comments() {
        for comment in [
            "model { target += missing; } // note \" # raven: ignore\n",
            "model { target += missing; } /* note \" # raven: ignore */\n",
        ] {
            assert!(
                mask_raven_directives(comment).is_none(),
                "comment-contained marker text is never Raven metadata: {comment:?}"
            );
        }

        let after_line_comment =
            "// note \" # raven: ignore\nmodel { target += missing; } # raven: ignore\n";
        let masked = mask_raven_directives(after_line_comment).expect("next-line real marker mask");
        assert!(masked.starts_with("// note \" # raven: ignore\nmodel { target += missing; } "));

        let after_block_comment = "model { target += missing; } /* note \" */ # raven: ignore\n";
        let masked =
            mask_raven_directives(after_block_comment).expect("post-block real marker mask");
        assert!(masked.starts_with("model { target += missing; } /* note \" */ "));
        assert!(!masked.ends_with("# raven: ignore\n"));
    }

    #[test]
    fn document_scan_blocks_every_directive_family_inside_multiline_comments() {
        let source = r#"/* opening
# raven: var missing
# raven: func missing_fun
# raven: ignore
# raven: ignore-next[undefined-variable]
# raven: ignore-file[undefined-variable]
# raven: ignore-start[undefined-variable]
# raven: ignore-end
*/
model { target += missing; }
"#;
        let view = directive_eligibility_view(source);
        assert_eq!(view.len(), source.len());
        assert_eq!(view.lines().count(), source.lines().count());
        assert_eq!(view.matches('#').count(), 0, "{view:?}");
        assert!(
            mask_raven_directives(source).is_none(),
            "comment prose must not be geometry-masked as directives"
        );

        let metadata = directive_metadata(source);
        assert!(metadata.declared_variables.is_empty());
        assert!(metadata.declared_functions.is_empty());
        assert!(metadata.ignored_lines.is_empty());
        assert!(metadata.ignored_next_lines.is_empty());
        assert!(metadata.ignored_file.is_none());
        assert!(metadata.ignored_ranges.is_empty());
        assert!(metadata.suppression_directives.is_empty());
    }

    #[test]
    fn document_scan_carries_strings_and_comments_across_lines_then_recovers() {
        let string_source = "model { print(\"opening\n# raven: ignore-file[undefined-variable]\nclosing\"); target += missing; }\n";
        let string_view = directive_eligibility_view(string_source);
        assert_eq!(string_view.matches('#').count(), 0, "{string_view:?}");
        assert!(directive_metadata(string_source).ignored_file.is_none());

        let line_then_real = "// fake # raven: ignore-file[undefined-variable]\n# raven: ignore-file[undefined-variable]\nmodel { target += muted; }\n";
        let line_view = directive_eligibility_view(line_then_real);
        assert_eq!(line_view.matches('#').count(), 1, "{line_view:?}");
        assert!(directive_metadata(line_then_real).ignored_file.is_some());

        let block_then_real = "/* opening\n# raven: ignore-file[undefined-variable]\n*/\nmodel { target += muted; } /* note \" # fake */ # raven: ignore[undefined-variable]\n";
        let block_view = directive_eligibility_view(block_then_real);
        assert_eq!(block_view.matches('#').count(), 1, "{block_view:?}");
        let masked = mask_raven_directives(block_then_real).expect("real post-block marker");
        assert!(masked.contains("# raven: ignore-file[undefined-variable]"));
        assert!(!masked.ends_with("# raven: ignore[undefined-variable]\n"));
    }
}
