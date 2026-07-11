//! R Smart Indentation Module
//!
//! This module provides AST-aware indentation for R code through the LSP
//! `textDocument/onTypeFormatting` handler. It implements a context-based
//! approach that detects syntactic context (pipe chains, function arguments,
//! brace blocks) and computes appropriate indentation.
//!
//! # Architecture
//!
//! - `context`: Detects syntactic context at cursor position using tree-sitter AST
//! - `calculator`: Computes indentation amount based on context and style configuration
//! - `formatter`: Generates LSP TextEdit for indentation replacement

use tower_lsp::lsp_types::DocumentOnTypeFormattingOptions;
use tower_lsp::lsp_types::Position;
use tree_sitter::Tree;

use crate::linting::InfixContinuationStyle;

mod calculator;
mod context;
mod formatter;
mod judge;

pub use calculator::{IndentationConfig, IndentationStyle, calculate_indentation};
pub use context::{IndentContext, OperatorType, detect_context};
pub use formatter::format_indentation;

/// Tier 2 on-type indent for an Enter press: repair-and-ask against the
/// lint's expectation engine, falling back to the legacy
/// `detect_context`/`calculate_indentation` path when the judge cannot answer.
pub fn on_type_indentation(
    tree: &Tree,
    source: &str,
    position: Position,
    config: &IndentationConfig,
    infix_style: InfixContinuationStyle,
) -> u32 {
    on_type_indentation_with_judge_unit(
        tree,
        source,
        position,
        config,
        config.tab_size,
        infix_style,
    )
}

/// Backend entry that lets the judge use the lint's resolved indentation
/// unit while preserving the editor unit for the frozen legacy fallback.
pub(crate) fn on_type_indentation_with_judge_unit(
    tree: &Tree,
    source: &str,
    position: Position,
    config: &IndentationConfig,
    judge_indent_unit: u32,
    infix_style: InfixContinuationStyle,
) -> u32 {
    let judge_config = IndentationConfig {
        tab_size: judge_indent_unit,
        ..config.clone()
    };
    if let Some(column) =
        judge::judge_backed_indentation(tree, source, position, &judge_config, infix_style)
    {
        log::trace!("on_type_indentation: judge-backed tier selected column {column}");
        return column;
    }

    log::trace!("on_type_indentation: falling back to legacy context detection");
    let context = detect_context(tree, source, position, config.tab_size);
    let column = calculate_indentation(context, config.clone(), source);
    log::trace!("on_type_indentation: legacy fallback tier selected column {column}");
    column
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
    fn multiline_string_interior_uses_legacy_fallback_column() {
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

        assert_eq!(
            column, 0,
            "judge bail must preserve the legacy multiline-string column"
        );
    }
}
