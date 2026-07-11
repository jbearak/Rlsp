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

mod calculator;
mod context;
mod formatter;
mod judge;

pub use calculator::{IndentationConfig, IndentationStyle, calculate_indentation};
pub use context::{IndentContext, OperatorType, detect_context};
pub use formatter::format_indentation;
pub use judge::judge_backed_indentation;

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
    use super::on_type_formatting_capability;

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
}
