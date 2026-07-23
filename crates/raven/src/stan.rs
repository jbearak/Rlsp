//! Stan-specific parsing preparation.
//!
//! Raven directives are valid editor metadata but are not Stan comments:
//! Stan reserves `#` for preprocessor directives such as `#include`. Before
//! parsing, recognized full-line Raven directives are replaced byte-for-byte
//! with ASCII spaces. Newlines and a leading UTF-8 BOM are retained, so every
//! tree-sitter byte/point coordinate still addresses the original document.

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

/// Mask recognized full-line Raven directives without changing byte length or
/// line boundaries. Returns `None` when no mask is needed.
pub(crate) fn mask_raven_directives(text: &str) -> Option<String> {
    let mut masked = None::<Vec<u8>>;
    let mut offset = 0usize;
    let mut in_header = true;

    for segment in text.split_inclusive('\n') {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let scan_line = if offset == 0 {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_only_recognized_raven_directives_and_preserves_geometry() {
        let source = "\u{feff}# raven: cd models\r\ndata { int N; }\n# raven: source shared.R\n#include helper.stanfunctions\n# unknown\n";
        let masked = mask_raven_directives(source).expect("recognized directives must mask");
        assert_eq!(masked.len(), source.len());
        assert_eq!(masked.lines().count(), source.lines().count());
        assert!(masked.starts_with('\u{feff}'));
        assert!(!masked.contains("raven:"));
        assert!(masked.contains("#include helper.stanfunctions"));
        assert!(masked.contains("# unknown"));
    }

    #[test]
    fn header_only_directive_after_code_is_not_masked() {
        let source = "model {}\n# raven: cd elsewhere\n";
        assert!(mask_raven_directives(source).is_none());
    }
}
