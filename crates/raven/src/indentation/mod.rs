//! R smart indentation.
//!
//! Tier 2 indentation is judge-or-nothing: the on-type formatting handler
//! repairs the just-edited buffer and asks the indentation lint's expectation
//! engine for a valid column. When the judge cannot answer, this module returns
//! `None` and emits no edit, preserving the indentation already supplied by
//! the editor's Tier 1 rules or native indentation.
//!
//! The judge deliberately declines inside multiline string-like nodes, in
//! tabs-mode or tab-shaped active contexts, when syntax errors intersect its
//! reference-to-probe window, and when the surrounding reference indentation
//! does not conform to the lint's accepted set. Invalid or ambiguous repairs
//! likewise produce no answer.

use tower_lsp::lsp_types::DocumentOnTypeFormattingOptions;
use tower_lsp::lsp_types::Position;
use tree_sitter::Tree;

use crate::linting::InfixContinuationStyle;

mod config;
mod formatter;
mod judge;

pub use config::{IndentationConfig, IndentationStyle};
pub use formatter::format_indentation;

/// Tier 2 on-type indent for an Enter press.
///
/// Returns the lint expectation engine's selected column, or `None` when the
/// judge cannot answer so the caller can preserve the editor's indentation.
pub fn on_type_indentation(
    tree: &Tree,
    source: &str,
    position: Position,
    config: &IndentationConfig,
    infix_style: InfixContinuationStyle,
) -> Option<u32> {
    on_type_indentation_with_judge_unit(
        tree,
        source,
        position,
        config,
        config.tab_size,
        infix_style,
    )
}

/// Backend entry that lets the judge use the lint's resolved indentation unit.
pub(crate) fn on_type_indentation_with_judge_unit(
    tree: &Tree,
    source: &str,
    position: Position,
    config: &IndentationConfig,
    judge_indent_unit: u32,
    infix_style: InfixContinuationStyle,
) -> Option<u32> {
    let judge_config = IndentationConfig {
        tab_size: judge_indent_unit,
        ..config.clone()
    };
    if let Some(column) =
        judge::judge_backed_indentation(tree, source, position, &judge_config, infix_style)
    {
        log::trace!("on_type_indentation: judge-backed tier selected column {column}");
        return Some(column);
    }

    log::trace!("on_type_indentation: judge bailed; preserving editor indentation");
    None
}

/// Returns the LSP capability options for on-type formatting.
///
/// Registers trigger characters:
/// - `\n` — AST-aware indentation when the user presses Enter
/// - `)`, `]`, `}` — auto-close duplicate delimiter removal
pub fn on_type_formatting_capability() -> DocumentOnTypeFormattingOptions {
    DocumentOnTypeFormattingOptions {
        first_trigger_character: "\n".to_string(),
        more_trigger_character: Some(vec![")".to_string(), "]".to_string(), "}".to_string()]),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IndentationConfig, IndentationStyle, on_type_formatting_capability, on_type_indentation,
    };
    use crate::linting::InfixContinuationStyle;
    use crate::parser_pool::with_parser;
    use tower_lsp::lsp_types::Position;

    #[test]
    fn test_on_type_formatting_capability_registration() {
        let capability = on_type_formatting_capability();

        assert_eq!(
            capability.first_trigger_character, "\n",
            "first_trigger_character should be newline"
        );

        let more = capability
            .more_trigger_character
            .expect("should have more triggers");
        assert!(more.contains(&")".to_string()), "should trigger on )");
        assert!(more.contains(&"]".to_string()), "should trigger on ]");
        assert!(more.contains(&"}".to_string()), "should trigger on }}");
    }

    #[test]
    fn multiline_string_interior_preserves_editor_indentation() {
        let source = "text <- \"first\nstill open\n";
        let tree = with_parser(|parser| parser.parse(source, None)).expect("parse must succeed");
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        let column = on_type_indentation(
            &tree,
            source,
            Position::new(1, 0),
            &config,
            InfixContinuationStyle::Indented,
        );

        assert_eq!(column, None, "multiline-string interiors must emit no edit");
    }

    #[test]
    fn offset_context_preserves_editor_indentation() {
        let source = "    {\n";
        let tree = with_parser(|parser| parser.parse(source, None)).expect("parse must succeed");
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        let column = on_type_indentation(
            &tree,
            source,
            Position::new(1, 0),
            &config,
            InfixContinuationStyle::Indented,
        );

        assert_eq!(column, None, "offset contexts must emit no edit");
    }
}
