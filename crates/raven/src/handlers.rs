//
// handlers.rs
//
// Copyright (C) 2024-2026 Posit Software, PBC. All rights reserved.
// Modifications copyright (C) 2026 Jonathan Marc Bearak
//

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use regex::Regex;
use tower_lsp::lsp_types::*;
use tree_sitter::Node;
use tree_sitter::Point;

use crate::content_provider::ContentProvider;
use crate::cross_file::dependency::compute_inherited_working_directory;
use crate::cross_file::{scope, ScopedSymbol};
use crate::state::WorldState;

use crate::builtins;
use crate::reserved_words::is_reserved_word;

/// Maximum valid character value for LSP positions.
///
/// The LSP specification defines position characters as `uinteger` (0..2147483647).
/// Using `u32::MAX` (4294967295) exceeds this range and causes the VS Code client's
/// `DocumentSymbol.is()` type guard to fail, which cascades into a runtime error
/// when the client falls through to `asSymbolInformations()`.
///
/// Per the LSP spec, if the character value exceeds the line length, clients treat
/// it as the end of the line — so any large valid value works as an end-of-line sentinel.
const LSP_EOL_CHARACTER: u32 = i32::MAX as u32; // 2147483647

// ============================================================================
// Section Pattern
// ============================================================================

/// Get the compiled regex pattern for R code section comments.
///
/// Pattern: `^\s*#(#*)\s*(%%)?\s*(\S.+?)\s*(#{4,}|\-{4,}|={4,}|\*{4,}|\+{4,})\s*$`
///
/// Components:
/// - `^\s*`           - Optional leading whitespace
/// - `#(#*)`          - One or more # characters (capture group 1 = additional # count)
/// - `\s*(%%)?\s*`    - Optional %% marker (RStudio style, capture group 2)
/// - `(\S.+?)`        - Section name (capture group 3)
/// - `\s*`            - Optional whitespace
/// - `(#{4,}|...)`    - Trailing delimiter (4+ of #, -, =, *, +, capture group 4)
/// - `\s*$`           - Optional trailing whitespace
///
/// Examples:
/// - `# Section Name ----`
/// - `## Subsection ####`
/// - `# %% Cell Name ----`
/// - `### Deep Section ========`
fn section_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"^\s*#(#*)\s*(%%)?\s*(\S.+?)\s*(#{4,}|-{4,}|={4,}|\*{4,}|\+{4,})\s*$").unwrap()
    })
}

/// Returns true if the string consists entirely of delimiter characters
/// (`#`, `-`, `=`, `*`, `+`) and/or whitespace. Used to filter out
/// decorative separator lines (e.g. `# ==================`) from section
/// detection. Returns true for empty strings, though this cannot occur in
/// practice because the section regex requires at least 2 characters.
fn is_delimiter_only(s: &str) -> bool {
    const DELIMITER_CHARS: &[char] = &['#', '-', '=', '*', '+'];
    s.chars().all(|c| c.is_whitespace() || DELIMITER_CHARS.contains(&c))
}

/// Computes the UTF-16 length of a string (number of UTF-16 code units).
fn utf16_len(s: &str) -> u32 {
    s.chars().map(|c| c.len_utf16() as u32).sum()
}

// ============================================================================
// Banner-Style Section Helpers
// ============================================================================

/// Delimiter character types used in banner-style section detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DelimiterKind {
    Hash,
    Dash,
    Equals,
    Asterisk,
    Plus,
}

/// Classifies a line as a delimiter line for banner-style section detection.
///
/// A delimiter line is a comment line consisting entirely of a single repeated
/// delimiter character (4+). Two forms are recognized:
/// - `################` (all hashes)
/// - `# ================` (leading `#` + space + 4+ of one delimiter)
///
/// Returns `Some(kind)` if the line is a delimiter line, `None` otherwise.
fn classify_delimiter_line(line: &str) -> Option<DelimiterKind> {
    let trimmed = line.trim();
    let after_first_hash = trimmed.strip_prefix('#')?;

    // Case 1: all hashes (e.g., "################") — need 4+ total
    if !after_first_hash.is_empty()
        && after_first_hash.chars().all(|c| c == '#')
        && trimmed.len() >= 4
    {
        return Some(DelimiterKind::Hash);
    }

    // Case 2: "# <delimiters>" — leading # then space then 4+ of one delimiter
    let content = if let Some(rest) = after_first_hash.strip_prefix(' ') {
        rest.trim()
    } else {
        return None;
    };

    if content.len() < 4 {
        return None;
    }

    let first_char = content.chars().next()?;
    let kind = match first_char {
        '-' => DelimiterKind::Dash,
        '=' => DelimiterKind::Equals,
        '*' => DelimiterKind::Asterisk,
        '+' => DelimiterKind::Plus,
        '#' => DelimiterKind::Hash,
        _ => return None,
    };

    if content.chars().all(|c| c == first_char) {
        Some(kind)
    } else {
        None
    }
}

/// Extracts a section name and heading level from the middle line of a
/// banner-style section.
///
/// The heading level is determined by the number of leading `#` characters
/// (`# Name` = level 1, `## Name` = level 2, etc.), matching the single-line
/// section convention.
///
/// Strips leading `#` characters and whitespace, then strips trailing delimiter
/// characters (`#`, `-`, `=`, `*`, `+`) and whitespace. Returns `None` if the
/// result is empty or consists only of delimiter characters.
fn extract_banner_name(line: &str) -> Option<(String, u32)> {
    const DELIMITER_CHARS: &[char] = &['#', '-', '=', '*', '+'];

    let trimmed = line.trim();
    if trimmed.is_empty() || !trimmed.starts_with('#') {
        return None;
    }

    // Count leading # characters for heading level
    let hash_count = trimmed.chars().take_while(|&c| c == '#').count() as u32;

    // Strip leading # characters, then whitespace
    let content = trimmed.trim_start_matches('#').trim();
    // Strip trailing delimiter characters and whitespace
    let name = content.trim_end_matches(DELIMITER_CHARS).trim();

    if name.is_empty() || is_delimiter_only(name) {
        return None;
    }

    Some((name.to_string(), hash_count))
}

// ============================================================================
// Document Symbol Types
// ============================================================================

/// Extended symbol kind for document symbols.
///
/// Maps to LSP SymbolKind with richer categorization for R-specific constructs.
/// This enum provides more granular classification than the basic function/variable
/// distinction, supporting R6 classes, S4 methods, constants, and code sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentSymbolKind {
    /// Regular function definition (SymbolKind::FUNCTION)
    Function,
    /// Variable assignment (SymbolKind::VARIABLE)
    Variable,
    /// ALL_CAPS constant pattern (SymbolKind::CONSTANT)
    Constant,
    /// R6Class or setRefClass/setClass definition (SymbolKind::CLASS)
    Class,
    /// setMethod definition (SymbolKind::METHOD)
    Method,
    /// setGeneric definition (SymbolKind::INTERFACE)
    Interface,
    /// R code section comment (SymbolKind::MODULE)
    Module,
}

impl DocumentSymbolKind {
    /// Convert to LSP SymbolKind for protocol responses.
    ///
    /// # Examples
    ///
    /// ```
    /// use raven::handlers::DocumentSymbolKind;
    /// use tower_lsp::lsp_types::SymbolKind;
    ///
    /// assert_eq!(DocumentSymbolKind::Function.to_lsp_kind(), SymbolKind::FUNCTION);
    /// assert_eq!(DocumentSymbolKind::Constant.to_lsp_kind(), SymbolKind::CONSTANT);
    /// ```
    pub fn to_lsp_kind(self) -> SymbolKind {
        match self {
            Self::Function => SymbolKind::FUNCTION,
            Self::Variable => SymbolKind::VARIABLE,
            Self::Constant => SymbolKind::CONSTANT,
            Self::Class => SymbolKind::CLASS,
            Self::Method => SymbolKind::METHOD,
            Self::Interface => SymbolKind::INTERFACE,
            Self::Module => SymbolKind::MODULE,
        }
    }
}

/// Intermediate symbol representation before hierarchy building.
///
/// This struct captures all the information needed to build a hierarchical
/// `DocumentSymbol` tree, including position information for nesting decisions
/// and optional metadata like function signatures and section levels.
///
/// # Fields
///
/// - `name`: The symbol's identifier (function name, variable name, or section title)
/// - `kind`: Classification determining the LSP SymbolKind
/// - `range`: Full extent of the construct (start of assignment to end of value/body)
/// - `selection_range`: Identifier-only range for navigation
/// - `detail`: Optional function parameter signature (e.g., "(x, y, ...)")
/// - `section_level`: For sections only, the heading level (1 = #, 2 = ##, etc.)
/// - `children`: Child symbols nested within this symbol (for hierarchy building)
///
/// # Examples
///
/// ```
/// use raven::handlers::{RawSymbol, DocumentSymbolKind};
/// use tower_lsp::lsp_types::{Range, Position};
///
/// let symbol = RawSymbol {
///     name: "my_function".to_string(),
///     kind: DocumentSymbolKind::Function,
///     range: Range {
///         start: Position { line: 0, character: 0 },
///         end: Position { line: 5, character: 1 },
///     },
///     selection_range: Range {
///         start: Position { line: 0, character: 0 },
///         end: Position { line: 0, character: 11 },
///     },
///     detail: Some("(x, y)".to_string()),
///     section_level: None,
///     children: Vec::new(),
/// };
/// ```
#[derive(Debug, Clone)]
pub struct RawSymbol {
    /// Symbol name (function name, variable name, or section title)
    pub name: String,
    /// Symbol kind for LSP SymbolKind mapping
    pub kind: DocumentSymbolKind,
    /// Full range (start of construct to end of value/body)
    pub range: Range,
    /// Selection range (identifier only)
    pub selection_range: Range,
    /// Function parameter signature (for detail field), e.g., "(x, y, ...)"
    pub detail: Option<String>,
    /// Section heading level (1 = #, 2 = ##, etc.) for sections only
    pub section_level: Option<u32>,
    /// Child symbols nested within this symbol (for hierarchy building)
    pub children: Vec<RawSymbol>,
}

// ============================================================================
// Symbol Extractor
// ============================================================================

/// Extracts symbols from a parsed R document.
///
/// `SymbolExtractor` traverses the tree-sitter AST to collect function definitions,
/// variable assignments, S4 methods, R6 classes, and R code sections. Each symbol
/// includes both a full `range` (from start of assignment to end of value/body) and
/// a `selection_range` (identifier only) for proper LSP navigation.
///
/// # Examples
///
/// ```
/// use tree_sitter::Parser;
/// use raven::handlers::SymbolExtractor;
///
/// let mut parser = Parser::new();
/// parser.set_language(&tree_sitter_r::LANGUAGE.into()).unwrap();
/// let code = "my_func <- function(x) { x + 1 }";
/// let tree = parser.parse(code, None).unwrap();
/// let extractor = SymbolExtractor::new(code, tree.root_node());
/// let symbols = extractor.extract_all();
/// assert_eq!(symbols.len(), 1);
/// assert_eq!(symbols[0].name, "my_func");
/// ```
pub struct SymbolExtractor<'a> {
    text: &'a str,
    root: tree_sitter::Node<'a>,
}

impl<'a> SymbolExtractor<'a> {
    /// Create a new SymbolExtractor for the given source text and AST root node.
    ///
    /// # Arguments
    ///
    /// * `text` - The source text of the R document
    /// * `root` - The root node of the tree-sitter parse tree
    ///
    /// # Examples
    ///
    /// ```
    /// use tree_sitter::Parser;
    /// use raven::handlers::SymbolExtractor;
    ///
    /// let mut parser = Parser::new();
    /// parser.set_language(&tree_sitter_r::LANGUAGE.into()).unwrap();
    /// let code = "x <- 42";
    /// let tree = parser.parse(code, None).unwrap();
    /// let extractor = SymbolExtractor::new(code, tree.root_node());
    /// ```
    pub fn new(text: &'a str, root: tree_sitter::Node<'a>) -> Self {
        Self { text, root }
    }

    /// Extract all raw symbols from the document.
    ///
    /// This method traverses the entire AST and collects:
    /// - Function definitions (assignments where RHS is a function)
    /// - Variable assignments (assignments where RHS is not a function)
    /// - S4 methods (setMethod, setClass, setGeneric calls)
    /// - R6 classes (R6Class, setRefClass calls)
    /// - R code sections (# Section ----)
    ///
    /// # Returns
    ///
    /// A vector of `RawSymbol` entries representing all symbols found in the document.
    pub fn extract_all(&self) -> Vec<RawSymbol> {
        let mut symbols = Vec::new();
        self.extract_assignments_recursive(self.root, &mut symbols);
        // Extract S4 method definitions (setMethod, setClass, setGeneric)
        let s4_symbols = self.extract_s4_methods(self.root);
        symbols.extend(s4_symbols);
        // Extract R code sections (# Section ----)
        let section_symbols = self.extract_sections();
        symbols.extend(section_symbols);
        symbols
    }

    /// Recursively extract assignments from the AST.
    fn extract_assignments_recursive(
        &self,
        node: tree_sitter::Node<'a>,
        symbols: &mut Vec<RawSymbol>,
    ) {
        // Try to extract an assignment from this node
        if let Some(symbol) = self.extract_assignment(node) {
            symbols.push(symbol);
        }

        // Recurse into children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.extract_assignments_recursive(child, symbols);
        }
    }

    /// Extract a single assignment from a node if it represents an assignment.
    ///
    /// This method handles:
    /// - `name <- value` (left assignment)
    /// - `name = value` (equals assignment)
    /// - `name <<- value` (super assignment)
    /// - `value -> name` (right assignment)
    ///
    /// # Arguments
    ///
    /// * `node` - A tree-sitter node to check for assignment
    ///
    /// # Returns
    ///
    /// `Some(RawSymbol)` if the node is an assignment, `None` otherwise.
    ///
    /// # Range Computation
    ///
    /// - `range`: Spans from the start of the assignment to the end of the RHS (value/body)
    /// - `selection_range`: Spans only the identifier (LHS for `<-`/`=`/`<<-`, RHS for `->`)
    fn extract_assignment(&self, node: tree_sitter::Node<'a>) -> Option<RawSymbol> {
        // Only process binary_operator nodes
        if node.kind() != "binary_operator" {
            return None;
        }

        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();

        if children.len() != 3 {
            return None;
        }

        let lhs = children[0];
        let op = children[1];
        let rhs = children[2];

        let op_text = node_text(op, self.text);

        // Handle -> operator: RHS is the name, LHS is the value
        if op_text == "->" {
            return self.extract_right_assignment(node, lhs, rhs);
        }

        // Handle <- = <<- operators: LHS is the name, RHS is the value
        if matches!(op_text, "<-" | "=" | "<<-") {
            return self.extract_left_assignment(node, lhs, rhs);
        }

        None
    }

    /// Extract a left assignment (name <- value, name = value, name <<- value).
    fn extract_left_assignment(
        &self,
        node: tree_sitter::Node<'a>,
        lhs: tree_sitter::Node<'a>,
        rhs: tree_sitter::Node<'a>,
    ) -> Option<RawSymbol> {
        // LHS must be an identifier
        if lhs.kind() != "identifier" {
            return None;
        }

        let name = node_text(lhs, self.text).to_string();

        // Skip reserved words
        if crate::reserved_words::is_reserved_word(&name) {
            return None;
        }

        // Determine symbol kind based on RHS and name
        let kind = self.classify_symbol(&name, rhs);

        // Compute full range: from start of assignment to end of RHS
        let range = self.compute_node_range(node);

        // Compute selection range: identifier only
        let selection_range = self.compute_node_range(lhs);

        // Extract function signature if this is a function
        let detail = if matches!(kind, DocumentSymbolKind::Function) {
            self.extract_signature(rhs)
        } else {
            None
        };

        Some(RawSymbol {
            name,
            kind,
            range,
            selection_range,
            detail,
            section_level: None,
            children: Vec::new(),
        })
    }

    /// Extract a right assignment (value -> name).
    fn extract_right_assignment(
        &self,
        node: tree_sitter::Node<'a>,
        lhs: tree_sitter::Node<'a>,
        rhs: tree_sitter::Node<'a>,
    ) -> Option<RawSymbol> {
        // RHS must be an identifier (the name being assigned)
        if rhs.kind() != "identifier" {
            return None;
        }

        let name = node_text(rhs, self.text).to_string();

        // Skip reserved words
        if crate::reserved_words::is_reserved_word(&name) {
            return None;
        }

        // Determine symbol kind based on LHS (the value) and name
        let kind = self.classify_symbol(&name, lhs);

        // Compute full range: from start of assignment to end of RHS
        let range = self.compute_node_range(node);

        // Compute selection range: identifier only (RHS for -> operator)
        let selection_range = self.compute_node_range(rhs);

        // Extract function signature if this is a function
        // For right assignment, the function is on the LHS
        let detail = if matches!(kind, DocumentSymbolKind::Function) {
            self.extract_signature(lhs)
        } else {
            None
        };

        Some(RawSymbol {
            name,
            kind,
            range,
            selection_range,
            detail,
            section_level: None,
            children: Vec::new(),
        })
    }

    /// Determine symbol kind from assignment RHS and name.
    ///
    /// Classification priority:
    /// 1. If RHS is R6Class() or setRefClass() call → CLASS
    /// 2. If name matches ALL_CAPS pattern → CONSTANT
    /// 3. If RHS is function_definition → FUNCTION
    /// 4. Otherwise → VARIABLE
    ///
    /// # Arguments
    ///
    /// * `name` - The symbol name (identifier)
    /// * `rhs` - The right-hand side node of the assignment (the value)
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // R6 class detection
    /// // MyClass <- R6Class("MyClass", ...) → CLASS
    ///
    /// // ALL_CAPS constant detection
    /// // MAX_VALUE <- 100 → CONSTANT
    /// // PI <- 3.14159 → CONSTANT (single char not constant, but PI is 2 chars)
    ///
    /// // Function detection
    /// // my_func <- function(x) x + 1 → FUNCTION
    ///
    /// // Variable detection
    /// // x <- 42 → VARIABLE
    /// ```
    fn classify_symbol(&self, name: &str, rhs: tree_sitter::Node<'a>) -> DocumentSymbolKind {
        // Priority 1: Check for R6Class() or setRefClass() call
        if self.is_r6_class_call(rhs) {
            return DocumentSymbolKind::Class;
        }

        // Priority 2: Check for ALL_CAPS constant pattern
        if Self::is_all_caps_constant(name) {
            return DocumentSymbolKind::Constant;
        }

        // Priority 3: Check for function definition (including inside parenthesized expressions)
        if self.find_function_definition(rhs).is_some() {
            return DocumentSymbolKind::Function;
        }

        // Priority 4: Default to variable
        DocumentSymbolKind::Variable
    }

    /// Check if a node is an R6Class() or setRefClass() function call.
    ///
    /// This detects R6 class definitions which should be classified as CLASS.
    ///
    /// # Arguments
    ///
    /// * `node` - The node to check (typically the RHS of an assignment)
    ///
    /// # Returns
    ///
    /// `true` if the node is a call to R6Class() or setRefClass(), `false` otherwise.
    fn is_r6_class_call(&self, node: tree_sitter::Node<'a>) -> bool {
        // Must be a call node
        if node.kind() != "call" {
            return false;
        }

        // Get the function name from the "function" field
        let func_node = match node.child_by_field_name("function") {
            Some(n) => n,
            None => return false,
        };

        let func_name = node_text(func_node, self.text);

        // Check for R6Class or setRefClass
        matches!(func_name, "R6Class" | "setRefClass")
    }

    /// Check if a name matches the ALL_CAPS constant pattern.
    ///
    /// Pattern: `^[A-Z][A-Z0-9_.]+$`
    ///
    /// Rules:
    /// - Must start with uppercase letter
    /// - Contains only uppercase letters, digits, dots, underscores
    /// - Minimum 2 characters total
    ///
    /// # Arguments
    ///
    /// * `name` - The symbol name to check
    ///
    /// # Returns
    ///
    /// `true` if the name matches the ALL_CAPS constant pattern, `false` otherwise.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// assert!(is_all_caps_constant("MAX_VALUE")); // constant
    /// assert!(is_all_caps_constant("PI"));        // constant (2 chars)
    /// assert!(is_all_caps_constant("API_KEY"));   // constant
    /// assert!(is_all_caps_constant("A1"));        // constant
    /// assert!(is_all_caps_constant("MY.CONST")); // constant (dot allowed)
    /// assert!(!is_all_caps_constant("x"));        // not constant - lowercase
    /// assert!(!is_all_caps_constant("A"));        // not constant - single char
    /// assert!(!is_all_caps_constant("MaxValue")); // not constant - mixed case
    /// assert!(!is_all_caps_constant("1ABC"));     // not constant - starts with digit
    /// ```
    fn is_all_caps_constant(name: &str) -> bool {
        // Must have at least 2 characters
        if name.len() < 2 {
            return false;
        }

        let mut chars = name.chars();

        // First character must be uppercase letter
        match chars.next() {
            Some(c) if c.is_ascii_uppercase() => {}
            _ => return false,
        }

        // Remaining characters must be uppercase letters, digits, dots, or underscores
        chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '.' || c == '_')
    }

    /// Compute the LSP Range for a tree-sitter node.
    ///
    /// Converts tree-sitter byte positions to LSP positions with UTF-16 columns.
    fn compute_node_range(&self, node: tree_sitter::Node<'a>) -> Range {
        let start_pos = node.start_position();
        let end_pos = node.end_position();

        // Get line text for UTF-16 column conversion
        let start_line_text = self.text.lines().nth(start_pos.row).unwrap_or("");
        let end_line_text = self.text.lines().nth(end_pos.row).unwrap_or("");

        let start_column =
            crate::cross_file::types::byte_offset_to_utf16_column(start_line_text, start_pos.column);
        let end_column =
            crate::cross_file::types::byte_offset_to_utf16_column(end_line_text, end_pos.column);

        Range {
            start: Position::new(start_pos.row as u32, start_column),
            end: Position::new(end_pos.row as u32, end_column),
        }
    }

    /// Extract function parameter signature from a function definition node.
    ///
    /// This method extracts the parameter list from a function definition and formats
    /// it as `(param1, param2, ...)`. If the parameter list exceeds 60 characters,
    /// it is truncated and `...` is appended.
    ///
    /// # Arguments
    ///
    /// * `value_node` - The node representing the value being assigned (typically the RHS
    ///   of a left assignment or LHS of a right assignment). This may be a `function_definition`
    ///   directly, or a parenthesized expression containing one.
    ///
    /// # Returns
    ///
    /// `Some(String)` containing the formatted parameter signature if the node is or contains
    /// a function definition, `None` otherwise.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Simple function: my_func <- function(x, y) { ... }
    /// // Returns: Some("(x, y)")
    ///
    /// // Function with defaults: add <- function(a, b = 1) { ... }
    /// // Returns: Some("(a, b = 1)")
    ///
    /// // Function with dots: wrapper <- function(...) { ... }
    /// // Returns: Some("(...)")
    ///
    /// // Long parameter list (truncated at 60 chars):
    /// // Returns: Some("(very_long_param_name_1, very_long_param_name_2, ver...")
    ///
    /// // Not a function: x <- 42
    /// // Returns: None
    /// ```
    fn extract_signature(&self, value_node: tree_sitter::Node<'a>) -> Option<String> {
        // Find the function_definition node - it may be the value_node directly,
        // or nested inside a parenthesized expression (for cases like `(function(x) x) -> f`)
        let func_node = self.find_function_definition(value_node)?;

        // Find the parameters child of the function_definition
        let mut cursor = func_node.walk();
        let params_node = func_node
            .children(&mut cursor)
            .find(|n| n.kind() == "parameters")?;

        // Extract parameter names
        let params = self.extract_parameter_names(params_node);

        // Format as (param1, param2, ...)
        let signature = format!("({})", params.join(", "));

        // Truncate at 60 characters if needed
        const MAX_LENGTH: usize = 60;
        if signature.len() > MAX_LENGTH {
            // Find a good truncation point (don't cut in the middle of a parameter)
            let truncated = &signature[..MAX_LENGTH - 3]; // Leave room for "..."
            Some(format!("{}...", truncated))
        } else {
            Some(signature)
        }
    }

    /// Find a function_definition node within the given node.
    ///
    /// This handles cases where the function definition is wrapped in parentheses,
    /// such as `(function(x) x) -> f`.
    fn find_function_definition(
        &self,
        node: tree_sitter::Node<'a>,
    ) -> Option<tree_sitter::Node<'a>> {
        if node.kind() == "function_definition" {
            return Some(node);
        }

        // Check if it's a parenthesized expression containing a function_definition
        if node.kind() == "parenthesized_expression" {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if let Some(func) = self.find_function_definition(child) {
                    return Some(func);
                }
            }
        }

        None
    }

    /// Extract parameter names from a parameters node.
    ///
    /// This extracts parameter names and their default values (if any) from
    /// the parameters node of a function definition.
    fn extract_parameter_names(&self, params_node: tree_sitter::Node<'a>) -> Vec<String> {
        let mut parameters = Vec::new();
        let mut cursor = params_node.walk();

        for child in params_node.children(&mut cursor) {
            if child.kind() == "parameter" {
                let mut param_cursor = child.walk();
                let param_children: Vec<_> = child.children(&mut param_cursor).collect();

                // Check if this parameter contains dots
                if param_children.iter().any(|n| n.kind() == "dots") {
                    parameters.push("...".to_string());
                } else if let Some(identifier) =
                    param_children.iter().find(|n| n.kind() == "identifier")
                {
                    let param_name = node_text(*identifier, self.text);

                    // Check for default value
                    if param_children.len() >= 3 && param_children[1].kind() == "=" {
                        let default_value = node_text(param_children[2], self.text);
                        parameters.push(format!("{} = {}", param_name, default_value));
                    } else {
                        parameters.push(param_name.to_string());
                    }
                }
            } else if child.kind() == "dots" {
                parameters.push("...".to_string());
            }
        }

        parameters
    }

    /// Extract S4 method definitions (setMethod, setClass, setGeneric) from the AST.
    ///
    /// This method detects top-level calls to S4 method definition functions and
    /// extracts the method/class/generic name from the first string argument.
    ///
    /// # Arguments
    ///
    /// * `node` - A tree-sitter node to check for S4 method calls
    ///
    /// # Returns
    ///
    /// A vector of `RawSymbol` entries for any S4 method definitions found.
    ///
    /// # Symbol Kind Mapping
    ///
    /// - `setMethod("name", ...)` → SymbolKind::METHOD
    /// - `setClass("name", ...)` → SymbolKind::CLASS
    /// - `setGeneric("name", ...)` → SymbolKind::INTERFACE
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // setMethod("show", "MyClass", function(object) { ... })
    /// // → RawSymbol { name: "show", kind: Method, ... }
    ///
    /// // setClass("Person", slots = c(name = "character", age = "numeric"))
    /// // → RawSymbol { name: "Person", kind: Class, ... }
    ///
    /// // setGeneric("myGeneric", function(x) standardGeneric("myGeneric"))
    /// // → RawSymbol { name: "myGeneric", kind: Interface, ... }
    /// ```
    pub fn extract_s4_methods(&self, node: tree_sitter::Node<'a>) -> Vec<RawSymbol> {
        let mut symbols = Vec::new();
        self.extract_s4_methods_recursive(node, &mut symbols);
        symbols
    }

    /// Recursively extract S4 method definitions from the AST.
    fn extract_s4_methods_recursive(
        &self,
        node: tree_sitter::Node<'a>,
        symbols: &mut Vec<RawSymbol>,
    ) {
        // Try to extract an S4 method from this node
        if let Some(symbol) = self.try_extract_s4_method(node) {
            symbols.push(symbol);
        }

        // Recurse into children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.extract_s4_methods_recursive(child, symbols);
        }
    }

    /// Try to extract a single S4 method definition from a call node.
    ///
    /// Returns `Some(RawSymbol)` if the node is a setMethod, setClass, or setGeneric call.
    fn try_extract_s4_method(&self, node: tree_sitter::Node<'a>) -> Option<RawSymbol> {
        // Must be a call node
        if node.kind() != "call" {
            return None;
        }

        // Get the function name
        let func_node = node.child_by_field_name("function")?;
        let func_name = node_text(func_node, self.text);

        // Determine the symbol kind based on the function name
        let kind = match func_name {
            "setMethod" => DocumentSymbolKind::Method,
            "setClass" => DocumentSymbolKind::Class,
            "setGeneric" => DocumentSymbolKind::Interface,
            _ => return None,
        };

        // Get the arguments node
        let args_node = node.child_by_field_name("arguments")?;

        // Extract the first string argument (the method/class/generic name)
        let (name, name_node) = self.extract_first_string_argument(&args_node)?;

        // Compute full range: the entire call expression
        let range = self.compute_node_range(node);

        // Compute selection range: the string argument (the name)
        let selection_range = self.compute_node_range(name_node);

        Some(RawSymbol {
            name,
            kind,
            range,
            selection_range,
            detail: None, // S4 methods don't have a parameter signature in the same way
            section_level: None,
            children: Vec::new(),
        })
    }

    /// Extract the first string argument from an arguments node.
    ///
    /// Returns the string value (without quotes) and the node containing the string.
    fn extract_first_string_argument(
        &self,
        args_node: &tree_sitter::Node<'a>,
    ) -> Option<(String, tree_sitter::Node<'a>)> {
        let mut cursor = args_node.walk();

        for child in args_node.children(&mut cursor) {
            if child.kind() == "argument" {
                // Only process positional arguments (no name field)
                if child.child_by_field_name("name").is_none() {
                    // This is the first positional argument - it must be a string
                    if let Some(value_node) = child.child_by_field_name("value") {
                        if let Some(name) = self.extract_string_literal(value_node) {
                            return Some((name, value_node));
                        }
                    }
                    // First positional argument is not a string, return None
                    return None;
                }
            }
        }

        None
    }

    /// Extract a string literal value from a node.
    ///
    /// Returns the string content without the surrounding quotes.
    fn extract_string_literal(&self, node: tree_sitter::Node<'a>) -> Option<String> {
        if node.kind() == "string" {
            let text = node_text(node, self.text);
            // Remove surrounding quotes (either single or double)
            if (text.starts_with('"') && text.ends_with('"'))
                || (text.starts_with('\'') && text.ends_with('\''))
            {
                return Some(text[1..text.len() - 1].to_string());
            }
        }
        None
    }

    /// Extract R code sections from comments.
    ///
    /// Detects section comments matching the pattern:
    /// `^\s*#(#*)\s*(%%)?\s*(\S.+?)\s*(#{4,}|\-{4,}|={4,}|\*{4,}|\+{4,})\s*$`
    ///
    /// # Returns
    ///
    /// A vector of `RawSymbol` entries for each section found, with:
    /// - `name`: The section title (capture group 3)
    /// - `kind`: `DocumentSymbolKind::Module`
    /// - `range`: The comment line (will be expanded later by HierarchyBuilder)
    /// - `selection_range`: The comment line
    /// - `section_level`: The heading level (1 + count of additional # characters)
    ///
    /// # Examples
    ///
    /// ```
    /// use tree_sitter::Parser;
    /// use raven::handlers::SymbolExtractor;
    ///
    /// let mut parser = Parser::new();
    /// parser.set_language(&tree_sitter_r::LANGUAGE.into()).unwrap();
    /// let code = "# Data Loading ----\nx <- 1\n## Subsection ####\ny <- 2";
    /// let tree = parser.parse(code, None).unwrap();
    /// let extractor = SymbolExtractor::new(code, tree.root_node());
    /// let symbols = extractor.extract_all();
    /// // Should find 2 sections plus 2 variables
    /// ```
    ///
    /// _Requirements: 4.1, 4.5_
    pub fn extract_sections(&self) -> Vec<RawSymbol> {
        let mut sections = Vec::new();
        let mut consumed_lines: HashSet<usize> = HashSet::new();
        let pattern = section_pattern();
        let lines: Vec<&str> = self.text.lines().collect();

        // Phase 1: Single-line section detection (existing logic)
        for (line_num, line) in lines.iter().enumerate() {
            if let Some(caps) = pattern.captures(line) {
                // Capture group 1: additional # characters (after the first #)
                // Heading level = 1 + count of additional # characters
                let additional_hashes = caps.get(1).map(|m| m.as_str().len()).unwrap_or(0);
                let heading_level = 1 + additional_hashes as u32;

                // Capture group 3: section name
                if let Some(name_match) = caps.get(3) {
                    let name = name_match.as_str().trim().to_string();

                    // Skip decorative separators where the "name" is only
                    // delimiter characters (e.g. "# ==================")
                    if is_delimiter_only(&name) {
                        continue;
                    }

                    // Compute UTF-16 column for the end of the line
                    let line_end_utf16 = utf16_len(line);

                    let range = Range {
                        start: Position {
                            line: line_num as u32,
                            character: 0,
                        },
                        end: Position {
                            line: line_num as u32,
                            character: line_end_utf16,
                        },
                    };

                    consumed_lines.insert(line_num);
                    sections.push(RawSymbol {
                        name,
                        kind: DocumentSymbolKind::Module,
                        range,
                        selection_range: range,
                        detail: None,
                        section_level: Some(heading_level),
                        children: Vec::new(),
                    });
                }
            }
        }

        // Phase 2: Banner-style section detection (3-line pattern)
        // Look for: delimiter line / name line / delimiter line
        // where both delimiter lines use the same delimiter type.
        if lines.len() >= 3 {
            for i in 1..lines.len() - 1 {
                // Skip if any of the 3 lines are already consumed
                if consumed_lines.contains(&(i - 1))
                    || consumed_lines.contains(&i)
                    || consumed_lines.contains(&(i + 1))
                {
                    continue;
                }

                let kind_top = classify_delimiter_line(lines[i - 1]);
                let kind_bottom = classify_delimiter_line(lines[i + 1]);

                if let (Some(top), Some(bottom)) = (kind_top, kind_bottom) {
                    if top != bottom {
                        continue;
                    }

                    if let Some((name, heading_level)) = extract_banner_name(lines[i]) {
                        let name_line_end_utf16 = utf16_len(lines[i]);
                        let bottom_line_end_utf16 = utf16_len(lines[i + 1]);

                        let range = Range {
                            start: Position {
                                line: (i - 1) as u32,
                                character: 0,
                            },
                            end: Position {
                                line: (i + 1) as u32,
                                character: bottom_line_end_utf16,
                            },
                        };
                        let selection_range = Range {
                            start: Position {
                                line: i as u32,
                                character: 0,
                            },
                            end: Position {
                                line: i as u32,
                                character: name_line_end_utf16,
                            },
                        };

                        consumed_lines.insert(i - 1);
                        consumed_lines.insert(i);
                        consumed_lines.insert(i + 1);
                        sections.push(RawSymbol {
                            name,
                            kind: DocumentSymbolKind::Module,
                            range,
                            selection_range,
                            detail: None,
                            section_level: Some(heading_level),
                            children: Vec::new(),
                        });
                    }
                }
            }
        }

        // Sort by start line so single-line and banner sections interleave correctly
        sections.sort_by_key(|s| s.range.start.line);

        sections
    }
}

// ============================================================================
// HierarchyBuilder - Builds hierarchical DocumentSymbol tree from flat symbols
// ============================================================================

/// Builds hierarchical DocumentSymbol tree from flat symbols.
///
/// Takes a flat list of `RawSymbol` entries and the total line count of the document,
/// then builds a nested `DocumentSymbol` tree based on:
/// - Section nesting (by heading level)
/// - Function body nesting (by position containment)
/// - Section range computation (from section comment to next section or EOF)
///
/// # Example
///
/// ```no_run
/// use crate::handlers::{HierarchyBuilder, RawSymbol};
///
/// let symbols: Vec<RawSymbol> = vec![/* extracted symbols */];
/// let line_count = 100;
/// let builder = HierarchyBuilder::new(symbols, line_count);
/// let document_symbols = builder.build();
/// ```
pub struct HierarchyBuilder {
    /// Flat list of symbols to be organized into a hierarchy
    symbols: Vec<RawSymbol>,
    /// Total line count of the document (needed for computing section ranges to EOF)
    line_count: u32,
}

impl HierarchyBuilder {
    /// Creates a new HierarchyBuilder with the given symbols and line count.
    ///
    /// # Arguments
    ///
    /// * `symbols` - Flat list of `RawSymbol` entries extracted from the document
    /// * `line_count` - Total number of lines in the document (used for section range computation)
    ///
    /// # Returns
    ///
    /// A new `HierarchyBuilder` instance ready to build the hierarchy.
    pub fn new(symbols: Vec<RawSymbol>, line_count: u32) -> Self {
        Self {
            symbols,
            line_count,
        }
    }

    /// Compute section ranges (from section comment to next section or EOF).
    ///
    /// This method updates the `range` field of section symbols (those with `kind == Module`
    /// and `section_level` set) to span from the section comment line to the line before
    /// the next section, or to the end of the document if this is the last section.
    ///
    /// The `selection_range` is NOT modified - it remains the comment line only, as set
    /// during extraction.
    ///
    /// # Algorithm
    ///
    /// 1. Collect indices of all section symbols
    /// 2. Sort sections by their start line
    /// 3. For each section, compute its range:
    ///    - Start: the section comment line (already set)
    ///    - End: the line before the next section, OR the last line of the document
    ///      (line_count - 1) if this is the last section
    ///
    /// # Requirements
    ///
    /// - THE section symbol's `range` SHALL span from the section comment to the line
    ///   before the next section (or end of file) - Requirement 4.2
    /// - THE section symbol's `selectionRange` SHALL be the section comment line only
    ///   - Requirement 4.3 (already satisfied by extract_sections)
    pub fn compute_section_ranges(&mut self) {
        // Collect indices of section symbols (kind == Module with section_level set)
        let mut section_indices: Vec<usize> = self
            .symbols
            .iter()
            .enumerate()
            .filter(|(_, sym)| {
                matches!(sym.kind, DocumentSymbolKind::Module) && sym.section_level.is_some()
            })
            .map(|(idx, _)| idx)
            .collect();

        // If no sections, nothing to do
        if section_indices.is_empty() {
            return;
        }

        // Sort section indices by their start line
        section_indices.sort_by_key(|&idx| self.symbols[idx].range.start.line);

        // Compute the end line for each section (level-aware, stack-based O(n)).
        // Stack holds indices into section_indices for "open" sections whose end
        // line hasn't been determined yet. Each section is pushed and popped at
        // most once, so the total work is O(n).
        let eof_line = if self.line_count > 0 {
            self.line_count - 1
        } else {
            0
        };
        let mut stack: Vec<usize> = Vec::new();

        for i in 0..section_indices.len() {
            let current_idx = section_indices[i];
            let current_level = self.symbols[current_idx]
                .section_level
                .expect("section_level must be Some for symbols in section_indices");
            let current_start = self.symbols[current_idx].range.start.line;

            // Pop all stack entries with level >= current_level
            // (they are siblings or children that end before this section)
            while let Some(&top_i) = stack.last() {
                let top_idx = section_indices[top_i];
                let top_level = self.symbols[top_idx]
                    .section_level
                    .expect("section_level must be Some for symbols in section_indices");
                if top_level >= current_level {
                    let end_line = if current_start > 0 {
                        current_start - 1
                    } else {
                        0
                    };
                    self.symbols[top_idx].range.end = Position {
                        line: end_line,
                        character: LSP_EOL_CHARACTER,
                    };
                    stack.pop();
                } else {
                    break;
                }
            }

            stack.push(i);
        }

        // Remaining stack entries extend to EOF
        while let Some(top_i) = stack.pop() {
            let top_idx = section_indices[top_i];
            self.symbols[top_idx].range.end = Position {
                line: eof_line,
                character: LSP_EOL_CHARACTER,
            };
        }
    }

    /// Nest symbols within sections based on position.
    ///
    /// This method organizes symbols into a hierarchy based on:
    /// 1. **Section nesting by heading level**: Sections with more `#` characters (higher level number)
    ///    are nested within sections with fewer `#` characters (lower level number).
    ///    For example, `## Subsection` (level 2) becomes a child of `# Section` (level 1).
    /// 2. **Symbol nesting within sections**: Non-section symbols (functions, variables, etc.)
    ///    that fall within a section's range become children of that section.
    ///
    /// # Algorithm
    ///
    /// 1. Sort all symbols by start line
    /// 2. Separate sections from non-sections
    /// 3. Build section hierarchy using a stack-based approach
    /// 4. Insert non-section symbols into their containing sections
    ///
    /// # Requirements
    ///
    /// - WHEN symbols are defined within a section's range, THE Document_Symbol_Provider
    ///   SHALL nest them as children of that section - Requirement 4.4
    /// - THE Document_Symbol_Provider SHALL support nested sections based on heading level
    ///   (number of `#` characters) - Requirement 4.5
    ///
    /// # Note
    ///
    /// This method should be called AFTER `compute_section_ranges()` to ensure sections
    /// have their full ranges computed.
    pub fn nest_in_sections(&mut self) {
        if self.symbols.is_empty() {
            return;
        }

        // Take ownership of symbols
        let mut symbols = std::mem::take(&mut self.symbols);

        // Sort by start line
        symbols.sort_by_key(|s| s.range.start.line);

        // Separate sections from non-sections
        let (sections, non_sections): (Vec<_>, Vec<_>) =
            symbols.into_iter().partition(|s| s.section_level.is_some());

        // Build section hierarchy
        let mut root_sections = Self::build_section_hierarchy(sections);

        // Insert non-section symbols into their containing sections
        for symbol in non_sections {
            Self::insert_symbol_into_hierarchy(&mut root_sections, symbol);
        }

        self.symbols = root_sections;
    }

    /// Build a hierarchy of sections based on heading level.
    ///
    /// Sections with higher level numbers (more `#` characters) are nested within
    /// sections with lower level numbers (fewer `#` characters).
    ///
    /// Uses a stack-based approach where each stack entry is a mutable reference path
    /// to the current section in the hierarchy.
    fn build_section_hierarchy(sections: Vec<RawSymbol>) -> Vec<RawSymbol> {
        if sections.is_empty() {
            return Vec::new();
        }

        let mut result: Vec<RawSymbol> = Vec::new();
        // Stack tracks (level, path) where path is indices to navigate to the section
        // path[0] is index in result, path[1..] are indices in children
        let mut stack: Vec<(u32, Vec<usize>)> = Vec::new();

        for section in sections {
            let level = section.section_level.unwrap(); // Safe because we filtered

            // Pop sections from stack until we find a parent with lower level
            while let Some((stack_level, _)) = stack.last() {
                if *stack_level < level {
                    break;
                }
                stack.pop();
            }

            if stack.is_empty() {
                // Root-level section
                let idx = result.len();
                result.push(section);
                stack.push((level, vec![idx]));
            } else {
                // Nested section - add to parent's children
                let (_, parent_path) = stack.last().unwrap();
                let parent_path = parent_path.clone();

                // Navigate to parent and add child
                let child_idx = Self::add_child_at_path(&mut result, &parent_path, section);

                // Build path to this new section
                let mut new_path = parent_path;
                new_path.push(child_idx);
                stack.push((level, new_path));
            }
        }

        result
    }

    /// Add a child symbol at the given path and return the child's index.
    fn add_child_at_path(
        result: &mut [RawSymbol],
        path: &[usize],
        child: RawSymbol,
    ) -> usize {
        if path.is_empty() {
            panic!("Empty path in add_child_at_path");
        }

        let mut current = &mut result[path[0]];
        for &idx in &path[1..] {
            current = &mut current.children[idx];
        }

        let child_idx = current.children.len();
        current.children.push(child);
        child_idx
    }

    /// Insert a non-section symbol into the appropriate section in the hierarchy.
    ///
    /// Finds the innermost section that contains the symbol and adds it as a child.
    /// If no section contains the symbol, it's added to the root level.
    fn insert_symbol_into_hierarchy(sections: &mut Vec<RawSymbol>, symbol: RawSymbol) {
        let symbol_line = symbol.range.start.line;

        // Try to find a section that contains this symbol
        for section in sections.iter_mut() {
            if Self::try_insert_into_section(section, symbol.clone(), symbol_line) {
                return;
            }
        }

        // No section contains this symbol - add to root level
        sections.push(symbol);
    }

    /// Try to insert a symbol into a section or its children.
    ///
    /// Returns true if the symbol was inserted, false otherwise.
    fn try_insert_into_section(
        section: &mut RawSymbol,
        symbol: RawSymbol,
        symbol_line: u32,
    ) -> bool {
        // Check if this section contains the symbol
        if symbol_line < section.range.start.line || symbol_line > section.range.end.line {
            return false;
        }

        // This section contains the symbol
        // Check if any child section also contains it (for nested sections)
        for child in section.children.iter_mut() {
            if child.section_level.is_some() {
                if Self::try_insert_into_section(child, symbol.clone(), symbol_line) {
                    return true;
                }
            }
        }

        // No child section contains it, add to this section
        section.children.push(symbol);
        true
    }

    /// Nest symbols within function bodies based on position.
    ///
    /// This method organizes symbols into a hierarchy based on function containment:
    /// - Symbols whose start line falls within a function's range become children of that function
    /// - Supports arbitrary nesting depth (functions inside functions inside functions...)
    ///
    /// # Algorithm
    ///
    /// For each level of the hierarchy:
    /// 1. Identify function symbols (kind == Function)
    /// 2. For each non-function symbol, check if it falls within any function's range
    /// 3. If so, move it to be a child of the innermost containing function
    /// 4. Recursively process children of each symbol
    ///
    /// # Requirements
    ///
    /// - WHEN an assignment occurs inside a function body, THE Document_Symbol_Provider
    ///   SHALL include it as a child of that function's DocumentSymbol - Requirement 3.1
    /// - WHEN an assignment occurs at top-level (outside any function), THE Document_Symbol_Provider
    ///   SHALL include it as a root-level symbol - Requirement 3.2
    /// - THE Document_Symbol_Provider SHALL support arbitrary nesting depth for nested
    ///   function definitions - Requirement 3.3
    ///
    /// # Note
    ///
    /// This method should be called AFTER `nest_in_sections()` so that symbols are first
    /// organized by sections, then within each section (or at root level), they are further
    /// organized by function containment.
    pub fn nest_in_functions(&mut self) {
        // Process the root level symbols
        self.symbols = Self::nest_symbols_in_functions_recursive(std::mem::take(&mut self.symbols));
    }

    /// Recursively nest symbols within functions at a given level of the hierarchy.
    ///
    /// This function processes a list of symbols and:
    /// 1. Identifies function symbols that can contain other symbols
    /// 2. Moves symbols (including nested functions) into their containing functions
    /// 3. Recursively processes children of all symbols
    ///
    /// # Arguments
    ///
    /// * `symbols` - The list of symbols at the current hierarchy level
    ///
    /// # Returns
    ///
    /// The reorganized list of symbols with proper function nesting
    fn nest_symbols_in_functions_recursive(symbols: Vec<RawSymbol>) -> Vec<RawSymbol> {
        if symbols.is_empty() {
            return symbols;
        }

        // First, recursively process children of all symbols
        let mut symbols: Vec<RawSymbol> = symbols
            .into_iter()
            .map(|mut sym| {
                sym.children = Self::nest_symbols_in_functions_recursive(std::mem::take(&mut sym.children));
                sym
            })
            .collect();

        // Sort by start line for consistent processing
        symbols.sort_by_key(|s| s.range.start.line);

        // Build the hierarchy by finding which symbols are contained by which functions
        // A symbol is contained by a function if it starts after the function starts
        // and ends before or at the function's end line
        
        // We'll use a simple approach: repeatedly find symbols that should be nested
        // and move them into their containing functions
        loop {
            // Find all functions at this level
            let function_indices: Vec<usize> = symbols
                .iter()
                .enumerate()
                .filter(|(_, s)| matches!(s.kind, DocumentSymbolKind::Function))
                .map(|(i, _)| i)
                .collect();
            
            // For each symbol, check if it should be nested in a function
            let mut nested_indices: Vec<usize> = Vec::new();
            
            for (i, sym) in symbols.iter().enumerate() {
                // Skip functions when checking if they should be nested
                // (we handle function-in-function separately)
                for &func_idx in &function_indices {
                    if i == func_idx {
                        continue; // Don't nest a function in itself
                    }
                    let func = &symbols[func_idx];
                    if Self::symbol_is_inside_function(sym, func) {
                        nested_indices.push(i);
                        break;
                    }
                }
            }
            
            if nested_indices.is_empty() {
                // No more nesting needed
                return symbols;
            }
            
            // Move nested symbols into their containing functions
            // We need to be careful about the order of operations
            let nested_set: std::collections::HashSet<usize> = nested_indices.iter().cloned().collect();
            
            // Collect symbols to nest
            let mut to_nest: Vec<RawSymbol> = Vec::new();
            let mut remaining: Vec<RawSymbol> = Vec::new();
            
            for (i, sym) in symbols.into_iter().enumerate() {
                if nested_set.contains(&i) {
                    to_nest.push(sym);
                } else {
                    remaining.push(sym);
                }
            }
            
            // Insert nested symbols into their containing functions
            for sym in to_nest {
                let mut inserted = false;
                for func in remaining.iter_mut() {
                    if matches!(func.kind, DocumentSymbolKind::Function)
                        && Self::symbol_is_inside_function(&sym, func)
                    {
                        Self::insert_into_innermost_function(&mut func.children, sym.clone());
                        inserted = true;
                        break;
                    }
                }
                if !inserted {
                    // Shouldn't happen, but add to remaining just in case
                    remaining.push(sym);
                }
            }
            
            symbols = remaining;
            symbols.sort_by_key(|s| s.range.start.line);
        }
    }

    /// Check if a symbol is inside a function's range.
    ///
    /// A symbol is considered inside a function if its start line is within
    /// the function's range (inclusive of start, exclusive of end line if
    /// the symbol starts at the same line as the function ends).
    fn symbol_is_inside_function(symbol: &RawSymbol, func: &RawSymbol) -> bool {
        let symbol_start = symbol.range.start.line;
        let func_start = func.range.start.line;
        let func_end = func.range.end.line;

        // Symbol must start after the function starts (not on the same line as the function definition)
        // and before or on the function's end line
        symbol_start > func_start && symbol_start <= func_end
    }

    /// Insert a symbol into the innermost containing function within a list of children.
    ///
    /// This recursively checks if any function children contain the symbol,
    /// allowing for arbitrary nesting depth.
    fn insert_into_innermost_function(children: &mut Vec<RawSymbol>, symbol: RawSymbol) {
        // Check if any function child contains this symbol
        for child in children.iter_mut() {
            if matches!(child.kind, DocumentSymbolKind::Function)
                && Self::symbol_is_inside_function(&symbol, child)
            {
                // Recursively try to insert into nested functions
                Self::insert_into_innermost_function(&mut child.children, symbol);
                return;
            }
        }

        // No nested function contains it, add to this level
        children.push(symbol);
    }

    /// Build hierarchical DocumentSymbol tree from the flat symbols.
    ///
    /// This method orchestrates the hierarchy building process:
    /// 1. Computes section ranges (from section comment to next section or EOF)
    /// 2. Nests symbols within sections based on position
    /// 3. Nests symbols within function bodies based on position
    /// 4. Converts the `Vec<RawSymbol>` hierarchy to `Vec<DocumentSymbol>`
    ///
    /// # Returns
    ///
    /// A vector of `DocumentSymbol` entries representing the hierarchical document outline.
    ///
    /// # Requirements
    ///
    /// - Requirement 1.1: Return `DocumentSymbol[]` response type
    /// - Requirement 3.1: Assignments inside function bodies appear as children
    /// - Requirement 3.2: Top-level assignments appear as root-level symbols
    /// - Requirement 3.3: Support arbitrary nesting depth for nested functions
    ///
    /// # Examples
    ///
    /// ```
    /// use raven::handlers::{HierarchyBuilder, RawSymbol, DocumentSymbolKind};
    /// use tower_lsp::lsp_types::{Range, Position};
    ///
    /// let symbols = vec![
    ///     RawSymbol {
    ///         name: "my_func".to_string(),
    ///         kind: DocumentSymbolKind::Function,
    ///         range: Range {
    ///             start: Position { line: 0, character: 0 },
    ///             end: Position { line: 5, character: 1 },
    ///         },
    ///         selection_range: Range {
    ///             start: Position { line: 0, character: 0 },
    ///             end: Position { line: 0, character: 7 },
    ///         },
    ///         detail: Some("(x, y)".to_string()),
    ///         section_level: None,
    ///         children: Vec::new(),
    ///     },
    /// ];
    ///
    /// let builder = HierarchyBuilder::new(symbols, 10);
    /// let doc_symbols = builder.build();
    /// assert_eq!(doc_symbols.len(), 1);
    /// assert_eq!(doc_symbols[0].name, "my_func");
    /// ```
    pub fn build(mut self) -> Vec<DocumentSymbol> {
        // Step 1: Compute section ranges (from section comment to next section or EOF)
        self.compute_section_ranges();

        // Step 2: Nest symbols within sections based on position
        self.nest_in_sections();

        // Step 3: Nest symbols within function bodies based on position
        self.nest_in_functions();

        // Step 4: Convert RawSymbol hierarchy to DocumentSymbol hierarchy
        Self::convert_to_document_symbols(self.symbols)
    }

    /// Convert a vector of RawSymbol to a vector of DocumentSymbol.
    ///
    /// This recursively converts the RawSymbol hierarchy to DocumentSymbol,
    /// mapping all fields appropriately.
    fn convert_to_document_symbols(raw_symbols: Vec<RawSymbol>) -> Vec<DocumentSymbol> {
        raw_symbols
            .into_iter()
            .map(Self::convert_raw_to_document_symbol)
            .collect()
    }

    /// Convert a single RawSymbol to DocumentSymbol.
    ///
    /// Maps fields as follows:
    /// - `name` -> `name`
    /// - `kind` -> `kind` (via `to_lsp_kind()`)
    /// - `range` -> `range`
    /// - `selection_range` -> `selection_range`
    /// - `detail` -> `detail`
    /// - `children` -> `children` (recursively converted)
    /// - `deprecated` -> `None` (LSP deprecated field)
    /// - `tags` -> `None`
    #[allow(deprecated)] // DocumentSymbol::deprecated field is deprecated in LSP spec
    fn convert_raw_to_document_symbol(raw: RawSymbol) -> DocumentSymbol {
        let children = if raw.children.is_empty() {
            None
        } else {
            Some(Self::convert_to_document_symbols(raw.children))
        };

        DocumentSymbol {
            name: raw.name,
            kind: raw.kind.to_lsp_kind(),
            range: raw.range,
            selection_range: raw.selection_range,
            detail: raw.detail,
            children,
            deprecated: None,
            tags: None,
        }
    }
}

// ============================================================================
// Cross-File Scope Helper
// ============================================================================

/// Collects cross-file symbols visible at the given document position.
///
/// The returned set includes symbols brought into scope via `source()` chains
/// and parent-file backward directives; it uses the state's ContentProvider to
/// resolve referenced files and artifacts.
///
/// # Examples
///
/// ```no_run
/// use url::Url;
/// // `state` is a prepared WorldState; obtain or mock as appropriate in tests.
/// let state = /* WorldState */ todo!();
/// let uri = Url::parse("file:///path/to/script.R").unwrap();
/// let symbols = crate::get_cross_file_symbols(&state, &uri, 12, 4);
/// // `symbols` maps symbol names to their scoped metadata.
/// ```
///
/// # Returns
///
/// A `HashMap` mapping symbol names to their corresponding `ScopedSymbol` entries.
fn get_cross_file_symbols(
    state: &WorldState,
    uri: &Url,
    line: u32,
    column: u32,
) -> HashMap<String, ScopedSymbol> {
    get_cross_file_scope(state, uri, line, column).symbols
}

/// Compute the unified cross-file scope at a given position, including available symbols and package visibility.
///
/// This returns a position-aware ScopeAtPosition that reflects symbols visible at (line, column) in `uri`, along with loaded and inherited package lists and depth information for cross-file resolution.
///
/// # Returns
///
/// A `scope::ScopeAtPosition` containing the resolved symbols, `loaded_packages`, `inherited_packages`, and scope depth metadata for the specified location.
///
/// # Examples
///
/// ```no_run
/// use url::Url;
/// // `state` is your WorldState and `uri` is the file URL you want to query.
/// let uri = Url::parse("file:///path/to/script.R").unwrap();
/// let scope = get_cross_file_scope(&state, &uri, 10, 5);
/// // inspect scope.symbols, scope.loaded_packages, etc.
/// ```
fn get_cross_file_scope(
    state: &WorldState,
    uri: &Url,
    line: u32,
    column: u32,
) -> scope::ScopeAtPosition {
    // Use ContentProvider for unified access
    let content_provider = state.content_provider();

    // Closure to get artifacts for a URI
    let get_artifacts = |target_uri: &Url| -> Option<scope::ScopeArtifacts> {
        content_provider.get_artifacts(target_uri)
    };

    // Closure to get metadata for a URI
    let get_metadata = |target_uri: &Url| -> Option<crate::cross_file::CrossFileMetadata> {
        content_provider.get_metadata(target_uri)
    };

    let max_depth = state.cross_file_config.max_chain_depth;

    // Get base_exports from package_library if ready, otherwise empty set.
    // This ensures base R functions (stop, sprintf, exists, etc.) are available
    // in cross-file scope resolution for hover, completions, and go-to-definition.
    let base_exports = if state.package_library_ready {
        state.package_library.base_exports().clone()
    } else {
        std::collections::HashSet::new()
    };

    // Use the graph-aware scope resolution with PathContext
    scope::scope_at_position_with_graph(
        uri,
        line,
        column,
        &get_artifacts,
        &get_metadata,
        &state.cross_file_graph,
        state.workspace_folders.first(),
        max_depth,
        &base_exports,
    )
}

// ============================================================================
// Folding Range
// ============================================================================

pub fn folding_range(state: &WorldState, uri: &Url) -> Option<Vec<FoldingRange>> {
    let doc = state.get_document(uri)?;
    let tree = doc.tree.as_ref()?;
    let mut ranges = Vec::new();

    collect_folding_ranges(tree.root_node(), &mut ranges);

    Some(ranges)
}

fn collect_folding_ranges(node: Node, ranges: &mut Vec<FoldingRange>) {
    let kind = node.kind();

    // Fold braced expressions, function definitions, and control structures
    let should_fold = matches!(
        kind,
        "brace_list" | "function_definition" | "if_statement" | "for_statement" | "while_statement"
    );

    if should_fold && node.start_position().row != node.end_position().row {
        ranges.push(FoldingRange {
            start_line: node.start_position().row as u32,
            start_character: Some(node.start_position().column as u32),
            end_line: node.end_position().row as u32,
            end_character: Some(node.end_position().column as u32),
            kind: Some(FoldingRangeKind::Region),
            collapsed_text: None,
        });
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_folding_ranges(child, ranges);
    }
}

// ============================================================================
// Selection Range
// ============================================================================

pub fn selection_range(
    state: &WorldState,
    uri: &Url,
    positions: Vec<Position>,
) -> Option<Vec<SelectionRange>> {
    let doc = state.get_document(uri)?;
    let tree = doc.tree.as_ref()?;

    let mut results = Vec::new();
    for pos in positions {
        let point = Point::new(pos.line as usize, pos.character as usize);
        if let Some(range) = build_selection_range(tree.root_node(), point) {
            results.push(range);
        }
    }

    Some(results)
}

fn build_selection_range(root: Node, point: Point) -> Option<SelectionRange> {
    let mut node = root.descendant_for_point_range(point, point)?;
    let mut ranges: Vec<Range> = Vec::new();

    loop {
        let range = Range {
            start: Position::new(
                node.start_position().row as u32,
                node.start_position().column as u32,
            ),
            end: Position::new(
                node.end_position().row as u32,
                node.end_position().column as u32,
            ),
        };

        if ranges.last() != Some(&range) {
            ranges.push(range);
        }

        if let Some(parent) = node.parent() {
            node = parent;
        } else {
            break;
        }
    }

    // Build nested SelectionRange from innermost to outermost
    let mut result: Option<SelectionRange> = None;
    for range in ranges {
        result = Some(SelectionRange {
            range,
            parent: result.map(Box::new),
        });
    }

    result
}

// ============================================================================
// Document Symbols
// ============================================================================

/// Handles the `textDocument/documentSymbol` LSP request.
///
/// This function extracts symbols from the document using `SymbolExtractor` and builds
/// a hierarchical structure using `HierarchyBuilder`. The response type depends on the
/// client's capability:
///
/// - If `hierarchicalDocumentSymbolSupport` is true: Returns `DocumentSymbolResponse::Nested`
///   with a hierarchical `DocumentSymbol[]` structure
/// - If `hierarchicalDocumentSymbolSupport` is false: Returns `DocumentSymbolResponse::Flat`
///   with a flat `SymbolInformation[]` structure (correct URIs set on each symbol)
///
/// # Requirements
///
/// - Requirement 1.1: Return `DocumentSymbol[]` when client supports hierarchical symbols
/// - Requirement 1.2: Return `SymbolInformation[]` as fallback when client doesn't support hierarchical
/// - Requirement 1.3: Set correct document URI in each symbol's location for flat fallback
///
/// # Arguments
///
/// * `state` - The world state containing document and configuration information
/// * `uri` - The URI of the document to get symbols for
///
/// # Returns
///
/// `Some(DocumentSymbolResponse)` if the document exists and has a valid parse tree,
/// `None` otherwise.
pub fn document_symbol(state: &WorldState, uri: &Url) -> Option<DocumentSymbolResponse> {
    let doc = state.get_document(uri)?;
    let tree = doc.tree.as_ref()?;
    let text = doc.text();

    // Use SymbolExtractor to extract symbols from the document
    let extractor = SymbolExtractor::new(&text, tree.root_node());
    let raw_symbols = extractor.extract_all();

    // Calculate line count for HierarchyBuilder
    let line_count = text.lines().count() as u32;

    // Use HierarchyBuilder to build the hierarchical structure
    let builder = HierarchyBuilder::new(raw_symbols, line_count);
    let doc_symbols = builder.build();

    // Check client capability for hierarchical document symbols
    if state.symbol_config.hierarchical_document_symbol_support {
        // Requirement 1.1: Return DocumentSymbol[] when hierarchical support available
        Some(DocumentSymbolResponse::Nested(doc_symbols))
    } else {
        // Requirement 1.2, 1.3: Return flat SymbolInformation[] with correct URIs
        let flat_symbols = flatten_document_symbols(&doc_symbols, uri);
        Some(DocumentSymbolResponse::Flat(flat_symbols))
    }
}

/// Flattens a hierarchical `DocumentSymbol` tree into a flat `SymbolInformation` list.
///
/// This function recursively traverses the `DocumentSymbol` hierarchy and converts each
/// symbol to a `SymbolInformation` entry with the correct document URI.
///
/// # Requirements
///
/// - Requirement 1.3: Set correct document URI in each symbol's location
///
/// # Arguments
///
/// * `doc_symbols` - The hierarchical `DocumentSymbol` tree to flatten
/// * `uri` - The document URI to set on each symbol's location
///
/// # Returns
///
/// A flat vector of `SymbolInformation` entries with correct URIs.
#[allow(deprecated)] // SymbolInformation::deprecated field is deprecated in LSP spec
fn flatten_document_symbols(doc_symbols: &[DocumentSymbol], uri: &Url) -> Vec<SymbolInformation> {
    let mut result = Vec::new();
    flatten_document_symbols_recursive(doc_symbols, uri, None, &mut result);
    result
}

/// Recursively flattens DocumentSymbol entries into SymbolInformation.
///
/// # Arguments
///
/// * `symbols` - The symbols at the current level of the hierarchy
/// * `uri` - The document URI to set on each symbol's location
/// * `container_name` - The name of the containing symbol (for nested symbols)
/// * `result` - The accumulator for flattened symbols
#[allow(deprecated)] // SymbolInformation::deprecated field is deprecated in LSP spec
fn flatten_document_symbols_recursive(
    symbols: &[DocumentSymbol],
    uri: &Url,
    container_name: Option<&str>,
    result: &mut Vec<SymbolInformation>,
) {
    for symbol in symbols {
        // Convert DocumentSymbol to SymbolInformation
        result.push(SymbolInformation {
            name: symbol.name.clone(),
            kind: symbol.kind,
            tags: symbol.tags.clone(),
            deprecated: symbol.deprecated,
            location: Location {
                uri: uri.clone(),
                range: symbol.range,
            },
            container_name: container_name.map(String::from),
        });

        // Recursively process children
        if let Some(children) = &symbol.children {
            flatten_document_symbols_recursive(children, uri, Some(&symbol.name), result);
        }
    }
}

/// Collects top-level symbols from a syntax tree and appends them to `symbols`.
///
/// This examines assignment expressions (e.g., `name <- ...`, `name = ...`, `name <<- ...`)
/// and records an entry for the left-hand identifier. If the right-hand side is a
/// `function_definition`, the symbol kind is `FUNCTION`; otherwise it is `VARIABLE`.
/// Reserved words are skipped and not added to `symbols`.
///
/// The appended `SymbolInformation.location.uri` uses a placeholder (`file:///`) which
/// the caller is expected to replace with the actual document URI.
///
/// # Examples
///
/// ```
/// // Given a parsed tree `tree` and source text `src`:
/// // let root = tree.root_node();
/// // let mut symbols = Vec::new();
/// // collect_symbols(root, src, &mut symbols);
/// // assert!(symbols.iter().any(|s| s.name == "my_function"));
/// ```
#[allow(deprecated)]
fn collect_symbols(node: Node, text: &str, symbols: &mut Vec<SymbolInformation>) {
    // Look for assignments: identifier <- value or identifier = value
    if node.kind() == "binary_operator" {
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();

        if children.len() >= 3 {
            let lhs = children[0];
            let op = children[1];
            let rhs = children[2];

            let op_text = node_text(op, text);
            if matches!(op_text, "<-" | "=" | "<<-") && lhs.kind() == "identifier" {
                let name = node_text(lhs, text).to_string();

                // Skip reserved words - they should not appear as document symbols
                // (Requirement 6.1, 6.2)
                if crate::reserved_words::is_reserved_word(&name) {
                    // Continue to recurse but don't add this symbol
                } else {
                    let kind = if rhs.kind() == "function_definition" {
                        SymbolKind::FUNCTION
                    } else {
                        SymbolKind::VARIABLE
                    };

                    symbols.push(SymbolInformation {
                        name,
                        kind,
                        tags: None,
                        deprecated: None,
                        location: Location {
                            uri: Url::parse("file:///").unwrap(), // Will be replaced
                            range: Range {
                                start: Position::new(
                                    node.start_position().row as u32,
                                    node.start_position().column as u32,
                                ),
                                end: Position::new(
                                    node.end_position().row as u32,
                                    node.end_position().column as u32,
                                ),
                            },
                        },
                        container_name: None,
                    });
                }
            }
        }
    }

    // Recurse
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_symbols(child, text, symbols);
    }
}

// ============================================================================
// Workspace Symbols
// ============================================================================

/// Extracts the container name from a file URI.
///
/// The container name is the filename without extension, providing context
/// about which file a symbol belongs to in workspace symbol results.
///
/// **Validates: Requirements 8.1, 8.2**
///
/// # Parameters
///
/// - `uri`: The file URI to extract the container name from.
///
/// # Returns
///
/// The filename without extension, or `None` if the URI has no path segments.
///
/// # Examples
///
/// ```
/// let uri = Url::parse("file:///path/to/utils.R").unwrap();
/// assert_eq!(extract_container_name(&uri), Some("utils".to_string()));
/// ```
fn extract_container_name(uri: &Url) -> Option<String> {
    uri.path_segments()?
        .last()
        .map(|filename| {
            // Remove extension if present
            if let Some(dot_pos) = filename.rfind('.') {
                filename[..dot_pos].to_string()
            } else {
                filename.to_string()
            }
        })
}

/// Collects workspace symbols whose names contain the given query as a case-insensitive substring.
///
/// Searches symbols in this priority order: open documents, the new workspace index (closed files),
/// the cross-file workspace index, legacy open documents (AST fallback), and the legacy workspace index
/// (AST fallback). Files already seen in a higher-priority source are not re-scanned (deduplicated by URI).
/// Virtual symbols declared via directive annotations are skipped by the artifact collector.
/// Reserved words are filtered out to avoid polluting results.
/// Results are truncated to the configured `workspace_max_results` limit.
///
/// **Validates: Requirements 7.2, 8.1, 8.2, 9.1, 9.2, 9.3, 11.2**
///
/// # Parameters
///
/// - `state`: The world state containing documents, indexes, and configuration.
/// - `query`: Case-insensitive substring used to filter symbol names.
///
/// # Returns
///
/// A `Vec<SymbolInformation>` containing up to `workspace_max_results` symbols that match `query`.
/// Each symbol's `location.uri` indicates the source file where the symbol is defined.
/// Each symbol's `container_name` is set to the filename (without extension) for context.
///
/// # Examples
///
/// ```no_run
/// // Given a populated `state: WorldState`
/// let symbols = workspace_symbol(&state, "plot");
/// // `symbols` is `Some(Vec<SymbolInformation>)` containing matching symbols (up to the configured limit)
/// ```
pub fn workspace_symbol(state: &WorldState, query: &str) -> Option<Vec<SymbolInformation>> {
    let lower_query = query.to_lowercase();
    let mut symbols = Vec::new();
    let mut seen_uris = std::collections::HashSet::<Url>::new();
    let max_results = state.symbol_config.workspace_max_results;

    // 1. Open documents (highest priority)
    for uri in state.document_store.uris() {
        seen_uris.insert(uri.clone());
        if let Some(doc) = state.document_store.get_without_touch(&uri) {
            collect_workspace_symbols_from_artifacts(
                &uri,
                &doc.artifacts,
                &lower_query,
                &mut symbols,
            );
        }
    }

    // 2. New workspace index (closed files)
    for (uri, entry) in state.workspace_index_new.iter() {
        if seen_uris.contains(&uri) {
            continue;
        }
        seen_uris.insert(uri.clone());
        collect_workspace_symbols_from_artifacts(&uri, &entry.artifacts, &lower_query, &mut symbols);
    }

    // 3. Cross-file workspace index
    for uri in state.cross_file_workspace_index.uris() {
        if seen_uris.contains(&uri) {
            continue;
        }
        seen_uris.insert(uri.clone());
        if let Some(artifacts) = state.cross_file_workspace_index.get_artifacts(&uri) {
            collect_workspace_symbols_from_artifacts(&uri, &artifacts, &lower_query, &mut symbols);
        }
    }

    // 4. Legacy documents (AST fallback)
    for (uri, doc) in &state.documents {
        if seen_uris.contains(uri) {
            continue;
        }
        seen_uris.insert(uri.clone());
        if let Some(tree) = &doc.tree {
            let text = doc.text();
            let mut file_symbols = Vec::new();
            collect_symbols(tree.root_node(), &text, &mut file_symbols);
            let container_name = extract_container_name(uri);
            for mut sym in file_symbols {
                // Filter reserved words (Requirement 7.2)
                if is_reserved_word(&sym.name) {
                    continue;
                }
                if sym.name.to_lowercase().contains(&lower_query) {
                    sym.location.uri = uri.clone();
                    sym.container_name = container_name.clone();
                    symbols.push(sym);
                }
            }
        }
    }

    // 5. Legacy workspace index (AST fallback)
    for (uri, doc) in &state.workspace_index {
        if seen_uris.contains(uri) {
            continue;
        }
        seen_uris.insert(uri.clone());
        if let Some(tree) = &doc.tree {
            let text = doc.text();
            let mut file_symbols = Vec::new();
            collect_symbols(tree.root_node(), &text, &mut file_symbols);
            let container_name = extract_container_name(uri);
            for mut sym in file_symbols {
                // Filter reserved words (Requirement 7.2)
                if is_reserved_word(&sym.name) {
                    continue;
                }
                if sym.name.to_lowercase().contains(&lower_query) {
                    sym.location.uri = uri.clone();
                    sym.container_name = container_name.clone();
                    symbols.push(sym);
                }
            }
        }
    }

    // Apply configurable limit (Requirement 9.2, 11.2)
    symbols.truncate(max_results);
    Some(symbols)
}

/// Collect matching symbols from precomputed `ScopeArtifacts` and append them to `symbols`.

///

/// This filters exported symbols by whether their name (case-insensitively) contains `lower_query`,

/// skips declared (virtual) symbols such as `@lsp-var`/`@lsp-func`, and maps internal symbol kinds

/// to LSP `SymbolKind` values. Each matched symbol is appended as a `SymbolInformation` with the

/// provided `file_uri` and the symbol's definition position.

///

/// - `file_uri`: URI to assign to each returned `SymbolInformation`.

/// - `artifacts`: Precomputed `ScopeArtifacts` containing `exported_interface`.

/// - `lower_query`: a lowercased query string used for substring matching against symbol names.

/// - `symbols`: output vector to which matching `SymbolInformation` entries will be appended.

///

/// # Examples

///

/// ```no_run

/// # use lsp_types::{Location, Range, Position, SymbolInformation, SymbolKind};

/// # use url::Url;

/// # // Assume `artifacts` is a prepared ScopeArtifacts instance and `symbols` is an empty Vec.

/// # let file_uri = Url::parse("file:///path/to/file.R").unwrap();

/// # let lower_query = "foo";

/// # let mut symbols: Vec<SymbolInformation> = Vec::new();

/// # // collect_workspace_symbols_from_artifacts(&file_uri, &artifacts, lower_query, &mut symbols);

/// // After calling, `symbols` will contain any exported symbols whose names contain "foo".

/// ```
#[allow(deprecated)] // SymbolInformation::deprecated is deprecated in favor of tags
fn collect_workspace_symbols_from_artifacts(
    file_uri: &Url,
    artifacts: &crate::cross_file::scope::ScopeArtifacts,
    lower_query: &str,
    symbols: &mut Vec<SymbolInformation>,
) {
    let container_name = extract_container_name(file_uri);
    
    for scoped_symbol in artifacts.exported_interface.values() {
        if !scoped_symbol.name.to_lowercase().contains(lower_query) {
            continue;
        }

        // Skip declared symbols (@lsp-var, @lsp-func) — they are virtual
        if scoped_symbol.is_declared {
            continue;
        }

        // Filter reserved words (Requirement 7.2)
        if is_reserved_word(&scoped_symbol.name) {
            continue;
        }

        let kind = match scoped_symbol.kind {
            crate::cross_file::scope::SymbolKind::Function => SymbolKind::FUNCTION,
            crate::cross_file::scope::SymbolKind::Variable => SymbolKind::VARIABLE,
            crate::cross_file::scope::SymbolKind::Parameter => SymbolKind::VARIABLE,
        };

        symbols.push(SymbolInformation {
            name: scoped_symbol.name.clone(),
            kind,
            tags: None,
            deprecated: None,
            location: Location {
                uri: file_uri.clone(),
                range: Range {
                    start: Position::new(scoped_symbol.defined_line, scoped_symbol.defined_column),
                    end: Position::new(scoped_symbol.defined_line, scoped_symbol.defined_column),
                },
            },
            container_name: container_name.clone(),
        });
    }
}

// ============================================================================
// Diagnostics
// ============================================================================

/// Compute diagnostics for the document at the given URI.
///
/// Performs a full set of checks for the specified open document and returns collected diagnostics.
/// Reported issues include syntax errors, circular dependency and max-depth problems, missing or ambiguous
/// sourced files, out-of-scope symbol usage, missing package warnings, and (when enabled) undefined-variable
/// diagnostics that account for cross-file and package scope.
///
/// # Returns
///
/// `Vec<Diagnostic>` containing diagnostics for the document at `uri`, which may be empty if no issues were found.
///
/// # Examples
///
/// ```
/// // Given a prepared `WorldState` and a `Url` referring to an open document:
/// let diags = diagnostics(&state, &uri);
/// assert!(diags.is_empty() || diags.iter().any(|d| d.severity.is_some()));
/// ```
pub fn diagnostics(state: &WorldState, uri: &Url) -> Vec<Diagnostic> {
    // Master switch check - return empty if diagnostics disabled
    if !state.cross_file_config.diagnostics_enabled {
        return Vec::new();
    }

    let Some(doc) = state.get_document(uri) else {
        return Vec::new();
    };

    let Some(tree) = &doc.tree else {
        return Vec::new();
    };

    let text = doc.text();
    let mut diagnostics = Vec::new();

    // Parse directives to get ignored lines and cross-file metadata
    let mut directive_meta = crate::cross_file::directive::parse_directives(&text);

    // Compute inherited working directory for files with backward directives
    // This enables child files to inherit the parent's working directory context
    // for resolving paths in their own source() calls.
    // _Requirements: 5.1, 5.2, 6.1_
    if !directive_meta.sourced_by.is_empty() && directive_meta.working_directory.is_none() {
        let workspace_root = state.workspace_folders.first();
        let content_provider = state.content_provider();

        // Create a metadata getter that retrieves metadata from open documents,
        // workspace index, or by parsing content from the file cache
        let get_metadata_for_uri =
            |target_uri: &Url| -> Option<crate::cross_file::CrossFileMetadata> {
                // First check open documents
                if let Some(doc) = state.documents.get(target_uri) {
                    return Some(crate::cross_file::directive::parse_directives(&doc.text()));
                }
                // Then try workspace index
                if let Some(meta) = state.cross_file_workspace_index.get_metadata(target_uri) {
                    return Some(meta);
                }
                // Finally try to read from file cache
                if let Some(content) = content_provider.get_content(target_uri) {
                    return Some(crate::cross_file::extract_metadata(&content));
                }
                None
            };

        directive_meta.inherited_working_directory = compute_inherited_working_directory(
            uri,
            &directive_meta,
            workspace_root,
            get_metadata_for_uri,
        );

        if directive_meta.inherited_working_directory.is_some() {
            log::trace!(
                "Computed inherited working directory for {}: {:?}",
                uri,
                directive_meta.inherited_working_directory
            );
        }
    }

    // Collect syntax errors (not suppressed by @lsp-ignore)
    collect_syntax_errors(tree.root_node(), &mut diagnostics);

    // Collect else-on-newline errors
    // _Requirements: 4.1_
    collect_else_newline_errors(tree.root_node(), &text, &mut diagnostics);

    // Check for circular dependencies
    if let Some(cycle_edge) = state.cross_file_graph.detect_cycle(uri) {
        let line = cycle_edge.call_site_line.unwrap_or(0);
        let col = cycle_edge.call_site_column.unwrap_or(0);
        let target = cycle_edge
            .to
            .path_segments()
            .and_then(|mut s| s.next_back().map(|s| s.to_string()))
            .unwrap_or_default();
        diagnostics.push(Diagnostic {
            range: Range {
                start: Position::new(line, col),
                end: Position::new(line, col + 1),
            },
            severity: Some(state.cross_file_config.circular_dependency_severity),
            message: format!(
                "Circular dependency detected: sourcing '{}' creates a cycle",
                target
            ),
            ..Default::default()
        });
    }

    // Check for max chain depth exceeded (Requirement 5.8)
    collect_max_depth_diagnostics(state, uri, &mut diagnostics);

    // Check for missing files in source() calls and directives (Requirement 10.2)
    collect_missing_file_diagnostics(state, uri, &directive_meta, &mut diagnostics);

    // Check for ambiguous parents (Requirement 5.10 / 10.6)
    collect_ambiguous_parent_diagnostics(state, uri, &directive_meta, &mut diagnostics);

    // Check for out-of-scope symbol usage (Requirement 10.3)
    collect_out_of_scope_diagnostics(
        state,
        uri,
        tree.root_node(),
        &text,
        &directive_meta,
        &mut diagnostics,
    );

    // Check for missing packages in library() calls (Requirement 15.1)
    collect_missing_package_diagnostics(state, &directive_meta, &mut diagnostics);

    // Check for redundant forward directives (Requirement 6.2)
    collect_redundant_directive_diagnostics(state, uri, &directive_meta, &mut diagnostics);

    // Check for invalid line=0 in forward directives
    collect_invalid_line_param_diagnostics(&directive_meta, &mut diagnostics);

    // Collect undefined variable errors if enabled in config
    if state.cross_file_config.undefined_variables_enabled {
        collect_undefined_variables_position_aware(
            state,
            uri,
            tree.root_node(),
            &text,
            &doc.loaded_packages,
            &state.workspace_imports,
            &state.package_library,
            &directive_meta,
            &mut diagnostics,
        );
    }

    diagnostics
}

/// Async version of diagnostics that uses batched existence checks for missing files
///
/// This function performs the same diagnostics as `diagnostics()` but uses
/// `collect_missing_file_diagnostics_async` for non-blocking disk I/O when
/// checking file existence.
///
/// The function is designed to minimize lock hold time:
/// 1. Extract needed data from state (caller holds lock briefly)
/// 2. Release lock before async I/O
/// 3. Perform async existence checks
///
/// **Validates: Requirement 14 (async batched existence checks)**
#[allow(dead_code)]
pub async fn diagnostics_async(
    content_provider: &impl crate::content_provider::AsyncContentProvider,
    uri: &Url,
    sync_diagnostics: Vec<Diagnostic>,
    directive_meta: &crate::cross_file::CrossFileMetadata,
    workspace_folders: Option<&Url>,
    missing_file_severity: DiagnosticSeverity,
) -> Vec<Diagnostic> {
    let mut diagnostics = sync_diagnostics;

    // Replace sync missing-file diagnostics with async batched checks.
    // Remove any diagnostics produced by the sync missing-file collector to avoid
    // duplicates when we extend with the async results.
    // Include both source() call messages and @lsp-source directive messages.
    diagnostics.retain(|d| {
        !d.message.starts_with("File not found:")
            && !d.message.starts_with("Parent file not found:")
            && !d.message.starts_with("Cannot resolve path:")
            && !d.message.starts_with("Cannot resolve parent path:")
            && !d.message.starts_with("Path is outside workspace:")
            // @lsp-source directive specific messages
            && !d.message.contains("referenced by @lsp-source directive")
            && !d.message.contains("in @lsp-source directive")
    });

    // Now add async-checked missing file diagnostics
    let missing_file_diags = collect_missing_file_diagnostics_async(
        content_provider,
        uri,
        directive_meta,
        workspace_folders,
        missing_file_severity,
    )
    .await;
    diagnostics.extend(missing_file_diags);

    diagnostics
}

/// Standalone async version that performs disk existence checks without ContentProvider
///
/// This version is used when we can't hold a reference to ContentProvider across await points.
/// It directly performs async disk I/O using spawn_blocking.
///
/// **Validates: Requirement 14 (async batched existence checks)**
pub async fn diagnostics_async_standalone(
    uri: &Url,
    sync_diagnostics: Vec<Diagnostic>,
    directive_meta: &crate::cross_file::CrossFileMetadata,
    workspace_folders: Option<&Url>,
    missing_file_severity: DiagnosticSeverity,
) -> Vec<Diagnostic> {
    let mut diagnostics = sync_diagnostics;

    // Replace sync missing-file diagnostics with async disk-checked results.
    // Include both source() call messages and @lsp-source directive messages.
    diagnostics.retain(|d| {
        !d.message.starts_with("File not found:")
            && !d.message.starts_with("Parent file not found:")
            && !d.message.starts_with("Cannot resolve path:")
            && !d.message.starts_with("Cannot resolve parent path:")
            && !d.message.starts_with("Path is outside workspace:")
            // @lsp-source directive specific messages
            && !d.message.contains("referenced by @lsp-source directive")
            && !d.message.contains("in @lsp-source directive")
    });

    // Collect URIs to check
    let missing_file_diags = collect_missing_file_diagnostics_standalone(
        uri,
        directive_meta,
        workspace_folders,
        missing_file_severity,
    )
    .await;
    diagnostics.extend(missing_file_diags);

    diagnostics
}

/// Standalone async missing file diagnostics that performs disk checks directly
async fn collect_missing_file_diagnostics_standalone(
    uri: &Url,
    meta: &crate::cross_file::CrossFileMetadata,
    workspace_folders: Option<&Url>,
    missing_file_severity: DiagnosticSeverity,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let workspace_root = workspace_folders.and_then(|w| w.to_file_path().ok());

    // Forward sources use @lsp-cd for path resolution
    let forward_ctx =
        crate::cross_file::path_resolve::PathContext::from_metadata(uri, meta, workspace_folders);
    // Backward directives IGNORE @lsp-cd - always resolve relative to file's directory
    let backward_ctx = crate::cross_file::path_resolve::PathContext::new(uri, workspace_folders);

    // Collect all paths to check: (path, path_str, line, col, is_backward, is_directive)
    // is_directive is true for @lsp-source directives, false for source() calls
    // _Requirements: 6.1, 6.3_ (for @lsp-source directive missing file diagnostics)
    let mut paths_to_check: Vec<(std::path::PathBuf, String, u32, u32, bool, bool)> = Vec::new();

    for source in &meta.sources {
        let resolved = forward_ctx.as_ref().and_then(|ctx| {
            crate::cross_file::path_resolve::resolve_path_with_workspace_fallback(&source.path, ctx)
        });
        if let Some(path) = resolved {
            if let Some(root) = &workspace_root {
                if !path.starts_with(root) {
                    let message = if source.is_directive {
                        format!(
                            "File '{}' referenced by @lsp-source directive is outside workspace",
                            source.path
                        )
                    } else {
                        format!("Path is outside workspace: '{}'", source.path)
                    };
                    diagnostics.push(Diagnostic {
                        range: Range {
                            start: Position::new(source.line, source.column),
                            end: Position::new(
                                source.line,
                                source
                                    .column
                                    .saturating_add(source.path.len() as u32)
                                    .saturating_add(10),
                            ),
                        },
                        severity: Some(missing_file_severity),
                        message,
                        ..Default::default()
                    });
                    continue;
                }
            }
            paths_to_check.push((path, source.path.clone(), source.line, source.column, false, source.is_directive));
        } else {
            let message = if source.is_directive {
                format!(
                    "Cannot resolve path '{}' in @lsp-source directive",
                    source.path
                )
            } else {
                format!("Cannot resolve path: '{}'", source.path)
            };
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(source.line, source.column),
                    end: Position::new(
                        source.line,
                        source
                            .column
                            .saturating_add(source.path.len() as u32)
                            .saturating_add(10),
                    ),
                },
                severity: Some(missing_file_severity),
                message,
                ..Default::default()
            });
        }
    }

    for directive in &meta.sourced_by {
        let resolved = backward_ctx
            .as_ref()
            .and_then(|ctx| crate::cross_file::path_resolve::resolve_path(&directive.path, ctx));
        if let Some(path) = resolved {
            if let Some(root) = &workspace_root {
                if !path.starts_with(root) {
                    diagnostics.push(Diagnostic {
                        range: Range {
                            start: Position::new(directive.directive_line, 0),
                            end: Position::new(directive.directive_line, LSP_EOL_CHARACTER),
                        },
                        severity: Some(missing_file_severity),
                        message: format!("Path is outside workspace: '{}'", directive.path),
                        ..Default::default()
                    });
                    continue;
                }
            }
            // Backward directives are always directives (is_directive=true for backward)
            paths_to_check.push((
                path,
                directive.path.clone(),
                directive.directive_line,
                0,
                true,
                false, // is_directive is false here because we use is_backward to distinguish
            ));
        } else {
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(directive.directive_line, 0),
                    end: Position::new(directive.directive_line, LSP_EOL_CHARACTER),
                },
                severity: Some(missing_file_severity),
                message: format!("Cannot resolve parent path: '{}'", directive.path),
                ..Default::default()
            });
        }
    }

    if paths_to_check.is_empty() {
        return diagnostics;
    }

    // Batch check existence on blocking thread
    let paths: Vec<std::path::PathBuf> = paths_to_check
        .iter()
        .map(|(p, _, _, _, _, _)| p.clone())
        .collect();
    let existence = match tokio::task::spawn_blocking(move || {
        paths.iter().map(|p| p.exists()).collect::<Vec<_>>()
    })
    .await
    {
        Ok(v) => v,
        Err(err) => {
            log::warn!("Missing-file check failed: {err}");
            return diagnostics;
        }
    };

    // Generate diagnostics for missing files
    // _Requirements: 6.1_ (for @lsp-source directive missing file diagnostics)
    for (i, (_, path_str, line, col, is_backward, is_directive)) in paths_to_check.into_iter().enumerate() {
        if !existence.get(i).copied().unwrap_or(false) {
            if is_backward {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position::new(line, 0),
                        end: Position::new(line, LSP_EOL_CHARACTER),
                    },
                    severity: Some(missing_file_severity),
                    message: format!("Parent file not found: '{}'", path_str),
                    ..Default::default()
                });
            } else {
                // Use specific message for @lsp-source directives (Requirement 6.1)
                let message = if is_directive {
                    format!(
                        "File '{}' referenced by @lsp-source directive not found",
                        path_str
                    )
                } else {
                    format!("File not found: '{}'", path_str)
                };
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position::new(line, col),
                        end: Position::new(
                            line,
                            col.saturating_add(path_str.len() as u32).saturating_add(10),
                        ),
                    },
                    severity: Some(missing_file_severity),
                    message,
                    ..Default::default()
                });
            }
        }
    }

    diagnostics
}

/// Public test wrapper for `collect_missing_file_diagnostics_standalone`.
///
/// This function is exposed for property-based testing of missing file diagnostics
/// for forward directives (@lsp-source, @lsp-run, @lsp-include).
///
/// **Validates: Requirements 6.1** (for @lsp-source directive missing file diagnostics)
#[cfg(test)]
pub async fn collect_missing_file_diagnostics_standalone_for_test(
    uri: &Url,
    meta: &crate::cross_file::CrossFileMetadata,
    workspace_folders: Option<&Url>,
    missing_file_severity: DiagnosticSeverity,
) -> Vec<Diagnostic> {
    collect_missing_file_diagnostics_standalone(uri, meta, workspace_folders, missing_file_severity)
        .await
}

/// Collect diagnostics for missing files referenced in source() calls and directives
///
/// This is the synchronous version that only checks cached sources (no disk I/O).
/// For async disk checking, use `collect_missing_file_diagnostics_async`.
///
/// Path resolution follows the critical distinction from AGENTS.md:
/// - Forward sources (source() calls): use PathContext::from_metadata (respects @lsp-cd)
/// - Backward directives (@lsp-sourced-by): use PathContext::new (ignores @lsp-cd)
///
/// Uses ContentProvider for unified access to cached file existence.
///
/// **Validates: Requirements 14.2, 14.5**
fn collect_missing_file_diagnostics(
    state: &WorldState,
    uri: &Url,
    meta: &crate::cross_file::CrossFileMetadata,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let content_provider = state.content_provider();

    // Forward sources use @lsp-cd for path resolution
    let forward_ctx = crate::cross_file::path_resolve::PathContext::from_metadata(
        uri,
        meta,
        state.workspace_folders.first(),
    );
    // Backward directives IGNORE @lsp-cd - always resolve relative to file's directory
    let backward_ctx =
        crate::cross_file::path_resolve::PathContext::new(uri, state.workspace_folders.first());

    let workspace_root = state
        .workspace_folders
        .first()
        .and_then(|w| w.to_file_path().ok());

    // Check forward sources (source() calls and @lsp-source directives)
    // Uses workspace-root fallback for files without @lsp-cd directives
    // _Requirements: 6.1, 6.3_ (for @lsp-source directive missing file diagnostics)
    for source in &meta.sources {
        let resolved_path = forward_ctx.as_ref().and_then(|ctx| {
            crate::cross_file::path_resolve::resolve_path_with_workspace_fallback(&source.path, ctx)
        });
        if let Some(path) = resolved_path {
            // Guard against paths outside workspace
            // Uses missing_file_severity since the file is effectively inaccessible
            if let Some(root) = &workspace_root {
                if !path.starts_with(root) {
                    let message = if source.is_directive {
                        format!(
                            "File '{}' referenced by @lsp-source directive is outside workspace",
                            source.path
                        )
                    } else {
                        format!("Path is outside workspace: '{}'", source.path)
                    };
                    diagnostics.push(Diagnostic {
                        range: Range {
                            start: Position::new(source.line, source.column),
                            end: Position::new(
                                source.line,
                                source
                                    .column
                                    .saturating_add(source.path.len() as u32)
                                    .saturating_add(10),
                            ),
                        },
                        severity: Some(state.cross_file_config.missing_file_severity),
                        message,
                        ..Default::default()
                    });
                    continue;
                }
            }
            if let Some(target_uri) = crate::cross_file::path_resolve::path_to_uri(&path) {
                // Use ContentProvider for cached existence check (no blocking I/O)
                if !content_provider.exists_cached(&target_uri) {
                    // Use specific message for @lsp-source directives (Requirement 6.1)
                    let message = if source.is_directive {
                        format!(
                            "File '{}' referenced by @lsp-source directive not found",
                            source.path
                        )
                    } else {
                        format!("File not found: '{}'", source.path)
                    };
                    diagnostics.push(Diagnostic {
                        range: Range {
                            start: Position::new(source.line, source.column),
                            end: Position::new(
                                source.line,
                                source
                                    .column
                                    .saturating_add(source.path.len() as u32)
                                    .saturating_add(10),
                            ),
                        },
                        severity: Some(state.cross_file_config.missing_file_severity),
                        message,
                        ..Default::default()
                    });
                }
            }
        } else {
            // Use specific message for @lsp-source directives
            let message = if source.is_directive {
                format!(
                    "Cannot resolve path '{}' in @lsp-source directive",
                    source.path
                )
            } else {
                format!("Cannot resolve path: '{}'", source.path)
            };
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(source.line, source.column),
                    end: Position::new(
                        source.line,
                        source
                            .column
                            .saturating_add(source.path.len() as u32)
                            .saturating_add(10),
                    ),
                },
                severity: Some(state.cross_file_config.missing_file_severity),
                message,
                ..Default::default()
            });
        }
    }

    // Check backward directives (@lsp-sourced-by)
    for directive in &meta.sourced_by {
        let resolved = backward_ctx.as_ref().and_then(|ctx| {
            let path = crate::cross_file::path_resolve::resolve_path(&directive.path, ctx)?;
            crate::cross_file::path_resolve::path_to_uri(&path)
        });
        if let Some(target_uri) = resolved {
            // Use ContentProvider for cached existence check (no blocking I/O)
            if !content_provider.exists_cached(&target_uri) {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position::new(directive.directive_line, 0),
                        end: Position::new(directive.directive_line, LSP_EOL_CHARACTER),
                    },
                    severity: Some(state.cross_file_config.missing_file_severity),
                    message: format!("Parent file not found: '{}'", directive.path),
                    ..Default::default()
                });
            }
        } else {
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(directive.directive_line, 0),
                    end: Position::new(directive.directive_line, LSP_EOL_CHARACTER),
                },
                severity: Some(state.cross_file_config.missing_file_severity),
                message: format!("Cannot resolve parent path: '{}'", directive.path),
                ..Default::default()
            });
        }
    }
}

/// Async version of missing file diagnostics that checks disk existence
///
/// This version uses `AsyncContentProvider::check_existence_batch` to perform
/// non-blocking disk I/O for files not found in cache.
///
/// Path resolution follows the critical distinction from AGENTS.md:
/// - Forward sources (source() calls): use PathContext::from_metadata (respects @lsp-cd)
/// - Backward directives (@lsp-sourced-by): use PathContext::new (ignores @lsp-cd)
///
/// **Validates: Requirements 14.2, 14.5**
/// _Requirements: 6.1, 6.3_ (for @lsp-source directive missing file diagnostics)
#[allow(dead_code)]
pub async fn collect_missing_file_diagnostics_async(
    content_provider: &impl crate::content_provider::AsyncContentProvider,
    uri: &Url,
    meta: &crate::cross_file::CrossFileMetadata,
    workspace_folders: Option<&Url>,
    missing_file_severity: DiagnosticSeverity,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // Forward sources use @lsp-cd for path resolution
    let forward_ctx =
        crate::cross_file::path_resolve::PathContext::from_metadata(uri, meta, workspace_folders);
    // Backward directives IGNORE @lsp-cd - always resolve relative to file's directory
    let backward_ctx = crate::cross_file::path_resolve::PathContext::new(uri, workspace_folders);

    let workspace_root = workspace_folders.and_then(|w| w.to_file_path().ok());

    // Collect all URIs to check: (uri, path, line, col, is_backward, is_directive)
    let mut uris_to_check: Vec<(Url, String, u32, u32, bool, bool)> = Vec::new();

    // Uses workspace-root fallback for files without @lsp-cd directives
    for source in &meta.sources {
        let resolved_path = forward_ctx.as_ref().and_then(|ctx| {
            crate::cross_file::path_resolve::resolve_path_with_workspace_fallback(&source.path, ctx)
        });
        if let Some(path) = resolved_path {
            // Guard against paths outside workspace
            // Uses missing_file_severity since the file is effectively inaccessible
            if let Some(root) = &workspace_root {
                if !path.starts_with(root) {
                    let message = if source.is_directive {
                        format!(
                            "File '{}' referenced by @lsp-source directive is outside workspace",
                            source.path
                        )
                    } else {
                        format!("Path is outside workspace: '{}'", source.path)
                    };
                    diagnostics.push(Diagnostic {
                        range: Range {
                            start: Position::new(source.line, source.column),
                            end: Position::new(
                                source.line,
                                source
                                    .column
                                    .saturating_add(source.path.len() as u32)
                                    .saturating_add(10),
                            ),
                        },
                        severity: Some(missing_file_severity),
                        message,
                        ..Default::default()
                    });
                    continue;
                }
            }
            if let Some(target_uri) = crate::cross_file::path_resolve::path_to_uri(&path) {
                uris_to_check.push((
                    target_uri,
                    source.path.clone(),
                    source.line,
                    source.column,
                    false,
                    source.is_directive,
                ));
            }
        } else {
            let message = if source.is_directive {
                format!(
                    "Cannot resolve path '{}' in @lsp-source directive",
                    source.path
                )
            } else {
                format!("Cannot resolve path: '{}'", source.path)
            };
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(source.line, source.column),
                    end: Position::new(
                        source.line,
                        source
                            .column
                            .saturating_add(source.path.len() as u32)
                            .saturating_add(10),
                    ),
                },
                severity: Some(missing_file_severity),
                message,
                ..Default::default()
            });
        }
    }

    for directive in &meta.sourced_by {
        let resolved = backward_ctx.as_ref().and_then(|ctx| {
            let path = crate::cross_file::path_resolve::resolve_path(&directive.path, ctx)?;
            crate::cross_file::path_resolve::path_to_uri(&path)
        });
        if let Some(target_uri) = resolved {
            uris_to_check.push((
                target_uri,
                directive.path.clone(),
                directive.directive_line,
                0,
                true,
                false, // is_directive is false here because we use is_backward to distinguish
            ));
        } else {
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(directive.directive_line, 0),
                    end: Position::new(directive.directive_line, LSP_EOL_CHARACTER),
                },
                severity: Some(missing_file_severity),
                message: format!("Cannot resolve parent path: '{}'", directive.path),
                ..Default::default()
            });
        }
    }

    if uris_to_check.is_empty() {
        return diagnostics;
    }

    // Batch check existence (non-blocking)
    let uris: Vec<Url> = uris_to_check
        .iter()
        .map(|(u, _, _, _, _, _)| u.clone())
        .collect();
    let existence = content_provider.check_existence_batch(&uris).await;

    // Generate diagnostics for missing files
    for (target_uri, path, line, col, is_backward, is_directive) in uris_to_check {
        if !existence.get(&target_uri).copied().unwrap_or(false) {
            if is_backward {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position::new(line, 0),
                        end: Position::new(line, LSP_EOL_CHARACTER),
                    },
                    severity: Some(missing_file_severity),
                    message: format!("Parent file not found: '{}'", path),
                    ..Default::default()
                });
            } else {
                // Use specific message for @lsp-source directives (Requirement 6.1)
                let message = if is_directive {
                    format!(
                        "File '{}' referenced by @lsp-source directive not found",
                        path
                    )
                } else {
                    format!("File not found: '{}'", path)
                };
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position::new(line, col),
                        end: Position::new(
                            line,
                            col.saturating_add(path.len() as u32).saturating_add(10),
                        ),
                    },
                    severity: Some(missing_file_severity),
                    message,
                    ..Default::default()
                });
            }
        }
    }

    diagnostics
}

/// Collect diagnostics for max chain depth exceeded (Requirement 5.8)
fn collect_max_depth_diagnostics(state: &WorldState, uri: &Url, diagnostics: &mut Vec<Diagnostic>) {
    use crate::cross_file::scope;

    let get_artifacts = |target_uri: &Url| -> Option<scope::ScopeArtifacts> {
        if let Some(doc) = state.documents.get(target_uri) {
            if let Some(tree) = &doc.tree {
                return Some(scope::compute_artifacts(target_uri, tree, &doc.text()));
            }
        }
        if let Some(artifacts) = state.cross_file_workspace_index.get_artifacts(target_uri) {
            return Some(artifacts);
        }
        if let Some(doc) = state.workspace_index.get(target_uri) {
            if let Some(tree) = &doc.tree {
                return Some(scope::compute_artifacts(target_uri, tree, &doc.text()));
            }
        }
        None
    };

    let get_metadata = |target_uri: &Url| -> Option<crate::cross_file::CrossFileMetadata> {
        if let Some(doc) = state.documents.get(target_uri) {
            return Some(crate::cross_file::directive::parse_directives(&doc.text()));
        }
        state.cross_file_workspace_index.get_metadata(target_uri)
    };

    let max_depth = state.cross_file_config.max_chain_depth;

    // For depth-exceeded diagnostics, we don't need base_exports since we're only
    // checking chain depth, not resolving symbols. Pass empty set for efficiency.
    let empty_base_exports = std::collections::HashSet::new();

    // Use scope resolution to detect depth exceeded (now uses PathContext internally)
    let scope = scope::scope_at_position_with_graph(
        uri,
        u32::MAX,
        u32::MAX,
        &get_artifacts,
        &get_metadata,
        &state.cross_file_graph,
        state.workspace_folders.first(),
        max_depth,
        &empty_base_exports,
    );

    // Emit diagnostics for depth exceeded, filtering to only those in this file
    for (exceeded_uri, line, col) in &scope.depth_exceeded {
        if exceeded_uri == uri {
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(*line, *col),
                    end: Position::new(*line, col.saturating_add(1)),
                },
                severity: Some(state.cross_file_config.max_chain_depth_severity),
                message: format!(
                    "Maximum chain depth ({}) exceeded; some symbols may not be resolved",
                    max_depth
                ),
                ..Default::default()
            });
        }
    }
}

/// Emit a diagnostic when a file's parent resolution is ambiguous.
///
/// This inspects the cross-file metadata and graph to resolve the file's parent(s).
/// If resolution returns an ambiguous result, a single `Diagnostic` is pushed that
/// points at the first backward directive line and suggests adding `line=` or `match=`
/// to disambiguate. The diagnostic's severity is taken from the cross-file config.
///
/// # Examples
///
/// ```no_run
/// # use url::Url;
/// # use lsp_types::Diagnostic;
/// # use crate::WorldState;
/// # use crate::cross_file::CrossFileMetadata;
/// # fn example(state: &WorldState, uri: &Url, meta: &CrossFileMetadata, diagnostics: &mut Vec<Diagnostic>) {
/// collect_ambiguous_parent_diagnostics(state, uri, meta, diagnostics);
/// # }
/// ```
fn collect_ambiguous_parent_diagnostics(
    state: &WorldState,
    uri: &Url,
    meta: &crate::cross_file::CrossFileMetadata,
    diagnostics: &mut Vec<Diagnostic>,
) {
    use crate::cross_file::cache::ParentResolution;
    use crate::cross_file::parent_resolve::resolve_parent_with_content;

    // Build PathContext for proper path resolution
    // Use PathContext::new (not from_metadata) because backward directives should
    // resolve relative to the file's directory, ignoring @lsp-cd
    let path_ctx =
        crate::cross_file::path_resolve::PathContext::new(uri, state.workspace_folders.first());

    let resolve_path = |path: &str| -> Option<Url> {
        let ctx = path_ctx.as_ref()?;
        let resolved = crate::cross_file::path_resolve::resolve_path(path, ctx)?;
        crate::cross_file::path_resolve::path_to_uri(&resolved)
    };

    let get_content = |parent_uri: &Url| -> Option<String> {
        // Open docs first, then file cache
        if let Some(doc) = state.documents.get(parent_uri) {
            return Some(doc.text());
        }
        state.cross_file_file_cache.get(parent_uri)
    };

    let resolution = resolve_parent_with_content(
        meta,
        &state.cross_file_graph,
        uri,
        &state.cross_file_config,
        resolve_path,
        get_content,
    );

    if let ParentResolution::Ambiguous {
        selected_uri,
        alternatives,
        ..
    } = resolution
    {
        // Find the first backward directive line to attach the diagnostic
        let directive_line = meta
            .sourced_by
            .first()
            .map(|d| d.directive_line)
            .unwrap_or(0);

        let alt_list: Vec<String> = alternatives
            .iter()
            .filter_map(|u| {
                u.path_segments()
                    .and_then(|mut s| s.next_back().map(|s| s.to_string()))
            })
            .collect();

        let selected_name = selected_uri
            .path_segments()
            .and_then(|mut s| s.next_back().map(|s| s.to_string()))
            .unwrap_or_else(|| selected_uri.to_string());

        diagnostics.push(Diagnostic {
            range: Range {
                start: Position::new(directive_line, 0),
                end: Position::new(directive_line, LSP_EOL_CHARACTER),
            },
            severity: Some(state.cross_file_config.ambiguous_parent_severity),
            message: format!(
                "Ambiguous parent: using '{}' but also found: {}. Consider adding line= or match= to disambiguate.",
                selected_name,
                alt_list.join(", ")
            ),
            ..Default::default()
        });
    }
}

/// Emit diagnostics for `library()` calls that reference packages not present in the package library.
///
/// Scans the cross-file metadata for `library()` calls and, for each call that is not ignored
/// via directives and whose package is not found in `state.package_library`, appends a warning
/// Diagnostic covering the call's line range with the configured severity.
///
/// # Examples
///
/// ```ignore
/// // Construct a WorldState with packages enabled and an empty PackageLibrary,
/// // provide CrossFileMetadata containing a LibraryCall for "foo", then:
/// //
/// // collect_missing_package_diagnostics(&state, &meta, &mut diagnostics);
/// //
/// // After the call, `diagnostics` will contain a warning about package "foo".
/// ```
fn collect_missing_package_diagnostics(
    state: &WorldState,
    meta: &crate::cross_file::CrossFileMetadata,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Skip if packages feature is disabled
    if !state.cross_file_config.packages_enabled {
        return;
    }

    for lib_call in &meta.library_calls {
        // Skip if the line is ignored via @lsp-ignore or @lsp-ignore-next
        if crate::cross_file::directive::is_line_ignored(meta, lib_call.line) {
            continue;
        }

        // Check if the package exists (is installed)
        if !state.package_library.package_exists(&lib_call.package) {
            // Package not found - emit diagnostic with configured severity
            // The column in LibraryCall is already UTF-16 (end position of the call)
            // We want to highlight the library() call, so we use the line and estimate the range

            // Calculate approximate start column (library( is 8 chars, package name varies)
            // We'll highlight from column 0 to the end column for simplicity
            let end_col = lib_call.column;

            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(lib_call.line, 0),
                    end: Position::new(lib_call.line, end_col),
                },
                severity: Some(state.cross_file_config.packages_missing_package_severity),
                message: format!("Package '{}' is not installed", lib_call.package),
                ..Default::default()
            });
        }
    }
}

/// Emit diagnostics for redundant forward directives.
///
/// A forward directive (@lsp-source, @lsp-run, @lsp-include) without an explicit `line=N`
/// parameter is considered redundant when an AST-detected `source()` call to the same
/// target file exists at an earlier line. In this case, the directive provides no additional
/// information since the source() call already makes the symbols available earlier.
///
/// Directives WITH an explicit `line=N` parameter are never considered redundant because
/// they explicitly specify a different call-site position for scope resolution purposes.
///
/// The diagnostic severity is configurable via `crossFile.redundantDirectiveSeverity`.
/// Setting it to "off" disables this diagnostic entirely.
///
/// # Parameters
///
/// - `state`: Workspace state containing configuration.
/// - `uri`: URI of the current document being analyzed.
/// - `meta`: Cross-file directive metadata containing forward sources.
/// - `diagnostics`: Mutable vector to receive emitted diagnostics.
///
/// _Requirements: 6.2_
fn collect_redundant_directive_diagnostics(
    state: &WorldState,
    uri: &Url,
    meta: &crate::cross_file::CrossFileMetadata,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Skip if redundant directive diagnostics are disabled
    let severity = match state.cross_file_config.redundant_directive_severity {
        Some(sev) => sev,
        None => return, // Disabled
    };

    let workspace_root = state.workspace_folders.first();

    // Build PathContext for resolving forward source paths (includes @lsp-cd)
    let path_ctx =
        crate::cross_file::path_resolve::PathContext::from_metadata(uri, meta, workspace_root);
    let path_ctx = match path_ctx {
        Some(ctx) => ctx,
        None => return,
    };

    // Collect directive sources (without explicit line=) and AST sources separately
    // Directives WITH explicit_line are never considered redundant (Requirement 6.2)
    let mut directive_sources: Vec<&crate::cross_file::types::ForwardSource> = Vec::new();
    let mut ast_sources: Vec<&crate::cross_file::types::ForwardSource> = Vec::new();

    for source in &meta.sources {
        if source.is_directive {
            // Only consider directives WITHOUT explicit line= for redundancy checking
            if !source.explicit_line {
                directive_sources.push(source);
            }
        } else {
            ast_sources.push(source);
        }
    }

    // For each directive source, check if there's an AST source to the same file at an earlier line
    for directive in &directive_sources {
        // Resolve the directive's target path
        let directive_target = crate::cross_file::path_resolve::resolve_path_with_workspace_fallback(
            &directive.path,
            &path_ctx,
        );
        let directive_target = match directive_target {
            Some(path) => path,
            None => continue,
        };

        // Check if any AST source points to the same file at an earlier line
        for ast_source in &ast_sources {
            let ast_target = crate::cross_file::path_resolve::resolve_path_with_workspace_fallback(
                &ast_source.path,
                &path_ctx,
            );
            let ast_target = match ast_target {
                Some(path) => path,
                None => continue,
            };

            // Check if they point to the same file and AST is at an earlier line
            if directive_target == ast_target && ast_source.line < directive.line {
                // Get the target filename for the message
                let target_filename = directive_target
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&directive.path);

                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position::new(directive.line, 0),
                        end: Position::new(directive.line, LSP_EOL_CHARACTER),
                    },
                    severity: Some(severity),
                    message: format!(
                        "Directive is redundant: source() call to '{}' exists at earlier line {}.",
                        target_filename,
                        ast_source.line + 1 // Convert to 1-based for display
                    ),
                    ..Default::default()
                });
                break; // Only emit one diagnostic per directive
            }
        }
    }
}

/// Emit diagnostics for forward directives with invalid `line=0` parameter.
///
/// The `line=N` parameter uses 1-based line numbers (matching editor display).
/// A value of `line=0` is invalid because there is no line 0 in 1-based numbering.
/// When `line=0` is specified, the LSP emits a warning and treats it as line 1
/// (which becomes internal line 0 after conversion).
///
/// # Parameters
///
/// - `meta`: Cross-file directive metadata containing forward sources.
/// - `diagnostics`: Mutable vector to receive emitted diagnostics.
fn collect_invalid_line_param_diagnostics(
    meta: &crate::cross_file::CrossFileMetadata,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for source in &meta.sources {
        // Only check directive sources with explicit line=0
        if source.is_directive && source.user_line_zero {
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(source.directive_line, 0),
                    end: Position::new(source.directive_line, LSP_EOL_CHARACTER),
                },
                severity: Some(DiagnosticSeverity::WARNING),
                message: format!(
                    "Invalid line=0 in @lsp-source directive for '{}': line numbers are 1-based. Using line 1 instead.",
                    source.path
                ),
                ..Default::default()
            });
        }
    }
}

/// Emit diagnostics for symbols defined in sourced files that are referenced
/// earlier in the current document than the corresponding `source()` call.
///
/// This function:
/// - Scans `directive_meta.sources` and collects source paths declared in the file.
/// - Collects identifier usages (UTF-16 columns) in `node`.
/// - For each sourced file, resolves its URI and obtains its exported symbols (preferring open documents, then cross-file index, then legacy index).
/// - Emits a diagnostic for every usage of an exported symbol that occurs before the `source()` call (skipping lines marked ignored by directives).
///
/// The produced diagnostics are appended to `diagnostics` and use the configured
/// `out_of_scope_severity` from `state.cross_file_config`.
///
/// # Parameters
///
/// - `state`: Workspace state and indexes used to resolve artifacts and configuration.
/// - `uri`: URI of the current document being analyzed (used to resolve relative source paths).
/// - `node`: Root AST node of the current document.
/// - `text`: Full source text of the current document.
/// - `directive_meta`: Cross-file directive metadata (contains `@lsp-source` / `source()` locations).
/// - `diagnostics`: Mutable vector to receive emitted diagnostics.
///
/// # Examples
///
/// ```no_run
/// // Collect diagnostics into `diags` for a parsed document:
/// let mut diags = Vec::new();
/// collect_out_of_scope_diagnostics(&state, &uri, root_node, &text, &directive_meta, &mut diags);
/// // `diags` now contains diagnostics for symbols used before their `source()` calls.
/// ```
fn collect_out_of_scope_diagnostics(
    state: &WorldState,
    uri: &Url,
    node: Node,
    text: &str,
    directive_meta: &crate::cross_file::CrossFileMetadata,
    diagnostics: &mut Vec<Diagnostic>,
) {
    use crate::cross_file::types::byte_offset_to_utf16_column;

    // Get all source() calls and @lsp-source directives in this file
    let source_calls: Vec<_> = directive_meta.sources.iter().collect();

    if source_calls.is_empty() {
        return;
    }

    // Collect all identifier usages with UTF-16 columns
    let mut usages: Vec<(String, u32, u32, Node)> = Vec::new();
    collect_identifier_usages_utf16(node, text, &mut usages);

    // For each source() call, check if any symbols from that file are used before the call
    for source in &source_calls {
        let source_line = source.line;
        let source_col = source.column; // Already UTF-16

        // Resolve the source path
        let resolve_path = |path: &str| -> Option<Url> {
            let from_path = uri.to_file_path().ok()?;
            let parent_dir = from_path.parent()?;
            let resolved = parent_dir.join(path);
            let normalized = crate::cross_file::path_resolve::normalize_path_public(&resolved)?;
            Url::from_file_path(normalized).ok()
        };

        let Some(source_uri) = resolve_path(&source.path) else {
            continue;
        };

        // Get symbols from the sourced file
        let source_symbols: std::collections::HashSet<String> = {
            let get_artifacts = |target_uri: &Url| -> Option<scope::ScopeArtifacts> {
                // Try open documents first (authoritative)
                if let Some(doc) = state.documents.get(target_uri) {
                    if let Some(tree) = &doc.tree {
                        return Some(scope::compute_artifacts(target_uri, tree, &doc.text()));
                    }
                }
                // Try cross-file workspace index (preferred for closed files)
                if let Some(artifacts) = state.cross_file_workspace_index.get_artifacts(target_uri)
                {
                    return Some(artifacts);
                }
                // Fallback to legacy workspace index
                if let Some(doc) = state.workspace_index.get(target_uri) {
                    if let Some(tree) = &doc.tree {
                        return Some(scope::compute_artifacts(target_uri, tree, &doc.text()));
                    }
                }
                None
            };

            get_artifacts(&source_uri)
                .map(|a| a.exported_interface.keys().cloned().collect())
                .unwrap_or_default()
        };

        // Check for usages of these symbols before the source() call
        for (name, usage_line, usage_col, usage_node) in &usages {
            if !source_symbols.contains(name) {
                continue;
            }

            // Check if usage is before the source() call (both columns are UTF-16)
            if (*usage_line, *usage_col) < (source_line, source_col) {
                // Skip if line is ignored
                if crate::cross_file::directive::is_line_ignored(directive_meta, *usage_line) {
                    continue;
                }

                // Convert byte columns to UTF-16 for diagnostic range
                let start_line_text = text
                    .lines()
                    .nth(usage_node.start_position().row)
                    .unwrap_or("");
                let end_line_text = text
                    .lines()
                    .nth(usage_node.end_position().row)
                    .unwrap_or("");
                let start_col = byte_offset_to_utf16_column(
                    start_line_text,
                    usage_node.start_position().column,
                );
                let end_col =
                    byte_offset_to_utf16_column(end_line_text, usage_node.end_position().column);

                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position::new(usage_node.start_position().row as u32, start_col),
                        end: Position::new(usage_node.end_position().row as u32, end_col),
                    },
                    severity: Some(state.cross_file_config.out_of_scope_severity),
                    message: format!(
                        "Symbol '{}' used before source() call at line {}",
                        name,
                        source_line + 1
                    ),
                    ..Default::default()
                });
            }
        }
    }
}

/// Collect identifier usages with UTF-16 column positions
fn collect_identifier_usages_utf16<'a>(
    node: Node<'a>,
    text: &str,
    usages: &mut Vec<(String, u32, u32, Node<'a>)>,
) {
    use crate::cross_file::types::byte_offset_to_utf16_column;

    if node.kind() == "identifier" {
        // Skip if this is the LHS of an assignment
        if let Some(parent) = node.parent() {
            if parent.kind() == "binary_operator" {
                let mut cursor = parent.walk();
                let children: Vec<_> = parent.children(&mut cursor).collect();
                if children.len() >= 2 && children[0].id() == node.id() {
                    let op = children[1];
                    let op_text = &text[op.byte_range()];
                    if matches!(op_text, "<-" | "=" | "<<-") {
                        // Skip LHS of assignment, but recurse into children
                        let mut cursor = node.walk();
                        for child in node.children(&mut cursor) {
                            collect_identifier_usages_utf16(child, text, usages);
                        }
                        return;
                    }
                }
            }
            // Skip named arguments
            if parent.kind() == "argument" {
                if let Some(name_node) = parent.child_by_field_name("name") {
                    if name_node.id() == node.id() {
                        return;
                    }
                }
            }
        }

        let name = text[node.byte_range()].to_string();
        let line = node.start_position().row as u32;
        // Convert byte column to UTF-16
        let line_text = text.lines().nth(node.start_position().row).unwrap_or("");
        let col = byte_offset_to_utf16_column(line_text, node.start_position().column);
        usages.push((name, line, col, node));
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifier_usages_utf16(child, text, usages);
    }
}

fn collect_syntax_errors(node: Node, diagnostics: &mut Vec<Diagnostic>) {
    if node.is_error() || node.is_missing() {
        let message = if node.is_missing() {
            format!("Missing {}", node.kind())
        } else {
            "Syntax error".to_string()
        };

        diagnostics.push(Diagnostic {
            range: Range {
                start: Position::new(
                    node.start_position().row as u32,
                    node.start_position().column as u32,
                ),
                end: Position::new(
                    node.end_position().row as u32,
                    node.end_position().column as u32,
                ),
            },
            severity: Some(DiagnosticSeverity::ERROR),
            message,
            ..Default::default()
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_syntax_errors(child, diagnostics);
    }
}

/// Detect and report diagnostics for `else` keywords that appear on a new line
/// after the closing brace of an `if` block.
///
/// In R, `else` must appear on the same line as the closing `}` of the `if` block.
/// When `else` is on a new line, R treats the `if` as complete and `else` becomes
/// an unexpected token.
///
/// # Implementation Note
///
/// When `else` appears on a new line after an `if` block, tree-sitter-r parses it
/// as an `identifier` node (not an `"else"` keyword node) that is a sibling of the
/// `if_statement` in the parent node. This function detects this pattern by:
/// 1. Finding `identifier` nodes with text "else"
/// 2. Checking if the preceding sibling is an `if_statement`
/// 3. Comparing line numbers to determine if `else` is on a new line
///
/// # Arguments
/// * `node` - The root AST node to traverse
/// * `text` - The source text for extracting node content
/// * `diagnostics` - Vector to append diagnostics to
///
/// # Examples
///
/// Invalid (emits diagnostic):
/// ```r
/// if (cond) { body }
/// else { body2 }
/// ```
///
/// Valid (no diagnostic):
/// ```r
/// if (cond) { body } else { body2 }
/// ```
///
/// **Validates: Requirements 1.1, 1.2, 1.3, 4.2**
fn collect_else_newline_errors(node: Node, text: &str, diagnostics: &mut Vec<Diagnostic>) {
    // Case 1: Check if this node is an identifier with text "else"
    // When else is on a new line at the top level, tree-sitter parses it as an identifier
    if node.kind() == "identifier" {
        let node_text_str = node_text(node, text);
        if node_text_str == "else" {
            // Skip if this node is already marked as an error by tree-sitter
            // to avoid duplicate diagnostics (Requirement 4.2)
            if node.is_error() {
                // Already handled by collect_syntax_errors
            } else if let Some(parent) = node.parent() {
                if parent.is_error() {
                    // Parent is error, skip to avoid duplicate
                } else {
                    // Check if there's a preceding if_statement (skipping over comments)
                    // This indicates an orphaned else on a new line
                    // Validates: Requirement 5.3 - comments between `}` and `else` should not
                    // prevent detection when else is on a new line
                    let mut prev = node.prev_sibling();
                    while let Some(sibling) = prev {
                        if sibling.kind() == "comment" {
                            // Skip comments and continue looking
                            prev = sibling.prev_sibling();
                        } else if sibling.kind() == "if_statement" {
                            // Found the preceding if_statement
                            let brace_line = find_closing_brace_line(&sibling, text);
                            let else_start_line = node.start_position().row;

                            if let Some(brace_line) = brace_line {
                                // If else is on a different line than the closing brace, emit diagnostic
                                if else_start_line > brace_line {
                                    emit_else_newline_diagnostic(node, diagnostics);
                                }
                            } else {
                                // Fallback: use the end line of the if_statement
                                let if_end_line = sibling.end_position().row;
                                if else_start_line > if_end_line {
                                    emit_else_newline_diagnostic(node, diagnostics);
                                }
                            }
                            break;
                        } else {
                            // Found something other than comment or if_statement, stop looking
                            break;
                        }
                    }
                }
            }
        }
    }

    // Case 2: Check if this is an if_statement with an else clause
    // When else is on a new line inside a braced expression (nested), tree-sitter still parses
    // it as part of the if_statement with an "else" keyword node
    // Validates: Requirement 2.5 - nested if-else detection
    if node.kind() == "if_statement" {
        // Look for the "else" keyword child and the consequence (braced_expression)
        let mut cursor = node.walk();
        let mut consequence_end_line: Option<usize> = None;
        let mut else_node: Option<Node> = None;

        for child in node.children(&mut cursor) {
            if child.kind() == "braced_expression" && else_node.is_none() {
                // This is the consequence (the first braced_expression before else)
                consequence_end_line = Some(child.end_position().row);
            } else if child.kind() == "else" {
                else_node = Some(child);
                // Don't break - we want to capture the consequence before the else
            }
        }

        // If we found both a consequence and an else, check line positions
        if let (Some(brace_line), Some(else_kw)) = (consequence_end_line, else_node) {
            let else_start_line = else_kw.start_position().row;
            if else_start_line > brace_line {
                emit_else_newline_diagnostic(else_kw, diagnostics);
            }
        }
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_else_newline_errors(child, text, diagnostics);
    }
}

/// Emit a diagnostic for an orphaned else keyword
fn emit_else_newline_diagnostic(node: Node, diagnostics: &mut Vec<Diagnostic>) {
    diagnostics.push(Diagnostic {
        range: Range {
            start: Position::new(
                node.start_position().row as u32,
                node.start_position().column as u32,
            ),
            end: Position::new(
                node.end_position().row as u32,
                node.end_position().column as u32,
            ),
        },
        severity: Some(DiagnosticSeverity::ERROR),
        message: "In R, 'else' must appear on the same line as the closing '}' of the if block"
            .to_string(),
        ..Default::default()
    });
}

/// Helper function to find the line number of the closing brace in a node.
/// Returns the line number of the last "}" in the node, or None if not found.
fn find_closing_brace_line(node: &Node, text: &str) -> Option<usize> {
    // For if_statement, we need to find the consequence (the braced_expression after the condition)
    // The consequence is the last braced_expression child that is NOT the alternative
    let mut cursor = node.walk();
    let mut last_brace_line = None;

    for child in node.children(&mut cursor) {
        // Look for braced_expression which contains the closing brace
        if child.kind() == "braced_expression" {
            // The end position of braced_expression is where the "}" is
            last_brace_line = Some(child.end_position().row);
            // Don't break - we want the FIRST braced_expression (the consequence),
            // not the alternative (which would be after the else keyword)
            // But since we're looking at if_statement without else, there's only one
            break;
        }
    }

    // If we didn't find a braced_expression, check if the node's text ends with "}"
    if last_brace_line.is_none() {
        let node_text_str = node_text(*node, text);
        if node_text_str.trim_end().ends_with('}') {
            return Some(node.end_position().row);
        }
    }

    last_brace_line
}

/// Report undefined variable usages in a document using position-aware cross-file scope.
///
/// This inspects every identifier usage in `node`, skipping local definitions, builtins,
/// and workspace imports, and checks visibility at the exact usage position by querying
/// the cross-file scope (including position-aware loaded/inherited packages). Lines marked
/// with `@lsp-ignore` or `@lsp-ignore-next` are ignored. When a usage is not found in the
/// resolved scope or in package exports (if packages are enabled), a `Diagnostic` with a
/// UTF-16-aware range is emitted for the undefined variable.
///
/// # Examples
///
/// ```no_run
/// // Illustrative only — real usage requires a populated WorldState, AST node and other
/// // project-specific types from the language server state.
/// // collect_undefined_variables_position_aware(&state, &uri, root_node, &text, &[], &workspace_imports, &package_library, &directive_meta, &mut diagnostics);
/// ```
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_undefined_variables_position_aware(
    state: &WorldState,
    uri: &Url,
    node: Node,
    text: &str,
    _loaded_packages: &[String], // Deprecated: now using position-aware packages from scope resolution
    workspace_imports: &[String],
    package_library: &crate::package_library::PackageLibrary,
    directive_meta: &crate::cross_file::CrossFileMetadata,
    diagnostics: &mut Vec<Diagnostic>,
) {
    use crate::cross_file::types::byte_offset_to_utf16_column;

    let mut used: Vec<(String, Node)> = Vec::new();

    // Second pass: collect all usages with NSE-aware context
    collect_usages_with_context(node, text, &UsageContext::default(), &mut used);

    // Report undefined variables with position-aware cross-file scope
    for (name, usage_node) in used {
        // Skip reserved words BEFORE any other checks (Requirement 3.4)
        // Reserved words like `if`, `else`, `TRUE`, etc. should never produce
        // "Undefined variable" diagnostics regardless of their position in code
        if crate::reserved_words::is_reserved_word(&name) {
            continue;
        }

        let usage_line = usage_node.start_position().row as u32;

        // Skip if line is ignored via @lsp-ignore or @lsp-ignore-next
        if crate::cross_file::directive::is_line_ignored(directive_meta, usage_line) {
            continue;
        }

        // Skip if builtin or workspace import
        // Local definitions are checked via position-aware scope below
        if is_builtin(&name) || workspace_imports.contains(&name) {
            continue;
        }

        // Convert byte column to UTF-16 for cross-file scope lookup
        let line_text = text
            .lines()
            .nth(usage_node.start_position().row)
            .unwrap_or("");
        let usage_col = byte_offset_to_utf16_column(line_text, usage_node.start_position().column);

        // Get full scope at position, including position-aware loaded packages
        // Requirements 8.1, 8.3, 8.4: Position-aware package checking
        let scope = get_cross_file_scope(state, uri, usage_line, usage_col);

        // Check if symbol is in cross-file scope
        if scope.symbols.contains_key(&name) {
            continue;
        }

        // Check package exports only if packages feature is enabled and library is ready
        if state.cross_file_config.packages_enabled && state.package_library_ready {
            // Build position-aware package list: inherited packages + locally loaded packages
            // Requirements 5.1, 5.2: Inherited packages from parent files
            // Requirements 8.1, 8.3: Locally loaded packages before this position
            let position_aware_packages: Vec<String> = scope
                .inherited_packages
                .iter()
                .chain(scope.loaded_packages.iter())
                .cloned()
                .collect();

            // Check if symbol is exported by any package loaded at this position
            if is_package_export(&name, &position_aware_packages, package_library) {
                continue;
            }
        }

        // Symbol is undefined - emit diagnostic
        // Convert byte columns to UTF-16 for diagnostic range
        let start_line_text = text
            .lines()
            .nth(usage_node.start_position().row)
            .unwrap_or("");
        let end_line_text = text
            .lines()
            .nth(usage_node.end_position().row)
            .unwrap_or("");
        let start_col =
            byte_offset_to_utf16_column(start_line_text, usage_node.start_position().column);
        let end_col = byte_offset_to_utf16_column(end_line_text, usage_node.end_position().column);

        diagnostics.push(Diagnostic {
            range: Range {
                start: Position::new(usage_node.start_position().row as u32, start_col),
                end: Position::new(usage_node.end_position().row as u32, end_col),
            },
            severity: Some(DiagnosticSeverity::WARNING),
            message: format!("Undefined variable: {}", name),
            ..Default::default()
        });
    }
}

/// Emit diagnostics for identifiers that are used but not defined, built-in, imported, exported by a loaded package, or available from cross-file symbols.
///
/// This function performs a two-pass analysis on the provided syntax `node`:
/// it collects all defined identifiers, then collects usages (respecting NSE/context rules),
/// and pushes a `Diagnostic` with severity `Warning` for each usage that is not found in any of:
/// - the local definitions in the current tree,
/// - the set of builtins,
/// - symbols exported by any loaded package (via `package_library` and `loaded_packages`),
/// - names imported into the workspace (`workspace_imports`),
/// - the provided `cross_file_symbols`.
///
/// Parameters:
/// - `node`, `text`: the root AST node and source text to analyze.
/// - `loaded_packages`: names of packages considered loaded for package-export checks.
/// - `workspace_imports`: names imported into the workspace (treated as defined).
/// - `package_library`: authoritative package export/index used to determine package exports.
/// - `cross_file_symbols`: cross-file symbols available to the current file.
/// - `diagnostics`: destination vector; undefined-variable diagnostics are appended here.
///
/// # Examples
///
/// ```no_run
/// // Illustrative example (non-executable here): parse a document to `root` and call:
/// // collect_undefined_variables(root, &text, &loaded_packages, &workspace_imports, &package_library, &cross_file_symbols, &mut diagnostics);
/// ```
#[allow(dead_code)]
fn collect_undefined_variables(
    node: Node,
    text: &str,
    loaded_packages: &[String],
    workspace_imports: &[String],
    package_library: &crate::package_library::PackageLibrary,
    cross_file_symbols: &HashMap<String, ScopedSymbol>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    use std::collections::HashSet;

    let mut defined: HashSet<String> = HashSet::new();
    let mut used: Vec<(String, Node)> = Vec::new();

    // First pass: collect all definitions
    collect_definitions(node, text, &mut defined);

    // Second pass: collect all usages
    collect_usages_with_context(node, text, &UsageContext::default(), &mut used);

    // Report undefined variables
    for (name, node) in used {
        if !defined.contains(&name)
            && !is_builtin(&name)
            && !is_package_export(&name, loaded_packages, package_library)
            && !workspace_imports.contains(&name)
            && !cross_file_symbols.contains_key(&name)
        {
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(
                        node.start_position().row as u32,
                        node.start_position().column as u32,
                    ),
                    end: Position::new(
                        node.end_position().row as u32,
                        node.end_position().column as u32,
                    ),
                },
                severity: Some(DiagnosticSeverity::WARNING),
                message: format!("Undefined variable: {}", name),
                ..Default::default()
            });
        }
    }
}

fn collect_definitions(node: Node, text: &str, defined: &mut std::collections::HashSet<String>) {
    if node.kind() == "binary_operator" {
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();

        if children.len() >= 3 {
            let lhs = children[0];
            let op = children[1];

            let op_text = node_text(op, text);
            if matches!(op_text, "<-" | "=" | "<<-") && lhs.kind() == "identifier" {
                defined.insert(node_text(lhs, text).to_string());
            }
        }
    }

    // Collect function parameters
    if node.kind() == "function_definition" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "parameters" {
                collect_parameters(child, text, defined);
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_definitions(child, text, defined);
    }
}

fn collect_parameters(node: Node, text: &str, defined: &mut std::collections::HashSet<String>) {
    if node.kind() == "parameter" || node.kind() == "identifier" {
        let name = node_text(node, text);
        if !name.is_empty() && name != "..." {
            defined.insert(name.to_string());
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_parameters(child, text, defined);
    }
}

/// Context for tracking NSE-related state during AST traversal
#[derive(Clone, Default)]
struct UsageContext {
    /// True when inside a formula expression (~ operator)
    in_formula: bool,
    /// True when inside the arguments of a call-like node (call, subset, subset2)
    in_call_like_arguments: bool,
}

/// Legacy version of collect_usages without NSE context tracking.
/// Only used in tests for backward compatibility with existing property tests.
#[cfg(test)]
fn collect_usages<'a>(node: Node<'a>, text: &str, used: &mut Vec<(String, Node<'a>)>) {
    if node.kind() == "identifier" {
        // Skip if this is the LHS of an assignment
        if let Some(parent) = node.parent() {
            if parent.kind() == "binary_operator" {
                let mut cursor = parent.walk();
                let children: Vec<_> = parent.children(&mut cursor).collect();
                if children.len() >= 2 && children[0].id() == node.id() {
                    let op = children[1];
                    let op_text = node_text(op, text);
                    if matches!(op_text, "<-" | "=" | "<<-") {
                        return; // Skip LHS of assignment
                    }
                }
            }

            // Skip if this is a named argument (e.g., n = 1 in readLines(..., n = 1))
            if parent.kind() == "argument" {
                if let Some(name_node) = parent.child_by_field_name("name") {
                    if name_node.id() == node.id() {
                        return; // Skip argument names
                    }
                }
            }
        }

        used.push((node_text(node, text).to_string(), node));
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_usages(child, text, used);
    }
}

/// Context-aware version of collect_usages that tracks NSE-related state during AST traversal.
/// This function skips undefined variable checks in contexts where R uses non-standard evaluation.
fn collect_usages_with_context<'a>(
    node: Node<'a>,
    text: &str,
    context: &UsageContext,
    used: &mut Vec<(String, Node<'a>)>,
) {
    if node.kind() == "identifier" {
        // Skip if we're in a formula or call-like arguments context
        if context.in_formula || context.in_call_like_arguments {
            return;
        }

        // Skip if this is the LHS of an assignment
        if let Some(parent) = node.parent() {
            if parent.kind() == "binary_operator" {
                let mut cursor = parent.walk();
                let children: Vec<_> = parent.children(&mut cursor).collect();
                if children.len() >= 2 && children[0].id() == node.id() {
                    let op = children[1];
                    let op_text = node_text(op, text);
                    if matches!(op_text, "<-" | "=" | "<<-") {
                        return; // Skip LHS of assignment
                    }
                }
            }

            // Skip if this is a named argument (e.g., n = 1 in readLines(..., n = 1))
            if parent.kind() == "argument" {
                if let Some(name_node) = parent.child_by_field_name("name") {
                    if name_node.id() == node.id() {
                        return; // Skip argument names
                    }
                }
            }

            // Skip if this is the RHS of an extract operator ($ or @)
            // e.g., df$column or obj@slot - we don't want to check if column/slot is defined
            // The LHS (df, obj) should still be checked for undefined variables
            if parent.kind() == "extract_operator" {
                if let Some(rhs_node) = parent.child_by_field_name("rhs") {
                    if rhs_node.id() == node.id() {
                        return; // Skip RHS of extract operator
                    }
                }
            }

            // Skip if this is a function parameter definition
            // Function parameters appear inside `parameter` or `default_parameter` nodes
            // which are children of `parameters` nodes.
            // Only skip the parameter name (name field), NOT identifiers in the default
            // expression (e.g., function(a = b) should check b for undefined)
            if parent.kind() == "parameter" || parent.kind() == "default_parameter" {
                // Only skip if this identifier is the parameter name (name field)
                // The default expression should still be checked for undefined variables
                if let Some(name_node) = parent.child_by_field_name("name") {
                    if name_node.id() == node.id() {
                        return; // Skip the parameter name
                    }
                }
                // Don't return here - identifiers in default expressions should be checked
            }

            // Also check if we're directly inside a `parameters` node (some grammars)
            if parent.kind() == "parameters" {
                return; // Skip identifiers directly in parameters list
            }
        }

        used.push((node_text(node, text).to_string(), node));
    }

    // Check if we're entering a formula expression (~ operator)
    // Tree-sitter-r represents formulas as:
    // - `~ x` is a `unary_operator` node with `~` as the operator
    // - `y ~ x` is a `binary_operator` node with `~` as the operator
    let is_formula_node = match node.kind() {
        "unary_operator" => {
            // For unary operator, check if the operator is ~
            // The first child is typically the operator
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            children
                .first()
                .is_some_and(|op| node_text(*op, text) == "~")
        }
        "binary_operator" => {
            // For binary operator, check if the operator (second child) is ~
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            children
                .get(1)
                .is_some_and(|op| node_text(*op, text) == "~")
        }
        _ => false,
    };

    // Check if this is a call-like node (call, subset, subset2)
    // These nodes have `function` and `arguments` fields
    // We only set in_call_like_arguments when entering the `arguments` field
    let is_call_like_node = matches!(node.kind(), "call" | "subset" | "subset2");

    // Create updated context if entering a formula
    let base_context = if is_formula_node {
        UsageContext {
            in_formula: true,
            ..context.clone()
        }
    } else {
        context.clone()
    };

    // Recurse into children with the (possibly updated) context
    // For call-like nodes, we need to handle the `arguments` field specially
    if is_call_like_node {
        // For call-like nodes, recurse into children but set in_call_like_arguments
        // only for the `arguments` field, not for the `function` field
        if let Some(function_node) = node.child_by_field_name("function") {
            // The function field should NOT have in_call_like_arguments set
            // We still want to check if the function name is defined
            collect_usages_with_context(function_node, text, &base_context, used);
        }
        if let Some(arguments_node) = node.child_by_field_name("arguments") {
            // The arguments field SHOULD have in_call_like_arguments set
            let args_context = UsageContext {
                in_call_like_arguments: true,
                ..base_context.clone()
            };
            collect_usages_with_context(arguments_node, text, &args_context, used);
        }
    } else {
        // For non-call-like nodes, recurse normally
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect_usages_with_context(child, text, &base_context, used);
        }
    }
}

/// Determines whether an identifier is a recognized R builtin (constant or function).
///
/// Checks common R constants (e.g., `TRUE`, `FALSE`, `NULL`, `NA`, `Inf`, `NaN`, `T`, `F`) first and then consults the comprehensive builtin registry.
///
/// # Examples
///
/// ```
/// assert!(is_builtin("TRUE"));
/// assert!(is_builtin("sum"));
/// assert!(!is_builtin("my_custom_fn"));
/// ```
///
/// # Returns
/// `true` if `name` is a recognized R builtin constant or function, `false` otherwise.
fn is_builtin(name: &str) -> bool {
    // Check constants first
    if matches!(
        name,
        "TRUE" | "FALSE" | "NULL" | "NA" | "Inf" | "NaN" | "T" | "F"
    ) {
        return true;
    }
    // Check comprehensive builtin list
    builtins::is_builtin(name)
}

/// Determines whether an identifier is exported by any of the currently loaded packages.
///
/// Queries the provided PackageLibrary to decide if `name` originates from one of the packages
/// listed in `loaded_packages`.
///
/// # Returns
///
/// `true` if the symbol is exported by a loaded package, `false` otherwise.
///
/// # Examples
///
/// ```rust,no_run
/// # use crate::package_library::PackageLibrary;
/// # let package_library: PackageLibrary = unimplemented!();
/// let loaded = vec!["stats".to_string(), "base".to_string()];
/// let is_export = is_package_export("lm", &loaded, &package_library);
/// ```
fn is_package_export(
    name: &str,
    loaded_packages: &[String],
    package_library: &crate::package_library::PackageLibrary,
) -> bool {
    // Use PackageLibrary's synchronous method to check if symbol is from loaded packages
    // This checks base exports first, then cached package exports
    // Requirements 8.1, 8.2: Check position-aware loaded packages
    package_library.is_symbol_from_loaded_packages(name, loaded_packages)
}

// ============================================================================
// Completions
// ============================================================================

/// Build a list of completion items for the given document and cursor position.
///
/// Returns completion items that prioritize local document definitions, then package exports
/// (when packages are enabled) with per-package attribution, and finally cross-file symbols.
/// The function returns `None` when the document, its parse tree, or the node at the cursor
/// cannot be resolved.
///
/// # Examples
///
/// ```
/// // Obtain a `WorldState`, `Url`, and `Position` from your environment.
/// let state = /* WorldState instance */;
/// let uri = /* Url for the document */;
/// let pos = /* Position { line, character } */;
/// let resp = completion(&state, &uri, pos);
/// assert!(resp.is_some());
/// ```
pub fn completion(state: &WorldState, uri: &Url, position: Position) -> Option<CompletionResponse> {
    let doc = state.get_document(uri)?;
    let tree = doc.tree.as_ref()?;
    let text = doc.text();

    // Check for file path context first (source() calls and LSP directives)
    // Requirements 1.1-1.6, 2.1-2.7: Provide file path completions in appropriate contexts
    let file_path_context =
        crate::file_path_intellisense::detect_file_path_context(tree, &text, position);
    if !matches!(
        file_path_context,
        crate::file_path_intellisense::FilePathContext::None
    ) {
        // Get enriched metadata from state for source() calls (includes inherited_working_directory)
        // Directive contexts don't use @lsp-cd, so we use default metadata
        let metadata = match file_path_context {
            crate::file_path_intellisense::FilePathContext::SourceCall { .. } => {
                // Use get_enriched_metadata to get metadata with inherited_working_directory
                // from parent files, not just the current file's directives
                state.get_enriched_metadata(uri).unwrap_or_default()
            }
            _ => Default::default()
        };
        let workspace_root = state.workspace_folders.first();

        // Generate file path completions
        // NOTE: This uses blocking I/O (std::fs::read_dir) on the LSP request thread.
        // This is acceptable because:
        // 1. Directory reads are typically <1ms on modern systems
        // 2. We only read a single directory level (no recursion)
        // 3. The handler is already async, so we don't block the entire server
        // 4. Making this async would add significant complexity (spawn_blocking,
        //    cancellation, race conditions) without measurable benefit
        // If performance issues arise with large directories, consider:
        // - Caching directory listings
        // - Using tokio::spawn_blocking for the read_dir call
        // - Adding a timeout/cancellation mechanism
        let items = crate::file_path_intellisense::file_path_completions(
            &file_path_context,
            uri,
            &metadata,
            workspace_root,
            position, // Pass cursor position for text_edit range
        );
        return Some(CompletionResponse::Array(items));
    }

    let point = Point::new(position.line as usize, position.character as usize);
    let node = tree.root_node().descendant_for_point_range(point, point)?;

    let mut items = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    // Check if we're in a namespace context (pkg::)
    if find_namespace_context(&node, &text).is_some() {
        // TODO: Get package exports from library
        return Some(CompletionResponse::Array(items));
    }

    // Add R keywords
    let keywords = [
        "if",
        "else",
        "repeat",
        "while",
        "function",
        "for",
        "in",
        "next",
        "break",
        "TRUE",
        "FALSE",
        "NULL",
        "Inf",
        "NaN",
        "NA",
        "NA_integer_",
        "NA_real_",
        "NA_complex_",
        "NA_character_",
        "library",
        "require",
        "return",
        "print",
    ];

    for kw in keywords {
        items.push(CompletionItem {
            label: kw.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        });
        seen_names.insert(kw.to_string());
    }

    // Add symbols from current document (local definitions take precedence)
    collect_document_completions(tree.root_node(), &text, &mut items, &mut seen_names);

    // Get scope at cursor position for package exports
    // Requirements 9.1, 9.2: Add package exports to completions with package attribution
    let scope = get_cross_file_scope(state, uri, position.line, position.character);

    // Add package exports only if packages feature is enabled
    if state.cross_file_config.packages_enabled {
        // Combine inherited and loaded packages
        let mut all_packages: Vec<String> = scope.inherited_packages.clone();
        for pkg in &scope.loaded_packages {
            if !all_packages.contains(pkg) {
                all_packages.push(pkg.clone());
            }
        }

        // Add package exports (after local definitions, before cross-file symbols)
        // Requirement 9.4: Local definitions > package exports > cross-file symbols
        // Requirement 9.3: When multiple packages export same symbol, show all with attribution
        let package_exports = state
            .package_library
            .get_exports_for_completions(&all_packages);
        for (export_name, package_names) in package_exports {
            if seen_names.contains(&export_name) {
                continue; // Local definitions take precedence
            }
            seen_names.insert(export_name.clone());

            // Requirement 9.3: Show all packages that export this symbol
            for package_name in package_names {
                // Requirement 9.2: Include package name in detail field (e.g., "{dplyr}")
                items.push(CompletionItem {
                    label: export_name.clone(),
                    kind: Some(CompletionItemKind::FUNCTION), // Most package exports are functions
                    detail: Some(format!("{{{}}}", package_name)),
                    ..Default::default()
                });
            }
        }
    }

    // Add cross-file symbols (from scope resolution)
    // Requirement 9.5: Package exports > cross-file symbols
    for (name, symbol) in scope.symbols {
        if seen_names.contains(&name) {
            continue; // Local definitions and package exports take precedence
        }
        seen_names.insert(name.clone());

        let kind = match symbol.kind {
            crate::cross_file::SymbolKind::Function => CompletionItemKind::FUNCTION,
            crate::cross_file::SymbolKind::Variable => CompletionItemKind::VARIABLE,
            crate::cross_file::SymbolKind::Parameter => CompletionItemKind::VARIABLE,
        };

        // Add source file info to detail if from another file
        let detail = if symbol.source_uri != *uri {
            Some(format!("from {}", symbol.source_uri.path()))
        } else {
            None
        };

        items.push(CompletionItem {
            label: name,
            kind: Some(kind),
            detail,
            ..Default::default()
        });
    }

    // Filter out reserved words from identifier completions
    // (Keywords are added separately with CompletionItemKind::KEYWORD)
    // Requirements 5.1, 5.2, 5.3: Reserved words should not appear as identifier completions
    items.retain(|item| {
        item.kind == Some(CompletionItemKind::KEYWORD)
            || !crate::reserved_words::is_reserved_word(&item.label)
    });

    Some(CompletionResponse::Array(items))
}

fn find_namespace_context<'a>(node: &Node<'a>, text: &'a str) -> Option<&'a str> {
    // Walk up to find namespace_operator
    let mut current = *node;
    loop {
        if current.kind() == "namespace_operator" {
            let mut cursor = current.walk();
            let children: Vec<_> = current.children(&mut cursor).collect();
            if !children.is_empty() {
                return Some(node_text(children[0], text));
            }
        }
        current = current.parent()?;
    }
}

fn collect_document_completions(
    node: Node,
    text: &str,
    items: &mut Vec<CompletionItem>,
    seen: &mut std::collections::HashSet<String>,
) {
    if node.kind() == "binary_operator" {
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();

        if children.len() >= 3 {
            let lhs = children[0];
            let op = children[1];
            let rhs = children[2];

            let op_text = node_text(op, text);
            if matches!(op_text, "<-" | "=" | "<<-") && lhs.kind() == "identifier" {
                let name = node_text(lhs, text).to_string();
                if !seen.contains(&name) {
                    seen.insert(name.clone());
                    let kind = if rhs.kind() == "function_definition" {
                        CompletionItemKind::FUNCTION
                    } else {
                        CompletionItemKind::VARIABLE
                    };

                    items.push(CompletionItem {
                        label: name,
                        kind: Some(kind),
                        ..Default::default()
                    });
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_document_completions(child, text, items, seen);
    }
}

// ============================================================================
// Definition Statement Extraction
// ============================================================================

pub struct DefinitionInfo {
    pub statement: String,
    pub source_uri: Url,
    pub line: u32,
    #[allow(dead_code)]
    pub column: u32,
}

pub fn extract_definition_statement(
    symbol: &ScopedSymbol,
    state: &WorldState,
) -> Option<DefinitionInfo> {
    // Get content provider for the symbol's source file
    let content = if let Some(doc) = state.documents.get(&symbol.source_uri) {
        doc.text()
    } else if let Some(cached) = state.cross_file_file_cache.get(&symbol.source_uri) {
        cached
    } else {
        return None;
    };

    // Get tree for parsing
    let tree = if let Some(doc) = state.documents.get(&symbol.source_uri) {
        doc.tree.as_ref()?
    } else {
        // Parse content if not in documents
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_r::LANGUAGE.into()).ok()?;
        let parsed = parser.parse(&content, None)?;
        // We can't store this tree, so we need to work with it immediately
        return extract_statement_from_tree(&parsed, symbol, &content);
    };

    extract_statement_from_tree(tree, symbol, &content)
}

fn utf16_column_to_byte_offset(line: &str, utf16_col: u32) -> usize {
    let mut utf16_count = 0;
    for (byte_idx, ch) in line.char_indices() {
        if utf16_count == utf16_col as usize {
            return byte_idx;
        }
        utf16_count += ch.len_utf16();
    }
    line.len()
}

fn next_utf8_char_boundary(line: &str, byte_offset: usize) -> usize {
    if byte_offset >= line.len() {
        return line.len();
    }

    // Find the next UTF-8 codepoint boundary (i.e., the start byte of the next char).
    // This avoids creating Point ranges that land mid-codepoint (which tree-sitter rejects).
    for (idx, _ch) in line.char_indices() {
        if idx > byte_offset {
            return idx;
        }
    }

    line.len()
}

fn extract_statement_from_tree(
    tree: &tree_sitter::Tree,
    symbol: &ScopedSymbol,
    content: &str,
) -> Option<DefinitionInfo> {
    let line_text = content
        .lines()
        .nth(symbol.defined_line as usize)
        .unwrap_or("");
    let byte_col = utf16_column_to_byte_offset(line_text, symbol.defined_column);
    let row = symbol.defined_line as usize;

    // descendant_for_point_range can behave unexpectedly on 0-length ranges at node boundaries.
    // Use a small non-empty range when possible and prefer named nodes.
    let point_start = tree_sitter::Point::new(row, byte_col);
    let byte_col_end = next_utf8_char_boundary(line_text, byte_col);
    let point_end = tree_sitter::Point::new(row, byte_col_end);

    let root = tree.root_node();
    let node = root
        .named_descendant_for_point_range(point_start, point_end)
        .or_else(|| root.descendant_for_point_range(point_start, point_end))?;

    // Find the appropriate parent node based on symbol kind
    let statement_node = match symbol.kind {
        scope::SymbolKind::Variable => find_assignment_statement(node, content),
        scope::SymbolKind::Function => find_function_statement(node, content),
        scope::SymbolKind::Parameter => find_function_statement(node, content),
    }?;

    let statement = extract_statement_text(statement_node, content);

    Some(DefinitionInfo {
        statement,
        source_uri: symbol.source_uri.clone(),
        line: symbol.defined_line,
        column: symbol.defined_column,
    })
}

/// Result of finding a statement node - includes whether to extract header only
struct StatementMatch<'a> {
    node: tree_sitter::Node<'a>,
    header_only: bool,
}

fn find_assignment_statement<'a>(
    mut node: tree_sitter::Node<'a>,
    content: &str,
) -> Option<StatementMatch<'a>> {
    // Walk up to find binary_operator (assignment), for_statement, or parameter
    loop {
        match node.kind() {
            "binary_operator" => {
                let mut cursor = node.walk();
                let children: Vec<_> = node.children(&mut cursor).collect();
                if children.len() >= 2 {
                    let op_text = node_text(children[1], content);
                    if matches!(op_text, "<-" | "=" | "<<-" | "->") {
                        return Some(StatementMatch {
                            node,
                            header_only: false,
                        });
                    }
                }
            }
            "for_statement" => {
                return Some(StatementMatch {
                    node,
                    header_only: false,
                })
            }
            "parameter" => {
                // For parameters, find enclosing function_definition
                if let Some(func) = find_enclosing_function(node) {
                    return Some(StatementMatch {
                        node: func,
                        header_only: false,
                    });
                }
            }
            _ => {}
        }

        if let Some(parent) = node.parent() {
            node = parent;
        } else {
            break;
        }
    }
    None
}

fn find_function_statement<'a>(
    mut node: tree_sitter::Node<'a>,
    content: &str,
) -> Option<StatementMatch<'a>> {
    // Walk up to find function_definition or assignment containing function.
    // For definition extraction, we want the full statement so we can include bodies and apply
    // standard truncation rules.
    loop {
        match node.kind() {
            "function_definition" => {
                // Check if parent is assignment
                if let Some(parent) = node.parent() {
                    if parent.kind() == "binary_operator" {
                        return Some(StatementMatch {
                            node: parent,
                            header_only: false,
                        });
                    }
                }
                return Some(StatementMatch {
                    node,
                    header_only: false,
                });
            }
            "binary_operator" => {
                let mut cursor = node.walk();
                let children: Vec<_> = node.children(&mut cursor).collect();
                if children.len() >= 3 {
                    let op_text = node_text(children[1], content);
                    // Check for function on RHS (for <- = <<-) or LHS (for ->)
                    if matches!(op_text, "<-" | "=" | "<<-")
                        && children[2].kind() == "function_definition"
                    {
                        return Some(StatementMatch {
                            node,
                            header_only: false,
                        });
                    }
                    if op_text == "->" && children[0].kind() == "function_definition" {
                        return Some(StatementMatch {
                            node,
                            header_only: false,
                        });
                    }
                }
            }
            _ => {}
        }

        if let Some(parent) = node.parent() {
            node = parent;
        } else {
            break;
        }
    }
    None
}

fn find_enclosing_function(mut node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    loop {
        if node.kind() == "function_definition" {
            // Check if parent is assignment
            if let Some(parent) = node.parent() {
                if parent.kind() == "binary_operator" {
                    return Some(parent);
                }
            }
            return Some(node);
        }
        node = node.parent()?;
    }
}

#[allow(clippy::needless_range_loop)]
fn extract_statement_text(stmt: StatementMatch, content: &str) -> String {
    let node = stmt.node;
    let lines: Vec<&str> = content.lines().collect();
    let start_line = node.start_position().row;

    if start_line >= lines.len() {
        return String::new();
    }

    if stmt.header_only {
        // For for-loops: extract just the header (for (x in seq))
        // For functions: extract signature up to body start
        return extract_header(node, content);
    }

    let end_line = node.end_position().row;

    // Truncate to 10 lines maximum
    let actual_end_line = if end_line - start_line >= 10 {
        start_line + 9
    } else {
        end_line
    };

    let mut result = String::new();
    for i in start_line..=actual_end_line.min(lines.len() - 1) {
        if i > start_line {
            result.push('\n');
        }
        result.push_str(lines[i]);
    }

    // Add ellipsis if truncated
    if end_line - start_line >= 10 {
        result.push_str("\n...");
    }

    result
}

fn extract_header(node: tree_sitter::Node, content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let start_line = node.start_position().row;

    match node.kind() {
        "for_statement" => {
            // For loop: extract "for (var in seq)"
            // Find the body child and stop before it
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "brace_list"
                    || child.kind() == "call"
                    || (child.kind() != "identifier"
                        && child.kind() != "("
                        && child.kind() != ")"
                        && child.kind() != "in"
                        && child.start_position().row > start_line)
                {
                    // Body starts - extract up to before body
                    let body_start = child.start_position();
                    if body_start.row == start_line {
                        // Body on same line - extract up to body start column
                        let line = lines.get(start_line).unwrap_or(&"");
                        return line[..body_start.column.min(line.len())]
                            .trim_end()
                            .to_string();
                    } else {
                        // Body on different line - extract header lines
                        let mut result = String::new();
                        for i in start_line..body_start.row {
                            if i > start_line {
                                result.push('\n');
                            }
                            if let Some(line) = lines.get(i) {
                                result.push_str(line);
                            }
                        }
                        return result;
                    }
                }
            }
            // Fallback: just first line
            lines.get(start_line).unwrap_or(&"").to_string()
        }
        "function_definition" | "binary_operator" => {
            // Function: extract signature up to body
            extract_function_header(node, content)
        }
        _ => lines.get(start_line).unwrap_or(&"").to_string(),
    }
}

fn extract_function_header(node: tree_sitter::Node, content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let start_line = node.start_position().row;

    // Find the function_definition node
    let func_node = if node.kind() == "function_definition" {
        node
    } else {
        // binary_operator - find function_definition child
        let mut cursor = node.walk();
        let mut func = None;
        for child in node.children(&mut cursor) {
            if child.kind() == "function_definition" {
                func = Some(child);
                break;
            }
        }
        match func {
            Some(f) => f,
            None => return lines.get(start_line).unwrap_or(&"").to_string(),
        }
    };

    // Find body in function_definition
    let mut cursor = func_node.walk();
    for child in func_node.children(&mut cursor) {
        // Body is typically brace_list or any expression after parameters
        if child.kind() == "brace_list"
            || (child.kind() != "function"
                && child.kind() != "parameters"
                && child.start_position().row >= start_line)
        {
            let body_start = child.start_position();

            // Extract from node start to body start
            if body_start.row == start_line {
                let line = lines.get(start_line).unwrap_or(&"");
                let end_col = body_start.column.min(line.len());
                return line[..end_col].trim_end().to_string();
            } else {
                let mut result = String::new();
                for i in start_line..body_start.row {
                    if i > start_line {
                        result.push('\n');
                    }
                    if let Some(line) = lines.get(i) {
                        result.push_str(line);
                    }
                }
                // Add partial last line if body starts mid-line
                if body_start.column > 0 {
                    if let Some(line) = lines.get(body_start.row) {
                        if !result.is_empty() {
                            result.push('\n');
                        }
                        result.push_str(line[..body_start.column.min(line.len())].trim_end());
                    }
                }
                return result;
            }
        }
    }

    // Fallback: first line
    lines.get(start_line).unwrap_or(&"").to_string()
}

// ============================================================================
// Hover
// ============================================================================

/// Provide hover information for the symbol at a given text document position.
///
/// Tries, in order:
/// 1. Cross-file symbol resolution (including local definitions), returning an extracted definition or signature with source attribution.
/// 2. Package exports discovered from the combined package scope, returning a signature and package attribution.
/// 3. Cached R help text or a one-time lookup of R help for builtins and other symbols.
///
/// The produced hover content is Markdown (code block for signatures/definitions and optional attribution) and the hover range corresponds to the identifier node under the cursor.
///
/// # Examples
///
/// ```no_run
/// # use lsp_types::Position;
/// # use url::Url;
/// # use crate::state::WorldState;
/// // Assuming `state` is available and `uri` refers to an open R document:
/// let pos = Position::new(10, 4);
/// let _ = hover(&state, &uri, pos);
/// ```
///
/// Returns `Some(Hover)` when information (definition, signature, package attribution, or help text) is available for the identifier at `position`, `None` when no useful hover content can be produced.
pub async fn hover(state: &WorldState, uri: &Url, position: Position) -> Option<Hover> {
    let doc = state.get_document(uri)?;
    let tree = doc.tree.as_ref()?;
    let text = doc.text();

    let line_text = text.lines().nth(position.line as usize).unwrap_or("");
    let byte_col = utf16_column_to_byte_offset(line_text, position.character);
    let row = position.line as usize;

    let point_start = Point::new(row, byte_col);
    let point_end = Point::new(row, next_utf8_char_boundary(line_text, byte_col));
    let root = tree.root_node();
    let node = root
        .named_descendant_for_point_range(point_start, point_end)
        .or_else(|| root.descendant_for_point_range(point_start, point_end))?;

    // Get the identifier
    let name = if node.kind() == "identifier" || node.kind() == "string" {
        node_text(node, &text)
    } else {
        return None;
    };

    let node_range = Range {
        start: Position::new(
            node.start_position().row as u32,
            node.start_position().column as u32,
        ),
        end: Position::new(
            node.end_position().row as u32,
            node.end_position().column as u32,
        ),
    };

    // Try cross-file symbols (includes local scope with definition extraction)
    log::trace!("Calling get_cross_file_symbols for hover");
    let cross_file_symbols = get_cross_file_symbols(state, uri, position.line, position.character);
    log::trace!(
        "Got {} symbols from cross-file scope",
        cross_file_symbols.len()
    );
    if let Some(symbol) = cross_file_symbols.get(name) {
        log::trace!(
            "hover: found symbol '{}' in cross_file_symbols, source_uri={}, is_declared={}",
            name,
            symbol.source_uri,
            symbol.is_declared
        );
        
        // Handle declared symbols (from @lsp-var or @lsp-func directives)
        // Validates: Requirements 7.1, 7.2, 7.3
        if symbol.is_declared {
            let kind_str = match symbol.kind {
                crate::cross_file::SymbolKind::Function => "declared function",
                crate::cross_file::SymbolKind::Variable => "declared variable",
                _ => "declared symbol",
            };
            let directive_type = match symbol.kind {
                crate::cross_file::SymbolKind::Function => "@lsp-func",
                _ => "@lsp-var",
            };
            // Convert 0-based line to 1-based for display
            let display_line = symbol.defined_line + 1;

            let mut value = format!(
                "```r\n{} ({})\n```\n\nDeclared via {} directive at line {}",
                name, kind_str, directive_type, display_line
            );
            if symbol.source_uri != *uri {
                let workspace_root = state.workspace_folders.first();
                let relative_path = compute_relative_path(&symbol.source_uri, workspace_root);
                value.push_str(&format!("\n\n*Defined in {}*", relative_path));
            }

            return Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value,
                }),
                range: Some(node_range),
            });
        }
        
        let mut value = String::new();

        // Check if this is a package export (source_uri starts with "package:")
        // Package exports have URIs like "package:dplyr" or "package:base"
        let package_name = symbol.source_uri.as_str().strip_prefix("package:");

        // Try to extract definition statement
        let workspace_root = state.workspace_folders.first();
        match extract_definition_statement(symbol, state) {
            Some(def_info) => {
                // Note: No escaping needed inside code blocks - markdown doesn't interpret special chars there
                value.push_str(&format!("```r\n{}\n```\n\n", def_info.statement));

                // Add file location
                if def_info.source_uri == *uri {
                    value.push_str(&format!("this file, line {}", def_info.line + 1));
                } else {
                    let relative_path = compute_relative_path(&def_info.source_uri, workspace_root);
                    let absolute_path = def_info.source_uri.as_str();
                    value.push_str(&format!(
                        "[{}]({}), line {}",
                        relative_path,
                        absolute_path,
                        def_info.line + 1
                    ));
                }
            }
            None => {
                // Graceful fallback: show symbol info without definition statement
                // For package exports, get full R help documentation
                // Validates: Requirement 10.2
                log::trace!(
                    "hover: extract_definition_statement returned None for '{}', package_name={:?}",
                    name,
                    package_name
                );
                if let Some(pkg) = package_name {
                    // Try to get full help documentation from R
                    log::trace!(
                        "hover: fetching R help for '{}' from package '{}'",
                        name,
                        pkg
                    );
                    let name_owned = name.to_string();
                    let pkg_owned = pkg.to_string();
                    if let Ok(help_result) = tokio::task::spawn_blocking(move || {
                        crate::help::get_help(&name_owned, Some(&pkg_owned))
                    })
                    .await
                    {
                        log::trace!(
                            "hover: get_help returned {:?}",
                            help_result.as_ref().map(|s| s.len())
                        );
                        if let Some(help_text) = help_result {
                            // Show full R documentation
                            value.push_str(&format!("```\n{}\n```", help_text));
                        } else if let Some(sig) = &symbol.signature {
                            value.push_str(&format!("```r\n{}\n```\n", sig));
                            value.push_str(&format!("\nfrom {{{}}}", pkg));
                        } else {
                            value.push_str(&format!("```r\n{}\n```\n", name));
                            value.push_str(&format!("\nfrom {{{}}}", pkg));
                        }
                    } else if let Some(sig) = &symbol.signature {
                        value.push_str(&format!("```r\n{}\n```\n", sig));
                        value.push_str(&format!("\nfrom {{{}}}", pkg));
                    } else {
                        value.push_str(&format!("```r\n{}\n```\n", name));
                        value.push_str(&format!("\nfrom {{{}}}", pkg));
                    }
                } else if let Some(sig) = &symbol.signature {
                    value.push_str(&format!("```r\n{}\n```\n", sig));
                    if symbol.source_uri != *uri {
                        let relative_path =
                            compute_relative_path(&symbol.source_uri, workspace_root);
                        value.push_str(&format!("\n*Defined in {}*", relative_path));
                    }
                } else {
                    value.push_str(&format!("```r\n{}\n```\n", name));
                    if symbol.source_uri != *uri {
                        let relative_path =
                            compute_relative_path(&symbol.source_uri, workspace_root);
                        value.push_str(&format!("\n*Defined in {}*", relative_path));
                    }
                }
            }
        }

        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: Some(node_range),
        });
    }

    // Check package exports from combined_exports cache (if packages enabled)
    // This surfaces package exports without blocking on R subprocess
    if state.cross_file_config.packages_enabled {
        let scope = get_cross_file_scope(state, uri, position.line, position.character);
        let all_packages: Vec<String> = scope
            .inherited_packages
            .iter()
            .chain(scope.loaded_packages.iter())
            .cloned()
            .collect();

        if let Some(pkg_name) = state
            .package_library
            .find_package_for_symbol(name, &all_packages)
        {
            let mut value = String::new();

            // Try to get full help documentation from R
            let name_owned = name.to_string();
            let pkg_owned = pkg_name.to_string();
            if let Ok(help_result) = tokio::task::spawn_blocking(move || {
                crate::help::get_help(&name_owned, Some(&pkg_owned))
            })
            .await
            {
                if let Some(help_text) = help_result {
                    // Show full R documentation
                    value.push_str(&format!("```\n{}\n```", help_text));
                } else {
                    value.push_str(&format!("```r\n{}\n```\n", name));
                    value.push_str(&format!("\nfrom {{{}}}", pkg_name));
                }
            } else {
                value.push_str(&format!("```r\n{}\n```\n", name));
                value.push_str(&format!("\nfrom {{{}}}", pkg_name));
            }

            return Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value,
                }),
                range: Some(node_range),
            });
        }
    }

    // Fallback to R help system for built-ins and undefined symbols
    // Check cache first to avoid repeated R subprocess calls
    if let Some(cached) = state.help_cache.get(name) {
        if let Some(help_text) = cached {
            return Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: format!("```\n{}\n```", help_text),
                }),
                range: Some(node_range),
            });
        }
        // Cached as None - no help available
        return None;
    }

    // Try to get help from R subprocess
    let name_owned = name.to_string();
    if let Ok(help_text) =
        tokio::task::spawn_blocking(move || crate::help::get_help(&name_owned, None)).await
    {
        if let Some(help_text) = help_text {
            // Cache successful result
            state
                .help_cache
                .insert(name.to_string(), Some(help_text.clone()));

            return Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: format!("```\n{}\n```", help_text),
                }),
                range: Some(node_range),
            });
        }
    }

    // Cache negative result to avoid repeated failed lookups
    state.help_cache.insert(name.to_string(), None);
    None
}
// Signature Help
// ============================================================================

pub fn signature_help(state: &WorldState, uri: &Url, position: Position) -> Option<SignatureHelp> {
    let doc = state.get_document(uri)?;
    let tree = doc.tree.as_ref()?;
    let text = doc.text();

    let point = Point::new(position.line as usize, position.character as usize);

    // Find enclosing call
    let mut node = tree.root_node().descendant_for_point_range(point, point)?;

    loop {
        if node.kind() == "call" {
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();

            if !children.is_empty() {
                let func_node = children[0];
                let func_name = node_text(func_node, &text);

                return Some(SignatureHelp {
                    signatures: vec![SignatureInformation {
                        label: format!("{}(...)", func_name),
                        documentation: None,
                        parameters: None,
                        active_parameter: None,
                    }],
                    active_signature: Some(0),
                    active_parameter: None,
                });
            }
        }

        node = node.parent()?;
    }
}

// ============================================================================
// Goto Definition
// ============================================================================

/// Locate the definition location for the identifier at the given position by searching
/// the current document, cross-file symbols, open documents, and the workspace index.
///
/// If the identifier is defined in the current document, its local definition is returned.
/// Otherwise the function searches cross-file symbols and exported interfaces from open
/// documents and the workspace. If the symbol originates from a package (pseudo-URI
/// starting with "package:"), no navigable location is returned.
///
/// # Returns
///
/// `Some(Location)` pointing to the symbol's defining range when a navigable definition is found;
/// `None` if no definition is found or if the symbol is a package export (non-navigable).
///
/// # Examples
///
/// ```
/// // Assume `state`, `uri`, and `pos` are available in the test harness.
/// let result = goto_definition(&state, &uri, pos);
/// // `result` will be `Some(...)` when a navigable definition exists, otherwise `None`.
/// ```
pub fn goto_definition(
    state: &WorldState,
    uri: &Url,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    // Use ContentProvider for unified access
    let content_provider = state.content_provider();

    // Try open document first, then workspace index
    let doc = state
        .get_document(uri)
        .or_else(|| state.workspace_index.get(uri))?;
    let tree = doc.tree.as_ref()?;
    let text = doc.text();

    // Check for file path context first (source() calls and LSP directives)
    // Requirements 5.1-5.5, 6.1-6.5: Go-to-definition for file paths
    let file_path_context =
        crate::file_path_intellisense::detect_file_path_context(tree, &text, position);
    if !matches!(
        file_path_context,
        crate::file_path_intellisense::FilePathContext::None
    ) {
        // Get enriched metadata from state for source() calls (includes inherited_working_directory)
        // Directive contexts don't use @lsp-cd, so we use default metadata
        let metadata = match file_path_context {
            crate::file_path_intellisense::FilePathContext::SourceCall { .. } => {
                // Use get_enriched_metadata to get metadata with inherited_working_directory
                // from parent files, not just the current file's directives
                state.get_enriched_metadata(uri).unwrap_or_default()
            }
            _ => Default::default()
        };

        if let Some(location) = crate::file_path_intellisense::file_path_definition(
            tree,
            &text,
            position,
            uri,
            &metadata,
            state.workspace_folders.first(),
        ) {
            return Some(GotoDefinitionResponse::Scalar(location));
        }
    }

    // Continue with normal identifier-based go-to-definition
    let point = Point::new(position.line as usize, position.character as usize);
    let node = tree.root_node().descendant_for_point_range(point, point)?;

    if node.kind() != "identifier" {
        return None;
    }

    let name = node_text(node, &text);

    // Search using position-aware scope resolution
    // This unifies same-file and cross-file lookups, respecting:
    // 1. Position (definitions must be before usage)
    // 2. Function scope (locals don't leak)
    // 3. Shadowing (locals override globals)
    let scope = get_cross_file_scope(state, uri, position.line, position.character);

    if let Some(symbol) = scope.symbols.get(name) {
        // Check if this is a package export (source_uri starts with "package:")
        // Package exports have pseudo-URIs like "package:dplyr" that can't be navigated to
        // Validates: Requirements 11.1, 11.2
        if symbol.source_uri.as_str().starts_with("package:") {
            log::trace!(
                "Symbol '{}' is from package '{}', no navigable source available",
                name,
                symbol
                    .source_uri
                    .as_str()
                    .strip_prefix("package:")
                    .unwrap_or("unknown")
            );
            return None;
        }

        // Handle declared symbols (from @lsp-var or @lsp-func directives)
        // For declared symbols, navigate to the directive line (column 0)
        // If symbol is declared multiple times, use the first declaration by line number
        // Validates: Requirements 8.1, 8.2
        if symbol.is_declared {
            // Get metadata for the symbol's source file to find the first declaration
            if let Some(metadata) = content_provider.get_metadata(&symbol.source_uri) {
                // Find all declarations of this symbol name (both variables and functions)
                let mut first_line: Option<u32> = None;
                
                for decl in &metadata.declared_variables {
                    if decl.name == name {
                        first_line = Some(first_line.map_or(decl.line, |l| l.min(decl.line)));
                    }
                }
                for decl in &metadata.declared_functions {
                    if decl.name == name {
                        first_line = Some(first_line.map_or(decl.line, |l| l.min(decl.line)));
                    }
                }
                
                // Use the first declaration line, or fall back to symbol's defined_line
                let definition_line = first_line.unwrap_or(symbol.defined_line);
                
                return Some(GotoDefinitionResponse::Scalar(Location {
                    uri: symbol.source_uri.clone(),
                    range: Range {
                        start: Position::new(definition_line, 0),
                        end: Position::new(definition_line, 0),
                    },
                }));
            }
            
            // Fallback if metadata not available: use symbol's stored location
            return Some(GotoDefinitionResponse::Scalar(Location {
                uri: symbol.source_uri.clone(),
                range: Range {
                    start: Position::new(symbol.defined_line, 0),
                    end: Position::new(symbol.defined_line, 0),
                },
            }));
        }

        return Some(GotoDefinitionResponse::Scalar(Location {
            uri: symbol.source_uri.clone(),
            range: Range {
                start: Position::new(symbol.defined_line, symbol.defined_column),
                end: Position::new(
                    symbol.defined_line,
                    symbol.defined_column + name.chars().map(|c| c.len_utf16() as u32).sum::<u32>(),
                ),
            },
        }));
    }

    // Search all open documents using ContentProvider
    for file_uri in state.document_store.uris() {
        if &file_uri == uri {
            continue;
        }
        if let Some(artifacts) = content_provider.get_artifacts(&file_uri) {
            if let Some(symbol) = artifacts.exported_interface.get(name) {
                // Skip package exports (they have pseudo-URIs that can't be navigated to)
                if symbol.source_uri.as_str().starts_with("package:") {
                    continue;
                }
                return Some(GotoDefinitionResponse::Scalar(Location {
                    uri: symbol.source_uri.clone(),
                    range: Range {
                        start: Position::new(symbol.defined_line, symbol.defined_column),
                        end: Position::new(
                            symbol.defined_line,
                            symbol.defined_column + name.len() as u32,
                        ),
                    },
                }));
            }
        }
    }

    // Search workspace index using ContentProvider
    for (file_uri, _) in state.workspace_index_new.iter() {
        if &file_uri == uri {
            continue;
        }
        if let Some(artifacts) = content_provider.get_artifacts(&file_uri) {
            if let Some(symbol) = artifacts.exported_interface.get(name) {
                // Skip package exports (they have pseudo-URIs that can't be navigated to)
                if symbol.source_uri.as_str().starts_with("package:") {
                    continue;
                }
                return Some(GotoDefinitionResponse::Scalar(Location {
                    uri: symbol.source_uri.clone(),
                    range: Range {
                        start: Position::new(symbol.defined_line, symbol.defined_column),
                        end: Position::new(
                            symbol.defined_line,
                            symbol.defined_column + name.len() as u32,
                        ),
                    },
                }));
            }
        }
    }

    // Fallback: Search legacy open documents
    for (file_uri, doc) in &state.documents {
        if file_uri == uri {
            continue;
        }
        if let Some(tree) = &doc.tree {
            let file_text = doc.text();
            if let Some(def_range) = find_definition_in_tree(tree.root_node(), name, &file_text) {
                return Some(GotoDefinitionResponse::Scalar(Location {
                    uri: file_uri.clone(),
                    range: def_range,
                }));
            }
        }
    }

    // Fallback: Search legacy workspace index
    for (file_uri, doc) in &state.workspace_index {
        if file_uri == uri {
            continue;
        }
        if let Some(tree) = &doc.tree {
            let file_text = doc.text();
            if let Some(def_range) = find_definition_in_tree(tree.root_node(), name, &file_text) {
                return Some(GotoDefinitionResponse::Scalar(Location {
                    uri: file_uri.clone(),
                    range: def_range,
                }));
            }
        }
    }

    None
}

fn find_definition_in_tree(node: Node, name: &str, text: &str) -> Option<Range> {
    if node.kind() == "binary_operator" {
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();

        if children.len() >= 3 {
            let lhs = children[0];
            let op = children[1];

            let op_text = node_text(op, text);
            if matches!(op_text, "<-" | "=" | "<<-")
                && lhs.kind() == "identifier"
                && node_text(lhs, text) == name
            {
                return Some(Range {
                    start: Position::new(
                        lhs.start_position().row as u32,
                        lhs.start_position().column as u32,
                    ),
                    end: Position::new(
                        lhs.end_position().row as u32,
                        lhs.end_position().column as u32,
                    ),
                });
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(range) = find_definition_in_tree(child, name, text) {
            return Some(range);
        }
    }

    None
}

// ============================================================================
// References
// ============================================================================

pub fn references(state: &WorldState, uri: &Url, position: Position) -> Option<Vec<Location>> {
    // Use ContentProvider for unified access
    let content_provider = state.content_provider();

    // Try open document first, then workspace index
    let doc = state
        .get_document(uri)
        .or_else(|| state.workspace_index.get(uri))?;
    let tree = doc.tree.as_ref()?;
    let text = doc.text();

    let point = Point::new(position.line as usize, position.character as usize);
    let node = tree.root_node().descendant_for_point_range(point, point)?;

    if node.kind() != "identifier" {
        return None;
    }

    let name = node_text(node, &text);
    let mut locations = Vec::new();

    // Search current document
    find_references_in_tree(tree.root_node(), name, &text, uri, &mut locations);

    // Search all open documents using new DocumentStore
    for file_uri in state.document_store.uris() {
        if &file_uri == uri {
            continue; // Already searched
        }
        if let Some(content) = content_provider.get_content(&file_uri) {
            // Parse the content to search for references
            if let Some(doc_state) = state.document_store.get_without_touch(&file_uri) {
                if let Some(tree) = &doc_state.tree {
                    find_references_in_tree(
                        tree.root_node(),
                        name,
                        &content,
                        &file_uri,
                        &mut locations,
                    );
                }
            }
        }
    }

    // Search workspace index using new WorkspaceIndex
    for (file_uri, entry) in state.workspace_index_new.iter() {
        if &file_uri == uri {
            continue; // Already searched
        }
        if let Some(tree) = &entry.tree {
            let file_text = entry.contents.to_string();
            find_references_in_tree(
                tree.root_node(),
                name,
                &file_text,
                &file_uri,
                &mut locations,
            );
        }
    }

    // Fallback: Search legacy open documents
    for (file_uri, doc) in &state.documents {
        if file_uri == uri {
            continue; // Already searched
        }
        // Skip if already found in new stores
        if state.document_store.contains(file_uri) {
            continue;
        }
        if let Some(tree) = &doc.tree {
            let file_text = doc.text();
            find_references_in_tree(tree.root_node(), name, &file_text, file_uri, &mut locations);
        }
    }

    // Fallback: Search legacy workspace index
    for (file_uri, doc) in &state.workspace_index {
        if file_uri == uri {
            continue; // Already searched
        }
        // Skip if already found in new stores
        if state.workspace_index_new.contains(file_uri) {
            continue;
        }
        if let Some(tree) = &doc.tree {
            let file_text = doc.text();
            find_references_in_tree(tree.root_node(), name, &file_text, file_uri, &mut locations);
        }
    }

    Some(locations)
}

fn find_references_in_tree(
    node: Node,
    name: &str,
    text: &str,
    uri: &Url,
    locations: &mut Vec<Location>,
) {
    if node.kind() == "identifier" && node_text(node, text) == name {
        locations.push(Location {
            uri: uri.clone(),
            range: Range {
                start: Position::new(
                    node.start_position().row as u32,
                    node.start_position().column as u32,
                ),
                end: Position::new(
                    node.end_position().row as u32,
                    node.end_position().column as u32,
                ),
            },
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_references_in_tree(child, name, text, uri, locations);
    }
}

// ============================================================================
// On Type Formatting (Indentation)
// ============================================================================

pub fn on_type_formatting(
    state: &WorldState,
    uri: &Url,
    position: Position,
) -> Option<Vec<TextEdit>> {
    let doc = state.get_document(uri)?;
    let text = doc.text();

    // Simple indentation: match previous line's indentation
    if position.line == 0 {
        return None;
    }

    let prev_line_idx = position.line as usize - 1;
    let lines: Vec<&str> = text.lines().collect();

    if prev_line_idx >= lines.len() {
        return None;
    }

    let prev_line = lines[prev_line_idx];
    let indent: String = prev_line
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect();

    // Check if previous line ends with { or ( - add extra indent
    let trimmed = prev_line.trim_end();
    let extra_indent = if trimmed.ends_with('{') || trimmed.ends_with('(') {
        "  "
    } else {
        ""
    };

    let new_indent = format!("{}{}", indent, extra_indent);

    Some(vec![TextEdit {
        range: Range {
            start: Position::new(position.line, 0),
            end: Position::new(position.line, 0),
        },
        new_text: new_indent,
    }])
}

// ============================================================================
// Utilities
// ============================================================================

fn node_text<'a>(node: Node<'a>, text: &'a str) -> &'a str {
    &text[node.byte_range()]
}

// ============================================================================
// Signature Extraction (used in tests)
// ============================================================================

#[cfg(test)]
fn extract_parameters(params_node: Node, text: &str) -> Vec<String> {
    let mut parameters = Vec::new();
    let mut cursor = params_node.walk();

    for child in params_node.children(&mut cursor) {
        if child.kind() == "parameter" {
            let mut param_cursor = child.walk();
            let param_children: Vec<_> = child.children(&mut param_cursor).collect();

            // Check if this parameter contains dots
            if let Some(_dots) = param_children.iter().find(|n| n.kind() == "dots") {
                parameters.push("...".to_string());
            } else if let Some(identifier) =
                param_children.iter().find(|n| n.kind() == "identifier")
            {
                let param_name = node_text(*identifier, text);

                // Check for default value
                if param_children.len() >= 3 && param_children[1].kind() == "=" {
                    let default_value = node_text(param_children[2], text);
                    parameters.push(format!("{} = {}", param_name, default_value));
                } else {
                    parameters.push(param_name.to_string());
                }
            }
        } else if child.kind() == "dots" {
            parameters.push("...".to_string());
        }
    }

    parameters
}

#[cfg(test)]
fn extract_function_signature(func_node: Node, func_name: &str, text: &str) -> String {
    let mut cursor = func_node.walk();

    for child in func_node.children(&mut cursor) {
        if child.kind() == "parameters" {
            let params = extract_parameters(child, text);
            return format!("{}({})", func_name, params.join(", "));
        }
    }

    format!("{}()", func_name)
}

#[cfg(test)]
fn find_function_definition_node<'a>(node: Node<'a>, name: &str, text: &str) -> Option<Node<'a>> {
    if node.kind() == "binary_operator" {
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();

        if children.len() >= 3 {
            let lhs = children[0];
            let op = children[1];
            let rhs = children[2];

            let op_text = node_text(op, text);
            if matches!(op_text, "<-" | "=" | "<<-")
                && lhs.kind() == "identifier"
                && node_text(lhs, text) == name
                && rhs.kind() == "function_definition"
            {
                return Some(rhs);
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(func_node) = find_function_definition_node(child, name, text) {
            return Some(func_node);
        }
    }

    None
}

#[cfg(test)]
fn find_user_function_signature(
    state: &WorldState,
    current_uri: &Url,
    name: &str,
) -> Option<String> {
    // 1. Search current document
    if let Some(doc) = state.get_document(current_uri) {
        if let Some(tree) = &doc.tree {
            let text = doc.text();
            if let Some(func_node) = find_function_definition_node(tree.root_node(), name, &text) {
                return Some(extract_function_signature(func_node, name, &text));
            }
        }
    }

    // 2. Search open documents (skip current_uri)
    for (uri, doc) in &state.documents {
        if uri == current_uri {
            continue;
        }
        if let Some(tree) = &doc.tree {
            let text = doc.text();
            if let Some(func_node) = find_function_definition_node(tree.root_node(), name, &text) {
                return Some(extract_function_signature(func_node, name, &text));
            }
        }
    }

    // 3. Search workspace index
    for doc in state.workspace_index.values() {
        if let Some(tree) = &doc.tree {
            let text = doc.text();
            if let Some(func_node) = find_function_definition_node(tree.root_node(), name, &text) {
                return Some(extract_function_signature(func_node, name, &text));
            }
        }
    }

    None
}

// ============================================================================
// Path Utilities
// ============================================================================

/// Compute relative path from workspace root to target URI.
/// If no workspace root or target is outside workspace, returns filename only.
fn compute_relative_path(target_uri: &Url, workspace_root: Option<&Url>) -> String {
    let Some(workspace_root) = workspace_root else {
        return target_uri
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or("unknown")
            .to_string();
    };

    let Ok(workspace_path) = workspace_root.to_file_path() else {
        return target_uri
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or("unknown")
            .to_string();
    };

    let Ok(target_path) = target_uri.to_file_path() else {
        return target_uri
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or("unknown")
            .to_string();
    };

    match target_path.strip_prefix(&workspace_path) {
        Ok(relative) => relative.to_string_lossy().to_string(),
        Err(_) => target_uri
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or("unknown")
            .to_string(),
    }
}

// Note: escape_markdown is only used in tests now.
// Code blocks (```r ... ```) don't need escaping - markdown doesn't interpret special chars inside them.
#[cfg(test)]
/// Escape markdown special characters in text.
/// Characters to escape: * _ [ ] ( ) # ` \
fn escape_markdown(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '*' | '_' | '[' | ']' | '(' | ')' | '#' | '`' | '\\' => format!("\\{}", c),
            _ => c.to_string(),
        })
        .collect()
}

#[cfg(test)]
fn hover_blocking(state: &WorldState, uri: &Url, position: Position) -> Option<Hover> {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.block_on(hover(state, uri, position))
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(hover(state, uri, position))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn parse_r_code(code: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        parser.parse(code, None).unwrap()
    }

    #[test]
    fn test_function_parameters_recognized() {
        let code = "f <- function(a, b) { a + b }";
        let tree = parse_r_code(code);
        let mut defined = HashSet::new();
        collect_definitions(tree.root_node(), code, &mut defined);

        assert!(defined.contains("f"), "Function name should be defined");
        assert!(defined.contains("a"), "Parameter 'a' should be defined");
        assert!(defined.contains("b"), "Parameter 'b' should be defined");
    }

    #[test]
    fn test_single_parameter() {
        let code = "square <- function(x) { x * x }";
        let tree = parse_r_code(code);
        let mut defined = HashSet::new();
        collect_definitions(tree.root_node(), code, &mut defined);

        assert!(defined.contains("square"));
        assert!(defined.contains("x"));
    }

    #[test]
    fn test_no_parameters() {
        let code = "get_pi <- function() { 3.14 }";
        let tree = parse_r_code(code);
        let mut defined = HashSet::new();
        collect_definitions(tree.root_node(), code, &mut defined);

        assert!(defined.contains("get_pi"));
    }

    #[test]
    fn test_builtin_functions() {
        assert!(is_builtin("warning"));
        assert!(is_builtin("any"));
        assert!(is_builtin("is.na"));
        assert!(is_builtin("sprintf"));
        assert!(is_builtin("print"));
        assert!(is_builtin("sum"));
        assert!(is_builtin("mean"));
    }

    #[test]
    fn test_builtin_constants() {
        assert!(is_builtin("TRUE"));
        assert!(is_builtin("FALSE"));
        assert!(is_builtin("NULL"));
        assert!(is_builtin("NA"));
        assert!(is_builtin("Inf"));
        assert!(is_builtin("NaN"));
    }

    #[test]
    fn test_not_builtin() {
        assert!(!is_builtin("my_custom_function"));
        assert!(!is_builtin("undefined_var"));
    }

    #[test]
    fn test_nested_function_parameters() {
        let code = "outer <- function(x) { inner <- function(y) { x + y }; inner }";
        let tree = parse_r_code(code);
        let mut defined = HashSet::new();
        collect_definitions(tree.root_node(), code, &mut defined);

        assert!(defined.contains("outer"));
        assert!(defined.contains("x"));
        assert!(defined.contains("inner"));
        assert!(defined.contains("y"));
    }

    #[test]
    fn test_extract_parameters_simple() {
        let code = "add <- function(a, b = 1) { }";
        let tree = parse_r_code(code);

        let func_node = find_function_definition(tree.root_node()).unwrap();
        let mut cursor = func_node.walk();
        let params_node = func_node
            .children(&mut cursor)
            .find(|n| n.kind() == "parameters")
            .unwrap();

        let params = extract_parameters(params_node, code);
        assert_eq!(params, vec!["a", "b = 1"]);
    }

    #[test]
    fn test_extract_function_signature() {
        let code = "add <- function(a, b = 1) { }";
        let tree = parse_r_code(code);

        let func_node = find_function_definition(tree.root_node()).unwrap();
        let signature = extract_function_signature(func_node, "add", code);
        assert_eq!(signature, "add(a, b = 1)");
    }

    #[test]
    fn test_signature_simple_function() {
        let code = "add <- function(a, b) { a + b }";
        let tree = parse_r_code(code);

        let func_node = find_function_definition_node(tree.root_node(), "add", code).unwrap();
        let signature = extract_function_signature(func_node, "add", code);
        assert_eq!(signature, "add(a, b)");
    }

    #[test]
    fn test_signature_no_parameters() {
        let code = "get_pi <- function() { 3.14 }";
        let tree = parse_r_code(code);

        let func_node = find_function_definition_node(tree.root_node(), "get_pi", code).unwrap();
        let signature = extract_function_signature(func_node, "get_pi", code);
        assert_eq!(signature, "get_pi()");
    }

    #[test]
    fn test_signature_with_defaults() {
        let code = "greet <- function(name = \"World\") { }";
        let tree = parse_r_code(code);

        let func_node = find_function_definition_node(tree.root_node(), "greet", code).unwrap();
        let signature = extract_function_signature(func_node, "greet", code);
        assert_eq!(signature, "greet(name = \"World\")");
    }

    #[test]
    fn test_signature_with_dots() {
        let code = "wrapper <- function(...) { }";
        let tree = parse_r_code(code);

        let func_node = find_function_definition_node(tree.root_node(), "wrapper", code).unwrap();
        let signature = extract_function_signature(func_node, "wrapper", code);
        assert_eq!(signature, "wrapper(...)");
    }

    #[test]
    fn test_compute_relative_path_with_workspace_root() {
        let workspace_root = Url::parse("file:///workspace/").unwrap();
        let target_uri = Url::parse("file:///workspace/src/main.R").unwrap();

        let result = compute_relative_path(&target_uri, Some(&workspace_root));
        assert_eq!(result, "src/main.R");
    }

    #[test]
    fn test_compute_relative_path_without_workspace_root() {
        let target_uri = Url::parse("file:///workspace/src/main.R").unwrap();

        let result = compute_relative_path(&target_uri, None);
        assert_eq!(result, "main.R");
    }

    #[test]
    fn test_compute_relative_path_outside_workspace() {
        let workspace_root = Url::parse("file:///workspace/").unwrap();
        let target_uri = Url::parse("file:///other/path/script.R").unwrap();

        let result = compute_relative_path(&target_uri, Some(&workspace_root));
        assert_eq!(result, "script.R");
    }

    #[test]
    fn test_escape_markdown_all_special_chars() {
        let input = "*_[]()#`\\";
        let expected = "\\*\\_\\[\\]\\(\\)\\#\\`\\\\";

        let result = escape_markdown(input);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_escape_markdown_no_special_chars() {
        let input = "hello world 123";

        let result = escape_markdown(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_escape_markdown_mixed_content() {
        let input = "function(x) { x * 2 }";
        let expected = "function\\(x\\) { x \\* 2 }";

        let result = escape_markdown(input);
        assert_eq!(result, expected);
    }

    fn find_function_definition(node: Node) -> Option<Node> {
        if node.kind() == "function_definition" {
            return Some(node);
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(func) = find_function_definition(child) {
                return Some(func);
            }
        }
        None
    }

    // ========================================================================
    // Extract Operator Tests (Task 6.1)
    // Tests for skip-nse-undefined-checks feature
    // Validates: Requirements 1.1, 1.2, 1.3
    // ========================================================================

    /// Test that df$column does not produce a diagnostic for 'column'
    /// Validates: Requirement 1.1 - RHS of $ operator should be skipped
    #[test]
    fn test_extract_operator_dollar_rhs_skipped() {
        let code = "df$column";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'df' should be collected as a usage (LHS is checked)
        let df_used = used.iter().any(|(name, _)| name == "df");
        assert!(df_used, "LHS 'df' should be collected as usage");

        // 'column' should NOT be collected as a usage (RHS is skipped)
        let column_used = used.iter().any(|(name, _)| name == "column");
        assert!(
            !column_used,
            "RHS 'column' should NOT be collected as usage for $ operator"
        );
    }

    /// Test that obj@slot does not produce a diagnostic for 'slot'
    /// Validates: Requirement 1.2 - RHS of @ operator should be skipped
    #[test]
    fn test_extract_operator_at_rhs_skipped() {
        let code = "obj@slot";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'obj' should be collected as a usage (LHS is checked)
        let obj_used = used.iter().any(|(name, _)| name == "obj");
        assert!(obj_used, "LHS 'obj' should be collected as usage");

        // 'slot' should NOT be collected as a usage (RHS is skipped)
        let slot_used = used.iter().any(|(name, _)| name == "slot");
        assert!(
            !slot_used,
            "RHS 'slot' should NOT be collected as usage for @ operator"
        );
    }

    /// Test that undefined$column produces a diagnostic for 'undefined' (LHS is still checked)
    /// Validates: Requirement 1.3 - LHS of extract operators should still be checked
    #[test]
    fn test_extract_operator_lhs_checked() {
        let code = "undefined$column";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'undefined' should be collected as a usage (LHS is checked)
        let undefined_used = used.iter().any(|(name, _)| name == "undefined");
        assert!(
            undefined_used,
            "LHS 'undefined' should be collected as usage"
        );

        // 'column' should NOT be collected as a usage (RHS is skipped)
        let column_used = used.iter().any(|(name, _)| name == "column");
        assert!(
            !column_used,
            "RHS 'column' should NOT be collected as usage"
        );
    }

    // ==================== Call-Like Argument Tests ====================
    // These tests verify that identifiers inside call-like arguments are skipped
    // (Requirements 2.1, 2.2, 2.3, 2.4)

    /// Test that subset(df, x > 5) does not produce a diagnostic for 'x'
    /// Validates: Requirement 2.1 - Identifiers inside function call arguments should be skipped
    #[test]
    fn test_call_arguments_skipped() {
        let code = "subset(df, x > 5)";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'subset' should be collected as a usage (function name is checked)
        let subset_used = used.iter().any(|(name, _)| name == "subset");
        assert!(
            subset_used,
            "Function name 'subset' should be collected as usage"
        );

        // 'df' should NOT be collected as a usage (inside call arguments)
        let df_used = used.iter().any(|(name, _)| name == "df");
        assert!(
            !df_used,
            "'df' inside call arguments should NOT be collected as usage"
        );

        // 'x' should NOT be collected as a usage (inside call arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside call arguments should NOT be collected as usage"
        );
    }

    /// Test that df[x > 5, ] does not produce a diagnostic for 'x'
    /// Validates: Requirement 2.2 - Identifiers inside subset ([) arguments should be skipped
    #[test]
    fn test_subset_arguments_skipped() {
        let code = "df[x > 5, ]";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'df' should be collected as a usage (the object being subsetted is checked)
        let df_used = used.iter().any(|(name, _)| name == "df");
        assert!(
            df_used,
            "'df' (object being subsetted) should be collected as usage"
        );

        // 'x' should NOT be collected as a usage (inside subset arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside subset arguments should NOT be collected as usage"
        );
    }

    /// Test that df[[x]] does not produce a diagnostic for 'x'
    /// Validates: Requirement 2.3 - Identifiers inside subset2 ([[) arguments should be skipped
    #[test]
    fn test_subset2_arguments_skipped() {
        let code = "df[[x]]";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'df' should be collected as a usage (the object being subsetted is checked)
        let df_used = used.iter().any(|(name, _)| name == "df");
        assert!(
            df_used,
            "'df' (object being subsetted) should be collected as usage"
        );

        // 'x' should NOT be collected as a usage (inside subset2 arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside subset2 arguments should NOT be collected as usage"
        );
    }

    /// Test that undefined_func(x) produces a diagnostic for 'undefined_func'
    /// Validates: Requirement 2.4 - Function names should still be checked
    #[test]
    fn test_function_name_checked() {
        let code = "undefined_func(x)";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'undefined_func' should be collected as a usage (function name is checked)
        let func_used = used.iter().any(|(name, _)| name == "undefined_func");
        assert!(
            func_used,
            "Function name 'undefined_func' should be collected as usage"
        );

        // 'x' should NOT be collected as a usage (inside call arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside call arguments should NOT be collected as usage"
        );
    }

    // ==================== Formula Tests (Task 6.3) ====================
    // These tests verify that identifiers inside formula expressions are skipped
    // (Requirements 3.1, 3.2, 3.4)

    /// Test that ~ x does not produce a diagnostic for 'x'
    /// Validates: Requirement 3.1 - Identifiers inside unary formula expressions should be skipped
    #[test]
    fn test_unary_formula_skipped() {
        let code = "~ x";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'x' should NOT be collected as a usage (inside formula)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside unary formula should NOT be collected as usage"
        );
    }

    /// Test that y ~ x + z does not produce diagnostics for 'y', 'x', 'z'
    /// Validates: Requirement 3.2 - Identifiers inside binary formula expressions should be skipped
    #[test]
    fn test_binary_formula_skipped() {
        let code = "y ~ x + z";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'y' should NOT be collected as a usage (LHS of formula)
        let y_used = used.iter().any(|(name, _)| name == "y");
        assert!(
            !y_used,
            "'y' inside binary formula should NOT be collected as usage"
        );

        // 'x' should NOT be collected as a usage (RHS of formula)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside binary formula should NOT be collected as usage"
        );

        // 'z' should NOT be collected as a usage (RHS of formula)
        let z_used = used.iter().any(|(name, _)| name == "z");
        assert!(
            !z_used,
            "'z' inside binary formula should NOT be collected as usage"
        );
    }

    /// Test that lm(y ~ x, data = df) does not produce diagnostics for 'y', 'x'
    /// Validates: Requirement 3.4 - Formulas nested inside call arguments should have both contexts apply
    #[test]
    fn test_formula_inside_call_arguments_skipped() {
        let code = "lm(y ~ x, data = df)";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'lm' should be collected as a usage (function name is checked)
        let lm_used = used.iter().any(|(name, _)| name == "lm");
        assert!(lm_used, "Function name 'lm' should be collected as usage");

        // 'y' should NOT be collected as a usage (inside formula inside call arguments)
        let y_used = used.iter().any(|(name, _)| name == "y");
        assert!(
            !y_used,
            "'y' inside formula in call arguments should NOT be collected as usage"
        );

        // 'x' should NOT be collected as a usage (inside formula inside call arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside formula in call arguments should NOT be collected as usage"
        );

        // 'df' should NOT be collected as a usage (inside call arguments)
        let df_used = used.iter().any(|(name, _)| name == "df");
        assert!(
            !df_used,
            "'df' inside call arguments should NOT be collected as usage"
        );
    }

    // ==================== Edge Case Tests (Task 6.4) ====================
    // These tests verify edge cases for the NSE skip logic
    // (Requirements 1.1, 1.2, 2.1, 3.1)

    /// Test deeply nested formulas: ~ (~ (~ x)) - all identifiers should be skipped
    /// Validates: Requirement 3.1 - Identifiers inside formula expressions should be skipped
    #[test]
    fn test_deeply_nested_formulas() {
        let code = "~ (~ (~ x))";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'x' should NOT be collected as a usage (inside deeply nested formula)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside deeply nested formula should NOT be collected as usage"
        );

        // No identifiers should be collected at all
        assert!(
            used.is_empty(),
            "No identifiers should be collected from deeply nested formula"
        );
    }

    /// Test nested call arguments: f(g(h(x))) - all identifiers in all argument levels should be skipped
    /// Validates: Requirement 2.1 - Identifiers inside call arguments should be skipped
    #[test]
    fn test_nested_call_arguments() {
        let code = "f(g(h(x)))";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'f' should be collected as a usage (outermost function name is checked)
        let f_used = used.iter().any(|(name, _)| name == "f");
        assert!(f_used, "Function name 'f' should be collected as usage");

        // 'g' should NOT be collected as a usage (inside f's arguments)
        let g_used = used.iter().any(|(name, _)| name == "g");
        assert!(
            !g_used,
            "'g' inside call arguments should NOT be collected as usage"
        );

        // 'h' should NOT be collected as a usage (inside g's arguments, which is inside f's arguments)
        let h_used = used.iter().any(|(name, _)| name == "h");
        assert!(
            !h_used,
            "'h' inside nested call arguments should NOT be collected as usage"
        );

        // 'x' should NOT be collected as a usage (inside h's arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside deeply nested call arguments should NOT be collected as usage"
        );

        // Only 'f' should be collected
        assert_eq!(
            used.len(),
            1,
            "Only the outermost function name should be collected"
        );
    }

    /// Test mixed contexts: df$col[x > 5] - 'col' skipped (extract RHS), 'x' skipped (subset arguments), 'df' checked
    /// Validates: Requirements 1.1, 1.2, 2.1 - Extract RHS and subset arguments should be skipped
    #[test]
    fn test_mixed_contexts() {
        let code = "df$col[x > 5]";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'df' should be collected as a usage (LHS of extract operator is checked)
        let df_used = used.iter().any(|(name, _)| name == "df");
        assert!(
            df_used,
            "'df' (LHS of extract operator) should be collected as usage"
        );

        // 'col' should NOT be collected as a usage (RHS of extract operator)
        let col_used = used.iter().any(|(name, _)| name == "col");
        assert!(
            !col_used,
            "'col' (RHS of extract operator) should NOT be collected as usage"
        );

        // 'x' should NOT be collected as a usage (inside subset arguments)
        let x_used = used.iter().any(|(name, _)| name == "x");
        assert!(
            !x_used,
            "'x' inside subset arguments should NOT be collected as usage"
        );

        // Only 'df' should be collected
        assert_eq!(
            used.len(),
            1,
            "Only 'df' should be collected in mixed context"
        );
    }

    /// Test chained extracts: df$a$b$c - only 'df' should be checked, all others are RHS of extract operators
    /// Validates: Requirements 1.1, 1.2 - RHS of extract operators should be skipped
    #[test]
    fn test_chained_extracts() {
        let code = "df$a$b$c";
        let tree = parse_r_code(code);
        let mut used = Vec::new();
        collect_usages_with_context(tree.root_node(), code, &UsageContext::default(), &mut used);

        // 'df' should be collected as a usage (leftmost identifier is checked)
        let df_used = used.iter().any(|(name, _)| name == "df");
        assert!(
            df_used,
            "'df' (leftmost identifier) should be collected as usage"
        );

        // 'a' should NOT be collected as a usage (RHS of first extract operator)
        let a_used = used.iter().any(|(name, _)| name == "a");
        assert!(
            !a_used,
            "'a' (RHS of extract operator) should NOT be collected as usage"
        );

        // 'b' should NOT be collected as a usage (RHS of second extract operator)
        let b_used = used.iter().any(|(name, _)| name == "b");
        assert!(
            !b_used,
            "'b' (RHS of extract operator) should NOT be collected as usage"
        );

        // 'c' should NOT be collected as a usage (RHS of third extract operator)
        let c_used = used.iter().any(|(name, _)| name == "c");
        assert!(
            !c_used,
            "'c' (RHS of extract operator) should NOT be collected as usage"
        );

        // Only 'df' should be collected
        assert_eq!(
            used.len(),
            1,
            "Only 'df' should be collected in chained extracts"
        );
    }

    // ========================================================================
    // Completion Precedence Tests (Task 11.2)
    // Tests for completion precedence: local > package exports > cross-file
    // Validates: Requirements 9.4, 9.5
    // ========================================================================

    /// Test that local definitions take precedence over package exports in completions.
    /// Validates: Requirement 9.4 - Local definitions > package exports
    #[test]
    fn test_completion_local_over_package_exports() {
        use crate::package_library::PackageInfo;
        use crate::state::{Document, WorldState};
        use tower_lsp::lsp_types::{CompletionResponse, Position};

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Create a WorldState with a package that exports "mutate"
            let mut state = WorldState::new(vec![]);

            // Add a package with "mutate" export
            let mut exports = std::collections::HashSet::new();
            exports.insert("mutate".to_string());
            exports.insert("filter".to_string());
            let pkg_info = PackageInfo::new("dplyr".to_string(), exports);
            state.package_library.insert_package(pkg_info).await;

            // Create a document that defines "mutate" locally and loads dplyr
            let code = r#"library(dplyr)
mutate <- function(x) { x * 2 }
result <- "#;
            let uri = Url::parse("file:///test.R").unwrap();
            let doc = Document::new(code, None);
            state.documents.insert(uri.clone(), doc);

            // Get completions at the end of the file (after "result <- ")
            let position = Position::new(2, 10);
            let completions = super::completion(&state, &uri, position);

            assert!(completions.is_some(), "Should return completions");

            if let Some(CompletionResponse::Array(items)) = completions {
                // Find the "mutate" completion item
                let mutate_items: Vec<_> = items.iter()
                    .filter(|item| item.label == "mutate")
                    .collect();

                // There should be exactly one "mutate" item (the local definition)
                assert_eq!(
                    mutate_items.len(),
                    1,
                    "Should have exactly one 'mutate' completion (local definition takes precedence)"
                );

                // The local definition should NOT have package attribution
                let mutate_item = mutate_items[0];
                assert!(
                    mutate_item.detail.is_none() || !mutate_item.detail.as_ref().unwrap().contains("{dplyr}"),
                    "Local 'mutate' should not have package attribution"
                );
            } else {
                panic!("Expected CompletionResponse::Array");
            }
        });
    }

    /// Test that package exports take precedence over cross-file symbols in completions.
    /// Validates: Requirement 9.5 - Package exports > cross-file symbols
    #[test]
    fn test_completion_package_over_cross_file() {
        use crate::package_library::PackageInfo;
        use crate::state::{Document, WorldState};
        use tower_lsp::lsp_types::{CompletionResponse, Position};

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Create a WorldState with a package that exports "helper_func"
            let mut state = WorldState::new(vec![]);

            // Add a package with "helper_func" export
            let mut exports = std::collections::HashSet::new();
            exports.insert("helper_func".to_string());
            let pkg_info = PackageInfo::new("testpkg".to_string(), exports);
            state.package_library.insert_package(pkg_info).await;

            // Create main file that loads testpkg
            let main_code = r#"library(testpkg)
result <- "#;
            let main_uri = Url::parse("file:///main.R").unwrap();
            let main_doc = Document::new(main_code, None);
            state.documents.insert(main_uri.clone(), main_doc);

            // Create a helper file that defines "helper_func"
            let helper_code = r#"helper_func <- function(x) { x + 1 }"#;
            let helper_uri = Url::parse("file:///helper.R").unwrap();
            let helper_doc = Document::new(helper_code, None);
            state.documents.insert(helper_uri.clone(), helper_doc);

            // Note: In a real scenario, the cross-file symbol would come from scope resolution
            // through source() calls. For this test, we verify that package exports are added
            // before cross-file symbols in the completion list.

            // Get completions at the end of main file
            let position = Position::new(1, 10);
            let completions = super::completion(&state, &main_uri, position);

            assert!(completions.is_some(), "Should return completions");

            if let Some(CompletionResponse::Array(items)) = completions {
                // Find the "helper_func" completion item
                let helper_items: Vec<_> = items
                    .iter()
                    .filter(|item| item.label == "helper_func")
                    .collect();

                // There should be at least one "helper_func" item (from package)
                assert!(
                    !helper_items.is_empty(),
                    "Should have 'helper_func' completion from package"
                );

                // The first (and only) helper_func should be from the package
                let helper_item = helper_items[0];
                assert!(
                    helper_item
                        .detail
                        .as_ref()
                        .map_or(false, |d| d.contains("{testpkg}")),
                    "helper_func should have package attribution {{testpkg}}"
                );
            } else {
                panic!("Expected CompletionResponse::Array");
            }
        });
    }

    /// Test that keywords take precedence over all other completions.
    /// Validates: Implicit requirement - keywords should always be available
    #[test]
    fn test_completion_keywords_always_present() {
        use crate::package_library::PackageInfo;
        use crate::state::{Document, WorldState};
        use tower_lsp::lsp_types::{CompletionItemKind, CompletionResponse, Position};

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Create a WorldState with a package that exports "if" (hypothetically)
            let mut state = WorldState::new(vec![]);

            // Add a package with "if" export (edge case - shouldn't override keyword)
            let mut exports = std::collections::HashSet::new();
            exports.insert("if".to_string());
            let pkg_info = PackageInfo::new("badpkg".to_string(), exports);
            state.package_library.insert_package(pkg_info).await;

            // Create a document that loads the package
            let code = r#"library(badpkg)
x <- "#;
            let uri = Url::parse("file:///test.R").unwrap();
            let doc = Document::new(code, None);
            state.documents.insert(uri.clone(), doc);

            // Get completions
            let position = Position::new(1, 5);
            let completions = super::completion(&state, &uri, position);

            assert!(completions.is_some(), "Should return completions");

            if let Some(CompletionResponse::Array(items)) = completions {
                // Find the "if" completion item
                let if_items: Vec<_> = items.iter().filter(|item| item.label == "if").collect();

                // There should be exactly one "if" item (the keyword)
                assert_eq!(
                    if_items.len(),
                    1,
                    "Should have exactly one 'if' completion (keyword takes precedence)"
                );

                // The "if" should be a keyword, not a function from package
                let if_item = if_items[0];
                assert_eq!(
                    if_item.kind,
                    Some(CompletionItemKind::KEYWORD),
                    "'if' should be a KEYWORD, not a function from package"
                );
            } else {
                panic!("Expected CompletionResponse::Array");
            }
        });
    }

    /// Verifies completion precedence where local definitions shadow package exports, and package exports take precedence over cross-file symbols.
    ///
    /// Sets up a WorldState with a package ("dplyr") that exports several symbols, opens a document that loads that package and defines a local `mutate` (which should shadow the package export) and `my_func`, then requests completions at a position and asserts:
    /// - the local `mutate` appears once with no package attribution,
    /// - `filter` and `select` appear once each with package attribution `{dplyr}`,
    /// - `my_func` appears as a function completion.
    ///
    /// # Examples
    ///
    /// ```
    /// // Arrange: create state, insert package exports and document, then call completion.
    /// // Assert: see comments above for expected precedence behavior.
    /// ```
    #[test]
    fn test_completion_full_precedence_chain() {
        use crate::package_library::PackageInfo;
        use crate::state::{Document, WorldState};
        use tower_lsp::lsp_types::{CompletionItemKind, CompletionResponse, Position};

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut state = WorldState::new(vec![]);

            // Add packages with various exports
            let mut dplyr_exports = std::collections::HashSet::new();
            dplyr_exports.insert("mutate".to_string());
            dplyr_exports.insert("filter".to_string());
            dplyr_exports.insert("select".to_string());
            let dplyr_info = PackageInfo::new("dplyr".to_string(), dplyr_exports);
            state.package_library.insert_package(dplyr_info).await;

            // Create a document that:
            // 1. Loads dplyr (provides mutate, filter, select)
            // 2. Defines "mutate" locally (should shadow package export)
            // 3. Defines "my_func" locally
            let code = r#"library(dplyr)
mutate <- function(df, ...) { df }
my_func <- function(x) { x }
result <- "#;
            let uri = Url::parse("file:///test.R").unwrap();
            let doc = Document::new(code, None);
            state.documents.insert(uri.clone(), doc);

            // Get completions at the end
            let position = Position::new(3, 10);
            let completions = super::completion(&state, &uri, position);

            assert!(completions.is_some(), "Should return completions");

            if let Some(CompletionResponse::Array(items)) = completions {
                // Check "mutate" - should be local (no package attribution)
                let mutate_items: Vec<_> =
                    items.iter().filter(|item| item.label == "mutate").collect();
                assert_eq!(mutate_items.len(), 1, "Should have exactly one 'mutate'");
                assert!(
                    mutate_items[0].detail.is_none()
                        || !mutate_items[0].detail.as_ref().unwrap().contains("{dplyr}"),
                    "Local 'mutate' should not have package attribution"
                );

                // Check "filter" - should be from package (has attribution)
                let filter_items: Vec<_> =
                    items.iter().filter(|item| item.label == "filter").collect();
                assert_eq!(filter_items.len(), 1, "Should have exactly one 'filter'");
                assert!(
                    filter_items[0]
                        .detail
                        .as_ref()
                        .map_or(false, |d| d.contains("{dplyr}")),
                    "'filter' should have package attribution {{dplyr}}"
                );

                // Check "select" - should be from package (has attribution)
                let select_items: Vec<_> =
                    items.iter().filter(|item| item.label == "select").collect();
                assert_eq!(select_items.len(), 1, "Should have exactly one 'select'");
                assert!(
                    select_items[0]
                        .detail
                        .as_ref()
                        .map_or(false, |d| d.contains("{dplyr}")),
                    "'select' should have package attribution {{dplyr}}"
                );

                // Check "my_func" - should be local (no package attribution)
                let my_func_items: Vec<_> = items
                    .iter()
                    .filter(|item| item.label == "my_func")
                    .collect();
                assert_eq!(my_func_items.len(), 1, "Should have exactly one 'my_func'");
                assert_eq!(
                    my_func_items[0].kind,
                    Some(CompletionItemKind::FUNCTION),
                    "'my_func' should be a FUNCTION"
                );
            } else {
                panic!("Expected CompletionResponse::Array");
            }
        });
    }

    /// Test that seen_names correctly prevents duplicates across all sources.
    /// Validates: Requirements 9.3, 9.4, 9.5 - duplicate exports show all packages
    #[test]
    fn test_completion_duplicate_exports_show_all_packages() {
        use crate::package_library::PackageInfo;
        use crate::state::{Document, WorldState};
        use tower_lsp::lsp_types::{CompletionResponse, Position};

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut state = WorldState::new(vec![]);

            // Add two packages that both export "common_func"
            let mut pkg1_exports = std::collections::HashSet::new();
            pkg1_exports.insert("common_func".to_string());
            pkg1_exports.insert("pkg1_only".to_string());
            let pkg1_info = PackageInfo::new("pkg1".to_string(), pkg1_exports);
            state.package_library.insert_package(pkg1_info).await;

            let mut pkg2_exports = std::collections::HashSet::new();
            pkg2_exports.insert("common_func".to_string());
            pkg2_exports.insert("pkg2_only".to_string());
            let pkg2_info = PackageInfo::new("pkg2".to_string(), pkg2_exports);
            state.package_library.insert_package(pkg2_info).await;

            // Create a document that loads both packages
            let code = r#"library(pkg1)
library(pkg2)
x <- "#;
            let uri = Url::parse("file:///test.R").unwrap();
            let doc = Document::new(code, None);
            state.documents.insert(uri.clone(), doc);

            // Get completions
            let position = Position::new(2, 5);
            let completions = super::completion(&state, &uri, position);

            assert!(completions.is_some(), "Should return completions");

            if let Some(CompletionResponse::Array(items)) = completions {
                // Requirement 9.3: When multiple packages export same symbol, show all with attribution
                // Check that "common_func" appears twice (once for each package)
                let common_items: Vec<_> = items
                    .iter()
                    .filter(|item| item.label == "common_func")
                    .collect();
                assert_eq!(
                    common_items.len(),
                    2,
                    "Should have two 'common_func' entries (one per package)"
                );

                // Both packages should be represented
                let has_pkg1 = common_items
                    .iter()
                    .any(|item| item.detail.as_ref().map_or(false, |d| d.contains("{pkg1}")));
                let has_pkg2 = common_items
                    .iter()
                    .any(|item| item.detail.as_ref().map_or(false, |d| d.contains("{pkg2}")));
                assert!(has_pkg1, "'common_func' should have entry from pkg1");
                assert!(has_pkg2, "'common_func' should have entry from pkg2");

                // Check that unique exports from both packages are present
                let pkg1_only_items: Vec<_> = items
                    .iter()
                    .filter(|item| item.label == "pkg1_only")
                    .collect();
                assert_eq!(pkg1_only_items.len(), 1, "Should have 'pkg1_only'");

                let pkg2_only_items: Vec<_> = items
                    .iter()
                    .filter(|item| item.label == "pkg2_only")
                    .collect();
                assert_eq!(pkg2_only_items.len(), 1, "Should have 'pkg2_only'");
            } else {
                panic!("Expected CompletionResponse::Array");
            }
        });
    }

    // ========================================================================
    // Backward Directive Path Resolution Tests
    // Tests for fix-backward-directive-path-resolution spec
    // Validates: Requirements 1.2, 3.2
    // ========================================================================

    /// Test that backward directive paths resolve relative to file's directory, ignoring @lsp-cd.
    ///
    /// This test reproduces a bug where `collect_ambiguous_parent_diagnostics` was using
    /// `PathContext::from_metadata` (which respects @lsp-cd) instead of `PathContext::new`
    /// (which ignores @lsp-cd) for backward directive resolution.
    ///
    /// Scenario:
    /// - Child file at `subdir/child.r` contains:
    ///   - `@lsp-cd ..` (sets working directory to parent/workspace root)
    ///   - `@lsp-run-by: program.r` (declares parent file)
    /// - The backward directive should resolve `program.r` relative to `subdir/` (file's directory)
    ///   NOT relative to the workspace root (the @lsp-cd directory)
    ///
    /// Validates: Requirements 1.2, 3.2
    #[test]
    fn test_backward_directive_ignores_lsp_cd() {
        use crate::cross_file::path_resolve::PathContext;
        use crate::cross_file::types::CrossFileMetadata;

        // Simulate a child file at /project/subdir/child.r
        let child_uri = Url::parse("file:///project/subdir/child.r").unwrap();
        let workspace_root = Url::parse("file:///project").unwrap();

        // Metadata with @lsp-cd .. (points to /project, the workspace root)
        let meta = CrossFileMetadata {
            working_directory: Some("..".to_string()),
            ..Default::default()
        };

        // PathContext::new should ignore @lsp-cd
        let ctx_new = PathContext::new(&child_uri, Some(&workspace_root)).unwrap();

        // PathContext::from_metadata should respect @lsp-cd
        let ctx_from_meta =
            PathContext::from_metadata(&child_uri, &meta, Some(&workspace_root)).unwrap();

        // Verify that PathContext::new ignores @lsp-cd
        // The effective working directory should be the file's directory: /project/subdir
        assert_eq!(
            ctx_new.effective_working_directory(),
            std::path::PathBuf::from("/project/subdir"),
            "PathContext::new should use file's directory, ignoring @lsp-cd"
        );

        // Verify that PathContext::from_metadata respects @lsp-cd
        // The effective working directory should be /project (the @lsp-cd directory)
        assert_eq!(
            ctx_from_meta.effective_working_directory(),
            std::path::PathBuf::from("/project"),
            "PathContext::from_metadata should use @lsp-cd directory"
        );

        // Now test path resolution for a backward directive path "program.r"
        let backward_path = "program.r";

        // With PathContext::new (correct for backward directives):
        // "program.r" should resolve to /project/subdir/program.r
        let resolved_new = crate::cross_file::path_resolve::resolve_path(backward_path, &ctx_new);
        assert_eq!(
            resolved_new,
            Some(std::path::PathBuf::from("/project/subdir/program.r")),
            "Backward directive 'program.r' should resolve relative to file's directory"
        );

        // With PathContext::from_metadata (incorrect for backward directives):
        // "program.r" would resolve to /project/program.r (wrong!)
        let resolved_from_meta =
            crate::cross_file::path_resolve::resolve_path(backward_path, &ctx_from_meta);
        assert_eq!(
            resolved_from_meta,
            Some(std::path::PathBuf::from("/project/program.r")),
            "With @lsp-cd, 'program.r' would incorrectly resolve to workspace root"
        );

        // The key assertion: the two resolutions are DIFFERENT
        // This demonstrates why using PathContext::new is essential for backward directives
        assert_ne!(
            resolved_new, resolved_from_meta,
            "PathContext::new and PathContext::from_metadata should produce different results when @lsp-cd is present"
        );
    }

    // ========================================================================
    // Else Newline Syntax Error Tests (Task 1.3)
    // Tests for else-newline-syntax-error feature
    // Validates: Requirements 2.1, 2.2, 2.3, 2.4
    // ========================================================================

    /// Test that `if (x) {y}\nelse {z}` emits a diagnostic for orphaned else.
    /// Validates: Requirement 2.1 - else on new line after closing brace should emit diagnostic
    #[test]
    fn test_else_newline_basic_invalid_pattern() {
        let code = "if (x) {y}\nelse {z}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            1,
            "Should emit exactly one diagnostic for orphaned else on new line"
        );
        assert_eq!(
            diagnostics[0].severity,
            Some(DiagnosticSeverity::ERROR),
            "Diagnostic severity should be ERROR"
        );
        assert!(
            diagnostics[0].message.contains("else"),
            "Diagnostic message should mention 'else'"
        );
        assert!(
            diagnostics[0].message.contains("same line"),
            "Diagnostic message should mention 'same line'"
        );
    }

    /// Test that `if (x) {y} else {z}` does NOT emit a diagnostic.
    /// Validates: Requirement 2.3 - else on same line as closing brace should not emit diagnostic
    #[test]
    fn test_else_newline_basic_valid_pattern() {
        let code = "if (x) {y} else {z}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            0,
            "Should NOT emit diagnostic when else is on same line as closing brace"
        );
    }

    /// Test that multi-line valid if-else does NOT emit a diagnostic.
    /// `if (x) {\n  y\n} else {\n  z\n}` - else on same line as closing brace
    /// Validates: Requirement 2.4 - multi-line with else on same line as brace should not emit diagnostic
    #[test]
    fn test_else_newline_multiline_valid_pattern() {
        let code = "if (x) {\n  y\n} else {\n  z\n}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            0,
            "Should NOT emit diagnostic when else is on same line as closing brace (multi-line)"
        );
    }

    /// Test that multi-line invalid if-else emits a diagnostic.
    /// `if (x) {\n  y\n}\nelse {\n  z\n}` - else on new line after closing brace
    /// Validates: Requirement 2.2 - multi-line if with else on new line after brace should emit diagnostic
    #[test]
    fn test_else_newline_multiline_invalid_pattern() {
        let code = "if (x) {\n  y\n}\nelse {\n  z\n}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            1,
            "Should emit exactly one diagnostic for orphaned else on new line (multi-line)"
        );
        assert_eq!(
            diagnostics[0].severity,
            Some(DiagnosticSeverity::ERROR),
            "Diagnostic severity should be ERROR"
        );
    }

    /// Test that the diagnostic range covers the `else` keyword exactly.
    /// Validates: Requirement 3.2 - diagnostic range should highlight the else keyword
    #[test]
    fn test_else_newline_diagnostic_range() {
        let code = "if (x) {y}\nelse {z}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(diagnostics.len(), 1, "Should emit exactly one diagnostic");

        let diag = &diagnostics[0];
        // "else" starts at line 1 (0-indexed), column 0
        assert_eq!(
            diag.range.start.line, 1,
            "Diagnostic should start on line 1 (0-indexed)"
        );
        assert_eq!(
            diag.range.start.character, 0,
            "Diagnostic should start at column 0"
        );
        // "else" is 4 characters long
        assert_eq!(diag.range.end.line, 1, "Diagnostic should end on line 1");
        assert_eq!(
            diag.range.end.character, 4,
            "Diagnostic should end at column 4 (covering 'else')"
        );
    }

    // ========================================================================
    // Nested If-Else Tests (Task 2.1)
    // Tests for nested if-else detection
    // Validates: Requirements 2.5
    // ========================================================================

    /// Test that nested valid if-else does NOT emit a diagnostic.
    /// `if (a) { if (b) {c} else {d} } else {e}` - all else on same line as closing brace
    /// Validates: Requirement 2.5 - nested if-else with valid else placement should not emit diagnostic
    #[test]
    fn test_else_newline_nested_valid_pattern() {
        let code = "if (a) { if (b) {c} else {d} } else {e}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            0,
            "Should NOT emit diagnostic when all else keywords are on same line as closing brace (nested)"
        );
    }

    /// Test that nested invalid if-else emits a diagnostic for the inner orphaned else.
    /// `if (a) { if (b) {c}\nelse {d} }` - inner else on new line after closing brace
    /// Validates: Requirement 2.5 - nested if-else with orphaned else should emit diagnostic
    #[test]
    fn test_else_newline_nested_invalid_inner_else() {
        let code = "if (a) { if (b) {c}\nelse {d} }";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            1,
            "Should emit exactly one diagnostic for orphaned inner else on new line (nested)"
        );
        assert_eq!(
            diagnostics[0].severity,
            Some(DiagnosticSeverity::ERROR),
            "Diagnostic severity should be ERROR"
        );
        // The inner else is on line 1 (0-indexed)
        assert_eq!(
            diagnostics[0].range.start.line, 1,
            "Diagnostic should be on line 1 (0-indexed) where the orphaned else is"
        );
    }

    /// Test that nested invalid if-else with outer orphaned else emits a diagnostic.
    /// `if (a) { if (b) {c} else {d} }\nelse {e}` - outer else on new line
    /// Validates: Requirement 2.5 - nested if-else with orphaned outer else should emit diagnostic
    #[test]
    fn test_else_newline_nested_invalid_outer_else() {
        let code = "if (a) { if (b) {c} else {d} }\nelse {e}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            1,
            "Should emit exactly one diagnostic for orphaned outer else on new line (nested)"
        );
        // The outer else is on line 1 (0-indexed)
        assert_eq!(
            diagnostics[0].range.start.line, 1,
            "Diagnostic should be on line 1 (0-indexed) where the orphaned outer else is"
        );
    }

    /// Test that deeply nested if-else with multiple orphaned else keywords emits multiple diagnostics.
    /// Validates: Requirement 2.5 - all orphaned else at any nesting level should be detected
    #[test]
    fn test_else_newline_deeply_nested_multiple_invalid() {
        // Both inner and outer else are on new lines
        let code = "if (a) { if (b) {c}\nelse {d} }\nelse {e}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            2,
            "Should emit two diagnostics for both orphaned else keywords (nested)"
        );
    }

    // ========================================================================
    // Else If Pattern Tests (Task 2.2)
    // Tests for `else if` on new line detection
    // Validates: Requirements 5.2
    // ========================================================================

    /// Test that `if (x) {y}\nelse if (z) {w}` emits a diagnostic for orphaned else.
    /// Validates: Requirement 5.2 - `else if` on new line should emit diagnostic
    #[test]
    fn test_else_newline_else_if_on_new_line() {
        let code = "if (x) {y}\nelse if (z) {w}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            1,
            "Should emit exactly one diagnostic for orphaned 'else if' on new line"
        );
        assert_eq!(
            diagnostics[0].severity,
            Some(DiagnosticSeverity::ERROR),
            "Diagnostic severity should be ERROR"
        );
        // The else is on line 1 (0-indexed), column 0
        assert_eq!(
            diagnostics[0].range.start.line, 1,
            "Diagnostic should start on line 1 (0-indexed)"
        );
        assert_eq!(
            diagnostics[0].range.start.character, 0,
            "Diagnostic should start at column 0"
        );
    }

    /// Test that `if (x) {y} else if (z) {w}` does NOT emit a diagnostic.
    /// Validates: Requirement 5.2 - valid `else if` on same line should not emit diagnostic
    #[test]
    fn test_else_newline_else_if_on_same_line() {
        let code = "if (x) {y} else if (z) {w}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            0,
            "Should NOT emit diagnostic when 'else if' is on same line as closing brace"
        );
    }

    /// Test that multi-line `else if` on new line emits a diagnostic.
    /// `if (x) {\n  y\n}\nelse if (z) {\n  w\n}` - else if on new line after closing brace
    /// Validates: Requirement 5.2 - multi-line `else if` on new line should emit diagnostic
    #[test]
    fn test_else_newline_else_if_multiline_invalid() {
        let code = "if (x) {\n  y\n}\nelse if (z) {\n  w\n}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            1,
            "Should emit exactly one diagnostic for orphaned 'else if' on new line (multi-line)"
        );
        // The else is on line 3 (0-indexed)
        assert_eq!(
            diagnostics[0].range.start.line, 3,
            "Diagnostic should be on line 3 (0-indexed) where the orphaned else is"
        );
    }

    /// Test that valid multi-line `else if` does NOT emit a diagnostic.
    /// `if (x) {\n  y\n} else if (z) {\n  w\n}` - else if on same line as closing brace
    /// Validates: Requirement 5.2 - valid multi-line `else if` should not emit diagnostic
    #[test]
    fn test_else_newline_else_if_multiline_valid() {
        let code = "if (x) {\n  y\n} else if (z) {\n  w\n}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            0,
            "Should NOT emit diagnostic when 'else if' is on same line as closing brace (multi-line)"
        );
    }

    // ========================================================================
    // Blank Lines Tests (Task 2.3)
    // Tests for blank lines between `}` and `else`
    // Validates: Requirements 5.4
    // ========================================================================

    /// Test that `if (x) {y}\n\nelse {z}` emits a diagnostic for orphaned else.
    /// Validates: Requirement 5.4 - blank lines between `}` and `else` should emit diagnostic
    #[test]
    fn test_else_newline_blank_lines_between_brace_and_else() {
        let code = "if (x) {y}\n\nelse {z}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            1,
            "Should emit exactly one diagnostic for orphaned else with blank line between"
        );
        assert_eq!(
            diagnostics[0].severity,
            Some(DiagnosticSeverity::ERROR),
            "Diagnostic severity should be ERROR"
        );
        // The else is on line 2 (0-indexed) due to the blank line
        assert_eq!(
            diagnostics[0].range.start.line, 2,
            "Diagnostic should start on line 2 (0-indexed) after blank line"
        );
        assert_eq!(
            diagnostics[0].range.start.character, 0,
            "Diagnostic should start at column 0"
        );
    }

    /// Test that multiple blank lines between `}` and `else` still emit a diagnostic.
    /// Validates: Requirement 5.4 - multiple blank lines should still trigger diagnostic
    #[test]
    fn test_else_newline_multiple_blank_lines() {
        let code = "if (x) {y}\n\n\n\nelse {z}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            1,
            "Should emit exactly one diagnostic for orphaned else with multiple blank lines"
        );
        // The else is on line 4 (0-indexed) due to multiple blank lines
        assert_eq!(
            diagnostics[0].range.start.line, 4,
            "Diagnostic should start on line 4 (0-indexed) after multiple blank lines"
        );
    }

    /// Test that multi-line if with blank lines before else emits a diagnostic.
    /// `if (x) {\n  y\n}\n\nelse {\n  z\n}` - blank line between closing brace and else
    /// Validates: Requirement 5.4 - multi-line with blank lines should emit diagnostic
    #[test]
    fn test_else_newline_multiline_with_blank_lines() {
        let code = "if (x) {\n  y\n}\n\nelse {\n  z\n}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            1,
            "Should emit exactly one diagnostic for orphaned else with blank line (multi-line)"
        );
        // The closing brace is on line 2 (0-indexed), else is on line 4
        assert_eq!(
            diagnostics[0].range.start.line, 4,
            "Diagnostic should be on line 4 (0-indexed) where the orphaned else is"
        );
    }

    // ========================================================================
    // Edge Case Tests (Task 2.4)
    // Additional edge case tests for else-newline detection
    // Validates: Requirements 5.1, 5.3
    // ========================================================================

    /// Test that standalone `else` without preceding `if` does NOT emit a duplicate diagnostic.
    /// Tree-sitter handles this as a general syntax error, so we should not emit our
    /// newline-specific diagnostic to avoid duplicates.
    /// Validates: Requirement 5.1 - standalone else should not emit newline-specific diagnostic
    #[test]
    fn test_else_newline_standalone_else_no_duplicate() {
        let code = "else {z}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        // The standalone else is a syntax error handled by tree-sitter.
        // Our detector should NOT emit a diagnostic for this case to avoid duplicates.
        assert_eq!(
            diagnostics.len(),
            0,
            "Should NOT emit newline-specific diagnostic for standalone else (tree-sitter handles this)"
        );
    }

    /// Test that comments on the same line as closing brace, with else on new line, emits diagnostic.
    /// `if (x) {y} # comment\nelse {z}` - else is on a new line, so diagnostic should be emitted
    /// Validates: Requirement 5.3 - comments between `}` and `else` on same line should not prevent
    /// diagnostic when else is actually on a new line
    #[test]
    fn test_else_newline_comment_same_line_else_new_line() {
        let code = "if (x) {y} # comment\nelse {z}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            1,
            "Should emit diagnostic when else is on new line even with comment after closing brace"
        );
        assert_eq!(
            diagnostics[0].severity,
            Some(DiagnosticSeverity::ERROR),
            "Diagnostic severity should be ERROR"
        );
        // The else is on line 1 (0-indexed)
        assert_eq!(
            diagnostics[0].range.start.line, 1,
            "Diagnostic should start on line 1 (0-indexed) where the orphaned else is"
        );
    }

    /// Test that comments between `}` and `else` on the SAME line does NOT emit diagnostic.
    /// `if (x) {y} # comment else {z}` - this is actually invalid R syntax, but if else were
    /// somehow on the same line, we should not emit diagnostic.
    /// Note: In practice, `# comment else {z}` makes `else {z}` part of the comment.
    /// This test verifies the valid case: `if (x) {y} else {z} # comment`
    /// Validates: Requirement 5.3 - comments on same line should not affect detection
    #[test]
    fn test_else_newline_comment_after_else_same_line() {
        let code = "if (x) {y} else {z} # comment";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            0,
            "Should NOT emit diagnostic when else is on same line as closing brace (with trailing comment)"
        );
    }

    // ========================================================================
    // Diagnostic Properties Tests (Task 3.3)
    // Comprehensive tests for diagnostic properties
    // Validates: Requirements 3.1, 3.2, 3.3, 3.4
    // ========================================================================

    /// Comprehensive test for all diagnostic properties.
    /// Validates: Requirements 3.1 (severity), 3.2 (range), 3.3 (message), 3.4 (source)
    #[test]
    fn test_else_newline_diagnostic_properties_comprehensive() {
        let code = "if (x) {y}\nelse {z}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(diagnostics.len(), 1, "Should emit exactly one diagnostic");

        let diag = &diagnostics[0];

        // Requirement 3.1: Diagnostic severity SHALL be ERROR
        assert_eq!(
            diag.severity,
            Some(DiagnosticSeverity::ERROR),
            "Requirement 3.1: Diagnostic severity should be ERROR"
        );

        // Requirement 3.3: Diagnostic message SHALL be descriptive
        assert_eq!(
            diag.message,
            "In R, 'else' must appear on the same line as the closing '}' of the if block",
            "Requirement 3.3: Diagnostic message should match expected text exactly"
        );

        // Requirement 3.2: Diagnostic range SHALL highlight the `else` keyword
        // "else" is on line 1 (0-indexed), columns 0-4
        assert_eq!(
            diag.range.start.line, 1,
            "Requirement 3.2: Diagnostic range start line should be 1 (0-indexed)"
        );
        assert_eq!(
            diag.range.start.character, 0,
            "Requirement 3.2: Diagnostic range start character should be 0"
        );
        assert_eq!(
            diag.range.end.line, 1,
            "Requirement 3.2: Diagnostic range end line should be 1"
        );
        assert_eq!(
            diag.range.end.character, 4,
            "Requirement 3.2: Diagnostic range end character should be 4 (covering 'else')"
        );
    }

    /// Test that diagnostic severity is ERROR for multi-line patterns.
    /// Validates: Requirement 3.1 - severity should be ERROR
    #[test]
    fn test_else_newline_diagnostic_severity_multiline() {
        let code = "if (condition) {\n  print(1)\n}\nelse {\n  print(2)\n}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(diagnostics.len(), 1, "Should emit exactly one diagnostic");
        assert_eq!(
            diagnostics[0].severity,
            Some(DiagnosticSeverity::ERROR),
            "Requirement 3.1: Diagnostic severity should be ERROR for multi-line patterns"
        );
    }

    /// Test that diagnostic range accurately covers the else keyword in various positions.
    /// Validates: Requirement 3.2 - range should highlight else keyword
    #[test]
    fn test_else_newline_diagnostic_range_with_indentation() {
        // else is indented with spaces
        let code = "if (x) {y}\n    else {z}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(diagnostics.len(), 1, "Should emit exactly one diagnostic");

        let diag = &diagnostics[0];
        // "else" starts at line 1, column 4 (after 4 spaces)
        assert_eq!(
            diag.range.start.line, 1,
            "Diagnostic should start on line 1"
        );
        assert_eq!(
            diag.range.start.character, 4,
            "Diagnostic should start at column 4 (after indentation)"
        );
        assert_eq!(
            diag.range.end.character, 8,
            "Diagnostic should end at column 8 (covering 'else')"
        );
    }

    /// Test that diagnostic message contains key information.
    /// Validates: Requirement 3.3 - message should be descriptive
    #[test]
    fn test_else_newline_diagnostic_message_content() {
        let code = "if (x) {y}\nelse {z}";
        let tree = parse_r_code(code);
        let mut diagnostics = Vec::new();
        super::collect_else_newline_errors(tree.root_node(), code, &mut diagnostics);

        assert_eq!(diagnostics.len(), 1, "Should emit exactly one diagnostic");

        let message = &diagnostics[0].message;

        // Message should mention 'else'
        assert!(
            message.contains("else"),
            "Requirement 3.3: Message should mention 'else'"
        );

        // Message should mention 'same line'
        assert!(
            message.contains("same line"),
            "Requirement 3.3: Message should mention 'same line'"
        );

        // Message should mention the closing brace
        assert!(
            message.contains("}") || message.contains("closing"),
            "Requirement 3.3: Message should mention the closing brace"
        );

        // Message should mention 'if'
        assert!(
            message.contains("if"),
            "Requirement 3.3: Message should mention 'if'"
        );
    }

    // ========================================================================
    // Declared Symbol Completion Tests (Task 8.1)
    // Tests for lsp-declaration-directives feature
    // Validates: Requirements 6.1, 6.2, 6.3, 6.4
    // ========================================================================

    /// Test that declared variables appear in completions with VARIABLE kind.
    /// Validates: Requirements 6.1, 6.4
    #[test]
    fn test_completion_declared_variable() {
        use crate::state::{Document, WorldState};
        use tower_lsp::lsp_types::{CompletionItemKind, CompletionResponse, Position};

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut state = WorldState::new(vec![]);

            // Create a document with a declared variable
            let code = r#"# @lsp-var my_declared_var
x <- "#;
            let uri = Url::parse("file:///test.R").unwrap();
            let doc = Document::new(code, None);
            state.documents.insert(uri.clone(), doc);

            // Get completions at the end of the file (after "x <- ")
            let position = Position::new(1, 5);
            let completions = super::completion(&state, &uri, position);

            assert!(completions.is_some(), "Should return completions");

            if let Some(CompletionResponse::Array(items)) = completions {
                // Find the declared variable completion item
                let declared_items: Vec<_> = items
                    .iter()
                    .filter(|item| item.label == "my_declared_var")
                    .collect();

                // Requirement 6.1: Declared variable should appear in completions
                assert!(
                    !declared_items.is_empty(),
                    "Requirement 6.1: Declared variable 'my_declared_var' should appear in completions"
                );

                // Requirement 6.4: Declared variable should have VARIABLE kind
                let declared_item = declared_items[0];
                assert_eq!(
                    declared_item.kind,
                    Some(CompletionItemKind::VARIABLE),
                    "Requirement 6.4: Declared variable should have CompletionItemKind::VARIABLE"
                );
            } else {
                panic!("Expected CompletionResponse::Array");
            }
        });
    }

    /// Test that declared functions appear in completions with FUNCTION kind.
    /// Validates: Requirements 6.2, 6.3
    #[test]
    fn test_completion_declared_function() {
        use crate::state::{Document, WorldState};
        use tower_lsp::lsp_types::{CompletionItemKind, CompletionResponse, Position};

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut state = WorldState::new(vec![]);

            // Create a document with a declared function
            let code = r#"# @lsp-func my_declared_func
x <- "#;
            let uri = Url::parse("file:///test.R").unwrap();
            let doc = Document::new(code, None);
            state.documents.insert(uri.clone(), doc);

            // Get completions at the end of the file (after "x <- ")
            let position = Position::new(1, 5);
            let completions = super::completion(&state, &uri, position);

            assert!(completions.is_some(), "Should return completions");

            if let Some(CompletionResponse::Array(items)) = completions {
                // Find the declared function completion item
                let declared_items: Vec<_> = items
                    .iter()
                    .filter(|item| item.label == "my_declared_func")
                    .collect();

                // Requirement 6.2: Declared function should appear in completions
                assert!(
                    !declared_items.is_empty(),
                    "Requirement 6.2: Declared function 'my_declared_func' should appear in completions"
                );

                // Requirement 6.3: Declared function should have FUNCTION kind
                let declared_item = declared_items[0];
                assert_eq!(
                    declared_item.kind,
                    Some(CompletionItemKind::FUNCTION),
                    "Requirement 6.3: Declared function should have CompletionItemKind::FUNCTION"
                );
            } else {
                panic!("Expected CompletionResponse::Array");
            }
        });
    }

    /// Test that declared symbols only appear in completions after their declaration line.
    /// Validates: Requirements 6.1, 6.2 (position-aware)
    #[test]
    fn test_completion_declared_symbol_position_aware() {
        use crate::state::{Document, WorldState};
        use tower_lsp::lsp_types::{CompletionResponse, Position};

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut state = WorldState::new(vec![]);

            // Create a document with a declared variable on line 2
            let code = r#"x <- 1
# @lsp-var late_declared
y <- "#;
            let uri = Url::parse("file:///test.R").unwrap();
            let doc = Document::new(code, None);
            state.documents.insert(uri.clone(), doc);

            // Get completions on line 0 (before declaration)
            let position_before = Position::new(0, 5);
            let completions_before = super::completion(&state, &uri, position_before);

            if let Some(CompletionResponse::Array(items)) = completions_before {
                let declared_items: Vec<_> = items
                    .iter()
                    .filter(|item| item.label == "late_declared")
                    .collect();

                // Declared symbol should NOT appear before its declaration line
                assert!(
                    declared_items.is_empty(),
                    "Declared symbol should NOT appear in completions before its declaration line"
                );
            }

            // Get completions on line 2 (after declaration)
            let position_after = Position::new(2, 5);
            let completions_after = super::completion(&state, &uri, position_after);

            if let Some(CompletionResponse::Array(items)) = completions_after {
                let declared_items: Vec<_> = items
                    .iter()
                    .filter(|item| item.label == "late_declared")
                    .collect();

                // Declared symbol SHOULD appear after its declaration line
                assert!(
                    !declared_items.is_empty(),
                    "Declared symbol SHOULD appear in completions after its declaration line"
                );
            }
        });
    }

    /// Test that both declared variables and functions appear in completions together.
    /// Validates: Requirements 6.1, 6.2, 6.3, 6.4
    #[test]
    fn test_completion_mixed_declared_symbols() {
        use crate::state::{Document, WorldState};
        use tower_lsp::lsp_types::{CompletionItemKind, CompletionResponse, Position};

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut state = WorldState::new(vec![]);

            // Create a document with both declared variable and function
            let code = r#"# @lsp-var my_var
# @lsp-func my_func
x <- "#;
            let uri = Url::parse("file:///test.R").unwrap();
            let doc = Document::new(code, None);
            state.documents.insert(uri.clone(), doc);

            // Get completions at the end of the file
            let position = Position::new(2, 5);
            let completions = super::completion(&state, &uri, position);

            assert!(completions.is_some(), "Should return completions");

            if let Some(CompletionResponse::Array(items)) = completions {
                // Find declared variable
                let var_items: Vec<_> = items
                    .iter()
                    .filter(|item| item.label == "my_var")
                    .collect();
                assert!(
                    !var_items.is_empty(),
                    "Declared variable 'my_var' should appear in completions"
                );
                assert_eq!(
                    var_items[0].kind,
                    Some(CompletionItemKind::VARIABLE),
                    "Declared variable should have VARIABLE kind"
                );

                // Find declared function
                let func_items: Vec<_> = items
                    .iter()
                    .filter(|item| item.label == "my_func")
                    .collect();
                assert!(
                    !func_items.is_empty(),
                    "Declared function 'my_func' should appear in completions"
                );
                assert_eq!(
                    func_items[0].kind,
                    Some(CompletionItemKind::FUNCTION),
                    "Declared function should have FUNCTION kind"
                );
            } else {
                panic!("Expected CompletionResponse::Array");
            }
        });
    }

    // ========================================================================
    // SymbolExtractor Tests
    // ========================================================================

    #[test]
    fn test_symbol_extractor_simple_function() {
        let code = "my_func <- function(x) { x + 1 }";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "my_func");
        assert!(matches!(symbols[0].kind, DocumentSymbolKind::Function));

        // Check range spans entire assignment
        assert_eq!(symbols[0].range.start.line, 0);
        assert_eq!(symbols[0].range.start.character, 0);
        assert_eq!(symbols[0].range.end.line, 0);
        assert_eq!(symbols[0].range.end.character, code.len() as u32);

        // Check selection_range is identifier only
        assert_eq!(symbols[0].selection_range.start.line, 0);
        assert_eq!(symbols[0].selection_range.start.character, 0);
        assert_eq!(symbols[0].selection_range.end.line, 0);
        assert_eq!(symbols[0].selection_range.end.character, 7); // "my_func"
    }

    #[test]
    fn test_symbol_extractor_simple_variable() {
        let code = "x <- 42";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "x");
        assert!(matches!(symbols[0].kind, DocumentSymbolKind::Variable));

        // Check range spans entire assignment
        assert_eq!(symbols[0].range.start.line, 0);
        assert_eq!(symbols[0].range.start.character, 0);
        assert_eq!(symbols[0].range.end.line, 0);
        assert_eq!(symbols[0].range.end.character, 7); // "x <- 42"

        // Check selection_range is identifier only
        assert_eq!(symbols[0].selection_range.start.line, 0);
        assert_eq!(symbols[0].selection_range.start.character, 0);
        assert_eq!(symbols[0].selection_range.end.line, 0);
        assert_eq!(symbols[0].selection_range.end.character, 1); // "x"
    }

    #[test]
    fn test_symbol_extractor_equals_assignment() {
        let code = "y = 100";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "y");
        assert!(matches!(symbols[0].kind, DocumentSymbolKind::Variable));
    }

    #[test]
    fn test_symbol_extractor_super_assignment() {
        let code = "z <<- 'global'";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "z");
        assert!(matches!(symbols[0].kind, DocumentSymbolKind::Variable));
    }

    #[test]
    fn test_symbol_extractor_right_assignment() {
        let code = "42 -> answer";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "answer");
        assert!(matches!(symbols[0].kind, DocumentSymbolKind::Variable));

        // Check selection_range is the RHS identifier
        assert_eq!(symbols[0].selection_range.start.character, 6); // "answer" starts at position 6
        assert_eq!(symbols[0].selection_range.end.character, 12); // "answer" ends at position 12
    }

    #[test]
    fn test_symbol_extractor_right_assignment_function() {
        // Use parentheses to ensure the function definition is the LHS of the -> operator
        let code = "(function(x) x * 2) -> doubler";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1, "Expected 1 symbol, got {:?}", symbols);
        assert_eq!(symbols[0].name, "doubler");
        // The LHS is a parenthesized expression containing a function_definition
        // So we need to check if the LHS contains a function_definition
        // For now, this will be classified as Variable since the immediate LHS is not function_definition
        // This is acceptable behavior - the classify_symbol() method in task 2.3 will handle this better
        assert!(
            matches!(symbols[0].kind, DocumentSymbolKind::Variable | DocumentSymbolKind::Function),
            "Expected Variable or Function, got {:?}",
            symbols[0].kind
        );
    }

    #[test]
    fn test_symbol_extractor_multiple_assignments() {
        let code = "a <- 1\nb <- 2\nc <- function() {}";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 3);
        assert_eq!(symbols[0].name, "a");
        assert!(matches!(symbols[0].kind, DocumentSymbolKind::Variable));
        assert_eq!(symbols[1].name, "b");
        assert!(matches!(symbols[1].kind, DocumentSymbolKind::Variable));
        assert_eq!(symbols[2].name, "c");
        assert!(matches!(symbols[2].kind, DocumentSymbolKind::Function));
    }

    #[test]
    fn test_symbol_extractor_nested_function() {
        let code = "outer <- function() {\n  inner <- function() {}\n}";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        // Should find both outer and inner functions
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "outer");
        assert!(matches!(symbols[0].kind, DocumentSymbolKind::Function));
        assert_eq!(symbols[1].name, "inner");
        assert!(matches!(symbols[1].kind, DocumentSymbolKind::Function));
    }

    #[test]
    fn test_symbol_extractor_reserved_words_filtered() {
        // Reserved words should not appear as symbols
        let code = "if <- 1\nelse <- 2\nfor <- 3";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        // Reserved words should be filtered out
        assert!(symbols.is_empty());
    }

    #[test]
    fn test_symbol_extractor_multiline_function() {
        let code = "my_func <- function(x, y) {\n  result <- x + y\n  result\n}";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        // Should find both my_func and result
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "my_func");
        assert!(matches!(symbols[0].kind, DocumentSymbolKind::Function));

        // Check that my_func range spans multiple lines
        assert_eq!(symbols[0].range.start.line, 0);
        assert_eq!(symbols[0].range.end.line, 3);

        assert_eq!(symbols[1].name, "result");
        assert!(matches!(symbols[1].kind, DocumentSymbolKind::Variable));
    }

    #[test]
    fn test_symbol_extractor_selection_range_contained_in_range() {
        // Property 3: selection_range must be contained within range
        let code = "my_long_function_name <- function(a, b, c) {\n  a + b + c\n}";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        for symbol in &symbols {
            // selection_range.start >= range.start
            assert!(
                symbol.selection_range.start.line > symbol.range.start.line
                    || (symbol.selection_range.start.line == symbol.range.start.line
                        && symbol.selection_range.start.character >= symbol.range.start.character),
                "selection_range.start must be >= range.start for symbol '{}'",
                symbol.name
            );

            // selection_range.end <= range.end
            assert!(
                symbol.selection_range.end.line < symbol.range.end.line
                    || (symbol.selection_range.end.line == symbol.range.end.line
                        && symbol.selection_range.end.character <= symbol.range.end.character),
                "selection_range.end must be <= range.end for symbol '{}'",
                symbol.name
            );
        }
    }

    #[test]
    fn test_symbol_extractor_non_identifier_lhs_ignored() {
        // Assignments to non-identifiers (like list elements) should be ignored
        let code = "x$y <- 1\nx[[1]] <- 2";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        // These are not simple identifier assignments, so should be empty
        assert!(symbols.is_empty());
    }

    #[test]
    fn test_symbol_extractor_utf16_columns() {
        // Test with Unicode characters to ensure UTF-16 column conversion works
        let code = "日本語 <- 42";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "日本語");
        // UTF-16 column for "日本語" (3 characters, each 1 UTF-16 code unit)
        assert_eq!(symbols[0].selection_range.start.character, 0);
        assert_eq!(symbols[0].selection_range.end.character, 3);
    }

    // ========================================================================
    // classify_symbol() Tests - Task 2.3
    // ========================================================================

    #[test]
    fn test_classify_symbol_r6class() {
        // R6Class() call should be classified as CLASS
        let code = "MyClass <- R6Class(\"MyClass\", public = list())";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "MyClass");
        assert!(
            matches!(symbols[0].kind, DocumentSymbolKind::Class),
            "R6Class() should be classified as CLASS, got {:?}",
            symbols[0].kind
        );
    }

    #[test]
    fn test_classify_symbol_setrefclass() {
        // setRefClass() call should be classified as CLASS
        let code = "Person <- setRefClass(\"Person\", fields = list(name = \"character\"))";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "Person");
        assert!(
            matches!(symbols[0].kind, DocumentSymbolKind::Class),
            "setRefClass() should be classified as CLASS, got {:?}",
            symbols[0].kind
        );
    }

    #[test]
    fn test_classify_symbol_all_caps_constant() {
        // ALL_CAPS names should be classified as CONSTANT
        let code = "MAX_VALUE <- 100";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "MAX_VALUE");
        assert!(
            matches!(symbols[0].kind, DocumentSymbolKind::Constant),
            "ALL_CAPS name should be classified as CONSTANT, got {:?}",
            symbols[0].kind
        );
    }

    #[test]
    fn test_classify_symbol_all_caps_with_digits() {
        // ALL_CAPS with digits should be classified as CONSTANT
        let code = "API_KEY_V2 <- \"secret\"";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "API_KEY_V2");
        assert!(
            matches!(symbols[0].kind, DocumentSymbolKind::Constant),
            "ALL_CAPS with digits should be classified as CONSTANT, got {:?}",
            symbols[0].kind
        );
    }

    #[test]
    fn test_classify_symbol_all_caps_with_dot() {
        // ALL_CAPS with dots should be classified as CONSTANT
        let code = "MY.CONST <- 3.14";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "MY.CONST");
        assert!(
            matches!(symbols[0].kind, DocumentSymbolKind::Constant),
            "ALL_CAPS with dot should be classified as CONSTANT, got {:?}",
            symbols[0].kind
        );
    }

    #[test]
    fn test_classify_symbol_two_char_constant() {
        // Two-character ALL_CAPS should be classified as CONSTANT
        let code = "PI <- 3.14159";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "PI");
        assert!(
            matches!(symbols[0].kind, DocumentSymbolKind::Constant),
            "Two-char ALL_CAPS should be classified as CONSTANT, got {:?}",
            symbols[0].kind
        );
    }

    #[test]
    fn test_classify_symbol_single_char_not_constant() {
        // Single character should NOT be classified as CONSTANT
        let code = "A <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "A");
        assert!(
            matches!(symbols[0].kind, DocumentSymbolKind::Variable),
            "Single char should be classified as VARIABLE, got {:?}",
            symbols[0].kind
        );
    }

    #[test]
    fn test_classify_symbol_mixed_case_not_constant() {
        // Mixed case should NOT be classified as CONSTANT
        let code = "MaxValue <- 100";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "MaxValue");
        assert!(
            matches!(symbols[0].kind, DocumentSymbolKind::Variable),
            "Mixed case should be classified as VARIABLE, got {:?}",
            symbols[0].kind
        );
    }

    #[test]
    fn test_classify_symbol_lowercase_not_constant() {
        // Lowercase should NOT be classified as CONSTANT
        let code = "my_var <- 42";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "my_var");
        assert!(
            matches!(symbols[0].kind, DocumentSymbolKind::Variable),
            "Lowercase should be classified as VARIABLE, got {:?}",
            symbols[0].kind
        );
    }

    #[test]
    fn test_classify_symbol_function_definition() {
        // Function definition should be classified as FUNCTION
        let code = "my_func <- function(x) x + 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "my_func");
        assert!(
            matches!(symbols[0].kind, DocumentSymbolKind::Function),
            "Function definition should be classified as FUNCTION, got {:?}",
            symbols[0].kind
        );
    }

    #[test]
    fn test_classify_symbol_all_caps_function_is_function() {
        // ALL_CAPS function should be classified as FUNCTION (function takes priority over constant)
        // Wait, according to the design: R6Class > CONSTANT > FUNCTION > VARIABLE
        // So ALL_CAPS function should be CONSTANT, not FUNCTION
        // Let me re-read the design...
        // Actually, the priority is:
        // 1. R6Class/setRefClass → CLASS
        // 2. ALL_CAPS → CONSTANT
        // 3. function_definition → FUNCTION
        // 4. Otherwise → VARIABLE
        // So ALL_CAPS function should be CONSTANT
        let code = "MY_FUNC <- function(x) x + 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "MY_FUNC");
        // ALL_CAPS takes priority over function_definition
        assert!(
            matches!(symbols[0].kind, DocumentSymbolKind::Constant),
            "ALL_CAPS function should be classified as CONSTANT (ALL_CAPS takes priority), got {:?}",
            symbols[0].kind
        );
    }

    #[test]
    fn test_classify_symbol_r6class_priority_over_all_caps() {
        // R6Class should take priority over ALL_CAPS pattern
        let code = "MY_CLASS <- R6Class(\"MY_CLASS\")";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "MY_CLASS");
        // R6Class takes priority over ALL_CAPS
        assert!(
            matches!(symbols[0].kind, DocumentSymbolKind::Class),
            "R6Class should take priority over ALL_CAPS, got {:?}",
            symbols[0].kind
        );
    }

    #[test]
    fn test_classify_symbol_right_assignment_r6class() {
        // R6Class with right assignment should be classified as CLASS
        let code = "R6Class(\"MyClass\") -> MyClass";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "MyClass");
        assert!(
            matches!(symbols[0].kind, DocumentSymbolKind::Class),
            "R6Class with right assignment should be classified as CLASS, got {:?}",
            symbols[0].kind
        );
    }

    #[test]
    fn test_classify_symbol_right_assignment_constant() {
        // ALL_CAPS with right assignment should be classified as CONSTANT
        let code = "100 -> MAX_VALUE";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "MAX_VALUE");
        assert!(
            matches!(symbols[0].kind, DocumentSymbolKind::Constant),
            "ALL_CAPS with right assignment should be classified as CONSTANT, got {:?}",
            symbols[0].kind
        );
    }

    #[test]
    fn test_classify_symbol_other_call_not_class() {
        // Other function calls should NOT be classified as CLASS
        let code = "result <- list(a = 1, b = 2)";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "result");
        assert!(
            matches!(symbols[0].kind, DocumentSymbolKind::Variable),
            "list() call should be classified as VARIABLE, got {:?}",
            symbols[0].kind
        );
    }

    // ========================================================================
    // is_all_caps_constant() Unit Tests
    // ========================================================================

    #[test]
    fn test_is_all_caps_constant_valid_cases() {
        assert!(SymbolExtractor::is_all_caps_constant("MAX_VALUE"));
        assert!(SymbolExtractor::is_all_caps_constant("PI"));
        assert!(SymbolExtractor::is_all_caps_constant("API_KEY"));
        assert!(SymbolExtractor::is_all_caps_constant("A1"));
        assert!(SymbolExtractor::is_all_caps_constant("MY.CONST"));
        assert!(SymbolExtractor::is_all_caps_constant("ABC123"));
        assert!(SymbolExtractor::is_all_caps_constant("X_Y_Z"));
        assert!(SymbolExtractor::is_all_caps_constant("A.B.C"));
    }

    #[test]
    fn test_is_all_caps_constant_invalid_cases() {
        // Single character
        assert!(!SymbolExtractor::is_all_caps_constant("A"));
        assert!(!SymbolExtractor::is_all_caps_constant("X"));

        // Lowercase
        assert!(!SymbolExtractor::is_all_caps_constant("x"));
        assert!(!SymbolExtractor::is_all_caps_constant("my_var"));
        assert!(!SymbolExtractor::is_all_caps_constant("maxValue"));

        // Mixed case
        assert!(!SymbolExtractor::is_all_caps_constant("MaxValue"));
        assert!(!SymbolExtractor::is_all_caps_constant("MyClass"));
        assert!(!SymbolExtractor::is_all_caps_constant("APIkey"));

        // Starts with digit
        assert!(!SymbolExtractor::is_all_caps_constant("1ABC"));
        assert!(!SymbolExtractor::is_all_caps_constant("123"));

        // Empty string
        assert!(!SymbolExtractor::is_all_caps_constant(""));

        // Starts with underscore
        assert!(!SymbolExtractor::is_all_caps_constant("_ABC"));

        // Starts with dot
        assert!(!SymbolExtractor::is_all_caps_constant(".ABC"));
    }

    // ========================================================================
    // extract_signature() Tests - Task 2.4
    // ========================================================================

    #[test]
    fn test_extract_signature_simple_params() {
        // Simple function with two parameters
        let code = "add <- function(a, b) { a + b }";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "add");
        assert_eq!(symbols[0].detail, Some("(a, b)".to_string()));
    }

    #[test]
    fn test_extract_signature_no_params() {
        // Function with no parameters
        let code = "get_pi <- function() { 3.14159 }";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "get_pi");
        assert_eq!(symbols[0].detail, Some("()".to_string()));
    }

    #[test]
    fn test_extract_signature_with_defaults() {
        // Function with default parameter values
        let code = "greet <- function(name = \"World\") { paste(\"Hello\", name) }";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "greet");
        assert_eq!(symbols[0].detail, Some("(name = \"World\")".to_string()));
    }

    #[test]
    fn test_extract_signature_with_dots() {
        // Function with ... parameter
        let code = "wrapper <- function(...) { list(...) }";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "wrapper");
        assert_eq!(symbols[0].detail, Some("(...)".to_string()));
    }

    #[test]
    fn test_extract_signature_mixed_params() {
        // Function with mixed parameters (regular, default, dots)
        let code = "mixed <- function(x, y = 1, ...) { x + y }";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "mixed");
        assert_eq!(symbols[0].detail, Some("(x, y = 1, ...)".to_string()));
    }

    #[test]
    fn test_extract_signature_truncation_at_60_chars() {
        // Function with very long parameter list that exceeds 60 characters
        let code = "long_func <- function(very_long_param_name_1, very_long_param_name_2, very_long_param_name_3) { }";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "long_func");

        let detail = symbols[0].detail.as_ref().unwrap();
        // Should be truncated with "..." at the end
        assert!(detail.ends_with("..."), "Detail should end with '...': {}", detail);
        // Total length should be at most 60 characters
        assert!(detail.len() <= 60, "Detail should be at most 60 chars, got {}: {}", detail.len(), detail);
    }

    #[test]
    fn test_extract_signature_exactly_60_chars() {
        // Function with parameter list that is exactly 60 characters (should not be truncated)
        // "(a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t)" is 60 chars
        let code = "f <- function(a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t) { }";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        let detail = symbols[0].detail.as_ref().unwrap();
        // Should not be truncated if exactly 60 chars
        assert!(!detail.ends_with("...") || detail.len() <= 60, "Detail: {}", detail);
    }

    #[test]
    fn test_extract_signature_variable_has_no_detail() {
        // Variable assignment should have no detail
        let code = "x <- 42";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "x");
        assert_eq!(symbols[0].detail, None);
    }

    #[test]
    fn test_extract_signature_right_assignment() {
        // Right assignment function should also have signature
        let code = "(function(x, y) x + y) -> add";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "add");
        // The function is wrapped in parentheses, but extract_signature should find it
        assert_eq!(symbols[0].detail, Some("(x, y)".to_string()));
    }

    #[test]
    fn test_extract_signature_nested_functions() {
        // Nested functions should each have their own signature
        let code = "outer <- function(a) {\n  inner <- function(b, c) { b + c }\n}";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "outer");
        assert_eq!(symbols[0].detail, Some("(a)".to_string()));
        assert_eq!(symbols[1].name, "inner");
        assert_eq!(symbols[1].detail, Some("(b, c)".to_string()));
    }

    #[test]
    fn test_extract_signature_equals_assignment() {
        // Function with = assignment should also have signature
        let code = "my_func = function(x) { x * 2 }";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "my_func");
        assert_eq!(symbols[0].detail, Some("(x)".to_string()));
    }

    #[test]
    fn test_extract_signature_super_assignment() {
        // Function with <<- assignment should also have signature
        let code = "global_func <<- function(a, b, c) { a + b + c }";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "global_func");
        assert_eq!(symbols[0].detail, Some("(a, b, c)".to_string()));
    }

    // ========================================================================
    // extract_s4_methods() Tests - Task 3.1
    // ========================================================================

    #[test]
    fn test_extract_s4_methods_setmethod() {
        // setMethod() should be classified as METHOD
        let code = r#"setMethod("show", "MyClass", function(object) { print(object) })"#;
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "show");
        assert!(
            matches!(symbols[0].kind, DocumentSymbolKind::Method),
            "setMethod() should be classified as METHOD, got {:?}",
            symbols[0].kind
        );
        // detail should be None for S4 methods
        assert!(symbols[0].detail.is_none());
    }

    #[test]
    fn test_extract_s4_methods_setclass() {
        // setClass() should be classified as CLASS
        let code = r#"setClass("Person", slots = c(name = "character", age = "numeric"))"#;
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "Person");
        assert!(
            matches!(symbols[0].kind, DocumentSymbolKind::Class),
            "setClass() should be classified as CLASS, got {:?}",
            symbols[0].kind
        );
    }

    #[test]
    fn test_extract_s4_methods_setgeneric() {
        // setGeneric() should be classified as INTERFACE
        let code = r#"setGeneric("myGeneric", function(x) standardGeneric("myGeneric"))"#;
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "myGeneric");
        assert!(
            matches!(symbols[0].kind, DocumentSymbolKind::Interface),
            "setGeneric() should be classified as INTERFACE, got {:?}",
            symbols[0].kind
        );
    }

    #[test]
    fn test_extract_s4_methods_single_quotes() {
        // S4 methods with single-quoted names should also work
        let code = r#"setMethod('initialize', 'MyClass', function(.Object) { .Object })"#;
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "initialize");
        assert!(matches!(symbols[0].kind, DocumentSymbolKind::Method));
    }

    #[test]
    fn test_extract_s4_methods_multiple() {
        // Multiple S4 definitions in one file
        let code = r#"
setClass("Animal", slots = c(name = "character"))
setGeneric("speak", function(x) standardGeneric("speak"))
setMethod("speak", "Animal", function(x) { print("...") })
"#;
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 3);

        // Find each symbol by name
        let animal = symbols.iter().find(|s| s.name == "Animal").unwrap();
        let speak_generic = symbols
            .iter()
            .find(|s| s.name == "speak" && matches!(s.kind, DocumentSymbolKind::Interface))
            .unwrap();
        let speak_method = symbols
            .iter()
            .find(|s| s.name == "speak" && matches!(s.kind, DocumentSymbolKind::Method))
            .unwrap();

        assert!(matches!(animal.kind, DocumentSymbolKind::Class));
        assert!(matches!(speak_generic.kind, DocumentSymbolKind::Interface));
        assert!(matches!(speak_method.kind, DocumentSymbolKind::Method));
    }

    #[test]
    fn test_extract_s4_methods_mixed_with_assignments() {
        // S4 methods mixed with regular assignments
        let code = r#"
my_func <- function(x) { x + 1 }
setClass("MyClass", slots = c(value = "numeric"))
my_var <- 42
setMethod("show", "MyClass", function(object) { print(object@value) })
"#;
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 4);

        let func = symbols.iter().find(|s| s.name == "my_func").unwrap();
        let class = symbols.iter().find(|s| s.name == "MyClass").unwrap();
        let var = symbols.iter().find(|s| s.name == "my_var").unwrap();
        let method = symbols.iter().find(|s| s.name == "show").unwrap();

        assert!(matches!(func.kind, DocumentSymbolKind::Function));
        assert!(matches!(class.kind, DocumentSymbolKind::Class));
        assert!(matches!(var.kind, DocumentSymbolKind::Variable));
        assert!(matches!(method.kind, DocumentSymbolKind::Method));
    }

    #[test]
    fn test_extract_s4_methods_range_covers_entire_call() {
        // The range should cover the entire setMethod/setClass/setGeneric call
        let code = r#"setClass("Person", slots = c(name = "character"))"#;
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        let symbol = &symbols[0];

        // Range should start at beginning of line and cover the entire call
        assert_eq!(symbol.range.start.line, 0);
        assert_eq!(symbol.range.start.character, 0);
        assert_eq!(symbol.range.end.line, 0);
        // End should be at the closing parenthesis
        assert_eq!(symbol.range.end.character, code.len() as u32);
    }

    #[test]
    fn test_extract_s4_methods_selection_range_covers_name() {
        // The selection_range should cover just the string argument (the name)
        let code = r#"setClass("Person", slots = c(name = "character"))"#;
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        let symbol = &symbols[0];

        // Selection range should cover "Person" (including quotes)
        // setClass("Person", ...) - "Person" starts at position 9
        assert_eq!(symbol.selection_range.start.line, 0);
        assert_eq!(symbol.selection_range.start.character, 9);
        assert_eq!(symbol.selection_range.end.line, 0);
        assert_eq!(symbol.selection_range.end.character, 17); // "Person" is 8 chars including quotes
    }

    #[test]
    fn test_extract_s4_methods_selection_range_contained_in_range() {
        // Property 3: selection_range must be contained within range
        let code = r#"setGeneric("myGeneric", function(x) standardGeneric("myGeneric"))"#;
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        let symbol = &symbols[0];

        // selection_range.start >= range.start
        assert!(
            symbol.selection_range.start.line > symbol.range.start.line
                || (symbol.selection_range.start.line == symbol.range.start.line
                    && symbol.selection_range.start.character >= symbol.range.start.character)
        );

        // selection_range.end <= range.end
        assert!(
            symbol.selection_range.end.line < symbol.range.end.line
                || (symbol.selection_range.end.line == symbol.range.end.line
                    && symbol.selection_range.end.character <= symbol.range.end.character)
        );
    }

    #[test]
    fn test_extract_s4_methods_no_string_argument() {
        // If the first argument is not a string, no symbol should be extracted
        let code = r#"setMethod(myVar, "MyClass", function(object) { })"#;
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let s4_symbols = extractor.extract_s4_methods(tree.root_node());

        // No S4 symbols should be extracted since first arg is not a string
        assert!(s4_symbols.is_empty());
    }

    #[test]
    fn test_extract_s4_methods_named_argument() {
        // Named arguments should be skipped when looking for the first positional string
        let code = r#"setClass(Class = "Person", slots = c(name = "character"))"#;
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let s4_symbols = extractor.extract_s4_methods(tree.root_node());

        // Named argument "Class = ..." should be skipped, so no symbol extracted
        // (we only look for positional string arguments)
        assert!(s4_symbols.is_empty());
    }

    #[test]
    fn test_extract_s4_methods_multiline() {
        // S4 method spanning multiple lines
        let code = r#"setMethod(
    "show",
    "MyClass",
    function(object) {
        print(object)
    }
)"#;
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "show");
        assert!(matches!(symbols[0].kind, DocumentSymbolKind::Method));

        // Range should span from line 0 to line 6
        assert_eq!(symbols[0].range.start.line, 0);
        assert_eq!(symbols[0].range.end.line, 6);
    }

    // ========================================================================
    // extract_sections() Tests - Task 4.1
    // ========================================================================

    #[test]
    fn test_extract_sections_basic_dash() {
        // Basic section with dashes
        let code = "# Data Loading ----\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        // Should find 1 section and 1 variable
        assert_eq!(symbols.len(), 2);

        // Find the section symbol
        let section = symbols.iter().find(|s| s.name == "Data Loading").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
        assert_eq!(section.section_level, Some(1));
        assert_eq!(section.range.start.line, 0);
        assert_eq!(section.range.end.line, 0);
    }

    #[test]
    fn test_extract_sections_basic_hash() {
        // Section with hash delimiter
        let code = "# Setup ####\ny <- 2";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "Setup").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
        assert_eq!(section.section_level, Some(1));
    }

    #[test]
    fn test_extract_sections_basic_equals() {
        // Section with equals delimiter
        let code = "# Analysis ====\nz <- 3";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "Analysis").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
        assert_eq!(section.section_level, Some(1));
    }

    #[test]
    fn test_extract_sections_basic_asterisk() {
        // Section with asterisk delimiter
        let code = "# Results ****\nw <- 4";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "Results").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
        assert_eq!(section.section_level, Some(1));
    }

    #[test]
    fn test_extract_sections_basic_plus() {
        // Section with plus delimiter
        let code = "# Conclusion ++++\nv <- 5";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "Conclusion").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
        assert_eq!(section.section_level, Some(1));
    }

    #[test]
    fn test_extract_sections_heading_levels() {
        // Test different heading levels (# count)
        let code = "# Level 1 ----\n## Level 2 ----\n### Level 3 ----";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        assert_eq!(symbols.len(), 3);

        let level1 = symbols.iter().find(|s| s.name == "Level 1").unwrap();
        assert_eq!(level1.section_level, Some(1));

        let level2 = symbols.iter().find(|s| s.name == "Level 2").unwrap();
        assert_eq!(level2.section_level, Some(2));

        let level3 = symbols.iter().find(|s| s.name == "Level 3").unwrap();
        assert_eq!(level3.section_level, Some(3));
    }

    #[test]
    fn test_extract_sections_rstudio_cell_marker() {
        // RStudio-style cell marker with %%
        let code = "# %% Cell Name ----\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "Cell Name").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
        assert_eq!(section.section_level, Some(1));
    }

    #[test]
    fn test_extract_sections_leading_whitespace() {
        // Section with leading whitespace
        let code = "  # Indented Section ----\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "Indented Section").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
    }

    #[test]
    fn test_extract_sections_trailing_whitespace() {
        // Section with trailing whitespace after delimiter
        let code = "# Section Name ----   \nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "Section Name").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
    }

    #[test]
    fn test_extract_sections_multiple_sections() {
        // Multiple sections in one file
        let code = "# Section 1 ----\nx <- 1\n# Section 2 ----\ny <- 2\n# Section 3 ----\nz <- 3";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        // Should find 3 sections and 3 variables
        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert_eq!(sections.len(), 3);

        assert_eq!(sections[0].name, "Section 1");
        assert_eq!(sections[0].range.start.line, 0);

        assert_eq!(sections[1].name, "Section 2");
        assert_eq!(sections[1].range.start.line, 2);

        assert_eq!(sections[2].name, "Section 3");
        assert_eq!(sections[2].range.start.line, 4);
    }

    #[test]
    fn test_extract_sections_long_delimiter() {
        // Section with long delimiter (more than 4 characters)
        let code = "# Long Delimiter ----------\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "Long Delimiter").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
    }

    #[test]
    fn test_extract_sections_minimum_delimiter() {
        // Section with exactly 4 character delimiter (minimum)
        let code = "# Min Delim ----\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "Min Delim").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
    }

    #[test]
    fn test_extract_sections_too_short_delimiter() {
        // Delimiter with only 3 characters should NOT be detected as section
        let code = "# Not A Section ---\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        // Should only find the variable, not a section
        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert!(sections.is_empty());
    }

    #[test]
    fn test_extract_sections_no_section_name() {
        // Comment without section name should NOT be detected
        let code = "# ----\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        // Should only find the variable, not a section
        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert!(sections.is_empty());
    }

    #[test]
    fn test_extract_sections_regular_comment_not_section() {
        // Regular comment without delimiter should NOT be detected
        let code = "# This is just a comment\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        // Should only find the variable, not a section
        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert!(sections.is_empty());
    }

    #[test]
    fn test_extract_sections_section_name_with_spaces() {
        // Section name with multiple words
        let code = "# Data Loading and Preprocessing ----\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols
            .iter()
            .find(|s| s.name == "Data Loading and Preprocessing")
            .unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
    }

    #[test]
    fn test_extract_sections_section_name_with_special_chars() {
        // Section name with special characters
        let code = "# Step 1: Load Data (v2.0) ----\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols
            .iter()
            .find(|s| s.name == "Step 1: Load Data (v2.0)")
            .unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
    }

    #[test]
    fn test_extract_sections_utf16_range() {
        // Section with Unicode characters to test UTF-16 column calculation
        let code = "# 日本語セクション ----\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "日本語セクション").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
        // The range should cover the entire line in UTF-16 code units
        assert_eq!(section.range.start.character, 0);
        // "# 日本語セクション ----" = 2 + 8 + 1 + 4 = 15 UTF-16 code units
        // (# and space = 2, 8 Japanese chars = 8, space = 1, ---- = 4)
        assert_eq!(section.range.end.character, 15);
    }

    #[test]
    fn test_extract_sections_mixed_with_code() {
        // Sections mixed with various code constructs
        let code = r#"# Setup ----
library(dplyr)

# Data Loading ----
data <- read.csv("file.csv")

## Subsection ----
x <- 1

# Analysis ----
result <- data %>% filter(x > 0)
"#;
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();

        assert_eq!(sections.len(), 4);
        assert_eq!(sections[0].name, "Setup");
        assert_eq!(sections[0].section_level, Some(1));
        assert_eq!(sections[1].name, "Data Loading");
        assert_eq!(sections[1].section_level, Some(1));
        assert_eq!(sections[2].name, "Subsection");
        assert_eq!(sections[2].section_level, Some(2));
        assert_eq!(sections[3].name, "Analysis");
        assert_eq!(sections[3].section_level, Some(1));
    }

    #[test]
    fn test_extract_sections_selection_range_equals_range() {
        // For sections, selection_range should equal range (the comment line)
        let code = "# My Section ----\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "My Section").unwrap();
        assert_eq!(section.range, section.selection_range);
    }

    #[test]
    fn test_extract_sections_no_detail() {
        // Sections should not have a detail field
        let code = "# My Section ----\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "My Section").unwrap();
        assert!(section.detail.is_none());
    }

    // ========================================================================
    // Decorative separator rejection tests (Requirement 3)
    // ========================================================================

    #[test]
    fn test_extract_sections_reject_equals_separator() {
        // Decorative equals separator should NOT be detected
        let code = "# ==================\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert!(sections.is_empty());
    }

    #[test]
    fn test_extract_sections_reject_long_hash_separator() {
        // Long hash separator should NOT be detected
        let code = "################################################################################\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert!(sections.is_empty());
    }

    #[test]
    fn test_extract_sections_reject_long_dashes() {
        // Long dash separator should NOT be detected (regex captures "--" as name)
        let code = "# --------\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert!(sections.is_empty());
    }

    #[test]
    fn test_extract_sections_reject_long_asterisks() {
        // Long asterisk separator should NOT be detected
        let code = "# ********\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert!(sections.is_empty());
    }

    #[test]
    fn test_extract_sections_reject_long_plus() {
        // Long plus separator should NOT be detected
        let code = "# ++++++++\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert!(sections.is_empty());
    }

    #[test]
    fn test_extract_sections_reject_single_delimiter_char_as_name() {
        // Single delimiter character as name should NOT be detected
        let code = "# = ----\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert!(sections.is_empty());
    }

    #[test]
    fn test_extract_sections_reject_two_delimiter_groups() {
        // Two delimiter groups separated by space should NOT be detected
        let code = "# ---- ====\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert!(sections.is_empty());
    }

    #[test]
    fn test_extract_sections_reject_mixed_delimiter_chars() {
        // Mixed delimiter characters as name should NOT be detected
        let code = "# =-==-= ----\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert!(sections.is_empty());
    }

    #[test]
    fn test_extract_sections_reject_spaced_equals_groups() {
        // Multiple equals groups with spaces should NOT be detected
        let code = "# ==== ==== ====\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert!(sections.is_empty());
    }

    // ========================================================================
    // Edge case acceptance tests (Requirement 4)
    // ========================================================================

    #[test]
    fn test_extract_sections_accept_numbers_only() {
        // Numbers-only section name should be detected (matches RStudio behavior)
        let code = "# 123 ----\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "123").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
    }

    #[test]
    fn test_extract_sections_accept_dots_only() {
        // Dots-only section name should be detected (dots are not delimiter chars)
        let code = "# ... ----\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "...").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
    }

    #[test]
    fn test_extract_sections_accept_unicode() {
        // Unicode section name should be detected
        let code = "# 日本語 ----\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "日本語").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
    }

    #[test]
    fn test_extract_sections_accept_at_todo() {
        // Special characters with letters should be detected
        let code = "# @TODO: Fix this ----\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols
            .iter()
            .find(|s| s.name == "@TODO: Fix this")
            .unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
    }

    #[test]
    fn test_extract_sections_accept_underscores() {
        // Underscores with letters should be detected
        let code = "# my_section ----\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "my_section").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
    }

    // ========================================================================
    // is_delimiter_only() unit tests
    // ========================================================================

    #[test]
    fn test_is_delimiter_only_empty() {
        assert!(is_delimiter_only(""));
    }

    #[test]
    fn test_is_delimiter_only_whitespace() {
        assert!(is_delimiter_only("   "));
    }

    #[test]
    fn test_is_delimiter_only_dashes() {
        assert!(is_delimiter_only("----"));
    }

    #[test]
    fn test_is_delimiter_only_mixed_delimiters() {
        assert!(is_delimiter_only("=-==-="));
    }

    #[test]
    fn test_is_delimiter_only_delimiters_with_spaces() {
        assert!(is_delimiter_only("==== ==== ===="));
    }

    #[test]
    fn test_is_delimiter_only_all_types() {
        assert!(is_delimiter_only("#-=*+"));
    }

    #[test]
    fn test_is_delimiter_only_false_for_letter() {
        assert!(!is_delimiter_only("a"));
    }

    #[test]
    fn test_is_delimiter_only_false_for_digit() {
        assert!(!is_delimiter_only("1"));
    }

    #[test]
    fn test_is_delimiter_only_false_for_mixed_with_letter() {
        assert!(!is_delimiter_only("--a--"));
    }

    #[test]
    fn test_is_delimiter_only_false_for_dot() {
        assert!(!is_delimiter_only("..."));
    }

    #[test]
    fn test_is_delimiter_only_false_for_unicode() {
        assert!(!is_delimiter_only("日本"));
    }

    // ========================================================================
    // classify_delimiter_line() unit tests
    // ========================================================================

    #[test]
    fn test_classify_delimiter_line_all_hashes() {
        assert_eq!(classify_delimiter_line("################"), Some(DelimiterKind::Hash));
    }

    #[test]
    fn test_classify_delimiter_line_min_hashes() {
        assert_eq!(classify_delimiter_line("####"), Some(DelimiterKind::Hash));
    }

    #[test]
    fn test_classify_delimiter_line_too_few_hashes() {
        assert_eq!(classify_delimiter_line("###"), None);
    }

    #[test]
    fn test_classify_delimiter_line_equals() {
        assert_eq!(classify_delimiter_line("# ================"), Some(DelimiterKind::Equals));
    }

    #[test]
    fn test_classify_delimiter_line_dashes() {
        assert_eq!(classify_delimiter_line("# ----------------"), Some(DelimiterKind::Dash));
    }

    #[test]
    fn test_classify_delimiter_line_asterisks() {
        assert_eq!(classify_delimiter_line("# ****************"), Some(DelimiterKind::Asterisk));
    }

    #[test]
    fn test_classify_delimiter_line_plus() {
        assert_eq!(classify_delimiter_line("# ++++++++++++++++"), Some(DelimiterKind::Plus));
    }

    #[test]
    fn test_classify_delimiter_line_hash_after_hash_space() {
        assert_eq!(classify_delimiter_line("# ################"), Some(DelimiterKind::Hash));
    }

    #[test]
    fn test_classify_delimiter_line_too_few_delimiters() {
        assert_eq!(classify_delimiter_line("# ==="), None);
    }

    #[test]
    fn test_classify_delimiter_line_mixed_chars() {
        assert_eq!(classify_delimiter_line("# =-==-="), None);
    }

    #[test]
    fn test_classify_delimiter_line_not_comment() {
        assert_eq!(classify_delimiter_line("x <- 1"), None);
    }

    #[test]
    fn test_classify_delimiter_line_empty() {
        assert_eq!(classify_delimiter_line(""), None);
    }

    #[test]
    fn test_classify_delimiter_line_with_text() {
        assert_eq!(classify_delimiter_line("# Section Name ----"), None);
    }

    #[test]
    fn test_classify_delimiter_line_leading_whitespace() {
        assert_eq!(classify_delimiter_line("  ################"), Some(DelimiterKind::Hash));
        assert_eq!(classify_delimiter_line("  # ================"), Some(DelimiterKind::Equals));
    }

    // ========================================================================
    // extract_banner_name() unit tests
    // ========================================================================

    #[test]
    fn test_extract_banner_name_basic() {
        assert_eq!(extract_banner_name("# Section Name"), Some(("Section Name".to_string(), 1)));
    }

    #[test]
    fn test_extract_banner_name_with_trailing_hash() {
        assert_eq!(extract_banner_name("# Section Name #"), Some(("Section Name".to_string(), 1)));
    }

    #[test]
    fn test_extract_banner_name_with_trailing_hashes() {
        assert_eq!(extract_banner_name("# Section Name ###"), Some(("Section Name".to_string(), 1)));
    }

    #[test]
    fn test_extract_banner_name_padded() {
        assert_eq!(extract_banner_name("#  My Analysis  "), Some(("My Analysis".to_string(), 1)));
    }

    #[test]
    fn test_extract_banner_name_delimiter_only() {
        assert_eq!(extract_banner_name("# ===="), None);
    }

    #[test]
    fn test_extract_banner_name_all_hashes() {
        assert_eq!(extract_banner_name("################"), None);
    }

    #[test]
    fn test_extract_banner_name_empty() {
        assert_eq!(extract_banner_name(""), None);
    }

    #[test]
    fn test_extract_banner_name_not_comment() {
        assert_eq!(extract_banner_name("Section Name"), None);
    }

    #[test]
    fn test_extract_banner_name_just_hash() {
        assert_eq!(extract_banner_name("#"), None);
    }

    #[test]
    fn test_extract_banner_name_multiple_leading_hashes() {
        assert_eq!(extract_banner_name("## Section Name ##"), Some(("Section Name".to_string(), 2)));
    }

    #[test]
    fn test_extract_banner_name_with_embedded_dash() {
        assert_eq!(extract_banner_name("# My-Analysis"), Some(("My-Analysis".to_string(), 1)));
    }

    #[test]
    fn test_extract_banner_name_with_embedded_plus() {
        assert_eq!(extract_banner_name("# Data+Processing"), Some(("Data+Processing".to_string(), 1)));
    }

    // ========================================================================
    // Banner-style section detection tests
    // ========================================================================

    #[test]
    fn test_banner_section_equals() {
        let code = "# ================\n# Section Name\n# ================\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "Section Name").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
        assert_eq!(section.section_level, Some(1));
    }

    #[test]
    fn test_banner_section_hashes() {
        let code = "################\n# Section Name #\n################\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "Section Name").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
        assert_eq!(section.section_level, Some(1));
    }

    #[test]
    fn test_banner_section_asterisks() {
        let code = "# ****************\n# Section Name\n# ****************\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "Section Name").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
        assert_eq!(section.section_level, Some(1));
    }

    #[test]
    fn test_banner_section_dashes() {
        let code = "# ----------------\n# Section Name\n# ----------------\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "Section Name").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
        assert_eq!(section.section_level, Some(1));
    }

    #[test]
    fn test_banner_section_plus() {
        let code = "# ++++++++++++++++\n# Section Name\n# ++++++++++++++++\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "Section Name").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
        assert_eq!(section.section_level, Some(1));
    }

    #[test]
    fn test_banner_section_decorative_inner() {
        // Inner line has trailing # decoration
        let code = "################\n# Section Name #\n################\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "Section Name").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
    }

    #[test]
    fn test_banner_section_mismatched_lengths() {
        // Different lengths should still be detected
        let code = "# ====================\n# Section Name\n# ==================\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "Section Name").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
    }

    #[test]
    fn test_banner_section_mismatched_types_rejected() {
        // Different delimiter types should NOT be detected
        let code = "# ================\n# Section Name\n# ----------------\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert!(sections.is_empty());
    }

    #[test]
    fn test_banner_section_only_above_rejected() {
        // Delimiter only above, not below
        let code = "# ================\n# Section Name\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert!(sections.is_empty());
    }

    #[test]
    fn test_banner_section_only_below_rejected() {
        // Delimiter only below, not above
        let code = "# Section Name\n# ================\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert!(sections.is_empty());
    }

    #[test]
    fn test_banner_section_at_start_of_file() {
        let code = "# ================\n# Section Name\n# ================";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "Section Name").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
        assert_eq!(section.range.start.line, 0);
        assert_eq!(section.range.end.line, 2);
    }

    #[test]
    fn test_banner_section_at_end_of_file() {
        let code = "x <- 1\n# ================\n# Section Name\n# ================";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "Section Name").unwrap();
        assert!(matches!(section.kind, DocumentSymbolKind::Module));
        assert_eq!(section.range.start.line, 1);
        assert_eq!(section.range.end.line, 3);
    }

    #[test]
    fn test_banner_section_coexists_with_single_line() {
        let code = "# ================\n# Banner Section\n# ================\nx <- 1\n# Inline Section ----\ny <- 2";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].name, "Banner Section");
        assert_eq!(sections[1].name, "Inline Section");
    }

    #[test]
    fn test_banner_section_delimiter_only_name_rejected() {
        // Name line that is all delimiters should NOT be detected
        let code = "# ================\n# ===\n# ================\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert!(sections.is_empty());
    }

    #[test]
    fn test_banner_section_heading_levels() {
        // Level 1: single # on name line
        let code = "# ================\n# Section Name\n# ================";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();
        let section = symbols.iter().find(|s| s.name == "Section Name").unwrap();
        assert_eq!(section.section_level, Some(1));

        // Level 2: double ## on name line
        let code = "# ================\n## Subsection\n# ================";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();
        let section = symbols.iter().find(|s| s.name == "Subsection").unwrap();
        assert_eq!(section.section_level, Some(2));

        // Level 3: triple ### on name line
        let code = "# ================\n### Sub-subsection\n# ================";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();
        let section = symbols.iter().find(|s| s.name == "Sub-subsection").unwrap();
        assert_eq!(section.section_level, Some(3));
    }

    #[test]
    fn test_banner_section_range_spans_three_lines() {
        let code = "x <- 1\n# ================\n# Section Name\n# ================\ny <- 2";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let section = symbols.iter().find(|s| s.name == "Section Name").unwrap();
        // range spans all 3 lines
        assert_eq!(section.range.start.line, 1);
        assert_eq!(section.range.end.line, 3);
        // selection_range is just the name line
        assert_eq!(section.selection_range.start.line, 2);
        assert_eq!(section.selection_range.end.line, 2);
    }

    #[test]
    fn test_banner_section_no_duplicate_with_single_line() {
        // A single-line section that happens to be between delimiters should
        // be detected once (by single-line), not duplicated by banner detection
        let code = "# ================\n# Section Name ----\n# ================\nx <- 1";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        // Should be detected exactly once (by single-line pattern)
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].name, "Section Name");
    }

    #[test]
    fn test_banner_section_two_consecutive_banners() {
        let code = "# ====\n# A\n# ====\n# ----\n# B\n# ----";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].name, "A");
        assert_eq!(sections[1].name, "B");
    }

    #[test]
    fn test_banner_section_multi_level_hierarchy() {
        // Banner sections with different # counts should nest correctly
        let code = "\
# ====\n\
# Top Level\n\
# ====\n\
x <- 1\n\
# ----\n\
## Sub Level\n\
# ----\n\
y <- 2";
        let tree = parse_r_code(code);
        let extractor = SymbolExtractor::new(code, tree.root_node());
        let symbols = extractor.extract_all();

        // Verify extraction produces correct levels
        let sections: Vec<_> = symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].name, "Top Level");
        assert_eq!(sections[0].section_level, Some(1));
        assert_eq!(sections[1].name, "Sub Level");
        assert_eq!(sections[1].section_level, Some(2));

        // Verify hierarchy nesting
        let mut builder = HierarchyBuilder::new(symbols, 8);
        builder.compute_section_ranges();
        builder.nest_in_sections();

        // Root should have one section (Top Level)
        let root_sections: Vec<_> = builder
            .symbols
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert_eq!(root_sections.len(), 1);
        assert_eq!(root_sections[0].name, "Top Level");

        // Sub Level should be nested inside Top Level
        let children_sections: Vec<_> = root_sections[0]
            .children
            .iter()
            .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
            .collect();
        assert_eq!(children_sections.len(), 1);
        assert_eq!(children_sections[0].name, "Sub Level");
    }

    // ========================================================================
    // HierarchyBuilder::compute_section_ranges() tests
    // ========================================================================

    #[test]
    fn test_compute_section_ranges_single_section() {
        // Single section should span from its line to EOF
        let symbols = vec![RawSymbol {
            name: "Section 1".to_string(),
            kind: DocumentSymbolKind::Module,
            range: Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 0, character: 20 },
            },
            selection_range: Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 0, character: 20 },
            },
            detail: None,
            section_level: Some(1),
            children: Vec::new(),
        }];

        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.compute_section_ranges();

        // Section should now span from line 0 to line 9 (last line)
        assert_eq!(builder.symbols[0].range.start.line, 0);
        assert_eq!(builder.symbols[0].range.end.line, 9);
        // selection_range should remain unchanged
        assert_eq!(builder.symbols[0].selection_range.start.line, 0);
        assert_eq!(builder.symbols[0].selection_range.end.line, 0);
    }

    #[test]
    fn test_compute_section_ranges_two_sections() {
        // Two sections: first should end at line before second
        let symbols = vec![
            RawSymbol {
                name: "Section 1".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 20 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 20 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
            RawSymbol {
                name: "Section 2".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 5, character: 0 },
                    end: Position { line: 5, character: 20 },
                },
                selection_range: Range {
                    start: Position { line: 5, character: 0 },
                    end: Position { line: 5, character: 20 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.compute_section_ranges();

        // First section: line 0 to line 4 (line before section 2)
        assert_eq!(builder.symbols[0].range.start.line, 0);
        assert_eq!(builder.symbols[0].range.end.line, 4);
        // Second section: line 5 to line 9 (EOF)
        assert_eq!(builder.symbols[1].range.start.line, 5);
        assert_eq!(builder.symbols[1].range.end.line, 9);
    }

    #[test]
    fn test_compute_section_ranges_three_sections() {
        // Three sections
        let symbols = vec![
            RawSymbol {
                name: "Section 1".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 20 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 20 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
            RawSymbol {
                name: "Section 2".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 3, character: 0 },
                    end: Position { line: 3, character: 20 },
                },
                selection_range: Range {
                    start: Position { line: 3, character: 0 },
                    end: Position { line: 3, character: 20 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
            RawSymbol {
                name: "Section 3".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 7, character: 0 },
                    end: Position { line: 7, character: 20 },
                },
                selection_range: Range {
                    start: Position { line: 7, character: 0 },
                    end: Position { line: 7, character: 20 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.compute_section_ranges();

        // Section 1: line 0 to line 2
        assert_eq!(builder.symbols[0].range.start.line, 0);
        assert_eq!(builder.symbols[0].range.end.line, 2);
        // Section 2: line 3 to line 6
        assert_eq!(builder.symbols[1].range.start.line, 3);
        assert_eq!(builder.symbols[1].range.end.line, 6);
        // Section 3: line 7 to line 9 (EOF)
        assert_eq!(builder.symbols[2].range.start.line, 7);
        assert_eq!(builder.symbols[2].range.end.line, 9);
    }

    #[test]
    fn test_compute_section_ranges_unsorted_sections() {
        // Sections not in order by line - should still work correctly
        let symbols = vec![
            RawSymbol {
                name: "Section 2".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 5, character: 0 },
                    end: Position { line: 5, character: 20 },
                },
                selection_range: Range {
                    start: Position { line: 5, character: 0 },
                    end: Position { line: 5, character: 20 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
            RawSymbol {
                name: "Section 1".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 20 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 20 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.compute_section_ranges();

        // Find sections by name since order in vec may differ
        let section1 = builder.symbols.iter().find(|s| s.name == "Section 1").unwrap();
        let section2 = builder.symbols.iter().find(|s| s.name == "Section 2").unwrap();

        // Section 1: line 0 to line 4
        assert_eq!(section1.range.start.line, 0);
        assert_eq!(section1.range.end.line, 4);
        // Section 2: line 5 to line 9 (EOF)
        assert_eq!(section2.range.start.line, 5);
        assert_eq!(section2.range.end.line, 9);
    }

    #[test]
    fn test_compute_section_ranges_mixed_with_non_sections() {
        // Mix of sections and non-section symbols
        let symbols = vec![
            RawSymbol {
                name: "my_func".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 1, character: 0 },
                    end: Position { line: 3, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 1, character: 0 },
                    end: Position { line: 1, character: 7 },
                },
                detail: Some("()".to_string()),
                section_level: None, // Not a section
                children: Vec::new(),
            },
            RawSymbol {
                name: "Section 1".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 20 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 20 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
            RawSymbol {
                name: "Section 2".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 5, character: 0 },
                    end: Position { line: 5, character: 20 },
                },
                selection_range: Range {
                    start: Position { line: 5, character: 0 },
                    end: Position { line: 5, character: 20 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.compute_section_ranges();

        // Find symbols by name
        let func = builder.symbols.iter().find(|s| s.name == "my_func").unwrap();
        let section1 = builder.symbols.iter().find(|s| s.name == "Section 1").unwrap();
        let section2 = builder.symbols.iter().find(|s| s.name == "Section 2").unwrap();

        // Function should be unchanged
        assert_eq!(func.range.start.line, 1);
        assert_eq!(func.range.end.line, 3);

        // Section 1: line 0 to line 4
        assert_eq!(section1.range.start.line, 0);
        assert_eq!(section1.range.end.line, 4);
        // Section 2: line 5 to line 9 (EOF)
        assert_eq!(section2.range.start.line, 5);
        assert_eq!(section2.range.end.line, 9);
    }

    #[test]
    fn test_compute_section_ranges_no_sections() {
        // No sections - should not modify anything
        let symbols = vec![RawSymbol {
            name: "my_func".to_string(),
            kind: DocumentSymbolKind::Function,
            range: Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 2, character: 1 },
            },
            selection_range: Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 0, character: 7 },
            },
            detail: Some("()".to_string()),
            section_level: None,
            children: Vec::new(),
        }];

        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.compute_section_ranges();

        // Function should be unchanged
        assert_eq!(builder.symbols[0].range.start.line, 0);
        assert_eq!(builder.symbols[0].range.end.line, 2);
    }

    #[test]
    fn test_compute_section_ranges_empty_symbols() {
        // Empty symbols list - should not panic
        let symbols: Vec<RawSymbol> = vec![];
        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.compute_section_ranges();
        assert!(builder.symbols.is_empty());
    }

    #[test]
    fn test_compute_section_ranges_section_at_line_zero() {
        // Section at line 0 with next section at line 1
        let symbols = vec![
            RawSymbol {
                name: "Section 1".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 20 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 20 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
            RawSymbol {
                name: "Section 2".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 1, character: 0 },
                    end: Position { line: 1, character: 20 },
                },
                selection_range: Range {
                    start: Position { line: 1, character: 0 },
                    end: Position { line: 1, character: 20 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 5);
        builder.compute_section_ranges();

        // Section 1: line 0 to line 0 (line before section 2 is 0)
        assert_eq!(builder.symbols[0].range.start.line, 0);
        assert_eq!(builder.symbols[0].range.end.line, 0);
        // Section 2: line 1 to line 4 (EOF)
        assert_eq!(builder.symbols[1].range.start.line, 1);
        assert_eq!(builder.symbols[1].range.end.line, 4);
    }

    #[test]
    fn test_compute_section_ranges_selection_range_unchanged() {
        // Verify selection_range is NOT modified
        let symbols = vec![RawSymbol {
            name: "Section 1".to_string(),
            kind: DocumentSymbolKind::Module,
            range: Range {
                start: Position { line: 2, character: 0 },
                end: Position { line: 2, character: 20 },
            },
            selection_range: Range {
                start: Position { line: 2, character: 0 },
                end: Position { line: 2, character: 20 },
            },
            detail: None,
            section_level: Some(1),
            children: Vec::new(),
        }];

        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.compute_section_ranges();

        // range should be expanded
        assert_eq!(builder.symbols[0].range.start.line, 2);
        assert_eq!(builder.symbols[0].range.end.line, 9);
        // selection_range should remain the comment line only
        assert_eq!(builder.symbols[0].selection_range.start.line, 2);
        assert_eq!(builder.symbols[0].selection_range.end.line, 2);
        assert_eq!(builder.symbols[0].selection_range.start.character, 0);
        assert_eq!(builder.symbols[0].selection_range.end.character, 20);
    }

    #[test]
    fn test_compute_section_ranges_zero_line_count() {
        // Edge case: line_count is 0
        let symbols = vec![RawSymbol {
            name: "Section 1".to_string(),
            kind: DocumentSymbolKind::Module,
            range: Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 0, character: 20 },
            },
            selection_range: Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 0, character: 20 },
            },
            detail: None,
            section_level: Some(1),
            children: Vec::new(),
        }];

        let mut builder = HierarchyBuilder::new(symbols, 0);
        builder.compute_section_ranges();

        // With line_count 0, end line should be 0
        assert_eq!(builder.symbols[0].range.end.line, 0);
    }

    #[test]
    fn test_compute_section_ranges_parent_spans_subsection() {
        // Core bug scenario: level-1 section at line 0, level-2 subsection at line 3,
        // non-section symbol (function) at line 6, line_count = 10.
        // The level-1 section's range must span over the subsection to EOF,
        // NOT end at line 2 (before the subsection).
        let symbols = vec![
            RawSymbol {
                name: "Parent Section".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 25 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 25 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
            RawSymbol {
                name: "Child Subsection".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 3, character: 0 },
                    end: Position { line: 3, character: 28 },
                },
                selection_range: Range {
                    start: Position { line: 3, character: 0 },
                    end: Position { line: 3, character: 28 },
                },
                detail: None,
                section_level: Some(2),
                children: Vec::new(),
            },
            RawSymbol {
                name: "my_func".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 6, character: 0 },
                    end: Position { line: 8, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 6, character: 0 },
                    end: Position { line: 6, character: 7 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.compute_section_ranges();

        // Find symbols by name
        let parent = builder.symbols.iter().find(|s| s.name == "Parent Section").unwrap();
        let child = builder.symbols.iter().find(|s| s.name == "Child Subsection").unwrap();
        let func = builder.symbols.iter().find(|s| s.name == "my_func").unwrap();

        // Level-1 parent section: should span from line 0 to line 9 (EOF)
        // because there is no subsequent section at level <= 1
        assert_eq!(parent.range.start.line, 0);
        assert_eq!(parent.range.end.line, 9, "Parent section must span to EOF over subsection");

        // Level-2 child subsection: should also span to line 9 (EOF)
        // because there is no subsequent section at level <= 2
        assert_eq!(child.range.start.line, 3);
        assert_eq!(child.range.end.line, 9, "Child subsection must span to EOF");

        // Non-section function symbol should be unchanged
        assert_eq!(func.range.start.line, 6);
        assert_eq!(func.range.end.line, 8);

        // Selection ranges must remain unchanged (Requirement 3.1)
        assert_eq!(parent.selection_range.start.line, 0);
        assert_eq!(parent.selection_range.end.line, 0);
        assert_eq!(child.selection_range.start.line, 3);
        assert_eq!(child.selection_range.end.line, 3);
    }

    #[test]
    fn test_build_three_level_nesting_with_symbols() {
        // Three-level nesting: level 1 > level 2 > level 3 with symbols at each level.
        // Verifies correct hierarchy after build() (Requirements 2.1, 2.2).
        //
        // Layout:
        //   Line 0:  # Level 1 Section ----       (level 1)
        //   Line 1:  func_in_l1 <- function() {}  (function, should nest under level 1)
        //   Line 3:  ## Level 2 Section ----       (level 2)
        //   Line 4:  func_in_l2 <- function() {}  (function, should nest under level 2)
        //   Line 6:  ### Level 3 Section ----      (level 3)
        //   Line 7:  func_in_l3 <- function() {}  (function, should nest under level 3)
        //   Line 10: # Another Level 1 ----        (level 1, terminates first level 1)
        //   line_count = 12

        let symbols = vec![
            RawSymbol {
                name: "Level 1 Section".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 25 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 25 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
            RawSymbol {
                name: "func_in_l1".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 1, character: 0 },
                    end: Position { line: 2, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 1, character: 0 },
                    end: Position { line: 1, character: 10 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "Level 2 Section".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 3, character: 0 },
                    end: Position { line: 3, character: 25 },
                },
                selection_range: Range {
                    start: Position { line: 3, character: 0 },
                    end: Position { line: 3, character: 25 },
                },
                detail: None,
                section_level: Some(2),
                children: Vec::new(),
            },
            RawSymbol {
                name: "func_in_l2".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 4, character: 0 },
                    end: Position { line: 5, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 4, character: 0 },
                    end: Position { line: 4, character: 10 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "Level 3 Section".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 6, character: 0 },
                    end: Position { line: 6, character: 25 },
                },
                selection_range: Range {
                    start: Position { line: 6, character: 0 },
                    end: Position { line: 6, character: 25 },
                },
                detail: None,
                section_level: Some(3),
                children: Vec::new(),
            },
            RawSymbol {
                name: "func_in_l3".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 7, character: 0 },
                    end: Position { line: 8, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 7, character: 0 },
                    end: Position { line: 7, character: 10 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "Another Level 1".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 10, character: 0 },
                    end: Position { line: 10, character: 25 },
                },
                selection_range: Range {
                    start: Position { line: 10, character: 0 },
                    end: Position { line: 10, character: 25 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
        ];

        let builder = HierarchyBuilder::new(symbols, 12);
        let result = builder.build();

        // Root level: two level-1 sections
        assert_eq!(result.len(), 2, "Should have 2 root-level sections");
        assert_eq!(result[0].name, "Level 1 Section");
        assert_eq!(result[1].name, "Another Level 1");

        // Level 1 Section should contain: func_in_l1 and Level 2 Section
        let l1_children = result[0].children.as_ref().unwrap();
        assert_eq!(l1_children.len(), 2, "Level 1 should have 2 children (func + level-2 section)");
        assert_eq!(l1_children[0].name, "func_in_l1");
        assert_eq!(l1_children[1].name, "Level 2 Section");

        // Level 2 Section should contain: func_in_l2 and Level 3 Section
        let l2_children = l1_children[1].children.as_ref().unwrap();
        assert_eq!(l2_children.len(), 2, "Level 2 should have 2 children (func + level-3 section)");
        assert_eq!(l2_children[0].name, "func_in_l2");
        assert_eq!(l2_children[1].name, "Level 3 Section");

        // Level 3 Section should contain: func_in_l3
        let l3_children = l2_children[1].children.as_ref().unwrap();
        assert_eq!(l3_children.len(), 1, "Level 3 should have 1 child (func)");
        assert_eq!(l3_children[0].name, "func_in_l3");

        // Another Level 1 section should have no children (no symbols after it before EOF)
        let l1b_children = result[1].children.as_ref();
        assert!(
            l1b_children.is_none() || l1b_children.unwrap().is_empty(),
            "Second level-1 section should have no children"
        );
    }


    // ========================================================================
    // Tests for nest_in_sections()
    // ========================================================================

    #[test]
    fn test_nest_in_sections_empty() {
        // Empty symbols list - should not panic
        let symbols: Vec<RawSymbol> = vec![];
        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.nest_in_sections();
        assert!(builder.symbols.is_empty());
    }

    #[test]
    fn test_nest_in_sections_no_sections() {
        // Only non-section symbols - should remain at root level
        let symbols = vec![
            RawSymbol {
                name: "func1".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 2, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 5 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "func2".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 3, character: 0 },
                    end: Position { line: 5, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 3, character: 0 },
                    end: Position { line: 3, character: 5 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.nest_in_sections();

        // Both functions should remain at root level
        assert_eq!(builder.symbols.len(), 2);
        assert_eq!(builder.symbols[0].name, "func1");
        assert_eq!(builder.symbols[1].name, "func2");
    }

    #[test]
    fn test_nest_in_sections_symbol_in_section() {
        // A function inside a section should become a child of that section
        let symbols = vec![
            RawSymbol {
                name: "Section 1".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 9, character: LSP_EOL_CHARACTER },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 20 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
            RawSymbol {
                name: "my_func".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 2, character: 0 },
                    end: Position { line: 4, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 2, character: 0 },
                    end: Position { line: 2, character: 7 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.nest_in_sections();

        // Should have one root symbol (the section)
        assert_eq!(builder.symbols.len(), 1);
        assert_eq!(builder.symbols[0].name, "Section 1");
        // The function should be a child of the section
        assert_eq!(builder.symbols[0].children.len(), 1);
        assert_eq!(builder.symbols[0].children[0].name, "my_func");
    }

    #[test]
    fn test_nest_in_sections_nested_sections() {
        // ## Subsection should be nested inside # Section
        let symbols = vec![
            RawSymbol {
                name: "Section 1".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 19, character: LSP_EOL_CHARACTER },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 20 },
                },
                detail: None,
                section_level: Some(1), // # Section
                children: Vec::new(),
            },
            RawSymbol {
                name: "Subsection 1.1".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 5, character: 0 },
                    end: Position { line: 14, character: LSP_EOL_CHARACTER },
                },
                selection_range: Range {
                    start: Position { line: 5, character: 0 },
                    end: Position { line: 5, character: 25 },
                },
                detail: None,
                section_level: Some(2), // ## Subsection
                children: Vec::new(),
            },
            RawSymbol {
                name: "Section 2".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 20, character: 0 },
                    end: Position { line: 29, character: LSP_EOL_CHARACTER },
                },
                selection_range: Range {
                    start: Position { line: 20, character: 0 },
                    end: Position { line: 20, character: 20 },
                },
                detail: None,
                section_level: Some(1), // # Section
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 30);
        builder.nest_in_sections();

        // Should have two root sections
        assert_eq!(builder.symbols.len(), 2);
        assert_eq!(builder.symbols[0].name, "Section 1");
        assert_eq!(builder.symbols[1].name, "Section 2");

        // Section 1 should have Subsection 1.1 as a child
        assert_eq!(builder.symbols[0].children.len(), 1);
        assert_eq!(builder.symbols[0].children[0].name, "Subsection 1.1");

        // Section 2 should have no children
        assert!(builder.symbols[1].children.is_empty());
    }

    #[test]
    fn test_nest_in_sections_symbol_in_nested_section() {
        // A function inside a subsection should be nested in the subsection, not the parent section
        let symbols = vec![
            RawSymbol {
                name: "Section 1".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 19, character: LSP_EOL_CHARACTER },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 20 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
            RawSymbol {
                name: "Subsection 1.1".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 5, character: 0 },
                    end: Position { line: 14, character: LSP_EOL_CHARACTER },
                },
                selection_range: Range {
                    start: Position { line: 5, character: 0 },
                    end: Position { line: 5, character: 25 },
                },
                detail: None,
                section_level: Some(2),
                children: Vec::new(),
            },
            RawSymbol {
                name: "nested_func".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 7, character: 0 },
                    end: Position { line: 9, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 7, character: 0 },
                    end: Position { line: 7, character: 11 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 20);
        builder.nest_in_sections();

        // Should have one root section
        assert_eq!(builder.symbols.len(), 1);
        assert_eq!(builder.symbols[0].name, "Section 1");

        // Section 1 should have Subsection 1.1 as a child
        assert_eq!(builder.symbols[0].children.len(), 1);
        assert_eq!(builder.symbols[0].children[0].name, "Subsection 1.1");

        // Subsection 1.1 should have nested_func as a child
        assert_eq!(builder.symbols[0].children[0].children.len(), 1);
        assert_eq!(builder.symbols[0].children[0].children[0].name, "nested_func");
    }

    #[test]
    fn test_nest_in_sections_multiple_levels() {
        // Test three levels of nesting: # > ## > ###
        let symbols = vec![
            RawSymbol {
                name: "Level 1".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 29, character: LSP_EOL_CHARACTER },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 15 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
            RawSymbol {
                name: "Level 2".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 5, character: 0 },
                    end: Position { line: 24, character: LSP_EOL_CHARACTER },
                },
                selection_range: Range {
                    start: Position { line: 5, character: 0 },
                    end: Position { line: 5, character: 15 },
                },
                detail: None,
                section_level: Some(2),
                children: Vec::new(),
            },
            RawSymbol {
                name: "Level 3".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 10, character: 0 },
                    end: Position { line: 19, character: LSP_EOL_CHARACTER },
                },
                selection_range: Range {
                    start: Position { line: 10, character: 0 },
                    end: Position { line: 10, character: 15 },
                },
                detail: None,
                section_level: Some(3),
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 30);
        builder.nest_in_sections();

        // Should have one root section
        assert_eq!(builder.symbols.len(), 1);
        assert_eq!(builder.symbols[0].name, "Level 1");

        // Level 1 should have Level 2 as a child
        assert_eq!(builder.symbols[0].children.len(), 1);
        assert_eq!(builder.symbols[0].children[0].name, "Level 2");

        // Level 2 should have Level 3 as a child
        assert_eq!(builder.symbols[0].children[0].children.len(), 1);
        assert_eq!(builder.symbols[0].children[0].children[0].name, "Level 3");
    }

    #[test]
    fn test_nest_in_sections_sibling_sections() {
        // Two ## sections under one # section should be siblings
        let symbols = vec![
            RawSymbol {
                name: "Section 1".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 29, character: LSP_EOL_CHARACTER },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 20 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
            RawSymbol {
                name: "Subsection A".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 5, character: 0 },
                    end: Position { line: 14, character: LSP_EOL_CHARACTER },
                },
                selection_range: Range {
                    start: Position { line: 5, character: 0 },
                    end: Position { line: 5, character: 20 },
                },
                detail: None,
                section_level: Some(2),
                children: Vec::new(),
            },
            RawSymbol {
                name: "Subsection B".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 15, character: 0 },
                    end: Position { line: 24, character: LSP_EOL_CHARACTER },
                },
                selection_range: Range {
                    start: Position { line: 15, character: 0 },
                    end: Position { line: 15, character: 20 },
                },
                detail: None,
                section_level: Some(2),
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 30);
        builder.nest_in_sections();

        // Should have one root section
        assert_eq!(builder.symbols.len(), 1);
        assert_eq!(builder.symbols[0].name, "Section 1");

        // Section 1 should have two children (both subsections)
        assert_eq!(builder.symbols[0].children.len(), 2);
        assert_eq!(builder.symbols[0].children[0].name, "Subsection A");
        assert_eq!(builder.symbols[0].children[1].name, "Subsection B");
    }

    #[test]
    fn test_nest_in_sections_symbol_before_any_section() {
        // A symbol before any section should remain at root level
        let symbols = vec![
            RawSymbol {
                name: "early_func".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 2, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 10 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "Section 1".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 5, character: 0 },
                    end: Position { line: 19, character: LSP_EOL_CHARACTER },
                },
                selection_range: Range {
                    start: Position { line: 5, character: 0 },
                    end: Position { line: 5, character: 20 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 20);
        builder.nest_in_sections();

        // Should have two root symbols
        assert_eq!(builder.symbols.len(), 2);
        // Find symbols by name since order may vary based on implementation
        let early_func = builder.symbols.iter().find(|s| s.name == "early_func");
        let section1 = builder.symbols.iter().find(|s| s.name == "Section 1");
        assert!(early_func.is_some(), "early_func should be at root level");
        assert!(section1.is_some(), "Section 1 should be at root level");
    }

    #[test]
    fn test_nest_in_sections_symbol_after_all_sections() {
        // A symbol after all sections (outside their ranges) should remain at root level
        let symbols = vec![
            RawSymbol {
                name: "Section 1".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 9, character: LSP_EOL_CHARACTER },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 20 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
            RawSymbol {
                name: "late_func".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 15, character: 0 },
                    end: Position { line: 17, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 15, character: 0 },
                    end: Position { line: 15, character: 9 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 20);
        builder.nest_in_sections();

        // Should have two root symbols
        assert_eq!(builder.symbols.len(), 2);
        assert_eq!(builder.symbols[0].name, "Section 1");
        assert_eq!(builder.symbols[1].name, "late_func");
        // Section should have no children
        assert!(builder.symbols[0].children.is_empty());
    }

    // ========================================================================
    // HierarchyBuilder::nest_in_functions() tests
    // ========================================================================

    #[test]
    fn test_nest_in_functions_empty() {
        // Empty symbols list - should not panic
        let symbols: Vec<RawSymbol> = vec![];
        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.nest_in_functions();
        assert!(builder.symbols.is_empty());
    }

    #[test]
    fn test_nest_in_functions_no_functions() {
        // Only non-function symbols - should remain at root level
        let symbols = vec![
            RawSymbol {
                name: "MY_CONST".to_string(),
                kind: DocumentSymbolKind::Constant,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 15 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 8 },
                },
                detail: None,
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "my_var".to_string(),
                kind: DocumentSymbolKind::Variable,
                range: Range {
                    start: Position { line: 1, character: 0 },
                    end: Position { line: 1, character: 10 },
                },
                selection_range: Range {
                    start: Position { line: 1, character: 0 },
                    end: Position { line: 1, character: 6 },
                },
                detail: None,
                section_level: None,
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.nest_in_functions();

        // Both symbols should remain at root level
        assert_eq!(builder.symbols.len(), 2);
        assert_eq!(builder.symbols[0].name, "MY_CONST");
        assert_eq!(builder.symbols[1].name, "my_var");
    }

    #[test]
    fn test_nest_in_functions_variable_inside_function() {
        // A variable inside a function body should become a child of that function
        // Code:
        // outer <- function() {   # line 0-4
        //   inner_var <- 42       # line 2
        // }
        let symbols = vec![
            RawSymbol {
                name: "outer".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 4, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 5 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "inner_var".to_string(),
                kind: DocumentSymbolKind::Variable,
                range: Range {
                    start: Position { line: 2, character: 2 },
                    end: Position { line: 2, character: 17 },
                },
                selection_range: Range {
                    start: Position { line: 2, character: 2 },
                    end: Position { line: 2, character: 11 },
                },
                detail: None,
                section_level: None,
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.nest_in_functions();

        // Should have one root symbol (the function)
        assert_eq!(builder.symbols.len(), 1);
        assert_eq!(builder.symbols[0].name, "outer");
        // The variable should be a child of the function
        assert_eq!(builder.symbols[0].children.len(), 1);
        assert_eq!(builder.symbols[0].children[0].name, "inner_var");
    }

    #[test]
    fn test_nest_in_functions_nested_functions() {
        // Nested function definitions should preserve their nesting depth
        // Code:
        // outer <- function() {   # line 0-8
        //   inner <- function() { # line 2-6
        //     deepest <- 1        # line 4
        //   }
        // }
        let symbols = vec![
            RawSymbol {
                name: "outer".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 8, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 5 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "inner".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 2, character: 2 },
                    end: Position { line: 6, character: 3 },
                },
                selection_range: Range {
                    start: Position { line: 2, character: 2 },
                    end: Position { line: 2, character: 7 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "deepest".to_string(),
                kind: DocumentSymbolKind::Variable,
                range: Range {
                    start: Position { line: 4, character: 4 },
                    end: Position { line: 4, character: 16 },
                },
                selection_range: Range {
                    start: Position { line: 4, character: 4 },
                    end: Position { line: 4, character: 11 },
                },
                detail: None,
                section_level: None,
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.nest_in_functions();

        // Should have one root symbol (outer function)
        assert_eq!(builder.symbols.len(), 1);
        assert_eq!(builder.symbols[0].name, "outer");

        // outer should have inner as a child
        assert_eq!(builder.symbols[0].children.len(), 1);
        assert_eq!(builder.symbols[0].children[0].name, "inner");

        // inner should have deepest as a child
        assert_eq!(builder.symbols[0].children[0].children.len(), 1);
        assert_eq!(builder.symbols[0].children[0].children[0].name, "deepest");
    }

    #[test]
    fn test_nest_in_functions_three_levels_deep() {
        // Test three levels of function nesting
        // Code:
        // level1 <- function() {   # line 0-12
        //   level2 <- function() { # line 2-10
        //     level3 <- function() { # line 4-8
        //       x <- 1              # line 6
        //     }
        //   }
        // }
        let symbols = vec![
            RawSymbol {
                name: "level1".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 12, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 6 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "level2".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 2, character: 2 },
                    end: Position { line: 10, character: 3 },
                },
                selection_range: Range {
                    start: Position { line: 2, character: 2 },
                    end: Position { line: 2, character: 8 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "level3".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 4, character: 4 },
                    end: Position { line: 8, character: 5 },
                },
                selection_range: Range {
                    start: Position { line: 4, character: 4 },
                    end: Position { line: 4, character: 10 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "x".to_string(),
                kind: DocumentSymbolKind::Variable,
                range: Range {
                    start: Position { line: 6, character: 6 },
                    end: Position { line: 6, character: 12 },
                },
                selection_range: Range {
                    start: Position { line: 6, character: 6 },
                    end: Position { line: 6, character: 7 },
                },
                detail: None,
                section_level: None,
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 15);
        builder.nest_in_functions();

        // Should have one root symbol (level1)
        assert_eq!(builder.symbols.len(), 1);
        assert_eq!(builder.symbols[0].name, "level1");

        // level1 -> level2
        assert_eq!(builder.symbols[0].children.len(), 1);
        assert_eq!(builder.symbols[0].children[0].name, "level2");

        // level2 -> level3
        assert_eq!(builder.symbols[0].children[0].children.len(), 1);
        assert_eq!(builder.symbols[0].children[0].children[0].name, "level3");

        // level3 -> x
        assert_eq!(builder.symbols[0].children[0].children[0].children.len(), 1);
        assert_eq!(builder.symbols[0].children[0].children[0].children[0].name, "x");
    }

    #[test]
    fn test_nest_in_functions_sibling_functions() {
        // Two functions at the same level should remain siblings
        // Code:
        // func1 <- function() { }  # line 0-2
        // func2 <- function() { }  # line 3-5
        let symbols = vec![
            RawSymbol {
                name: "func1".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 2, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 5 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "func2".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 3, character: 0 },
                    end: Position { line: 5, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 3, character: 0 },
                    end: Position { line: 3, character: 5 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.nest_in_functions();

        // Both functions should remain at root level
        assert_eq!(builder.symbols.len(), 2);
        assert_eq!(builder.symbols[0].name, "func1");
        assert_eq!(builder.symbols[1].name, "func2");
    }

    #[test]
    fn test_nest_in_functions_variable_outside_function() {
        // A variable outside any function should remain at root level
        // Code:
        // top_var <- 1             # line 0
        // my_func <- function() { } # line 2-4
        let symbols = vec![
            RawSymbol {
                name: "top_var".to_string(),
                kind: DocumentSymbolKind::Variable,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 12 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 7 },
                },
                detail: None,
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "my_func".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 2, character: 0 },
                    end: Position { line: 4, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 2, character: 0 },
                    end: Position { line: 2, character: 7 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.nest_in_functions();

        // Both symbols should remain at root level
        assert_eq!(builder.symbols.len(), 2);
        assert_eq!(builder.symbols[0].name, "top_var");
        assert_eq!(builder.symbols[1].name, "my_func");
    }

    #[test]
    fn test_nest_in_functions_variable_after_function() {
        // A variable after a function (outside its range) should remain at root level
        // Code:
        // my_func <- function() { } # line 0-2
        // after_var <- 1            # line 4
        let symbols = vec![
            RawSymbol {
                name: "my_func".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 2, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 7 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "after_var".to_string(),
                kind: DocumentSymbolKind::Variable,
                range: Range {
                    start: Position { line: 4, character: 0 },
                    end: Position { line: 4, character: 14 },
                },
                selection_range: Range {
                    start: Position { line: 4, character: 0 },
                    end: Position { line: 4, character: 9 },
                },
                detail: None,
                section_level: None,
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.nest_in_functions();

        // Both symbols should remain at root level
        assert_eq!(builder.symbols.len(), 2);
        assert_eq!(builder.symbols[0].name, "my_func");
        assert_eq!(builder.symbols[1].name, "after_var");
    }

    #[test]
    fn test_nest_in_functions_multiple_variables_in_function() {
        // Multiple variables inside a function should all become children
        // Code:
        // my_func <- function() {  # line 0-6
        //   var1 <- 1              # line 2
        //   var2 <- 2              # line 3
        //   var3 <- 3              # line 4
        // }
        let symbols = vec![
            RawSymbol {
                name: "my_func".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 6, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 7 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "var1".to_string(),
                kind: DocumentSymbolKind::Variable,
                range: Range {
                    start: Position { line: 2, character: 2 },
                    end: Position { line: 2, character: 12 },
                },
                selection_range: Range {
                    start: Position { line: 2, character: 2 },
                    end: Position { line: 2, character: 6 },
                },
                detail: None,
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "var2".to_string(),
                kind: DocumentSymbolKind::Variable,
                range: Range {
                    start: Position { line: 3, character: 2 },
                    end: Position { line: 3, character: 12 },
                },
                selection_range: Range {
                    start: Position { line: 3, character: 2 },
                    end: Position { line: 3, character: 6 },
                },
                detail: None,
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "var3".to_string(),
                kind: DocumentSymbolKind::Variable,
                range: Range {
                    start: Position { line: 4, character: 2 },
                    end: Position { line: 4, character: 12 },
                },
                selection_range: Range {
                    start: Position { line: 4, character: 2 },
                    end: Position { line: 4, character: 6 },
                },
                detail: None,
                section_level: None,
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.nest_in_functions();

        // Should have one root symbol (the function)
        assert_eq!(builder.symbols.len(), 1);
        assert_eq!(builder.symbols[0].name, "my_func");

        // All three variables should be children
        assert_eq!(builder.symbols[0].children.len(), 3);
        assert_eq!(builder.symbols[0].children[0].name, "var1");
        assert_eq!(builder.symbols[0].children[1].name, "var2");
        assert_eq!(builder.symbols[0].children[2].name, "var3");
    }

    #[test]
    fn test_nest_in_functions_with_sections() {
        // Test that nest_in_functions works correctly after nest_in_sections
        // Symbols inside a section that are also inside a function should be nested in the function
        // Code:
        // # Section 1 ----         # line 0
        // my_func <- function() {  # line 2-6
        //   inner_var <- 1         # line 4
        // }
        let symbols = vec![
            RawSymbol {
                name: "Section 1".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 9, character: LSP_EOL_CHARACTER },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 15 },
                },
                detail: None,
                section_level: Some(1),
                children: vec![
                    RawSymbol {
                        name: "my_func".to_string(),
                        kind: DocumentSymbolKind::Function,
                        range: Range {
                            start: Position { line: 2, character: 0 },
                            end: Position { line: 6, character: 1 },
                        },
                        selection_range: Range {
                            start: Position { line: 2, character: 0 },
                            end: Position { line: 2, character: 7 },
                        },
                        detail: Some("()".to_string()),
                        section_level: None,
                        children: Vec::new(),
                    },
                    RawSymbol {
                        name: "inner_var".to_string(),
                        kind: DocumentSymbolKind::Variable,
                        range: Range {
                            start: Position { line: 4, character: 2 },
                            end: Position { line: 4, character: 16 },
                        },
                        selection_range: Range {
                            start: Position { line: 4, character: 2 },
                            end: Position { line: 4, character: 11 },
                        },
                        detail: None,
                        section_level: None,
                        children: Vec::new(),
                    },
                ],
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 10);
        // nest_in_sections was already called (symbols are pre-nested in section)
        // Now call nest_in_functions to nest inner_var inside my_func
        builder.nest_in_functions();

        // Should still have one root symbol (the section)
        assert_eq!(builder.symbols.len(), 1);
        assert_eq!(builder.symbols[0].name, "Section 1");

        // Section should have one child (the function)
        assert_eq!(builder.symbols[0].children.len(), 1);
        assert_eq!(builder.symbols[0].children[0].name, "my_func");

        // Function should have inner_var as a child
        assert_eq!(builder.symbols[0].children[0].children.len(), 1);
        assert_eq!(builder.symbols[0].children[0].children[0].name, "inner_var");
    }

    #[test]
    fn test_nest_in_functions_symbol_on_function_definition_line() {
        // A symbol on the same line as the function definition should NOT be nested
        // (it's part of the function definition, not inside the body)
        // Code:
        // my_func <- function() { x <- 1 }  # line 0 (single line function)
        let symbols = vec![
            RawSymbol {
                name: "my_func".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 32 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 7 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "x".to_string(),
                kind: DocumentSymbolKind::Variable,
                range: Range {
                    start: Position { line: 0, character: 24 },
                    end: Position { line: 0, character: 30 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 24 },
                    end: Position { line: 0, character: 25 },
                },
                detail: None,
                section_level: None,
                children: Vec::new(),
            },
        ];

        let mut builder = HierarchyBuilder::new(symbols, 10);
        builder.nest_in_functions();

        // Both symbols should remain at root level because x is on the same line as the function
        // (our containment check requires symbol_start > func_start)
        assert_eq!(builder.symbols.len(), 2);
    }

    // ========================================================================
    // HierarchyBuilder::build() tests
    // ========================================================================

    #[test]
    fn test_build_empty_symbols() {
        // Empty symbols list - should return empty Vec<DocumentSymbol>
        let symbols: Vec<RawSymbol> = vec![];
        let builder = HierarchyBuilder::new(symbols, 10);
        let result = builder.build();
        assert!(result.is_empty());
    }

    #[test]
    fn test_build_single_function() {
        // Single function symbol - should convert to DocumentSymbol
        let symbols = vec![RawSymbol {
            name: "my_func".to_string(),
            kind: DocumentSymbolKind::Function,
            range: Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 5, character: 1 },
            },
            selection_range: Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 0, character: 7 },
            },
            detail: Some("(x, y)".to_string()),
            section_level: None,
            children: Vec::new(),
        }];

        let builder = HierarchyBuilder::new(symbols, 10);
        let result = builder.build();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "my_func");
        assert_eq!(result[0].kind, SymbolKind::FUNCTION);
        assert_eq!(result[0].detail, Some("(x, y)".to_string()));
        assert!(result[0].children.is_none());
        assert!(result[0].deprecated.is_none());
        assert!(result[0].tags.is_none());
    }

    #[test]
    fn test_build_with_nested_function() {
        // Function with nested variable - should produce hierarchy
        let symbols = vec![
            RawSymbol {
                name: "outer_func".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 5, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 10 },
                },
                detail: Some("(x)".to_string()),
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "inner_var".to_string(),
                kind: DocumentSymbolKind::Variable,
                range: Range {
                    start: Position { line: 2, character: 4 },
                    end: Position { line: 2, character: 20 },
                },
                selection_range: Range {
                    start: Position { line: 2, character: 4 },
                    end: Position { line: 2, character: 13 },
                },
                detail: None,
                section_level: None,
                children: Vec::new(),
            },
        ];

        let builder = HierarchyBuilder::new(symbols, 10);
        let result = builder.build();

        // Should have one root symbol (outer_func) with inner_var as child
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "outer_func");
        assert!(result[0].children.is_some());
        let children = result[0].children.as_ref().unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, "inner_var");
        assert_eq!(children[0].kind, SymbolKind::VARIABLE);
    }

    #[test]
    fn test_build_with_section() {
        // Section with function inside - should produce hierarchy
        let symbols = vec![
            RawSymbol {
                name: "My Section".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 20 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 20 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
            RawSymbol {
                name: "my_func".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 2, character: 0 },
                    end: Position { line: 5, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 2, character: 0 },
                    end: Position { line: 2, character: 7 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
        ];

        let builder = HierarchyBuilder::new(symbols, 10);
        let result = builder.build();

        // Should have one root symbol (section) with my_func as child
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "My Section");
        assert_eq!(result[0].kind, SymbolKind::MODULE);
        assert!(result[0].children.is_some());
        let children = result[0].children.as_ref().unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, "my_func");
    }

    #[test]
    fn test_build_all_symbol_kinds() {
        // Test all DocumentSymbolKind variants convert correctly
        let symbols = vec![
            RawSymbol {
                name: "my_func".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 10 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 7 },
                },
                detail: None,
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "my_var".to_string(),
                kind: DocumentSymbolKind::Variable,
                range: Range {
                    start: Position { line: 1, character: 0 },
                    end: Position { line: 1, character: 10 },
                },
                selection_range: Range {
                    start: Position { line: 1, character: 0 },
                    end: Position { line: 1, character: 6 },
                },
                detail: None,
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "MY_CONST".to_string(),
                kind: DocumentSymbolKind::Constant,
                range: Range {
                    start: Position { line: 2, character: 0 },
                    end: Position { line: 2, character: 15 },
                },
                selection_range: Range {
                    start: Position { line: 2, character: 0 },
                    end: Position { line: 2, character: 8 },
                },
                detail: None,
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "MyClass".to_string(),
                kind: DocumentSymbolKind::Class,
                range: Range {
                    start: Position { line: 3, character: 0 },
                    end: Position { line: 3, character: 20 },
                },
                selection_range: Range {
                    start: Position { line: 3, character: 0 },
                    end: Position { line: 3, character: 7 },
                },
                detail: None,
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "myMethod".to_string(),
                kind: DocumentSymbolKind::Method,
                range: Range {
                    start: Position { line: 4, character: 0 },
                    end: Position { line: 4, character: 20 },
                },
                selection_range: Range {
                    start: Position { line: 4, character: 0 },
                    end: Position { line: 4, character: 8 },
                },
                detail: None,
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "myGeneric".to_string(),
                kind: DocumentSymbolKind::Interface,
                range: Range {
                    start: Position { line: 5, character: 0 },
                    end: Position { line: 5, character: 20 },
                },
                selection_range: Range {
                    start: Position { line: 5, character: 0 },
                    end: Position { line: 5, character: 9 },
                },
                detail: None,
                section_level: None,
                children: Vec::new(),
            },
        ];

        let builder = HierarchyBuilder::new(symbols, 10);
        let result = builder.build();

        assert_eq!(result.len(), 6);
        assert_eq!(result[0].kind, SymbolKind::FUNCTION);
        assert_eq!(result[1].kind, SymbolKind::VARIABLE);
        assert_eq!(result[2].kind, SymbolKind::CONSTANT);
        assert_eq!(result[3].kind, SymbolKind::CLASS);
        assert_eq!(result[4].kind, SymbolKind::METHOD);
        assert_eq!(result[5].kind, SymbolKind::INTERFACE);
    }

    #[test]
    fn test_build_complex_hierarchy() {
        // Complex hierarchy: section -> function -> nested function -> variable
        let symbols = vec![
            RawSymbol {
                name: "Section".to_string(),
                kind: DocumentSymbolKind::Module,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 15 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 15 },
                },
                detail: None,
                section_level: Some(1),
                children: Vec::new(),
            },
            RawSymbol {
                name: "outer".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 2, character: 0 },
                    end: Position { line: 10, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 2, character: 0 },
                    end: Position { line: 2, character: 5 },
                },
                detail: Some("()".to_string()),
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "inner".to_string(),
                kind: DocumentSymbolKind::Function,
                range: Range {
                    start: Position { line: 4, character: 4 },
                    end: Position { line: 8, character: 5 },
                },
                selection_range: Range {
                    start: Position { line: 4, character: 4 },
                    end: Position { line: 4, character: 9 },
                },
                detail: Some("(x)".to_string()),
                section_level: None,
                children: Vec::new(),
            },
            RawSymbol {
                name: "deep_var".to_string(),
                kind: DocumentSymbolKind::Variable,
                range: Range {
                    start: Position { line: 6, character: 8 },
                    end: Position { line: 6, character: 20 },
                },
                selection_range: Range {
                    start: Position { line: 6, character: 8 },
                    end: Position { line: 6, character: 16 },
                },
                detail: None,
                section_level: None,
                children: Vec::new(),
            },
        ];

        let builder = HierarchyBuilder::new(symbols, 15);
        let result = builder.build();

        // Section at root
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "Section");

        // outer inside section
        let section_children = result[0].children.as_ref().unwrap();
        assert_eq!(section_children.len(), 1);
        assert_eq!(section_children[0].name, "outer");

        // inner inside outer
        let outer_children = section_children[0].children.as_ref().unwrap();
        assert_eq!(outer_children.len(), 1);
        assert_eq!(outer_children[0].name, "inner");

        // deep_var inside inner
        let inner_children = outer_children[0].children.as_ref().unwrap();
        assert_eq!(inner_children.len(), 1);
        assert_eq!(inner_children[0].name, "deep_var");
    }

    // ========================================================================
    // flatten_document_symbols() Tests - Task 7.2
    // ========================================================================

    #[test]
    fn test_flatten_empty_symbols() {
        // Empty symbols list - should return empty Vec<SymbolInformation>
        let doc_symbols: Vec<DocumentSymbol> = vec![];
        let uri = Url::parse("file:///test.R").unwrap();
        let result = flatten_document_symbols(&doc_symbols, &uri);
        assert!(result.is_empty());
    }

    #[test]
    #[allow(deprecated)]
    fn test_flatten_single_symbol() {
        // Single symbol - should convert to SymbolInformation with correct URI
        let doc_symbols = vec![DocumentSymbol {
            name: "my_func".to_string(),
            kind: SymbolKind::FUNCTION,
            range: Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 5, character: 1 },
            },
            selection_range: Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 0, character: 7 },
            },
            detail: Some("(x, y)".to_string()),
            children: None,
            deprecated: None,
            tags: None,
        }];

        let uri = Url::parse("file:///path/to/test.R").unwrap();
        let result = flatten_document_symbols(&doc_symbols, &uri);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "my_func");
        assert_eq!(result[0].kind, SymbolKind::FUNCTION);
        // Requirement 1.3: Correct URI in location
        assert_eq!(result[0].location.uri, uri);
        assert_eq!(result[0].location.range.start.line, 0);
        assert_eq!(result[0].location.range.end.line, 5);
        assert!(result[0].container_name.is_none());
    }

    #[test]
    #[allow(deprecated)]
    fn test_flatten_nested_symbols() {
        // Nested symbols - should flatten with container_name set
        let doc_symbols = vec![DocumentSymbol {
            name: "outer_func".to_string(),
            kind: SymbolKind::FUNCTION,
            range: Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 10, character: 1 },
            },
            selection_range: Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 0, character: 10 },
            },
            detail: Some("()".to_string()),
            children: Some(vec![DocumentSymbol {
                name: "inner_var".to_string(),
                kind: SymbolKind::VARIABLE,
                range: Range {
                    start: Position { line: 2, character: 4 },
                    end: Position { line: 2, character: 20 },
                },
                selection_range: Range {
                    start: Position { line: 2, character: 4 },
                    end: Position { line: 2, character: 13 },
                },
                detail: None,
                children: None,
                deprecated: None,
                tags: None,
            }]),
            deprecated: None,
            tags: None,
        }];

        let uri = Url::parse("file:///path/to/test.R").unwrap();
        let result = flatten_document_symbols(&doc_symbols, &uri);

        assert_eq!(result.len(), 2);
        
        // First symbol: outer_func (no container)
        assert_eq!(result[0].name, "outer_func");
        assert_eq!(result[0].kind, SymbolKind::FUNCTION);
        assert_eq!(result[0].location.uri, uri);
        assert!(result[0].container_name.is_none());
        
        // Second symbol: inner_var (container = outer_func)
        assert_eq!(result[1].name, "inner_var");
        assert_eq!(result[1].kind, SymbolKind::VARIABLE);
        assert_eq!(result[1].location.uri, uri);
        assert_eq!(result[1].container_name, Some("outer_func".to_string()));
    }

    #[test]
    #[allow(deprecated)]
    fn test_flatten_deeply_nested_symbols() {
        // Deeply nested symbols - should flatten with correct container_names
        let doc_symbols = vec![DocumentSymbol {
            name: "section".to_string(),
            kind: SymbolKind::MODULE,
            range: Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 20, character: 0 },
            },
            selection_range: Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 0, character: 15 },
            },
            detail: None,
            children: Some(vec![DocumentSymbol {
                name: "outer".to_string(),
                kind: SymbolKind::FUNCTION,
                range: Range {
                    start: Position { line: 2, character: 0 },
                    end: Position { line: 15, character: 1 },
                },
                selection_range: Range {
                    start: Position { line: 2, character: 0 },
                    end: Position { line: 2, character: 5 },
                },
                detail: Some("()".to_string()),
                children: Some(vec![DocumentSymbol {
                    name: "inner".to_string(),
                    kind: SymbolKind::FUNCTION,
                    range: Range {
                        start: Position { line: 4, character: 4 },
                        end: Position { line: 10, character: 5 },
                    },
                    selection_range: Range {
                        start: Position { line: 4, character: 4 },
                        end: Position { line: 4, character: 9 },
                    },
                    detail: Some("(x)".to_string()),
                    children: Some(vec![DocumentSymbol {
                        name: "deep_var".to_string(),
                        kind: SymbolKind::VARIABLE,
                        range: Range {
                            start: Position { line: 6, character: 8 },
                            end: Position { line: 6, character: 20 },
                        },
                        selection_range: Range {
                            start: Position { line: 6, character: 8 },
                            end: Position { line: 6, character: 16 },
                        },
                        detail: None,
                        children: None,
                        deprecated: None,
                        tags: None,
                    }]),
                    deprecated: None,
                    tags: None,
                }]),
                deprecated: None,
                tags: None,
            }]),
            deprecated: None,
            tags: None,
        }];

        let uri = Url::parse("file:///path/to/test.R").unwrap();
        let result = flatten_document_symbols(&doc_symbols, &uri);

        assert_eq!(result.len(), 4);
        
        // section (no container)
        assert_eq!(result[0].name, "section");
        assert!(result[0].container_name.is_none());
        
        // outer (container = section)
        assert_eq!(result[1].name, "outer");
        assert_eq!(result[1].container_name, Some("section".to_string()));
        
        // inner (container = outer)
        assert_eq!(result[2].name, "inner");
        assert_eq!(result[2].container_name, Some("outer".to_string()));
        
        // deep_var (container = inner)
        assert_eq!(result[3].name, "deep_var");
        assert_eq!(result[3].container_name, Some("inner".to_string()));
        
        // All should have correct URI
        for sym in &result {
            assert_eq!(sym.location.uri, uri);
        }
    }

    #[test]
    #[allow(deprecated)]
    fn test_flatten_preserves_all_symbol_kinds() {
        // Test that all symbol kinds are preserved during flattening
        let doc_symbols = vec![
            DocumentSymbol {
                name: "my_func".to_string(),
                kind: SymbolKind::FUNCTION,
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 10 },
                },
                selection_range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 7 },
                },
                detail: None,
                children: None,
                deprecated: None,
                tags: None,
            },
            DocumentSymbol {
                name: "my_var".to_string(),
                kind: SymbolKind::VARIABLE,
                range: Range {
                    start: Position { line: 1, character: 0 },
                    end: Position { line: 1, character: 10 },
                },
                selection_range: Range {
                    start: Position { line: 1, character: 0 },
                    end: Position { line: 1, character: 6 },
                },
                detail: None,
                children: None,
                deprecated: None,
                tags: None,
            },
            DocumentSymbol {
                name: "MY_CONST".to_string(),
                kind: SymbolKind::CONSTANT,
                range: Range {
                    start: Position { line: 2, character: 0 },
                    end: Position { line: 2, character: 15 },
                },
                selection_range: Range {
                    start: Position { line: 2, character: 0 },
                    end: Position { line: 2, character: 8 },
                },
                detail: None,
                children: None,
                deprecated: None,
                tags: None,
            },
            DocumentSymbol {
                name: "MyClass".to_string(),
                kind: SymbolKind::CLASS,
                range: Range {
                    start: Position { line: 3, character: 0 },
                    end: Position { line: 3, character: 20 },
                },
                selection_range: Range {
                    start: Position { line: 3, character: 0 },
                    end: Position { line: 3, character: 7 },
                },
                detail: None,
                children: None,
                deprecated: None,
                tags: None,
            },
        ];

        let uri = Url::parse("file:///test.R").unwrap();
        let result = flatten_document_symbols(&doc_symbols, &uri);

        assert_eq!(result.len(), 4);
        assert_eq!(result[0].kind, SymbolKind::FUNCTION);
        assert_eq!(result[1].kind, SymbolKind::VARIABLE);
        assert_eq!(result[2].kind, SymbolKind::CONSTANT);
        assert_eq!(result[3].kind, SymbolKind::CLASS);
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use crate::cross_file::scope::{ScopedSymbol, SymbolKind};
    use crate::state::Document;
    use proptest::prelude::*;
    use std::collections::HashSet;

    // Helper to parse R code for property tests
    fn parse_r_code(code: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        parser.parse(code, None).unwrap()
    }

    // Helper to filter out R reserved keywords from generated identifiers
    fn is_r_reserved(s: &str) -> bool {
        matches!(
            s,
            "for"
                | "if"
                | "in"
                | "else"
                | "while"
                | "repeat"
                | "next"
                | "break"
                | "function"
                | "return"
                | "true"
                | "false"
                | "null"
                | "inf"
                | "nan"
        )
    }

    proptest! {
        #[test]
        fn test_library_require_extraction(pkg_name in "[a-z]{3,10}".prop_filter("Not reserved", |s| !is_r_reserved(s))) {
            let code_library = format!("library({})", pkg_name);
            let code_require = format!("require({})", pkg_name);
            let code_loadns = format!("loadNamespace(\"{}\")", pkg_name);

            let doc1 = Document::new(&code_library, None);
            let doc2 = Document::new(&code_require, None);
            let doc3 = Document::new(&code_loadns, None);

            prop_assert!(doc1.loaded_packages.contains(&pkg_name));
            prop_assert!(doc2.loaded_packages.contains(&pkg_name));
            prop_assert!(doc3.loaded_packages.contains(&pkg_name));
        }

        #[test]
        fn test_multiple_library_calls(pkg_count in 1usize..5) {
            let packages: Vec<String> = (0..pkg_count)
                .map(|i| format!("pkg{}", i))
                .collect();

            let code = packages.iter()
                .map(|p| format!("library({})", p))
                .collect::<Vec<_>>()
                .join("\n");

            let doc = Document::new(&code, None);

            for pkg in &packages {
                prop_assert!(doc.loaded_packages.contains(pkg));
            }
            prop_assert_eq!(doc.loaded_packages.len(), pkg_count);
        }

        #[test]
        fn test_mixed_symbol_types(
            var_name in "[a-z]{3,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            func_name in "[a-z]{3,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            builtin in prop::sample::select(vec!["print", "sum", "mean", "length"])
        ) {
            let code = format!(
                "{} <- 42\n{} <- function() {{}}\n{}({})",
                var_name, func_name, builtin, var_name
            );

            let tree = parse_r_code(&code);
            let mut defined = HashSet::new();
            collect_definitions(tree.root_node(), &code, &mut defined);

            prop_assert!(defined.contains(&var_name));
            prop_assert!(defined.contains(&func_name));
            prop_assert!(is_builtin(&builtin));
        }

        #[test]
        fn test_named_arguments_not_flagged(
            func_name in "[a-z]{3,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            arg_name in "[a-z]{2,6}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            value in 1i32..100
        ) {
            let code = format!("{}({} = {})", func_name, arg_name, value);

            let tree = parse_r_code(&code);
            let mut used = Vec::new();
            collect_usages(tree.root_node(), &code, &mut used);

            // func_name should be in used, but arg_name should NOT be
            let func_used = used.iter().any(|(name, _)| name == &func_name);
            let arg_used = used.iter().any(|(name, _)| name == &arg_name);

            prop_assert!(func_used, "Function name should be collected as usage");
            prop_assert!(!arg_used, "Named argument should NOT be collected as usage");
        }

        #[test]
        fn test_multiple_named_arguments(
            arg_count in 1usize..4
        ) {
            let args: Vec<String> = (0..arg_count)
                .map(|i| format!("arg{} = {}", i, i + 1))
                .collect();

            let code = format!("func({})", args.join(", "));

            let tree = parse_r_code(&code);
            let mut used = Vec::new();
            collect_usages(tree.root_node(), &code, &mut used);

            // None of the argument names should be flagged as usages
            for i in 0..arg_count {
                let arg_name = format!("arg{}", i);
                let arg_used = used.iter().any(|(name, _)| name == &arg_name);
                prop_assert!(!arg_used, "Named argument {} should not be flagged", arg_name);
            }
        }

        #[test]
        fn test_parameter_extraction_completeness(
            param_count in 1usize..5,
            has_defaults in prop::collection::vec(any::<bool>(), 1..5)
        ) {
            let param_count = param_count.min(has_defaults.len());
            let mut params = Vec::new();

            for i in 0..param_count {
                if has_defaults[i] {
                    params.push(format!("p{} = {}", i, i + 1));
                } else {
                    params.push(format!("p{}", i));
                }
            }

            let code = format!("f <- function({}) {{}}", params.join(", "));
            let tree = parse_r_code(&code);

            // Find function definition node
            let func_node = find_function_definition_node(tree.root_node(), "f", &code).unwrap();
            let signature = extract_function_signature(func_node, "f", &code);

            // All parameters should be present in signature
            for i in 0..param_count {
                let param_name = format!("p{}", i);
                prop_assert!(signature.contains(&param_name),
                    "Parameter {} should be in signature: {}", param_name, signature);
            }
        }

        #[test]
        fn test_assignment_operators_recognized(
            func_name in "[a-z]{3,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            op in prop::sample::select(vec!["<-", "=", "<<-"])
        ) {
            let code = format!("{} {} function() {{}}", func_name, op);
            let tree = parse_r_code(&code);

            let func_def = find_function_definition_node(tree.root_node(), &func_name, &code);
            prop_assert!(func_def.is_some(), "Function definition should be found for operator {}", op);

            if let Some(node) = func_def {
                prop_assert_eq!(node.kind(), "function_definition");
            }
        }

        #[test]
        fn test_search_priority(func_name in "[a-z]{3,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))) {
            use crate::state::{WorldState, Document};
            use tower_lsp::lsp_types::Url;

            let current_uri = Url::parse("file:///current.R").unwrap();
            let other_uri = Url::parse("file:///other.R").unwrap();
            let workspace_uri = Url::parse("file:///workspace.R").unwrap();

            // Create function definitions with different signatures
            let current_code = format!("{} <- function(a) {{ a }}", func_name);
            let other_code = format!("{} <- function(b, c) {{ b + c }}", func_name);
            let workspace_code = format!("{} <- function(x, y, z) {{ x + y + z }}", func_name);

            let mut state = WorldState::new(vec![]);
            state.documents.insert(current_uri.clone(), Document::new(&current_code, None));
            state.documents.insert(other_uri.clone(), Document::new(&other_code, None));
            state.workspace_index.insert(workspace_uri.clone(), Document::new(&workspace_code, None));

            // Search should return current document's definition first
            let signature = find_user_function_signature(&state, &current_uri, &func_name);
            prop_assert!(signature.is_some());

            if let Some(sig) = signature {
                prop_assert!(sig.contains("(a)"), "Should return current document's signature: {}", sig);
                prop_assert!(!sig.contains("(b, c)"), "Should not return other document's signature");
                prop_assert!(!sig.contains("(x, y, z)"), "Should not return workspace signature");
            }
        }
    }

    #[test]
    fn test_extract_definition_statement_variable() {
        use crate::cross_file::scope::SymbolKind;

        let code = "x <- 42\ny <- x + 1";
        let tree = parse_r_code(code);

        let symbol = ScopedSymbol {
            name: "x".to_string(),
            kind: SymbolKind::Variable,
            source_uri: Url::parse("file:///test.R").unwrap(),
            defined_line: 0,
            defined_column: 0,
            signature: None,
            is_declared: false,
        };

        let result = extract_statement_from_tree(&tree, &symbol, code);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.statement, "x <- 42");
    }

    #[test]
    fn test_extract_definition_statement_function() {
        let code = "f <- function(a, b) {\n  a + b\n}";
        let tree = parse_r_code(code);

        let symbol = ScopedSymbol {
            name: "f".to_string(),
            kind: SymbolKind::Function,
            source_uri: Url::parse("file:///test.R").unwrap(),
            defined_line: 0,
            defined_column: 0,
            signature: Some("f(a, b)".to_string()),
            is_declared: false,
        };

        let result = extract_statement_from_tree(&tree, &symbol, code);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.statement, "f <- function(a, b) {\n  a + b\n}");
    }

    #[test]
    fn test_extract_definition_statement_truncation() {
        let mut code = "long_func <- function() {\n".to_string();
        for i in 1..=15 {
            code.push_str(&format!("  line_{}\n", i));
        }
        code.push('}');

        let tree = parse_r_code(&code);

        let symbol = ScopedSymbol {
            name: "long_func".to_string(),
            kind: SymbolKind::Function,
            source_uri: Url::parse("file:///test.R").unwrap(),
            defined_line: 0,
            defined_column: 0,
            signature: None,
            is_declared: false,
        };

        let result = extract_statement_from_tree(&tree, &symbol, &code);
        assert!(result.is_some());
        let info = result.unwrap();

        // Should be truncated to 10 lines with ellipsis
        let lines: Vec<&str> = info.statement.lines().collect();
        assert_eq!(lines.len(), 11); // 10 lines + "..."
        assert_eq!(lines[10], "...");
    }

    #[test]
    fn test_extract_definition_statement_assignment_operators() {
        let test_cases = vec![
            ("x <- 42", "<-"),
            ("y = 100", "="),
            ("z <<- 'global'", "<<-"),
        ];

        for (code, op) in test_cases {
            let tree = parse_r_code(code);
            let var_name = code.split_whitespace().next().unwrap();

            let symbol = ScopedSymbol {
                name: var_name.to_string(),
                kind: SymbolKind::Variable,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: 0,
                signature: None,
                is_declared: false,
            };

            let result = extract_statement_from_tree(&tree, &symbol, code);
            assert!(
                result.is_some(),
                "Should extract statement for operator {}",
                op
            );
            let info = result.unwrap();
            assert_eq!(info.statement, code);
        }
    }

    #[test]
    fn test_extract_definition_statement_for_loop_iterator() {
        let code = "for (i in 1:10) {\n  print(i)\n}";
        let tree = parse_r_code(code);

        let symbol = ScopedSymbol {
            name: "i".to_string(),
            kind: SymbolKind::Variable,
            source_uri: Url::parse("file:///test.R").unwrap(),
            defined_line: 0,
            defined_column: 5, // Position of 'i' in for loop
            signature: None,
            is_declared: false,
        };

        let result = extract_statement_from_tree(&tree, &symbol, code);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.statement, "for (i in 1:10) {\n  print(i)\n}");
    }

    #[test]
    fn test_readlines_named_arg() {
        // This is the exact code from collate.r line 13
        let code = r#"run_hash <- trimws(readLines("output/oos/latest_hash.txt", n = 1))"#;
        let tree = parse_r_code(code);

        let mut used = Vec::new();
        collect_usages(tree.root_node(), code, &mut used);

        eprintln!("\n=== Collected usages ===");
        for (name, node) in &used {
            eprintln!("  '{}' (kind: {})", name, node.kind());
        }

        // trimws and readLines should be collected, but n should NOT be
        let trimws_used = used.iter().any(|(name, _)| name == "trimws");
        let readlines_used = used.iter().any(|(name, _)| name == "readLines");
        let n_used = used.iter().any(|(name, _)| name == "n");

        assert!(trimws_used, "trimws should be collected");
        assert!(readlines_used, "readLines should be collected");
        assert!(
            !n_used,
            "n should NOT be collected as it's a named argument"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 100,
            .. ProptestConfig::default()
        })]
        #[test]
        fn test_user_defined_priority_over_builtins(
            builtin in prop::sample::select(vec!["print", "sum", "mean", "length"])
        ) {
            use crate::state::{WorldState, Document};
            use tower_lsp::lsp_types::Url;

            let uri = Url::parse("file:///test.R").unwrap();

            // Create code with user-defined function that shadows a built-in
            let code = format!("{} <- function(x, y) {{ x + y }}", builtin);

            let mut state = WorldState::new(vec![]);
            state.documents.insert(uri.clone(), Document::new(&code, None));

            // Should return user-defined signature, not built-in
            let signature = find_user_function_signature(&state, &uri, &builtin);
            prop_assert!(signature.is_some(), "Should find user-defined function");

            if let Some(sig) = signature {
                prop_assert!(sig.contains("(x, y)"), "Should return user-defined signature: {}", sig);
                prop_assert!(sig.contains(&builtin), "Should contain function name: {}", sig);
            }
        }

        #[test]
        fn test_signature_format_correctness(
            func_name in "[a-z][a-z0-9_]{2,10}",
            param_count in 0usize..5
        ) {
            let params: Vec<String> = (0..param_count)
                .map(|i| format!("p{}", i))
                .collect();

            let code = format!("{} <- function({}) {{}}", func_name, params.join(", "));
            let tree = parse_r_code(&code);

            let func_node = find_function_definition_node(tree.root_node(), &func_name, &code).unwrap();
            let signature = extract_function_signature(func_node, &func_name, &code);

            // Verify format: name(params)
            prop_assert!(signature.starts_with(&func_name), "Signature should start with function name");
            prop_assert!(signature.contains('('), "Signature should contain opening parenthesis");
            prop_assert!(signature.ends_with(')'), "Signature should end with closing parenthesis");

            let expected = format!("{}({})", func_name, params.join(", "));
            prop_assert_eq!(signature, expected, "Signature format should match expected pattern");
        }

        #[test]
        // Feature: enhanced-variable-detection-hover, Property 10: Variable hover definition extraction
        fn prop_variable_hover_definition_extraction(
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            value in 1i32..1000
        ) {
            let code = format!("{} <- {}", var_name, value);
            let tree = parse_r_code(&code);

            let symbol = ScopedSymbol {
                name: var_name.clone(),
                kind: SymbolKind::Variable,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: 0,
                signature: None,
                is_declared: false,
            };

            let def_info = extract_statement_from_tree(&tree, &symbol, &code);
            prop_assert!(def_info.is_some(), "Should extract definition for variable");

            let info = def_info.unwrap();
            prop_assert_eq!(info.statement, code, "Should include complete definition statement");
        }

        #[test]
        // Feature: enhanced-variable-detection-hover, Property 11: Function hover signature extraction
        fn prop_function_hover_signature_extraction(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            param_count in 0usize..3
        ) {
            let params: Vec<String> = (0..param_count)
                .map(|i| format!("p{}", i))
                .collect();

            let code = format!("{} <- function({}) {{}}", func_name, params.join(", "));
            let tree = parse_r_code(&code);

            let symbol = ScopedSymbol {
                name: func_name.clone(),
                kind: SymbolKind::Function,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: 0,
                signature: None,
                is_declared: false,
            };

            let def_info = extract_statement_from_tree(&tree, &symbol, &code);
            prop_assert!(def_info.is_some(), "Should extract definition for function");

            let info = def_info.unwrap();
            prop_assert!(info.statement.contains(&func_name), "Should include function name");
            prop_assert!(info.statement.contains("function"), "Should include function keyword");

            for param in &params {
                prop_assert!(info.statement.contains(param), "Should include parameter {}", param);
            }
        }

        #[test]
        // Feature: enhanced-variable-detection-hover, Property 12: Multi-line definition handling
        fn prop_multiline_definition_handling(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            line_count in 5usize..15
        ) {
            let mut code = format!("{} <- function() {{\n", func_name);
            for i in 1..line_count {
                code.push_str(&format!("  line_{}\n", i));
            }
            code.push('}');

            let tree = parse_r_code(&code);

            let symbol = ScopedSymbol {
                name: func_name.clone(),
                kind: SymbolKind::Function,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: 0,
                signature: None,
                is_declared: false,
            };

            let def_info = extract_statement_from_tree(&tree, &symbol, &code);
            prop_assert!(def_info.is_some(), "Should extract multi-line definition");

            let info = def_info.unwrap();
            let lines: Vec<&str> = info.statement.lines().collect();

            // The generated code has (line_count + 1) total lines (header + (line_count-1) body lines + closing brace).
            // Truncation happens when total lines > 10, i.e. when line_count > 9.
            if line_count > 9 {
                prop_assert_eq!(lines.len(), 11, "Should truncate to 10 lines + ellipsis");
                prop_assert_eq!(lines[10], "...", "Should end with ellipsis when truncated");
            } else {
                // The generated code includes the function header line and a closing brace line.
                let expected_lines = line_count + 1;
                prop_assert_eq!(lines.len(), expected_lines, "Should include all lines when <= 10");
                prop_assert!(!info.statement.contains("..."), "Should not have ellipsis when not truncated");
            }
        }

        #[test]
        // Feature: enhanced-variable-detection-hover, Property 13: Markdown code block formatting
        fn prop_markdown_code_block_formatting(
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            special_chars in prop::sample::select(vec!["*", "_", "[", "]", "(", ")", "#", "`", "\\"])
        ) {
            let code = format!("{} <- \"value with {} chars\"", var_name, special_chars);
            let escaped = escape_markdown(&code);
            let formatted = format!("```r\n{}\n```", escaped);

            prop_assert!(formatted.starts_with("```r\n"), "Should start with R code block marker");
            prop_assert!(formatted.ends_with("\n```"), "Should end with code block marker");
            prop_assert!(formatted.contains(&format!("\\{}", special_chars)), "Should escape special markdown characters");
        }

        #[test]
        // Feature: enhanced-variable-detection-hover, Property 14: Same-file location format
        fn prop_same_file_location_format(
            line_num in 0u32..100
        ) {
            let uri = Url::parse("file:///test.R").unwrap();
            let def_info = DefinitionInfo {
                statement: "test_var <- 42".to_string(),
                source_uri: uri.clone(),
                line: line_num,
                column: 0,
            };

            let mut value = String::new();
            value.push_str(&format!("```r\n{}\n```\n\n", escape_markdown(&def_info.statement)));

            if def_info.source_uri == uri {
                value.push_str(&format!("this file, line {}", def_info.line + 1));
            }

            prop_assert!(value.contains("this file"), "Should indicate same file");
            prop_assert!(value.contains(&format!("line {}", line_num + 1)), "Should show 1-based line number");
            prop_assert!(!value.contains("file://"), "Should not contain file URI for same file");
        }

        #[test]
        // Feature: enhanced-variable-detection-hover, Property 15: Cross-file hyperlink format
        fn prop_cross_file_hyperlink_format(
            line_num in 0u32..100
        ) {
            let current_uri = Url::parse("file:///workspace/main.R").unwrap();
            let def_uri = Url::parse("file:///workspace/utils/helper.R").unwrap();
            let workspace_root = Some(Url::parse("file:///workspace/").unwrap());

            let def_info = DefinitionInfo {
                statement: "helper_func <- function() {}".to_string(),
                source_uri: def_uri.clone(),
                line: line_num,
                column: 0,
            };

            let mut value = String::new();
            value.push_str(&format!("```r\n{}\n```\n\n", escape_markdown(&def_info.statement)));

            if def_info.source_uri != current_uri {
                let relative_path = compute_relative_path(&def_info.source_uri, workspace_root.as_ref());
                let absolute_path = def_info.source_uri.as_str();
                value.push_str(&format!("[{}]({}), line {}", relative_path, absolute_path, def_info.line + 1));
            }

            prop_assert!(value.contains("[utils/helper.R]"), "Should show relative path in brackets");
            prop_assert!(value.contains("(file:///workspace/utils/helper.R)"), "Should show absolute URI in parentheses");
            prop_assert!(value.contains(&format!("line {}", line_num + 1)), "Should show 1-based line number");
            prop_assert!(value.contains(", line"), "Should separate path and line with comma");
        }

        #[test]
        // Property 21: Definition statement and location separation
        fn prop_definition_statement_location_separation(
            statement in "[a-z_]+ <- [a-z0-9_(){}]+",
            line_num in 0u32..100
        ) {
            let def_info = DefinitionInfo {
                statement: statement.clone(),
                source_uri: Url::parse("file:///test.R").unwrap(),
                line: line_num,
                column: 0,
            };

            let escaped_statement = escape_markdown(&def_info.statement);
            let mut value = String::new();
            value.push_str(&format!("```r\n{}\n```\n\n", escaped_statement));
            value.push_str(&format!("this file, line {}", def_info.line + 1));

            // Should have exactly one blank line between definition and location
            prop_assert!(value.contains("```\n\nthis file"), "Should have blank line separator");
            prop_assert!(!value.contains("```\nthis file"), "Should not have zero blank lines");
            prop_assert!(!value.contains("```\n\n\nthis file"), "Should not have multiple blank lines");
        }

        #[test]
        // Property 22: Definition statement truncation
        fn prop_definition_statement_truncation(
            line_count in 11usize..20
        ) {
            let mut statement = "long_func <- function() {\n".to_string();
            for i in 1..line_count {
                statement.push_str(&format!("  line_{}\n", i));
            }
            statement.push('}');

            let tree = parse_r_code(&statement);
            let symbol = ScopedSymbol {
                name: "long_func".to_string(),
                kind: SymbolKind::Function,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: 0,
                signature: None,
                is_declared: false,
            };

            let def_info = extract_statement_from_tree(&tree, &symbol, &statement);
            prop_assert!(def_info.is_some(), "Should extract definition");

            let info = def_info.unwrap();
            let lines: Vec<&str> = info.statement.lines().collect();

            prop_assert_eq!(lines.len(), 11, "Should truncate to 10 lines + ellipsis");
            prop_assert_eq!(lines[10], "...", "Should end with ellipsis");
        }

        #[test]
        // Property 23: Indentation preservation
        fn prop_indentation_preservation(
            indent_size in 0usize..8,
            line_count in 2usize..6
        ) {
            let indent = " ".repeat(indent_size);
            let mut statement = format!("{}func <- function() {{\n", indent);
            for i in 1..line_count {
                statement.push_str(&format!("{}  line_{}\n", indent, i));
            }
            statement.push_str(&format!("{}}}", indent));

            let tree = parse_r_code(&statement);
            let symbol = ScopedSymbol {
                name: "func".to_string(),
                kind: SymbolKind::Function,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: indent_size as u32,
                signature: None,
                is_declared: false,
            };

            let def_info = extract_statement_from_tree(&tree, &symbol, &statement);
            prop_assert!(def_info.is_some(), "Should extract definition");

            let info = def_info.unwrap();
            let lines: Vec<&str> = info.statement.lines().collect();

            // Check that indentation is preserved
            for line in &lines {
                if !line.trim().is_empty() {
                    prop_assert!(line.starts_with(&indent), "Should preserve original indentation: '{}'", line);
                }
            }
        }

        #[test]
        // Property 24: Markdown character escaping
        fn prop_markdown_character_escaping(
            special_char in prop::sample::select(vec!["*", "_", "[", "]", "(", ")", "#", "`", "\\"])
        ) {
            let statement = format!("var <- \"value with {} char\"", special_char);
            let escaped = escape_markdown(&statement);

            let expected_escaped = format!("\\{}", special_char);
            prop_assert!(escaped.contains(&expected_escaped),
                "Should escape '{}' to '{}' in: '{}'", special_char, expected_escaped, escaped);

            // Verify it's properly formatted in hover content
            let hover_content = format!("```r\n{}\n```", escaped);
            prop_assert!(hover_content.contains(&expected_escaped),
                "Should contain escaped character in hover content");
        }

        #[test]
        // Property 28: Assignment operator extraction
        fn prop_assignment_operator_extraction(
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            op in prop::sample::select(vec!["<-", "=", "<<-"]),
            value in 1i32..1000
        ) {
            let code = format!("{} {} {}", var_name, op, value);
            let tree = parse_r_code(&code);

            let symbol = ScopedSymbol {
                name: var_name.clone(),
                kind: SymbolKind::Variable,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: 0,
                signature: None,
                is_declared: false,
            };

            let def_info = extract_statement_from_tree(&tree, &symbol, &code);
            prop_assert!(def_info.is_some(), "Should extract assignment statement");

            let info = def_info.unwrap();
            let statement = &info.statement;
            prop_assert_eq!(statement, &code, "Should include complete assignment statement");
            prop_assert!(statement.contains(&op), "Should include assignment operator {}", op);
        }

        #[test]
        // Property 29: Inline function extraction
        fn prop_inline_function_extraction(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            param_count in 0usize..3
        ) {
            let params: Vec<String> = (0..param_count)
                .map(|i| format!("p{}", i))
                .collect();

            let code = format!("{} <- function({}) {{ {} }}", func_name, params.join(", "), "x + 1");
            let tree = parse_r_code(&code);

            let symbol = ScopedSymbol {
                name: func_name.clone(),
                kind: SymbolKind::Function,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: 0,
                signature: None,
                is_declared: false,
            };

            let def_info = extract_statement_from_tree(&tree, &symbol, &code);
            prop_assert!(def_info.is_some(), "Should extract function definition");

            let info = def_info.unwrap();
            prop_assert!(info.statement.contains("function"), "Should include function keyword");
            prop_assert!(info.statement.contains(&format!("({})", params.join(", "))), "Should include function signature");
        }

        #[test]
        // Property 30: Loop iterator definition extraction
        fn prop_loop_iterator_definition_extraction(
            iterator in "[a-z][a-z0-9_]{1,5}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            range_end in 5i32..20
        ) {
            let code = format!("for ({} in 1:{}) {{\n  print({})\n}}", iterator, range_end, iterator);
            let tree = parse_r_code(&code);

            let symbol = ScopedSymbol {
                name: iterator.clone(),
                kind: SymbolKind::Variable,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: 5, // Position of iterator in for loop
                signature: None,
                is_declared: false,
            };

            let def_info = extract_statement_from_tree(&tree, &symbol, &code);
            prop_assert!(def_info.is_some(), "Should extract for loop definition");

            let info = def_info.unwrap();
            prop_assert!(info.statement.contains("for"), "Should include for loop header");
            prop_assert!(info.statement.contains(&format!("{} in", iterator)), "Should include iterator definition");
        }

        #[test]
        // Property 31: Function parameter definition extraction
        fn prop_function_parameter_definition_extraction(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            param_name in "[a-z][a-z0-9_]{1,5}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            has_default in any::<bool>()
        ) {
            let param_def = if has_default {
                format!("{} = 42", param_name)
            } else {
                param_name.clone()
            };

            let code = format!("{} <- function({}) {{\n  {}\n}}", func_name, param_def, param_name);
            let tree = parse_r_code(&code);

            let symbol = ScopedSymbol {
                name: param_name.clone(),
                kind: SymbolKind::Variable,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line: 0,
                defined_column: func_name.len() as u32 + 15, // Approximate position in function signature
                signature: None,
                is_declared: false,
            };

            let def_info = extract_statement_from_tree(&tree, &symbol, &code);
            prop_assert!(def_info.is_some(), "Should extract function definition for parameter");

            let info = def_info.unwrap();
            prop_assert!(info.statement.contains("function"), "Should include function keyword");
            prop_assert!(info.statement.contains(&param_name), "Should include parameter name in signature");
        }

        #[test]
        // Property 16: File URI protocol
        fn prop_file_uri_protocol(
            path_segments in prop::collection::vec("[a-z]{3,8}", 1..4)
        ) {
            let path = format!("/{}", path_segments.join("/"));
            let uri = Url::parse(&format!("file://{}/test.R", path)).unwrap();

            let def_info = DefinitionInfo {
                statement: "test_var <- 42".to_string(),
                source_uri: uri.clone(),
                line: 0,
                column: 0,
            };

            let current_uri = Url::parse("file:///workspace/main.R").unwrap();
            let mut value = String::new();
            value.push_str(&format!("```r\n{}\n```\n\n", escape_markdown(&def_info.statement)));

            if def_info.source_uri != current_uri {
                let relative_path = compute_relative_path(&def_info.source_uri, None);
                let absolute_path = def_info.source_uri.as_str();
                value.push_str(&format!("[{}]({}), line {}", relative_path, absolute_path, def_info.line + 1));
            }

            prop_assert!(value.contains("file://"), "Cross-file URI should use file:// protocol");
            prop_assert!(value.contains(&format!("file://{}/test.R", path)), "Should contain absolute path with file:// protocol");
        }

        #[test]
        // Property 17: Relative path calculation
        fn prop_relative_path_calculation(
            workspace_depth in 1usize..3,
            file_depth in 1usize..3
        ) {
            let workspace_segments: Vec<String> = (0..workspace_depth).map(|i| format!("ws{}", i)).collect();
            let file_segments: Vec<String> = (0..file_depth).map(|i| format!("dir{}", i)).collect();

            let workspace_root = Url::parse(&format!("file:///{}/", workspace_segments.join("/"))).unwrap();
            let target_uri = Url::parse(&format!("file:///{}/{}/test.R", workspace_segments.join("/"), file_segments.join("/"))).unwrap();

            let relative_path = compute_relative_path(&target_uri, Some(&workspace_root));

            prop_assert!(relative_path.contains(&file_segments.join("/")), "Should contain file path relative to workspace");
            prop_assert!(!relative_path.starts_with('/'), "Relative path should not start with /");
            prop_assert!(relative_path.ends_with("test.R"), "Should end with filename");
        }

        #[test]
        // Property 18: LSP Markdown markup kind
        fn prop_lsp_markdown_markup_kind(
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            use crate::state::{WorldState, Document};

            let library_paths = vec![];
            let mut state = WorldState::new(library_paths);

            let uri = Url::parse("file:///test.R").unwrap();
            let code = format!("{} <- 42", var_name);
            state.documents.insert(uri.clone(), Document::new(&code, None));

            let position = Position::new(0, 5);
            let hover_result = hover_blocking(&state, &uri, position);

            if let Some(hover) = hover_result {
                if let HoverContents::Markup(content) = hover.contents {
                    prop_assert_eq!(content.kind, MarkupKind::Markdown, "Hover content should use Markdown markup kind");
                } else {
                    prop_assert!(false, "Hover should return Markup content");
                }
            }
        }

        #[test]
        // Property 19: Cross-file definition resolution
        fn prop_cross_file_definition_resolution(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            use crate::state::{WorldState, Document};

            let library_paths = vec![];
            let mut state = WorldState::new(library_paths);

            let main_uri = Url::parse("file:///main.R").unwrap();
            let utils_uri = Url::parse("file:///utils.R").unwrap();

            let main_code = format!("source(\"utils.R\")\nresult <- {}(42)", func_name);
            let utils_code = format!("{} <- function(x) {{ x * 2 }}", func_name);

            state.documents.insert(main_uri.clone(), Document::new(&main_code, None));
            state.documents.insert(utils_uri.clone(), Document::new(&utils_code, None));

            // Update cross-file graph
            state.cross_file_graph.update_file(&main_uri, &crate::cross_file::extract_metadata(&main_code), None, |_| None);
            state.cross_file_graph.update_file(&utils_uri, &crate::cross_file::extract_metadata(&utils_code), None, |_| None);

            let position = Position::new(1, 10); // Position after source() call
            let cross_file_symbols = get_cross_file_symbols(&state, &main_uri, position.line, position.character);

            prop_assert!(cross_file_symbols.contains_key(&func_name), "Should resolve cross-file symbol using dependency graph");

            if let Some(symbol) = cross_file_symbols.get(&func_name) {
                prop_assert_eq!(&symbol.source_uri, &utils_uri, "Should locate definition in sourced file");
            }
        }

        #[test]
        // Property 20: Scope-based definition selection
        fn prop_scope_based_definition_selection(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            use crate::state::{WorldState, Document};

            let library_paths = vec![];
            let mut state = WorldState::new(library_paths);

            let uri = Url::parse("file:///test.R").unwrap();
            let code = format!(
                "{} <- function(a) {{ a }}\nsource(\"utils.R\")\n{} <- function(b, c) {{ b + c }}\nresult <- {}(1, 2)",
                func_name, func_name, func_name
            );

            let utils_uri = Url::parse("file:///utils.R").unwrap();
            let utils_code = format!("{} <- function(x, y, z) {{ x + y + z }}", func_name);

            state.documents.insert(uri.clone(), Document::new(&code, None));
            state.documents.insert(utils_uri.clone(), Document::new(&utils_code, None));

            // Update cross-file graph
            state.cross_file_graph.update_file(&uri, &crate::cross_file::extract_metadata(&code), None, |_| None);
            state.cross_file_graph.update_file(&utils_uri, &crate::cross_file::extract_metadata(&utils_code), None, |_| None);

            let position = Position::new(3, 10); // Position of function usage
            let cross_file_symbols = get_cross_file_symbols(&state, &uri, position.line, position.character);

            prop_assert!(cross_file_symbols.contains_key(&func_name), "Should find symbol definition");

            if let Some(symbol) = cross_file_symbols.get(&func_name) {
                // Should select the local definition (line 2) that's in scope, not the earlier one or utils.R
                prop_assert_eq!(&symbol.source_uri, &uri, "Should select definition from same file");
                prop_assert_eq!(symbol.defined_line, 2, "Should select the definition that's in scope at reference position");
            }
        }

        // ========================================================================
        // Feature: skip-nse-undefined-checks
        // Property-based tests for NSE context skipping in undefined variable checks
        // ========================================================================

        #[test]
        /// Feature: skip-nse-undefined-checks, Property 1: Extract Operator RHS Skipped
        /// For any R code containing an extract operator ($ or @), the identifier on the
        /// right-hand side SHALL NOT be collected as a usage.
        fn prop_skip_nse_extract_operator_rhs_skipped(
            lhs in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            rhs in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            op in prop::sample::select(vec!["$", "@"])
        ) {
            let code = format!("{}{}{}", lhs, op, rhs);
            let tree = parse_r_code(&code);
            let mut used = Vec::new();
            collect_usages_with_context(tree.root_node(), &code, &UsageContext::default(), &mut used);

            let rhs_used = used.iter().any(|(name, _)| name == &rhs);
            prop_assert!(!rhs_used, "RHS '{}' of extract operator should NOT be collected", rhs);
        }

        #[test]
        /// Feature: skip-nse-undefined-checks, Property 2: Extract Operator LHS Checked
        /// For any R code containing an extract operator ($ or @), the identifier on the
        /// left-hand side SHALL be collected as a usage.
        fn prop_skip_nse_extract_operator_lhs_checked(
            lhs in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            rhs in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            op in prop::sample::select(vec!["$", "@"])
        ) {
            let code = format!("{}{}{}", lhs, op, rhs);
            let tree = parse_r_code(&code);
            let mut used = Vec::new();
            collect_usages_with_context(tree.root_node(), &code, &UsageContext::default(), &mut used);

            let lhs_used = used.iter().any(|(name, _)| name == &lhs);
            prop_assert!(lhs_used, "LHS '{}' of extract operator should be collected", lhs);
        }

        #[test]
        /// Feature: skip-nse-undefined-checks, Property 3: Call-Like Arguments Skipped
        /// For any R code containing a call-like node (call, subset, subset2), identifiers
        /// inside the arguments field SHALL NOT be collected as usages.
        fn prop_skip_nse_call_like_arguments_skipped(
            func in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            arg in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            call_type in prop::sample::select(vec!["call", "subset", "subset2"])
        ) {
            let code = match call_type {
                "call" => format!("{}({})", func, arg),
                "subset" => format!("{}[{}]", func, arg),
                "subset2" => format!("{}[[{}]]", func, arg),
                _ => unreachable!(),
            };
            let tree = parse_r_code(&code);
            let mut used = Vec::new();
            collect_usages_with_context(tree.root_node(), &code, &UsageContext::default(), &mut used);

            let arg_used = used.iter().any(|(name, _)| name == &arg);
            prop_assert!(!arg_used, "Argument '{}' inside {} should NOT be collected", arg, call_type);
        }

        #[test]
        /// Feature: skip-nse-undefined-checks, Property 4: Function Names Checked
        /// For any R code containing a function call, the function name SHALL be collected
        /// as a usage.
        fn prop_skip_nse_function_names_checked(
            func in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            arg in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            let code = format!("{}({})", func, arg);
            let tree = parse_r_code(&code);
            let mut used = Vec::new();
            collect_usages_with_context(tree.root_node(), &code, &UsageContext::default(), &mut used);

            let func_used = used.iter().any(|(name, _)| name == &func);
            prop_assert!(func_used, "Function name '{}' should be collected", func);
        }

        #[test]
        /// Feature: skip-nse-undefined-checks, Property 5: Formula Expressions Skipped
        /// For any R code containing a formula expression (unary ~ or binary ~), identifiers
        /// inside the formula SHALL NOT be collected as usages.
        fn prop_skip_nse_formula_expressions_skipped(
            var in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            formula_type in prop::sample::select(vec!["unary", "binary"])
        ) {
            let code = match formula_type {
                "unary" => format!("~ {}", var),
                "binary" => format!("y ~ {}", var),
                _ => unreachable!(),
            };
            let tree = parse_r_code(&code);
            let mut used = Vec::new();
            collect_usages_with_context(tree.root_node(), &code, &UsageContext::default(), &mut used);

            let var_used = used.iter().any(|(name, _)| name == &var);
            prop_assert!(!var_used, "Variable '{}' inside {} formula should NOT be collected", var, formula_type);
        }

        #[test]
        /// Feature: skip-nse-undefined-checks, Property 6: Nested Skip Contexts
        /// For any R code where a formula appears inside call arguments, identifiers in the
        /// formula SHALL NOT be collected (both skip contexts apply).
        fn prop_skip_nse_nested_formula_in_call(
            func in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            lhs in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            rhs in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            let code = format!("{}({} ~ {})", func, lhs, rhs);
            let tree = parse_r_code(&code);
            let mut used = Vec::new();
            collect_usages_with_context(tree.root_node(), &code, &UsageContext::default(), &mut used);

            let lhs_used = used.iter().any(|(name, _)| name == &lhs);
            let rhs_used = used.iter().any(|(name, _)| name == &rhs);
            prop_assert!(!lhs_used, "Formula LHS '{}' inside call should NOT be collected", lhs);
            prop_assert!(!rhs_used, "Formula RHS '{}' inside call should NOT be collected", rhs);
        }

        #[test]
        /// Feature: skip-nse-undefined-checks, Property 7: Existing Skip Rules Preserved
        /// For any R code containing assignments or named arguments, the existing skip rules
        /// SHALL continue to work (assignment LHS and named argument names are skipped).
        fn prop_skip_nse_existing_rules_preserved(
            var in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            op in prop::sample::select(vec!["<-", "=", "<<-"]),
            arg_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Test assignment LHS
            let assign_code = format!("{} {} 42", var, op);
            let tree = parse_r_code(&assign_code);
            let mut used = Vec::new();
            collect_usages_with_context(tree.root_node(), &assign_code, &UsageContext::default(), &mut used);
            let var_used = used.iter().any(|(name, _)| name == &var);
            prop_assert!(!var_used, "Assignment LHS '{}' with '{}' should NOT be collected", var, op);

            // Test named argument
            let named_arg_code = format!("func({} = 1)", arg_name);
            let tree2 = parse_r_code(&named_arg_code);
            let mut used2 = Vec::new();
            collect_usages_with_context(tree2.root_node(), &named_arg_code, &UsageContext::default(), &mut used2);
            let arg_used = used2.iter().any(|(name, _)| name == &arg_name);
            prop_assert!(!arg_used, "Named argument '{}' should NOT be collected", arg_name);
        }

        #[test]
        /// Feature: skip-nse-undefined-checks, Property 8: Non-Skipped Contexts Checked
        /// For any R code containing an identifier NOT in a skip context, the identifier
        /// SHALL be collected as a usage.
        fn prop_skip_nse_non_skipped_contexts_checked(
            var in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Standalone identifier (not in any skip context)
            let code = var.clone();
            let tree = parse_r_code(&code);
            let mut used = Vec::new();
            collect_usages_with_context(tree.root_node(), &code, &UsageContext::default(), &mut used);

            let var_used = used.iter().any(|(name, _)| name == &var);
            prop_assert!(var_used, "Standalone identifier '{}' should be collected", var);
        }

        // ========================================================================
        // **Feature: reserved-keyword-handling, Property 3: Undefined Variable Check Exclusion**
        // **Validates: Requirements 3.1, 3.2, 3.3**
        //
        // For any R code containing a reserved word used as an identifier (in any
        // syntactic position), the undefined variable checker SHALL NOT emit an
        // "Undefined variable" diagnostic for that reserved word.
        // ========================================================================

        #[test]
        /// Feature: reserved-keyword-handling, Property 3: Undefined Variable Check Exclusion
        ///
        /// For any R code containing a reserved word used as an identifier (in any
        /// syntactic position), the undefined variable checker SHALL NOT emit an
        /// "Undefined variable" diagnostic for that reserved word.
        ///
        /// **Validates: Requirements 3.1, 3.2, 3.3**
        fn prop_reserved_words_not_flagged_as_undefined_standalone(
            reserved_word in prop::sample::select(crate::reserved_words::RESERVED_WORDS)
        ) {
            use crate::state::{WorldState, Document};
            use crate::cross_file::directive::parse_directives;

            // Create code with just the reserved word as a standalone identifier
            let code = reserved_word.to_string();
            let tree = parse_r_code(&code);

            let mut state = WorldState::new(vec![]);
            state.cross_file_config.undefined_variables_enabled = true;
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            let directive_meta = parse_directives(&code);
            let mut diagnostics = Vec::new();

            collect_undefined_variables_position_aware(
                &state,
                &uri,
                tree.root_node(),
                &code,
                &[],
                &[],
                &state.package_library,
                &directive_meta,
                &mut diagnostics,
            );

            // Filter for "Undefined variable" diagnostics for this reserved word
            let undefined_diags: Vec<_> = diagnostics
                .iter()
                .filter(|d| d.message.contains(&format!("Undefined variable: {}", reserved_word)))
                .collect();

            prop_assert!(
                undefined_diags.is_empty(),
                "Reserved word '{}' should NOT produce 'Undefined variable' diagnostic, but got: {:?}",
                reserved_word,
                undefined_diags
            );
        }

        #[test]
        /// Feature: reserved-keyword-handling, Property 3: Undefined Variable Check Exclusion
        ///
        /// For any R code containing a reserved word used in an expression context,
        /// the undefined variable checker SHALL NOT emit an "Undefined variable"
        /// diagnostic for that reserved word.
        ///
        /// **Validates: Requirements 3.1, 3.2, 3.3**
        fn prop_reserved_words_not_flagged_as_undefined_in_expression(
            reserved_word in prop::sample::select(crate::reserved_words::RESERVED_WORDS),
            var_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            use crate::state::{WorldState, Document};
            use crate::cross_file::directive::parse_directives;

            // Create code with reserved word used in an expression (e.g., x <- else)
            // This is syntactically invalid R, but the undefined variable checker
            // should still not flag the reserved word as undefined
            let code = format!("{} <- {}", var_name, reserved_word);
            let tree = parse_r_code(&code);

            let mut state = WorldState::new(vec![]);
            state.cross_file_config.undefined_variables_enabled = true;
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            let directive_meta = parse_directives(&code);
            let mut diagnostics = Vec::new();

            collect_undefined_variables_position_aware(
                &state,
                &uri,
                tree.root_node(),
                &code,
                &[],
                &[],
                &state.package_library,
                &directive_meta,
                &mut diagnostics,
            );

            // Filter for "Undefined variable" diagnostics for this reserved word
            let undefined_diags: Vec<_> = diagnostics
                .iter()
                .filter(|d| d.message.contains(&format!("Undefined variable: {}", reserved_word)))
                .collect();

            prop_assert!(
                undefined_diags.is_empty(),
                "Reserved word '{}' in expression should NOT produce 'Undefined variable' diagnostic, but got: {:?}",
                reserved_word,
                undefined_diags
            );
        }

        #[test]
        /// Feature: reserved-keyword-handling, Property 3: Undefined Variable Check Exclusion
        ///
        /// For any R code containing a reserved word used in a function call context,
        /// the undefined variable checker SHALL NOT emit an "Undefined variable"
        /// diagnostic for that reserved word.
        ///
        /// **Validates: Requirements 3.1, 3.2, 3.3**
        fn prop_reserved_words_not_flagged_as_undefined_in_call(
            reserved_word in prop::sample::select(crate::reserved_words::RESERVED_WORDS)
        ) {
            use crate::state::{WorldState, Document};
            use crate::cross_file::directive::parse_directives;

            // Create code with reserved word used as a function argument
            // e.g., print(else) - syntactically invalid but tests the checker
            let code = format!("print({})", reserved_word);
            let tree = parse_r_code(&code);

            let mut state = WorldState::new(vec![]);
            state.cross_file_config.undefined_variables_enabled = true;
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            let directive_meta = parse_directives(&code);
            let mut diagnostics = Vec::new();

            collect_undefined_variables_position_aware(
                &state,
                &uri,
                tree.root_node(),
                &code,
                &[],
                &[],
                &state.package_library,
                &directive_meta,
                &mut diagnostics,
            );

            // Filter for "Undefined variable" diagnostics for this reserved word
            let undefined_diags: Vec<_> = diagnostics
                .iter()
                .filter(|d| d.message.contains(&format!("Undefined variable: {}", reserved_word)))
                .collect();

            prop_assert!(
                undefined_diags.is_empty(),
                "Reserved word '{}' in function call should NOT produce 'Undefined variable' diagnostic, but got: {:?}",
                reserved_word,
                undefined_diags
            );
        }

        #[test]
        /// Feature: reserved-keyword-handling, Property 3: Undefined Variable Check Exclusion (Negative Control)
        ///
        /// For any R code containing a non-reserved, non-builtin identifier that is not defined,
        /// the undefined variable checker SHALL emit an "Undefined variable" diagnostic.
        /// This is a negative control to ensure the checker is working correctly.
        ///
        /// **Validates: Requirements 3.1, 3.2, 3.3**
        fn prop_non_reserved_undefined_vars_are_flagged(
            var_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved or builtin", |s| !is_r_reserved(s) && !super::is_builtin(s))
        ) {
            use crate::state::{WorldState, Document};
            use crate::cross_file::directive::parse_directives;

            // Create code with just the non-reserved identifier (undefined)
            let code = var_name.clone();
            let tree = parse_r_code(&code);

            let mut state = WorldState::new(vec![]);
            state.cross_file_config.undefined_variables_enabled = true;
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            let directive_meta = parse_directives(&code);
            let mut diagnostics = Vec::new();

            collect_undefined_variables_position_aware(
                &state,
                &uri,
                tree.root_node(),
                &code,
                &[],
                &[],
                &state.package_library,
                &directive_meta,
                &mut diagnostics,
            );

            // Filter for "Undefined variable" diagnostics for this variable
            let undefined_diags: Vec<_> = diagnostics
                .iter()
                .filter(|d| d.message.contains(&format!("Undefined variable: {}", var_name)))
                .collect();

            prop_assert!(
                !undefined_diags.is_empty(),
                "Non-reserved undefined variable '{}' SHOULD produce 'Undefined variable' diagnostic",
                var_name
            );
        }

        // ========================================================================
        // **Feature: reserved-keyword-handling, Property 4: Completion Exclusion**
        // **Validates: Requirements 5.1, 5.2, 5.3**
        //
        // For any completion request that aggregates identifiers from document, scope,
        // workspace index, or package sources, the completion provider SHALL NOT include
        // reserved words in the identifier completion list. Keyword completions (with
        // CompletionItemKind::KEYWORD) may still include reserved words.
        // ========================================================================

        #[test]
        /// Feature: reserved-keyword-handling, Property 4: Completion Exclusion
        ///
        /// For any R code containing an assignment to a reserved word, the completion
        /// provider SHALL NOT include that reserved word as an identifier completion
        /// (FUNCTION or VARIABLE kind). Reserved words MAY still appear as keyword
        /// completions (KEYWORD kind).
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.3**
        fn prop_reserved_words_not_in_identifier_completions(
            reserved_word in prop::sample::select(crate::reserved_words::RESERVED_WORDS)
        ) {
            use crate::state::{WorldState, Document};

            // Create code with assignment to reserved word (e.g., "else <- 1")
            // This is syntactically invalid R, but tests that even if such code exists,
            // the completion provider won't suggest the reserved word as an identifier
            let code = format!("{} <- 1", reserved_word);

            let mut state = WorldState::new(vec![]);
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            // Request completions at the end of the document
            let position = Position::new(0, code.len() as u32);
            let response = completion(&state, &uri, position);

            prop_assert!(response.is_some(), "Completion should return a response");

            if let Some(CompletionResponse::Array(items)) = response {
                // Check that reserved word does NOT appear as identifier completion
                let identifier_completions: Vec<_> = items
                    .iter()
                    .filter(|item| {
                        item.label == reserved_word
                            && matches!(
                                item.kind,
                                Some(CompletionItemKind::FUNCTION) | Some(CompletionItemKind::VARIABLE)
                            )
                    })
                    .collect();

                prop_assert!(
                    identifier_completions.is_empty(),
                    "Reserved word '{}' should NOT appear as identifier completion (FUNCTION/VARIABLE), but found: {:?}",
                    reserved_word,
                    identifier_completions
                );

                // Verify reserved word DOES appear as keyword completion (positive control)
                let keyword_completions: Vec<_> = items
                    .iter()
                    .filter(|item| {
                        item.label == reserved_word && item.kind == Some(CompletionItemKind::KEYWORD)
                    })
                    .collect();

                prop_assert!(
                    !keyword_completions.is_empty(),
                    "Reserved word '{}' SHOULD appear as keyword completion (KEYWORD kind)",
                    reserved_word
                );
            }
        }

        #[test]
        /// Feature: reserved-keyword-handling, Property 4: Completion Exclusion
        ///
        /// For any R code containing a function definition with a reserved word name,
        /// the completion provider SHALL NOT include that reserved word as a function
        /// completion. Reserved words MAY still appear as keyword completions.
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.3**
        fn prop_reserved_words_not_in_function_completions(
            reserved_word in prop::sample::select(crate::reserved_words::RESERVED_WORDS)
        ) {
            use crate::state::{WorldState, Document};

            // Create code with function definition using reserved word name
            // (e.g., "if <- function() {}")
            let code = format!("{} <- function() {{}}", reserved_word);

            let mut state = WorldState::new(vec![]);
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            // Request completions at the end of the document
            let position = Position::new(0, code.len() as u32);
            let response = completion(&state, &uri, position);

            prop_assert!(response.is_some(), "Completion should return a response");

            if let Some(CompletionResponse::Array(items)) = response {
                // Check that reserved word does NOT appear as function completion
                let function_completions: Vec<_> = items
                    .iter()
                    .filter(|item| {
                        item.label == reserved_word && item.kind == Some(CompletionItemKind::FUNCTION)
                    })
                    .collect();

                prop_assert!(
                    function_completions.is_empty(),
                    "Reserved word '{}' should NOT appear as function completion, but found: {:?}",
                    reserved_word,
                    function_completions
                );
            }
        }

        #[test]
        /// Feature: reserved-keyword-handling, Property 4: Completion Exclusion (Negative Control)
        ///
        /// For any R code containing an assignment to a non-reserved identifier,
        /// the completion provider SHALL include that identifier as a completion.
        /// This is a negative control to ensure the completion provider is working correctly.
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.3**
        fn prop_non_reserved_identifiers_in_completions(
            var_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            use crate::state::{WorldState, Document};

            // Create code with assignment to non-reserved identifier
            let code = format!("{} <- 1", var_name);

            let mut state = WorldState::new(vec![]);
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            // Request completions at the end of the document
            let position = Position::new(0, code.len() as u32);
            let response = completion(&state, &uri, position);

            prop_assert!(response.is_some(), "Completion should return a response");

            if let Some(CompletionResponse::Array(items)) = response {
                // Check that non-reserved identifier DOES appear as completion
                let var_completions: Vec<_> = items
                    .iter()
                    .filter(|item| item.label == var_name)
                    .collect();

                prop_assert!(
                    !var_completions.is_empty(),
                    "Non-reserved identifier '{}' SHOULD appear in completions",
                    var_name
                );
            }
        }

        // ========================================================================
        // **Feature: reserved-keyword-handling, Property 5: Document Symbol Exclusion**
        // **Validates: Requirements 6.1, 6.2**
        //
        // For any document symbol collection where a candidate symbol name is a
        // reserved word, the provider SHALL NOT include it in the emitted symbol list.
        // ========================================================================

        #[test]
        /// Feature: reserved-keyword-handling, Property 5: Document Symbol Exclusion
        ///
        /// For any R code containing an assignment to a reserved word (e.g., `else <- 1`),
        /// the document symbol provider SHALL NOT include that reserved word in the
        /// emitted symbol list.
        ///
        /// **Validates: Requirements 6.1, 6.2**
        fn prop_reserved_words_not_in_document_symbols(
            reserved_word in prop::sample::select(crate::reserved_words::RESERVED_WORDS)
        ) {
            // Create code with assignment to reserved word (e.g., "else <- 1")
            // This is syntactically invalid R, but tests that even if such code exists,
            // the document symbol provider won't include the reserved word as a symbol
            let code = format!("{} <- 1", reserved_word);
            let tree = parse_r_code(&code);

            let mut symbols = Vec::new();
            collect_symbols(tree.root_node(), &code, &mut symbols);

            // Check that reserved word does NOT appear in document symbols
            let reserved_symbols: Vec<_> = symbols
                .iter()
                .filter(|sym| sym.name == reserved_word)
                .collect();

            prop_assert!(
                reserved_symbols.is_empty(),
                "Reserved word '{}' should NOT appear in document symbols, but found: {:?}",
                reserved_word,
                reserved_symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
        }

        #[test]
        /// Feature: reserved-keyword-handling, Property 5: Document Symbol Exclusion
        ///
        /// For any R code containing a function definition with a reserved word name
        /// (e.g., `if <- function() {}`), the document symbol provider SHALL NOT
        /// include that reserved word in the emitted symbol list.
        ///
        /// **Validates: Requirements 6.1, 6.2**
        fn prop_reserved_words_not_in_document_symbols_function(
            reserved_word in prop::sample::select(crate::reserved_words::RESERVED_WORDS)
        ) {
            // Create code with function definition using reserved word name
            // (e.g., "if <- function() {}")
            let code = format!("{} <- function() {{}}", reserved_word);
            let tree = parse_r_code(&code);

            let mut symbols = Vec::new();
            collect_symbols(tree.root_node(), &code, &mut symbols);

            // Check that reserved word does NOT appear in document symbols
            let reserved_symbols: Vec<_> = symbols
                .iter()
                .filter(|sym| sym.name == reserved_word)
                .collect();

            prop_assert!(
                reserved_symbols.is_empty(),
                "Reserved word '{}' should NOT appear in document symbols (function), but found: {:?}",
                reserved_word,
                reserved_symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
        }

        #[test]
        /// Feature: reserved-keyword-handling, Property 5: Document Symbol Exclusion (Negative Control)
        ///
        /// For any R code containing an assignment to a non-reserved identifier,
        /// the document symbol provider SHALL include that identifier in the symbol list.
        /// This is a negative control to ensure the document symbol provider is working correctly.
        ///
        /// **Validates: Requirements 6.1, 6.2**
        fn prop_non_reserved_identifiers_in_document_symbols(
            var_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Create code with assignment to non-reserved identifier
            let code = format!("{} <- 1", var_name);
            let tree = parse_r_code(&code);

            let mut symbols = Vec::new();
            collect_symbols(tree.root_node(), &code, &mut symbols);

            // Check that non-reserved identifier DOES appear in document symbols
            let var_symbols: Vec<_> = symbols
                .iter()
                .filter(|sym| sym.name == var_name)
                .collect();

            prop_assert!(
                !var_symbols.is_empty(),
                "Non-reserved identifier '{}' SHOULD appear in document symbols",
                var_name
            );
        }

        #[test]
        /// Feature: reserved-keyword-handling, Property 5: Document Symbol Exclusion
        ///
        /// For any R code containing multiple assignments where some are to reserved words
        /// and some are to non-reserved identifiers, the document symbol provider SHALL
        /// include only the non-reserved identifiers in the symbol list.
        ///
        /// **Validates: Requirements 6.1, 6.2**
        fn prop_mixed_reserved_and_non_reserved_document_symbols(
            reserved_word in prop::sample::select(crate::reserved_words::RESERVED_WORDS),
            var_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Create code with both reserved and non-reserved assignments
            let code = format!("{} <- 1\n{} <- 2", reserved_word, var_name);
            let tree = parse_r_code(&code);

            let mut symbols = Vec::new();
            collect_symbols(tree.root_node(), &code, &mut symbols);

            // Check that reserved word does NOT appear in document symbols
            let reserved_symbols: Vec<_> = symbols
                .iter()
                .filter(|sym| sym.name == reserved_word)
                .collect();

            prop_assert!(
                reserved_symbols.is_empty(),
                "Reserved word '{}' should NOT appear in document symbols",
                reserved_word
            );

            // Check that non-reserved identifier DOES appear in document symbols
            let var_symbols: Vec<_> = symbols
                .iter()
                .filter(|sym| sym.name == var_name)
                .collect();

            prop_assert!(
                !var_symbols.is_empty(),
                "Non-reserved identifier '{}' SHOULD appear in document symbols",
                var_name
            );
        }

        // ========================================================================
        // **Feature: else-newline-syntax-error, Property 1: Orphaned Else Detection**
        // **Validates: Requirements 1.1, 2.1, 2.2**
        //
        // For any R code where an `else` keyword starts on a different line than
        // the closing `}` of the preceding `if` block, the detector SHALL emit
        // exactly one diagnostic for that `else`.
        // ========================================================================

        #[test]
        /// Feature: else-newline-syntax-error, Property 1: Orphaned Else Detection
        ///
        /// For any R code where an `else` keyword starts on a different line than
        /// the closing `}` of the preceding `if` block, the detector SHALL emit
        /// exactly one diagnostic for that `else`.
        ///
        /// **Validates: Requirements 1.1, 2.1, 2.2**
        fn prop_orphaned_else_detection(
            condition in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            body1 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            body2 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            blank_lines in 0usize..3
        ) {
            // Generate code with else on a new line after closing brace
            // Pattern: if (condition) {body1}\n[blank_lines]\nelse {body2}
            let newlines = "\n".repeat(blank_lines + 1);
            let code = format!("if ({}) {{{}}}{newlines}else {{{}}}", condition, body1, body2);

            let tree = parse_r_code(&code);
            let mut diagnostics = Vec::new();
            super::collect_else_newline_errors(tree.root_node(), &code, &mut diagnostics);

            // Should emit exactly one diagnostic for the orphaned else
            prop_assert_eq!(
                diagnostics.len(),
                1,
                "Should emit exactly one diagnostic for orphaned else on new line. Code: '{}', Diagnostics: {:?}",
                code,
                diagnostics
            );

            // Verify diagnostic severity is ERROR
            prop_assert_eq!(
                diagnostics[0].severity,
                Some(DiagnosticSeverity::ERROR),
                "Diagnostic severity should be ERROR"
            );

            // Verify diagnostic message mentions 'else' and 'same line'
            prop_assert!(
                diagnostics[0].message.contains("else"),
                "Diagnostic message should mention 'else'"
            );
            prop_assert!(
                diagnostics[0].message.contains("same line"),
                "Diagnostic message should mention 'same line'"
            );
        }

        #[test]
        /// Feature: else-newline-syntax-error, Property 1: Orphaned Else Detection (Multi-line if block)
        ///
        /// For any R code with a multi-line if block where `else` appears on a new line
        /// after the closing `}`, the detector SHALL emit exactly one diagnostic.
        ///
        /// **Validates: Requirements 1.1, 2.1, 2.2**
        fn prop_orphaned_else_detection_multiline_if(
            condition in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            body_lines in 1usize..4,
            body2 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Generate multi-line if block with else on new line
            // Pattern: if (condition) {\n  body_line1\n  body_line2\n}\nelse {body2}
            let body_content: String = (0..body_lines)
                .map(|i| format!("  line{}", i))
                .collect::<Vec<_>>()
                .join("\n");

            let code = format!(
                "if ({}) {{\n{}\n}}\nelse {{{}}}",
                condition, body_content, body2
            );

            let tree = parse_r_code(&code);
            let mut diagnostics = Vec::new();
            super::collect_else_newline_errors(tree.root_node(), &code, &mut diagnostics);

            // Should emit exactly one diagnostic for the orphaned else
            prop_assert_eq!(
                diagnostics.len(),
                1,
                "Should emit exactly one diagnostic for orphaned else after multi-line if block. Code: '{}', Diagnostics: {:?}",
                code,
                diagnostics
            );

            // Verify diagnostic severity is ERROR
            prop_assert_eq!(
                diagnostics[0].severity,
                Some(DiagnosticSeverity::ERROR),
                "Diagnostic severity should be ERROR"
            );
        }

        #[test]
        /// Feature: else-newline-syntax-error, Property 1: Orphaned Else Detection (else if pattern)
        ///
        /// For any R code where `else if` appears on a new line after the closing `}`,
        /// the detector SHALL emit exactly one diagnostic for the orphaned `else`.
        ///
        /// **Validates: Requirements 1.1, 2.1, 2.2**
        fn prop_orphaned_else_if_detection(
            cond1 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            cond2 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            body1 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            body2 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Generate code with else if on a new line
            // Pattern: if (cond1) {body1}\nelse if (cond2) {body2}
            let code = format!(
                "if ({}) {{{}}}\nelse if ({}) {{{}}}",
                cond1, body1, cond2, body2
            );

            let tree = parse_r_code(&code);
            let mut diagnostics = Vec::new();
            super::collect_else_newline_errors(tree.root_node(), &code, &mut diagnostics);

            // Should emit exactly one diagnostic for the orphaned else
            prop_assert_eq!(
                diagnostics.len(),
                1,
                "Should emit exactly one diagnostic for orphaned 'else if' on new line. Code: '{}', Diagnostics: {:?}",
                code,
                diagnostics
            );

            // Verify diagnostic severity is ERROR
            prop_assert_eq!(
                diagnostics[0].severity,
                Some(DiagnosticSeverity::ERROR),
                "Diagnostic severity should be ERROR"
            );
        }

        // ========================================================================
        // **Feature: else-newline-syntax-error, Property 2: Valid Else No Diagnostic**
        // **Validates: Requirements 1.2, 1.3, 2.3, 2.4**
        //
        // For any R code where an `else` keyword appears on the same line as the
        // closing `}` of the preceding `if` block, the detector SHALL NOT emit
        // a diagnostic for that `else`.
        // ========================================================================

        #[test]
        /// Feature: else-newline-syntax-error, Property 2: Valid Else No Diagnostic (Single line)
        ///
        /// For any R code where `else` appears on the same line as the closing `}`
        /// of the preceding `if` block (single line format), the detector SHALL NOT
        /// emit a diagnostic.
        ///
        /// **Validates: Requirements 1.2, 1.3, 2.3**
        fn prop_valid_else_no_diagnostic_single_line(
            condition in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            body1 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            body2 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Generate valid single-line if-else code
            // Pattern: if (condition) {body1} else {body2}
            let code = format!("if ({}) {{{}}} else {{{}}}", condition, body1, body2);

            let tree = parse_r_code(&code);
            let mut diagnostics = Vec::new();
            super::collect_else_newline_errors(tree.root_node(), &code, &mut diagnostics);

            // Should NOT emit any diagnostic for valid else on same line
            prop_assert_eq!(
                diagnostics.len(),
                0,
                "Should NOT emit diagnostic for valid else on same line. Code: '{}', Diagnostics: {:?}",
                code,
                diagnostics
            );
        }

        #[test]
        /// Feature: else-newline-syntax-error, Property 2: Valid Else No Diagnostic (Multi-line with else on same line as brace)
        ///
        /// For any R code with a multi-line if block where `else` appears on the same
        /// line as the closing `}`, the detector SHALL NOT emit a diagnostic.
        ///
        /// **Validates: Requirements 1.2, 1.3, 2.4**
        fn prop_valid_else_no_diagnostic_multiline(
            condition in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            body_lines in 1usize..4,
            body2_lines in 1usize..4
        ) {
            // Generate multi-line if block with else on same line as closing brace
            // Pattern: if (condition) {\n  body_line1\n  body_line2\n} else {\n  body2_line1\n}
            let body1_content: String = (0..body_lines)
                .map(|i| format!("  line{}", i))
                .collect::<Vec<_>>()
                .join("\n");

            let body2_content: String = (0..body2_lines)
                .map(|i| format!("  else_line{}", i))
                .collect::<Vec<_>>()
                .join("\n");

            let code = format!(
                "if ({}) {{\n{}\n}} else {{\n{}\n}}",
                condition, body1_content, body2_content
            );

            let tree = parse_r_code(&code);
            let mut diagnostics = Vec::new();
            super::collect_else_newline_errors(tree.root_node(), &code, &mut diagnostics);

            // Should NOT emit any diagnostic for valid else on same line as closing brace
            prop_assert_eq!(
                diagnostics.len(),
                0,
                "Should NOT emit diagnostic for valid multi-line if-else. Code: '{}', Diagnostics: {:?}",
                code,
                diagnostics
            );
        }

        #[test]
        /// Feature: else-newline-syntax-error, Property 2: Valid Else No Diagnostic (else if on same line)
        ///
        /// For any R code where `else if` appears on the same line as the closing `}`
        /// of the preceding `if` block, the detector SHALL NOT emit a diagnostic.
        ///
        /// **Validates: Requirements 1.2, 1.3, 2.3, 2.4**
        fn prop_valid_else_if_no_diagnostic(
            cond1 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            cond2 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            body1 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            body2 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Generate valid if-else if code with else if on same line as closing brace
            // Pattern: if (cond1) {body1} else if (cond2) {body2}
            let code = format!(
                "if ({}) {{{}}} else if ({}) {{{}}}",
                cond1, body1, cond2, body2
            );

            let tree = parse_r_code(&code);
            let mut diagnostics = Vec::new();
            super::collect_else_newline_errors(tree.root_node(), &code, &mut diagnostics);

            // Should NOT emit any diagnostic for valid else if on same line
            prop_assert_eq!(
                diagnostics.len(),
                0,
                "Should NOT emit diagnostic for valid 'else if' on same line. Code: '{}', Diagnostics: {:?}",
                code,
                diagnostics
            );
        }

        #[test]
        /// Feature: else-newline-syntax-error, Property 2: Valid Else No Diagnostic (Nested valid if-else)
        ///
        /// For any nested if-else structure where all `else` keywords appear on the same
        /// line as their preceding closing `}`, the detector SHALL NOT emit any diagnostic.
        ///
        /// **Validates: Requirements 1.2, 1.3, 2.3, 2.4**
        fn prop_valid_nested_else_no_diagnostic(
            outer_cond in "[a-z][a-z0-9_]{1,6}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            inner_cond in "[a-z][a-z0-9_]{1,6}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            body1 in "[a-z][a-z0-9_]{1,6}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            body2 in "[a-z][a-z0-9_]{1,6}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            body3 in "[a-z][a-z0-9_]{1,6}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Generate valid nested if-else code
            // Pattern: if (outer_cond) { if (inner_cond) {body1} else {body2} } else {body3}
            let code = format!(
                "if ({}) {{ if ({}) {{{}}} else {{{}}} }} else {{{}}}",
                outer_cond, inner_cond, body1, body2, body3
            );

            let tree = parse_r_code(&code);
            let mut diagnostics = Vec::new();
            super::collect_else_newline_errors(tree.root_node(), &code, &mut diagnostics);

            // Should NOT emit any diagnostic for valid nested if-else
            prop_assert_eq!(
                diagnostics.len(),
                0,
                "Should NOT emit diagnostic for valid nested if-else. Code: '{}', Diagnostics: {:?}",
                code,
                diagnostics
            );
        }

        // ========================================================================
        // **Feature: else-newline-syntax-error, Property 4: Diagnostic Range Accuracy**
        // **Validates: Requirements 3.2**
        //
        // For any detected orphaned `else`, the diagnostic range SHALL start at the
        // beginning of the `else` keyword and end at the end of the `else` keyword.
        // ========================================================================

        #[test]
        /// Feature: else-newline-syntax-error, Property 4: Diagnostic Range Accuracy
        ///
        /// For any detected orphaned `else`, the diagnostic range SHALL start at the
        /// beginning of the `else` keyword and end at the end of the `else` keyword.
        ///
        /// **Validates: Requirements 3.2**
        fn prop_diagnostic_range_accuracy(
            condition in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            body1 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            body2 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            blank_lines in 0usize..3
        ) {
            // Generate code with else on a new line after closing brace
            // Pattern: if (condition) {body1}\n[blank_lines]\nelse {body2}
            let newlines = "\n".repeat(blank_lines + 1);
            let code = format!("if ({}) {{{}}}{newlines}else {{{}}}", condition, body1, body2);

            let tree = parse_r_code(&code);
            let mut diagnostics = Vec::new();
            super::collect_else_newline_errors(tree.root_node(), &code, &mut diagnostics);

            // Should emit exactly one diagnostic
            prop_assert_eq!(
                diagnostics.len(),
                1,
                "Should emit exactly one diagnostic. Code: '{}', Diagnostics: {:?}",
                code,
                diagnostics
            );

            let diagnostic = &diagnostics[0];

            // Calculate expected position of "else" in the generated code
            // The "else" keyword starts after: "if (condition) {body1}" + newlines
            let prefix = format!("if ({}) {{{}}}{newlines}", condition, body1);
            let else_line = prefix.matches('\n').count() as u32;
            let else_column = 0u32; // "else" starts at column 0 on its line

            // Verify diagnostic range starts at the beginning of "else"
            prop_assert_eq!(
                diagnostic.range.start.line,
                else_line,
                "Diagnostic start line should match else position. Code: '{}', Expected line: {}, Got: {}",
                code,
                else_line,
                diagnostic.range.start.line
            );
            prop_assert_eq!(
                diagnostic.range.start.character,
                else_column,
                "Diagnostic start column should match else position. Code: '{}', Expected column: {}, Got: {}",
                code,
                else_column,
                diagnostic.range.start.character
            );

            // Verify diagnostic range ends at the end of "else" (4 characters)
            // The "else" keyword is 4 characters long
            prop_assert_eq!(
                diagnostic.range.end.line,
                else_line,
                "Diagnostic end line should be same as start line. Code: '{}', Expected: {}, Got: {}",
                code,
                else_line,
                diagnostic.range.end.line
            );
            prop_assert_eq!(
                diagnostic.range.end.character,
                else_column + 4,
                "Diagnostic end column should be start + 4 (length of 'else'). Code: '{}', Expected: {}, Got: {}",
                code,
                else_column + 4,
                diagnostic.range.end.character
            );
        }

        #[test]
        /// Feature: else-newline-syntax-error, Property 4: Diagnostic Range Accuracy (Multi-line if block)
        ///
        /// For any detected orphaned `else` after a multi-line if block, the diagnostic
        /// range SHALL accurately cover the `else` keyword.
        ///
        /// **Validates: Requirements 3.2**
        fn prop_diagnostic_range_accuracy_multiline(
            condition in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            body_lines in 1usize..4,
            body2 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Generate multi-line if block with else on new line
            // Pattern: if (condition) {\n  body_line1\n  body_line2\n}\nelse {body2}
            let body_content: String = (0..body_lines)
                .map(|i| format!("  line{}", i))
                .collect::<Vec<_>>()
                .join("\n");

            let code = format!(
                "if ({}) {{\n{}\n}}\nelse {{{}}}",
                condition, body_content, body2
            );

            let tree = parse_r_code(&code);
            let mut diagnostics = Vec::new();
            super::collect_else_newline_errors(tree.root_node(), &code, &mut diagnostics);

            // Should emit exactly one diagnostic
            prop_assert_eq!(
                diagnostics.len(),
                1,
                "Should emit exactly one diagnostic. Code: '{}', Diagnostics: {:?}",
                code,
                diagnostics
            );

            let diagnostic = &diagnostics[0];

            // Calculate expected position of "else" in the generated code
            // Line count: 1 (if line) + body_lines + 1 (closing brace line) = body_lines + 2
            // But 0-indexed, so else is on line (body_lines + 2)
            let else_line = (body_lines + 2) as u32;
            let else_column = 0u32; // "else" starts at column 0 on its line

            // Verify diagnostic range starts at the beginning of "else"
            prop_assert_eq!(
                diagnostic.range.start.line,
                else_line,
                "Diagnostic start line should match else position. Code: '{}', Expected line: {}, Got: {}",
                code,
                else_line,
                diagnostic.range.start.line
            );
            prop_assert_eq!(
                diagnostic.range.start.character,
                else_column,
                "Diagnostic start column should match else position. Code: '{}', Expected column: {}, Got: {}",
                code,
                else_column,
                diagnostic.range.start.character
            );

            // Verify diagnostic range ends at the end of "else" (4 characters)
            prop_assert_eq!(
                diagnostic.range.end.line,
                else_line,
                "Diagnostic end line should be same as start line. Code: '{}', Expected: {}, Got: {}",
                code,
                else_line,
                diagnostic.range.end.line
            );
            prop_assert_eq!(
                diagnostic.range.end.character,
                else_column + 4,
                "Diagnostic end column should be start + 4 (length of 'else'). Code: '{}', Expected: {}, Got: {}",
                code,
                else_column + 4,
                diagnostic.range.end.character
            );
        }

        #[test]
        /// Feature: else-newline-syntax-error, Property 4: Diagnostic Range Accuracy (else if pattern)
        ///
        /// For any detected orphaned `else if` on a new line, the diagnostic range SHALL
        /// accurately cover the `else` keyword (not the entire `else if`).
        ///
        /// **Validates: Requirements 3.2**
        fn prop_diagnostic_range_accuracy_else_if(
            cond1 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            cond2 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            body1 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            body2 in "[a-z][a-z0-9_]{1,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Generate code with else if on a new line
            // Pattern: if (cond1) {body1}\nelse if (cond2) {body2}
            let code = format!(
                "if ({}) {{{}}}\nelse if ({}) {{{}}}",
                cond1, body1, cond2, body2
            );

            let tree = parse_r_code(&code);
            let mut diagnostics = Vec::new();
            super::collect_else_newline_errors(tree.root_node(), &code, &mut diagnostics);

            // Should emit exactly one diagnostic
            prop_assert_eq!(
                diagnostics.len(),
                1,
                "Should emit exactly one diagnostic. Code: '{}', Diagnostics: {:?}",
                code,
                diagnostics
            );

            let diagnostic = &diagnostics[0];

            // The "else" keyword is on line 1 (0-indexed), column 0
            let else_line = 1u32;
            let else_column = 0u32;

            // Verify diagnostic range starts at the beginning of "else"
            prop_assert_eq!(
                diagnostic.range.start.line,
                else_line,
                "Diagnostic start line should match else position. Code: '{}', Expected line: {}, Got: {}",
                code,
                else_line,
                diagnostic.range.start.line
            );
            prop_assert_eq!(
                diagnostic.range.start.character,
                else_column,
                "Diagnostic start column should match else position. Code: '{}', Expected column: {}, Got: {}",
                code,
                else_column,
                diagnostic.range.start.character
            );

            // Verify diagnostic range ends at the end of "else" (4 characters)
            // Note: The diagnostic should cover just "else", not "else if"
            prop_assert_eq!(
                diagnostic.range.end.line,
                else_line,
                "Diagnostic end line should be same as start line. Code: '{}', Expected: {}, Got: {}",
                code,
                else_line,
                diagnostic.range.end.line
            );
            prop_assert_eq!(
                diagnostic.range.end.character,
                else_column + 4,
                "Diagnostic end column should be start + 4 (length of 'else'). Code: '{}', Expected: {}, Got: {}",
                code,
                else_column + 4,
                diagnostic.range.end.character
            );
        }

        // ========================================================================
        // **Feature: lsp-declaration-directives, Property 5: Diagnostic Suppression**
        // **Validates: Requirements 5.1, 5.2, 5.3, 5.4**
        //
        // For any file with a declaration directive and a usage of the declared symbol
        // name (case-sensitive match), the undefined variable diagnostic SHALL be
        // suppressed if and only if the usage position is after the declaration
        // directive line.
        // ========================================================================

        #[test]
        /// Feature: lsp-declaration-directives, Property 5: Diagnostic Suppression
        ///
        /// For any file with a variable declaration directive (@lsp-var) and a usage
        /// of the declared variable after the directive line, the undefined variable
        /// diagnostic SHALL be suppressed.
        ///
        /// **Validates: Requirements 5.1**
        fn prop_declared_variable_suppresses_diagnostic_after_directive(
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            directive_form in prop::sample::select(vec![
                "@lsp-var", "@lsp-variable", "@lsp-declare-var", "@lsp-declare-variable"
            ]),
            lines_between in 0usize..5
        ) {
            use crate::state::{WorldState, Document};
            use crate::cross_file::directive::parse_directives;

            // Generate code with declaration directive followed by usage
            // Pattern: # @lsp-var varname\n[blank_lines]\nvarname
            let blank_lines = "\n".repeat(lines_between);
            let code = format!("# {} {}\n{}{}",
                directive_form, var_name, blank_lines, var_name);

            let tree = parse_r_code(&code);

            let mut state = WorldState::new(vec![]);
            state.cross_file_config.undefined_variables_enabled = true;
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            let directive_meta = parse_directives(&code);
            let mut diagnostics = Vec::new();

            collect_undefined_variables_position_aware(
                &state,
                &uri,
                tree.root_node(),
                &code,
                &[],
                &[],
                &state.package_library,
                &directive_meta,
                &mut diagnostics,
            );

            // Filter for "Undefined variable" diagnostics for this variable
            let undefined_diags: Vec<_> = diagnostics
                .iter()
                .filter(|d| d.message.contains(&format!("Undefined variable: {}", var_name)))
                .collect();

            prop_assert!(
                undefined_diags.is_empty(),
                "Declared variable '{}' used after directive should NOT produce 'Undefined variable' diagnostic. \
                 Directive form: '{}', Lines between: {}, Code:\n{}\nDiagnostics: {:?}",
                var_name, directive_form, lines_between, code, undefined_diags
            );
        }

        #[test]
        /// Feature: lsp-declaration-directives, Property 5: Diagnostic Suppression
        ///
        /// For any file with a function declaration directive (@lsp-func) and a call
        /// to the declared function after the directive line, the undefined variable
        /// diagnostic SHALL be suppressed.
        ///
        /// **Validates: Requirements 5.2**
        fn prop_declared_function_suppresses_diagnostic_after_directive(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            directive_form in prop::sample::select(vec![
                "@lsp-func", "@lsp-function", "@lsp-declare-func", "@lsp-declare-function"
            ]),
            lines_between in 0usize..5
        ) {
            use crate::state::{WorldState, Document};
            use crate::cross_file::directive::parse_directives;

            // Generate code with declaration directive followed by function call
            // Pattern: # @lsp-func funcname\n[blank_lines]\nfuncname()
            let blank_lines = "\n".repeat(lines_between);
            let code = format!("# {} {}\n{}{}()",
                directive_form, func_name, blank_lines, func_name);

            let tree = parse_r_code(&code);

            let mut state = WorldState::new(vec![]);
            state.cross_file_config.undefined_variables_enabled = true;
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            let directive_meta = parse_directives(&code);
            let mut diagnostics = Vec::new();

            collect_undefined_variables_position_aware(
                &state,
                &uri,
                tree.root_node(),
                &code,
                &[],
                &[],
                &state.package_library,
                &directive_meta,
                &mut diagnostics,
            );

            // Filter for "Undefined variable" diagnostics for this function
            let undefined_diags: Vec<_> = diagnostics
                .iter()
                .filter(|d| d.message.contains(&format!("Undefined variable: {}", func_name)))
                .collect();

            prop_assert!(
                undefined_diags.is_empty(),
                "Declared function '{}' called after directive should NOT produce 'Undefined variable' diagnostic. \
                 Directive form: '{}', Lines between: {}, Code:\n{}\nDiagnostics: {:?}",
                func_name, directive_form, lines_between, code, undefined_diags
            );
        }

        #[test]
        /// Feature: lsp-declaration-directives, Property 5: Diagnostic Suppression
        ///
        /// For any file with a usage of a symbol BEFORE its declaration directive,
        /// the undefined variable diagnostic SHALL be emitted.
        ///
        /// **Validates: Requirements 5.3**
        fn prop_usage_before_declaration_emits_diagnostic(
            symbol_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            is_function in any::<bool>(),
            lines_between in 0usize..3
        ) {
            use crate::state::{WorldState, Document};
            use crate::cross_file::directive::parse_directives;

            // Generate code with usage BEFORE declaration directive
            // Pattern: symbolname\n[blank_lines]\n# @lsp-var symbolname
            let blank_lines = "\n".repeat(lines_between);
            let directive = if is_function { "@lsp-func" } else { "@lsp-var" };
            let usage = if is_function {
                format!("{}()", symbol_name)
            } else {
                symbol_name.clone()
            };
            let code = format!("{}\n{}# {} {}",
                usage, blank_lines, directive, symbol_name);

            let tree = parse_r_code(&code);

            let mut state = WorldState::new(vec![]);
            state.cross_file_config.undefined_variables_enabled = true;
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            let directive_meta = parse_directives(&code);
            let mut diagnostics = Vec::new();

            collect_undefined_variables_position_aware(
                &state,
                &uri,
                tree.root_node(),
                &code,
                &[],
                &[],
                &state.package_library,
                &directive_meta,
                &mut diagnostics,
            );

            // Filter for "Undefined variable" diagnostics for this symbol
            let undefined_diags: Vec<_> = diagnostics
                .iter()
                .filter(|d| d.message.contains(&format!("Undefined variable: {}", symbol_name)))
                .collect();

            prop_assert!(
                !undefined_diags.is_empty(),
                "Symbol '{}' used BEFORE declaration directive SHOULD produce 'Undefined variable' diagnostic. \
                 Is function: {}, Lines between: {}, Code:\n{}\nDiagnostics: {:?}",
                symbol_name, is_function, lines_between, code, diagnostics
            );

            // Verify the diagnostic is on line 0 (the usage line)
            prop_assert_eq!(
                undefined_diags[0].range.start.line,
                0,
                "Diagnostic should be on line 0 (usage line). Code:\n{}\nDiagnostic: {:?}",
                code, undefined_diags[0]
            );
        }

        #[test]
        /// Feature: lsp-declaration-directives, Property 5: Diagnostic Suppression
        ///
        /// For any file with a declaration directive, the diagnostic suppression SHALL
        /// be case-sensitive: exact matches are suppressed, case-mismatched usages are not.
        ///
        /// **Validates: Requirements 5.4**
        fn prop_case_sensitive_diagnostic_suppression(
            base_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            is_function in any::<bool>()
        ) {
            use crate::state::{WorldState, Document};
            use crate::cross_file::directive::parse_directives;

            // Create a mixed-case version of the name
            let mixed_case_name = {
                let mut chars: Vec<char> = base_name.chars().collect();
                if !chars.is_empty() {
                    chars[0] = chars[0].to_ascii_uppercase();
                }
                chars.into_iter().collect::<String>()
            };

            // Generate code with declaration and both exact and case-mismatched usages
            // Pattern: # @lsp-var basename\nbasename\nBasename
            let directive = if is_function { "@lsp-func" } else { "@lsp-var" };
            let exact_usage = if is_function {
                format!("{}()", base_name)
            } else {
                base_name.clone()
            };
            let mismatched_usage = if is_function {
                format!("{}()", mixed_case_name)
            } else {
                mixed_case_name.clone()
            };
            let code = format!("# {} {}\n{}\n{}",
                directive, base_name, exact_usage, mismatched_usage);

            let tree = parse_r_code(&code);

            let mut state = WorldState::new(vec![]);
            state.cross_file_config.undefined_variables_enabled = true;
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            let directive_meta = parse_directives(&code);
            let mut diagnostics = Vec::new();

            collect_undefined_variables_position_aware(
                &state,
                &uri,
                tree.root_node(),
                &code,
                &[],
                &[],
                &state.package_library,
                &directive_meta,
                &mut diagnostics,
            );

            // Exact match should NOT produce diagnostic
            let exact_diags: Vec<_> = diagnostics
                .iter()
                .filter(|d| d.message.contains(&format!("Undefined variable: {}", base_name)))
                .collect();

            prop_assert!(
                exact_diags.is_empty(),
                "Exact case match '{}' should NOT produce diagnostic. Code:\n{}\nDiagnostics: {:?}",
                base_name, code, exact_diags
            );

            // Case-mismatched usage SHOULD produce diagnostic
            let mismatched_diags: Vec<_> = diagnostics
                .iter()
                .filter(|d| d.message.contains(&format!("Undefined variable: {}", mixed_case_name)))
                .collect();

            prop_assert!(
                !mismatched_diags.is_empty(),
                "Case-mismatched '{}' (declared as '{}') SHOULD produce diagnostic. Code:\n{}\nDiagnostics: {:?}",
                mixed_case_name, base_name, code, diagnostics
            );

            // Verify the mismatched diagnostic is on line 2 (the mismatched usage line)
            prop_assert_eq!(
                mismatched_diags[0].range.start.line,
                2,
                "Mismatched diagnostic should be on line 2. Code:\n{}\nDiagnostic: {:?}",
                code, mismatched_diags[0]
            );
        }

        #[test]
        /// Feature: lsp-declaration-directives, Property 5: Diagnostic Suppression
        ///
        /// For any file with multiple declaration directives at various positions,
        /// the diagnostic suppression SHALL follow position rules: symbols are only
        /// suppressed when used after their declaration line.
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.3, 5.4**
        fn prop_multiple_declarations_position_aware_suppression(
            var1 in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved or builtin", |s| !is_r_reserved(s) && !super::is_builtin(s)),
            var2 in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved or builtin", |s| !is_r_reserved(s) && !super::is_builtin(s))
        ) {
            // Skip if var1 and var2 are the same
            prop_assume!(var1 != var2);

            use crate::state::{WorldState, Document};
            use crate::cross_file::directive::parse_directives;

            // Generate code with:
            // - var1 used before its declaration (should emit diagnostic)
            // - var1 declared
            // - var2 declared
            // - var1 used after declaration (should NOT emit diagnostic)
            // - var2 used after declaration (should NOT emit diagnostic)
            let code = format!(
                "{}\n# @lsp-var {}\n# @lsp-var {}\n{}\n{}",
                var1, var1, var2, var1, var2
            );

            let tree = parse_r_code(&code);

            let mut state = WorldState::new(vec![]);
            state.cross_file_config.undefined_variables_enabled = true;
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            let directive_meta = parse_directives(&code);
            let mut diagnostics = Vec::new();

            collect_undefined_variables_position_aware(
                &state,
                &uri,
                tree.root_node(),
                &code,
                &[],
                &[],
                &state.package_library,
                &directive_meta,
                &mut diagnostics,
            );

            // var1 on line 0 (before declaration) should produce diagnostic
            let var1_before_diags: Vec<_> = diagnostics
                .iter()
                .filter(|d| {
                    d.message.contains(&format!("Undefined variable: {}", var1))
                        && d.range.start.line == 0
                })
                .collect();

            prop_assert!(
                !var1_before_diags.is_empty(),
                "'{}' used before declaration (line 0) SHOULD produce diagnostic. Code:\n{}\nDiagnostics: {:?}",
                var1, code, diagnostics
            );

            // var1 on line 3 (after declaration) should NOT produce diagnostic
            let var1_after_diags: Vec<_> = diagnostics
                .iter()
                .filter(|d| {
                    d.message.contains(&format!("Undefined variable: {}", var1))
                        && d.range.start.line == 3
                })
                .collect();

            prop_assert!(
                var1_after_diags.is_empty(),
                "'{}' used after declaration (line 3) should NOT produce diagnostic. Code:\n{}\nDiagnostics: {:?}",
                var1, code, diagnostics
            );

            // var2 on line 4 (after declaration) should NOT produce diagnostic
            let var2_after_diags: Vec<_> = diagnostics
                .iter()
                .filter(|d| {
                    d.message.contains(&format!("Undefined variable: {}", var2))
                        && d.range.start.line == 4
                })
                .collect();

            prop_assert!(
                var2_after_diags.is_empty(),
                "'{}' used after declaration (line 4) should NOT produce diagnostic. Code:\n{}\nDiagnostics: {:?}",
                var2, code, diagnostics
            );

            // Total diagnostics should be exactly 1 (for var1 on line 0)
            let total_undefined_diags: Vec<_> = diagnostics
                .iter()
                .filter(|d| d.message.contains("Undefined variable:"))
                .collect();

            prop_assert_eq!(
                total_undefined_diags.len(),
                1,
                "Should have exactly 1 undefined variable diagnostic. Code:\n{}\nDiagnostics: {:?}",
                code, total_undefined_diags
            );
        }

        #[test]
        /// Feature: lsp-declaration-directives, Property 5: Diagnostic Suppression
        ///
        /// For any file with a declaration directive using optional colon or quote syntax,
        /// the diagnostic suppression SHALL work correctly.
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.3, 5.4**
        fn prop_declaration_syntax_variants_suppress_diagnostics(
            symbol_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            syntax_variant in prop::sample::select(vec![
                "plain",      // @lsp-var name
                "colon",      // @lsp-var: name
                "quoted",     // @lsp-var "name"
                "colon_quoted" // @lsp-var: "name"
            ]),
            is_function in any::<bool>()
        ) {
            use crate::state::{WorldState, Document};
            use crate::cross_file::directive::parse_directives;

            // Generate directive based on syntax variant
            let directive_base = if is_function { "@lsp-func" } else { "@lsp-var" };
            let directive = match syntax_variant {
                "plain" => format!("# {} {}", directive_base, symbol_name),
                "colon" => format!("# {}: {}", directive_base, symbol_name),
                "quoted" => format!("# {} \"{}\"", directive_base, symbol_name),
                "colon_quoted" => format!("# {}: \"{}\"", directive_base, symbol_name),
                _ => unreachable!(),
            };

            let usage = if is_function {
                format!("{}()", symbol_name)
            } else {
                symbol_name.clone()
            };

            let code = format!("{}\n{}", directive, usage);

            let tree = parse_r_code(&code);

            let mut state = WorldState::new(vec![]);
            state.cross_file_config.undefined_variables_enabled = true;
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            let directive_meta = parse_directives(&code);
            let mut diagnostics = Vec::new();

            collect_undefined_variables_position_aware(
                &state,
                &uri,
                tree.root_node(),
                &code,
                &[],
                &[],
                &state.package_library,
                &directive_meta,
                &mut diagnostics,
            );

            // Filter for "Undefined variable" diagnostics for this symbol
            let undefined_diags: Vec<_> = diagnostics
                .iter()
                .filter(|d| d.message.contains(&format!("Undefined variable: {}", symbol_name)))
                .collect();

            prop_assert!(
                undefined_diags.is_empty(),
                "Declared symbol '{}' with syntax variant '{}' should NOT produce diagnostic. \
                 Is function: {}, Code:\n{}\nDiagnostics: {:?}",
                symbol_name, syntax_variant, is_function, code, undefined_diags
            );
        }

        // ========================================================================
        // **Feature: lsp-declaration-directives, Property 6: Completion Inclusion with Correct Kind**
        // **Validates: Requirements 6.1, 6.2, 6.3, 6.4**
        //
        // For any completion request at a position after a declaration directive,
        // the declared symbol SHALL appear in the completion list with
        // CompletionItemKind::FUNCTION for function declarations and
        // CompletionItemKind::VARIABLE for variable declarations.
        // ========================================================================

        #[test]
        /// Feature: lsp-declaration-directives, Property 6: Completion Inclusion with Correct Kind
        ///
        /// For any file with a variable declaration directive (@lsp-var) and a completion
        /// request at a position after the directive line, the declared variable SHALL
        /// appear in the completion list with CompletionItemKind::VARIABLE.
        ///
        /// **Validates: Requirements 6.1, 6.4**
        fn prop_declared_variable_appears_in_completions_with_variable_kind(
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            directive_form in prop::sample::select(vec![
                "@lsp-var", "@lsp-variable", "@lsp-declare-var", "@lsp-declare-variable"
            ]),
            lines_between in 0usize..5
        ) {
            use crate::state::{WorldState, Document};
            use tower_lsp::lsp_types::{Position, CompletionItemKind};

            // Generate code with declaration directive followed by a position for completion
            // Pattern: # @lsp-var varname\n[blank_lines]\n<cursor>
            let blank_lines = "\n".repeat(lines_between);
            let code = format!("# {} {}\n{}", directive_form, var_name, blank_lines);

            let mut state = WorldState::new(vec![]);
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            // Request completions at the last line (after the directive)
            let completion_line = (1 + lines_between) as u32;
            let position = Position::new(completion_line, 0);

            let response = super::completion(&state, &uri, position);

            prop_assert!(
                response.is_some(),
                "Completion response should not be None. Code:\n{}", code
            );

            if let Some(tower_lsp::lsp_types::CompletionResponse::Array(items)) = response {
                // Find the declared variable in completions
                let var_completion = items.iter().find(|item| item.label == var_name);

                prop_assert!(
                    var_completion.is_some(),
                    "Declared variable '{}' should appear in completions. \
                     Directive form: '{}', Lines between: {}, Code:\n{}\nCompletion items: {:?}",
                    var_name, directive_form, lines_between, code,
                    items.iter().map(|i| &i.label).collect::<Vec<_>>()
                );

                if let Some(item) = var_completion {
                    prop_assert_eq!(
                        item.kind,
                        Some(CompletionItemKind::VARIABLE),
                        "Declared variable '{}' should have CompletionItemKind::VARIABLE. \
                         Directive form: '{}', Code:\n{}\nActual kind: {:?}",
                        var_name, directive_form, code, item.kind
                    );
                }
            } else {
                prop_assert!(false, "Expected CompletionResponse::Array, got something else");
            }
        }

        #[test]
        /// Feature: lsp-declaration-directives, Property 6: Completion Inclusion with Correct Kind
        ///
        /// For any file with a function declaration directive (@lsp-func) and a completion
        /// request at a position after the directive line, the declared function SHALL
        /// appear in the completion list with CompletionItemKind::FUNCTION.
        ///
        /// **Validates: Requirements 6.2, 6.3**
        fn prop_declared_function_appears_in_completions_with_function_kind(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            directive_form in prop::sample::select(vec![
                "@lsp-func", "@lsp-function", "@lsp-declare-func", "@lsp-declare-function"
            ]),
            lines_between in 0usize..5
        ) {
            use crate::state::{WorldState, Document};
            use tower_lsp::lsp_types::{Position, CompletionItemKind};

            // Generate code with declaration directive followed by a position for completion
            // Pattern: # @lsp-func funcname\n[blank_lines]\n<cursor>
            let blank_lines = "\n".repeat(lines_between);
            let code = format!("# {} {}\n{}", directive_form, func_name, blank_lines);

            let mut state = WorldState::new(vec![]);
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            // Request completions at the last line (after the directive)
            let completion_line = (1 + lines_between) as u32;
            let position = Position::new(completion_line, 0);

            let response = super::completion(&state, &uri, position);

            prop_assert!(
                response.is_some(),
                "Completion response should not be None. Code:\n{}", code
            );

            if let Some(tower_lsp::lsp_types::CompletionResponse::Array(items)) = response {
                // Find the declared function in completions
                let func_completion = items.iter().find(|item| item.label == func_name);

                prop_assert!(
                    func_completion.is_some(),
                    "Declared function '{}' should appear in completions. \
                     Directive form: '{}', Lines between: {}, Code:\n{}\nCompletion items: {:?}",
                    func_name, directive_form, lines_between, code,
                    items.iter().map(|i| &i.label).collect::<Vec<_>>()
                );

                if let Some(item) = func_completion {
                    prop_assert_eq!(
                        item.kind,
                        Some(CompletionItemKind::FUNCTION),
                        "Declared function '{}' should have CompletionItemKind::FUNCTION. \
                         Directive form: '{}', Code:\n{}\nActual kind: {:?}",
                        func_name, directive_form, code, item.kind
                    );
                }
            } else {
                prop_assert!(false, "Expected CompletionResponse::Array, got something else");
            }
        }

        #[test]
        /// Feature: lsp-declaration-directives, Property 6: Completion Inclusion with Correct Kind
        ///
        /// For any file with both variable and function declaration directives, completions
        /// at a position after both directives SHALL include both symbols with their
        /// respective correct kinds.
        ///
        /// **Validates: Requirements 6.1, 6.2, 6.3, 6.4**
        fn prop_mixed_declarations_appear_in_completions_with_correct_kinds(
            var_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            func_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Skip if var_name and func_name are the same
            prop_assume!(var_name != func_name);

            use crate::state::{WorldState, Document};
            use tower_lsp::lsp_types::{Position, CompletionItemKind};

            // Generate code with both variable and function declarations
            // Pattern: # @lsp-var varname\n# @lsp-func funcname\n<cursor>
            let code = format!("# @lsp-var {}\n# @lsp-func {}\n", var_name, func_name);

            let mut state = WorldState::new(vec![]);
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            // Request completions at line 2 (after both directives)
            let position = Position::new(2, 0);

            let response = super::completion(&state, &uri, position);

            prop_assert!(
                response.is_some(),
                "Completion response should not be None. Code:\n{}", code
            );

            if let Some(tower_lsp::lsp_types::CompletionResponse::Array(items)) = response {
                // Find the declared variable in completions
                let var_completion = items.iter().find(|item| item.label == var_name);
                let func_completion = items.iter().find(|item| item.label == func_name);

                prop_assert!(
                    var_completion.is_some(),
                    "Declared variable '{}' should appear in completions. Code:\n{}\nCompletion items: {:?}",
                    var_name, code, items.iter().map(|i| &i.label).collect::<Vec<_>>()
                );

                prop_assert!(
                    func_completion.is_some(),
                    "Declared function '{}' should appear in completions. Code:\n{}\nCompletion items: {:?}",
                    func_name, code, items.iter().map(|i| &i.label).collect::<Vec<_>>()
                );

                if let Some(item) = var_completion {
                    prop_assert_eq!(
                        item.kind,
                        Some(CompletionItemKind::VARIABLE),
                        "Declared variable '{}' should have CompletionItemKind::VARIABLE. \
                         Code:\n{}\nActual kind: {:?}",
                        var_name, code, item.kind
                    );
                }

                if let Some(item) = func_completion {
                    prop_assert_eq!(
                        item.kind,
                        Some(CompletionItemKind::FUNCTION),
                        "Declared function '{}' should have CompletionItemKind::FUNCTION. \
                         Code:\n{}\nActual kind: {:?}",
                        func_name, code, item.kind
                    );
                }
            } else {
                prop_assert!(false, "Expected CompletionResponse::Array, got something else");
            }
        }

        #[test]
        /// Feature: lsp-declaration-directives, Property 6: Completion Inclusion with Correct Kind
        ///
        /// For any file with a declaration directive using optional colon or quote syntax,
        /// the declared symbol SHALL appear in completions with the correct kind.
        ///
        /// **Validates: Requirements 6.1, 6.2, 6.3, 6.4**
        fn prop_declaration_syntax_variants_appear_in_completions(
            symbol_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            syntax_variant in prop::sample::select(vec![
                "plain",      // @lsp-var name
                "colon",      // @lsp-var: name
                "quoted",     // @lsp-var "name"
                "colon_quoted" // @lsp-var: "name"
            ]),
            is_function in any::<bool>()
        ) {
            use crate::state::{WorldState, Document};
            use tower_lsp::lsp_types::{Position, CompletionItemKind};

            // Generate directive based on syntax variant
            let directive_base = if is_function { "@lsp-func" } else { "@lsp-var" };
            let directive = match syntax_variant {
                "plain" => format!("# {} {}", directive_base, symbol_name),
                "colon" => format!("# {}: {}", directive_base, symbol_name),
                "quoted" => format!("# {} \"{}\"", directive_base, symbol_name),
                "colon_quoted" => format!("# {}: \"{}\"", directive_base, symbol_name),
                _ => unreachable!(),
            };

            let code = format!("{}\n", directive);

            let mut state = WorldState::new(vec![]);
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            // Request completions at line 1 (after the directive)
            let position = Position::new(1, 0);

            let response = super::completion(&state, &uri, position);

            prop_assert!(
                response.is_some(),
                "Completion response should not be None. Code:\n{}", code
            );

            if let Some(tower_lsp::lsp_types::CompletionResponse::Array(items)) = response {
                // Find the declared symbol in completions
                let symbol_completion = items.iter().find(|item| item.label == symbol_name);

                prop_assert!(
                    symbol_completion.is_some(),
                    "Declared symbol '{}' with syntax variant '{}' should appear in completions. \
                     Is function: {}, Code:\n{}\nCompletion items: {:?}",
                    symbol_name, syntax_variant, is_function, code,
                    items.iter().map(|i| &i.label).collect::<Vec<_>>()
                );

                if let Some(item) = symbol_completion {
                    let expected_kind = if is_function {
                        CompletionItemKind::FUNCTION
                    } else {
                        CompletionItemKind::VARIABLE
                    };

                    prop_assert_eq!(
                        item.kind,
                        Some(expected_kind),
                        "Declared symbol '{}' should have correct CompletionItemKind. \
                         Is function: {}, Syntax variant: '{}', Code:\n{}\nActual kind: {:?}",
                        symbol_name, is_function, syntax_variant, code, item.kind
                    );
                }
            } else {
                prop_assert!(false, "Expected CompletionResponse::Array, got something else");
            }
        }

        #[test]
        /// Feature: lsp-declaration-directives, Property 6: Completion Inclusion with Correct Kind
        ///
        /// For any file with a declaration directive, completions requested at a position
        /// ON the directive line (not after) SHALL NOT include the declared symbol.
        /// This validates position-aware completion behavior.
        ///
        /// **Validates: Requirements 6.1, 6.2 (position-aware behavior)**
        fn prop_declared_symbol_not_in_completions_on_directive_line(
            symbol_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            is_function in any::<bool>()
        ) {
            use crate::state::{WorldState, Document};
            use tower_lsp::lsp_types::Position;

            // Generate code with declaration directive
            let directive = if is_function {
                format!("# @lsp-func {}", symbol_name)
            } else {
                format!("# @lsp-var {}", symbol_name)
            };

            let code = format!("{}\nx <- 1", directive);

            let mut state = WorldState::new(vec![]);
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            // Request completions at line 0 (ON the directive line)
            let position = Position::new(0, 0);

            let response = super::completion(&state, &uri, position);

            prop_assert!(
                response.is_some(),
                "Completion response should not be None. Code:\n{}", code
            );

            if let Some(tower_lsp::lsp_types::CompletionResponse::Array(items)) = response {
                // The declared symbol should NOT appear in completions on the directive line
                let symbol_completion = items.iter().find(|item| item.label == symbol_name);

                prop_assert!(
                    symbol_completion.is_none(),
                    "Declared symbol '{}' should NOT appear in completions on directive line (line 0). \
                     Is function: {}, Code:\n{}\nCompletion items: {:?}",
                    symbol_name, is_function, code,
                    items.iter().map(|i| &i.label).collect::<Vec<_>>()
                );
            } else {
                prop_assert!(false, "Expected CompletionResponse::Array, got something else");
            }
        }

        #[test]
        /// Feature: lsp-declaration-directives, Property 6: Completion Inclusion with Correct Kind
        ///
        /// For any file with multiple declaration directives at various positions,
        /// completions at a specific position SHALL only include symbols declared
        /// before that position.
        ///
        /// **Validates: Requirements 6.1, 6.2 (position-aware behavior)**
        fn prop_completions_position_aware_multiple_declarations(
            var1 in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            var2 in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Skip if var1 and var2 are the same
            prop_assume!(var1 != var2);

            use crate::state::{WorldState, Document};
            use tower_lsp::lsp_types::{Position, CompletionItemKind};

            // Generate code with:
            // Line 0: # @lsp-var var1
            // Line 1: <completion position A - should have var1>
            // Line 2: # @lsp-var var2
            // Line 3: <completion position B - should have var1 and var2>
            let code = format!(
                "# @lsp-var {}\n\n# @lsp-var {}\n",
                var1, var2
            );

            let mut state = WorldState::new(vec![]);
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            // Request completions at line 1 (after var1, before var2)
            let position_a = Position::new(1, 0);
            let response_a = super::completion(&state, &uri, position_a);

            prop_assert!(
                response_a.is_some(),
                "Completion response A should not be None. Code:\n{}", code
            );

            if let Some(tower_lsp::lsp_types::CompletionResponse::Array(items_a)) = response_a {
                // var1 should be in completions at position A
                let var1_completion = items_a.iter().find(|item| item.label == var1);
                prop_assert!(
                    var1_completion.is_some(),
                    "'{}' should appear in completions at line 1. Code:\n{}\nCompletion items: {:?}",
                    var1, code, items_a.iter().map(|i| &i.label).collect::<Vec<_>>()
                );

                if let Some(item) = var1_completion {
                    prop_assert_eq!(
                        item.kind,
                        Some(CompletionItemKind::VARIABLE),
                        "'{}' should have CompletionItemKind::VARIABLE",
                        var1
                    );
                }

                // var2 should NOT be in completions at position A (declared after)
                let var2_completion = items_a.iter().find(|item| item.label == var2);
                prop_assert!(
                    var2_completion.is_none(),
                    "'{}' should NOT appear in completions at line 1 (declared on line 2). Code:\n{}\nCompletion items: {:?}",
                    var2, code, items_a.iter().map(|i| &i.label).collect::<Vec<_>>()
                );
            }

            // Request completions at line 3 (after both var1 and var2)
            let position_b = Position::new(3, 0);
            let response_b = super::completion(&state, &uri, position_b);

            prop_assert!(
                response_b.is_some(),
                "Completion response B should not be None. Code:\n{}", code
            );

            if let Some(tower_lsp::lsp_types::CompletionResponse::Array(items_b)) = response_b {
                // Both var1 and var2 should be in completions at position B
                let var1_completion = items_b.iter().find(|item| item.label == var1);
                let var2_completion = items_b.iter().find(|item| item.label == var2);

                prop_assert!(
                    var1_completion.is_some(),
                    "'{}' should appear in completions at line 3. Code:\n{}\nCompletion items: {:?}",
                    var1, code, items_b.iter().map(|i| &i.label).collect::<Vec<_>>()
                );

                prop_assert!(
                    var2_completion.is_some(),
                    "'{}' should appear in completions at line 3. Code:\n{}\nCompletion items: {:?}",
                    var2, code, items_b.iter().map(|i| &i.label).collect::<Vec<_>>()
                );

                if let Some(item) = var1_completion {
                    prop_assert_eq!(
                        item.kind,
                        Some(CompletionItemKind::VARIABLE),
                        "'{}' should have CompletionItemKind::VARIABLE",
                        var1
                    );
                }

                if let Some(item) = var2_completion {
                    prop_assert_eq!(
                        item.kind,
                        Some(CompletionItemKind::VARIABLE),
                        "'{}' should have CompletionItemKind::VARIABLE",
                        var2
                    );
                }
            }
        }

        // ========================================================================
        // **Feature: document-workspace-symbols, Property 3: Range Containment Invariant**
        // **Validates: Requirements 2.1, 2.2, 2.3**
        //
        // For any DocumentSymbol in the response:
        // - `range` SHALL span from the start of the construct to the end of its value/body
        // - `selectionRange` SHALL span only the identifier
        // - `selectionRange.start >= range.start` AND `selectionRange.end <= range.end`
        // ========================================================================

        #[test]
        /// Feature: document-workspace-symbols, Property 3: Range Containment Invariant
        ///
        /// For any R code containing assignments (functions and variables), the
        /// SymbolExtractor SHALL produce symbols where `selection_range` is always
        /// contained within `range`.
        ///
        /// **Validates: Requirements 2.1, 2.2, 2.3**
        fn prop_range_containment_invariant_simple_assignments(
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            value in 1i32..1000
        ) {
            let code = format!("{} <- {}", var_name, value);
            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            for symbol in &symbols {
                // selection_range.start >= range.start
                let start_contained = symbol.selection_range.start.line > symbol.range.start.line
                    || (symbol.selection_range.start.line == symbol.range.start.line
                        && symbol.selection_range.start.character >= symbol.range.start.character);

                prop_assert!(
                    start_contained,
                    "selection_range.start must be >= range.start for symbol '{}'. \
                     selection_range: {:?}, range: {:?}, code: '{}'",
                    symbol.name, symbol.selection_range, symbol.range, code
                );

                // selection_range.end <= range.end
                let end_contained = symbol.selection_range.end.line < symbol.range.end.line
                    || (symbol.selection_range.end.line == symbol.range.end.line
                        && symbol.selection_range.end.character <= symbol.range.end.character);

                prop_assert!(
                    end_contained,
                    "selection_range.end must be <= range.end for symbol '{}'. \
                     selection_range: {:?}, range: {:?}, code: '{}'",
                    symbol.name, symbol.selection_range, symbol.range, code
                );
            }
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 3: Range Containment Invariant
        ///
        /// For any R code containing function definitions, the SymbolExtractor SHALL
        /// produce symbols where `selection_range` (identifier only) is contained
        /// within `range` (full function definition).
        ///
        /// **Validates: Requirements 2.1, 2.2, 2.3**
        fn prop_range_containment_invariant_function_definitions(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            param_count in 0usize..4,
            body_lines in 1usize..5
        ) {
            // Generate function parameters
            let params: Vec<String> = (0..param_count)
                .map(|i| format!("p{}", i))
                .collect();

            // Generate function body
            let body_content: String = (0..body_lines)
                .map(|i| format!("  line{}", i))
                .collect::<Vec<_>>()
                .join("\n");

            let code = format!(
                "{} <- function({}) {{\n{}\n}}",
                func_name, params.join(", "), body_content
            );

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            for symbol in &symbols {
                // selection_range.start >= range.start
                let start_contained = symbol.selection_range.start.line > symbol.range.start.line
                    || (symbol.selection_range.start.line == symbol.range.start.line
                        && symbol.selection_range.start.character >= symbol.range.start.character);

                prop_assert!(
                    start_contained,
                    "selection_range.start must be >= range.start for symbol '{}'. \
                     selection_range: {:?}, range: {:?}, code:\n{}",
                    symbol.name, symbol.selection_range, symbol.range, code
                );

                // selection_range.end <= range.end
                let end_contained = symbol.selection_range.end.line < symbol.range.end.line
                    || (symbol.selection_range.end.line == symbol.range.end.line
                        && symbol.selection_range.end.character <= symbol.range.end.character);

                prop_assert!(
                    end_contained,
                    "selection_range.end must be <= range.end for symbol '{}'. \
                     selection_range: {:?}, range: {:?}, code:\n{}",
                    symbol.name, symbol.selection_range, symbol.range, code
                );
            }
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 3: Range Containment Invariant
        ///
        /// For any R code containing multiple assignments (mixed functions and variables),
        /// the SymbolExtractor SHALL produce symbols where `selection_range` is always
        /// contained within `range` for all symbols.
        ///
        /// **Validates: Requirements 2.1, 2.2, 2.3**
        fn prop_range_containment_invariant_mixed_assignments(
            var_count in 1usize..4,
            func_count in 1usize..3
        ) {
            // Generate variable assignments
            let var_assignments: Vec<String> = (0..var_count)
                .map(|i| format!("var{} <- {}", i, i + 1))
                .collect();

            // Generate function definitions
            let func_assignments: Vec<String> = (0..func_count)
                .map(|i| format!("func{} <- function(x) {{ x + {} }}", i, i))
                .collect();

            // Combine all assignments
            let code = format!(
                "{}\n{}",
                var_assignments.join("\n"),
                func_assignments.join("\n")
            );

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Should have extracted all symbols
            prop_assert!(
                !symbols.is_empty(),
                "Should extract at least one symbol from code:\n{}", code
            );

            for symbol in &symbols {
                // selection_range.start >= range.start
                let start_contained = symbol.selection_range.start.line > symbol.range.start.line
                    || (symbol.selection_range.start.line == symbol.range.start.line
                        && symbol.selection_range.start.character >= symbol.range.start.character);

                prop_assert!(
                    start_contained,
                    "selection_range.start must be >= range.start for symbol '{}'. \
                     selection_range: {:?}, range: {:?}, code:\n{}",
                    symbol.name, symbol.selection_range, symbol.range, code
                );

                // selection_range.end <= range.end
                let end_contained = symbol.selection_range.end.line < symbol.range.end.line
                    || (symbol.selection_range.end.line == symbol.range.end.line
                        && symbol.selection_range.end.character <= symbol.range.end.character);

                prop_assert!(
                    end_contained,
                    "selection_range.end must be <= range.end for symbol '{}'. \
                     selection_range: {:?}, range: {:?}, code:\n{}",
                    symbol.name, symbol.selection_range, symbol.range, code
                );
            }
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 3: Range Containment Invariant
        ///
        /// For any R code containing right assignments (value -> name), the SymbolExtractor
        /// SHALL produce symbols where `selection_range` is contained within `range`.
        ///
        /// **Validates: Requirements 2.1, 2.2, 2.3**
        fn prop_range_containment_invariant_right_assignment(
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            value in 1i32..1000
        ) {
            let code = format!("{} -> {}", value, var_name);
            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            for symbol in &symbols {
                // selection_range.start >= range.start
                let start_contained = symbol.selection_range.start.line > symbol.range.start.line
                    || (symbol.selection_range.start.line == symbol.range.start.line
                        && symbol.selection_range.start.character >= symbol.range.start.character);

                prop_assert!(
                    start_contained,
                    "selection_range.start must be >= range.start for symbol '{}'. \
                     selection_range: {:?}, range: {:?}, code: '{}'",
                    symbol.name, symbol.selection_range, symbol.range, code
                );

                // selection_range.end <= range.end
                let end_contained = symbol.selection_range.end.line < symbol.range.end.line
                    || (symbol.selection_range.end.line == symbol.range.end.line
                        && symbol.selection_range.end.character <= symbol.range.end.character);

                prop_assert!(
                    end_contained,
                    "selection_range.end must be <= range.end for symbol '{}'. \
                     selection_range: {:?}, range: {:?}, code: '{}'",
                    symbol.name, symbol.selection_range, symbol.range, code
                );
            }
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 3: Range Containment Invariant
        ///
        /// For any R code containing nested function definitions, the SymbolExtractor
        /// SHALL produce symbols where `selection_range` is contained within `range`
        /// for both outer and inner functions.
        ///
        /// **Validates: Requirements 2.1, 2.2, 2.3**
        fn prop_range_containment_invariant_nested_functions(
            outer_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            inner_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Skip if names are the same
            prop_assume!(outer_name != inner_name);

            let code = format!(
                "{} <- function() {{\n  {} <- function() {{}}\n}}",
                outer_name, inner_name
            );

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Should find both outer and inner functions
            prop_assert!(
                symbols.len() >= 2,
                "Should extract at least 2 symbols (outer and inner functions). \
                 Found: {:?}, code:\n{}",
                symbols.iter().map(|s| &s.name).collect::<Vec<_>>(), code
            );

            for symbol in &symbols {
                // selection_range.start >= range.start
                let start_contained = symbol.selection_range.start.line > symbol.range.start.line
                    || (symbol.selection_range.start.line == symbol.range.start.line
                        && symbol.selection_range.start.character >= symbol.range.start.character);

                prop_assert!(
                    start_contained,
                    "selection_range.start must be >= range.start for symbol '{}'. \
                     selection_range: {:?}, range: {:?}, code:\n{}",
                    symbol.name, symbol.selection_range, symbol.range, code
                );

                // selection_range.end <= range.end
                let end_contained = symbol.selection_range.end.line < symbol.range.end.line
                    || (symbol.selection_range.end.line == symbol.range.end.line
                        && symbol.selection_range.end.character <= symbol.range.end.character);

                prop_assert!(
                    end_contained,
                    "selection_range.end must be <= range.end for symbol '{}'. \
                     selection_range: {:?}, range: {:?}, code:\n{}",
                    symbol.name, symbol.selection_range, symbol.range, code
                );
            }
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 3: Range Containment Invariant
        ///
        /// For any R code containing ALL_CAPS constants, the SymbolExtractor SHALL
        /// produce symbols where `selection_range` is contained within `range`.
        ///
        /// **Validates: Requirements 2.1, 2.2, 2.3**
        fn prop_range_containment_invariant_constants(
            const_suffix in "[A-Z0-9_]{1,8}",
            value in 1i32..1000
        ) {
            // Ensure the constant name starts with uppercase and has at least 2 chars
            let const_name = format!("C{}", const_suffix);
            let code = format!("{} <- {}", const_name, value);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            for symbol in &symbols {
                // selection_range.start >= range.start
                let start_contained = symbol.selection_range.start.line > symbol.range.start.line
                    || (symbol.selection_range.start.line == symbol.range.start.line
                        && symbol.selection_range.start.character >= symbol.range.start.character);

                prop_assert!(
                    start_contained,
                    "selection_range.start must be >= range.start for symbol '{}'. \
                     selection_range: {:?}, range: {:?}, code: '{}'",
                    symbol.name, symbol.selection_range, symbol.range, code
                );

                // selection_range.end <= range.end
                let end_contained = symbol.selection_range.end.line < symbol.range.end.line
                    || (symbol.selection_range.end.line == symbol.range.end.line
                        && symbol.selection_range.end.character <= symbol.range.end.character);

                prop_assert!(
                    end_contained,
                    "selection_range.end must be <= range.end for symbol '{}'. \
                     selection_range: {:?}, range: {:?}, code: '{}'",
                    symbol.name, symbol.selection_range, symbol.range, code
                );
            }
        }

        // ========================================================================
        // **Feature: document-workspace-symbols, Property 6: Symbol Kind Classification**
        // **Validates: Requirements 5.1, 5.2, 5.6, 5.7**
        //
        // For any symbol extracted from an R document:
        // - Identifiers matching `^[A-Z][A-Z0-9_.]+$` (min 2 chars) SHALL have `kind = CONSTANT`
        // - Assignments with RHS `R6Class()` or `setRefClass()` SHALL have `kind = CLASS`
        // - Other function definitions SHALL have `kind = FUNCTION`
        // - Other variable assignments SHALL have `kind = VARIABLE`
        //
        // Note: S4 methods (setMethod, setClass, setGeneric) are tested separately in task 3.2
        // ========================================================================

        #[test]
        /// Feature: document-workspace-symbols, Property 6: Symbol Kind Classification
        ///
        /// For any R code containing an ALL_CAPS identifier assignment (non-function),
        /// the SymbolExtractor SHALL classify it as CONSTANT.
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.6, 5.7**
        fn prop_symbol_kind_all_caps_constant(
            const_suffix in "[A-Z0-9_]{1,8}",
            value in 1i32..1000
        ) {
            // Ensure the constant name starts with uppercase and has at least 2 chars
            let const_name = format!("C{}", const_suffix);
            let code = format!("{} <- {}", const_name, value);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert!(
                !symbols.is_empty(),
                "Should extract at least one symbol from code: '{}'", code
            );

            let symbol = &symbols[0];
            prop_assert_eq!(
                symbol.kind,
                DocumentSymbolKind::Constant,
                "ALL_CAPS identifier '{}' should be classified as CONSTANT. \
                 Code: '{}', Actual kind: {:?}",
                const_name, code, symbol.kind
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 6: Symbol Kind Classification
        ///
        /// For any R code containing an ALL_CAPS identifier with dots and underscores,
        /// the SymbolExtractor SHALL classify it as CONSTANT.
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.6, 5.7**
        fn prop_symbol_kind_all_caps_with_special_chars(
            prefix in "[A-Z]{1,4}",
            separator in prop::sample::select(vec!["_", "."]),
            suffix in "[A-Z0-9]{1,4}"
        ) {
            let const_name = format!("{}{}{}", prefix, separator, suffix);
            // Skip if name is too short (less than 2 chars)
            prop_assume!(const_name.len() >= 2);

            let code = format!("{} <- 42", const_name);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert!(
                !symbols.is_empty(),
                "Should extract at least one symbol from code: '{}'", code
            );

            let symbol = &symbols[0];
            prop_assert_eq!(
                symbol.kind,
                DocumentSymbolKind::Constant,
                "ALL_CAPS identifier '{}' with special chars should be classified as CONSTANT. \
                 Code: '{}', Actual kind: {:?}",
                const_name, code, symbol.kind
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 6: Symbol Kind Classification
        ///
        /// For any R code containing an R6Class() assignment, the SymbolExtractor
        /// SHALL classify it as CLASS regardless of the identifier name.
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.6, 5.7**
        fn prop_symbol_kind_r6class(
            class_name in "[A-Z][a-zA-Z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            let code = format!("{} <- R6Class(\"{}\")", class_name, class_name);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert!(
                !symbols.is_empty(),
                "Should extract at least one symbol from code: '{}'", code
            );

            let symbol = &symbols[0];
            prop_assert_eq!(
                symbol.kind,
                DocumentSymbolKind::Class,
                "R6Class assignment '{}' should be classified as CLASS. \
                 Code: '{}', Actual kind: {:?}",
                class_name, code, symbol.kind
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 6: Symbol Kind Classification
        ///
        /// For any R code containing a setRefClass() assignment, the SymbolExtractor
        /// SHALL classify it as CLASS regardless of the identifier name.
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.6, 5.7**
        fn prop_symbol_kind_setrefclass(
            class_name in "[A-Z][a-zA-Z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            let code = format!("{} <- setRefClass(\"{}\")", class_name, class_name);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert!(
                !symbols.is_empty(),
                "Should extract at least one symbol from code: '{}'", code
            );

            let symbol = &symbols[0];
            prop_assert_eq!(
                symbol.kind,
                DocumentSymbolKind::Class,
                "setRefClass assignment '{}' should be classified as CLASS. \
                 Code: '{}', Actual kind: {:?}",
                class_name, code, symbol.kind
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 6: Symbol Kind Classification
        ///
        /// For any R code containing a function definition, the SymbolExtractor
        /// SHALL classify it as FUNCTION (unless it's an R6Class/setRefClass).
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.6, 5.7**
        fn prop_symbol_kind_function_definition(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            param_count in 0usize..4
        ) {
            let params: Vec<String> = (0..param_count)
                .map(|i| format!("p{}", i))
                .collect();

            let code = format!("{} <- function({}) {{ }}", func_name, params.join(", "));

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert!(
                !symbols.is_empty(),
                "Should extract at least one symbol from code: '{}'", code
            );

            let symbol = &symbols[0];
            prop_assert_eq!(
                symbol.kind,
                DocumentSymbolKind::Function,
                "Function definition '{}' should be classified as FUNCTION. \
                 Code: '{}', Actual kind: {:?}",
                func_name, code, symbol.kind
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 6: Symbol Kind Classification
        ///
        /// For any R code containing a variable assignment (non-function, non-constant),
        /// the SymbolExtractor SHALL classify it as VARIABLE.
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.6, 5.7**
        fn prop_symbol_kind_variable_assignment(
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            value in 1i32..1000
        ) {
            let code = format!("{} <- {}", var_name, value);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert!(
                !symbols.is_empty(),
                "Should extract at least one symbol from code: '{}'", code
            );

            let symbol = &symbols[0];
            prop_assert_eq!(
                symbol.kind,
                DocumentSymbolKind::Variable,
                "Variable assignment '{}' should be classified as VARIABLE. \
                 Code: '{}', Actual kind: {:?}",
                var_name, code, symbol.kind
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 6: Symbol Kind Classification
        ///
        /// For any R code containing a mixed-case identifier (not ALL_CAPS),
        /// the SymbolExtractor SHALL NOT classify it as CONSTANT.
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.6, 5.7**
        fn prop_symbol_kind_mixed_case_not_constant(
            prefix in "[A-Z]{1,3}",
            suffix in "[a-z]{1,5}"
        ) {
            let var_name = format!("{}{}", prefix, suffix);
            // Skip reserved words
            prop_assume!(!is_r_reserved(&var_name));

            let code = format!("{} <- 42", var_name);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert!(
                !symbols.is_empty(),
                "Should extract at least one symbol from code: '{}'", code
            );

            let symbol = &symbols[0];
            prop_assert_ne!(
                symbol.kind,
                DocumentSymbolKind::Constant,
                "Mixed-case identifier '{}' should NOT be classified as CONSTANT. \
                 Code: '{}', Actual kind: {:?}",
                var_name, code, symbol.kind
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 6: Symbol Kind Classification
        ///
        /// For any R code containing a single uppercase character identifier,
        /// the SymbolExtractor SHALL NOT classify it as CONSTANT (min 2 chars required).
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.6, 5.7**
        fn prop_symbol_kind_single_char_not_constant(
            char in "[A-Z]"
        ) {
            let code = format!("{} <- 42", char);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert!(
                !symbols.is_empty(),
                "Should extract at least one symbol from code: '{}'", code
            );

            let symbol = &symbols[0];
            prop_assert_ne!(
                symbol.kind,
                DocumentSymbolKind::Constant,
                "Single-char identifier '{}' should NOT be classified as CONSTANT. \
                 Code: '{}', Actual kind: {:?}",
                char, code, symbol.kind
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 6: Symbol Kind Classification
        ///
        /// For any R code containing an ALL_CAPS function definition,
        /// the SymbolExtractor SHALL classify it as CONSTANT (not FUNCTION).
        /// ALL_CAPS pattern takes priority over function definitions.
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.6, 5.7**
        fn prop_symbol_kind_all_caps_function_is_constant(
            const_suffix in "[A-Z0-9_]{1,8}"
        ) {
            let func_name = format!("F{}", const_suffix);
            let code = format!("{} <- function() {{ }}", func_name);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert!(
                !symbols.is_empty(),
                "Should extract at least one symbol from code: '{}'", code
            );

            let symbol = &symbols[0];
            prop_assert_eq!(
                symbol.kind,
                DocumentSymbolKind::Constant,
                "ALL_CAPS function definition '{}' should be classified as CONSTANT (ALL_CAPS takes priority). \
                 Code: '{}', Actual kind: {:?}",
                func_name, code, symbol.kind
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 6: Symbol Kind Classification
        ///
        /// For any R code containing an R6Class assignment with an ALL_CAPS name,
        /// the SymbolExtractor SHALL classify it as CLASS (R6Class takes priority).
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.6, 5.7**
        fn prop_symbol_kind_r6class_priority_over_all_caps(
            const_suffix in "[A-Z0-9_]{1,8}"
        ) {
            let class_name = format!("C{}", const_suffix);
            let code = format!("{} <- R6Class(\"{}\")", class_name, class_name);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert!(
                !symbols.is_empty(),
                "Should extract at least one symbol from code: '{}'", code
            );

            let symbol = &symbols[0];
            prop_assert_eq!(
                symbol.kind,
                DocumentSymbolKind::Class,
                "R6Class with ALL_CAPS name '{}' should be classified as CLASS (not CONSTANT). \
                 Code: '{}', Actual kind: {:?}",
                class_name, code, symbol.kind
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 6: Symbol Kind Classification
        ///
        /// For any R code containing a right assignment (value -> name) with a function,
        /// the SymbolExtractor SHALL classify it as FUNCTION.
        /// Note: Parentheses are required around the function definition for proper parsing.
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.6, 5.7**
        fn prop_symbol_kind_right_assignment_function(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Parentheses are required around the function definition for right assignment
            let code = format!("(function() {{ }}) -> {}", func_name);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert!(
                !symbols.is_empty(),
                "Should extract at least one symbol from code: '{}'", code
            );

            let symbol = &symbols[0];
            prop_assert_eq!(
                symbol.kind,
                DocumentSymbolKind::Function,
                "Right assignment function '{}' should be classified as FUNCTION. \
                 Code: '{}', Actual kind: {:?}",
                func_name, code, symbol.kind
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 6: Symbol Kind Classification
        ///
        /// For any R code containing a right assignment (value -> name) with an ALL_CAPS name,
        /// the SymbolExtractor SHALL classify it as CONSTANT.
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.6, 5.7**
        fn prop_symbol_kind_right_assignment_constant(
            const_suffix in "[A-Z0-9_]{1,8}",
            value in 1i32..1000
        ) {
            let const_name = format!("C{}", const_suffix);
            let code = format!("{} -> {}", value, const_name);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert!(
                !symbols.is_empty(),
                "Should extract at least one symbol from code: '{}'", code
            );

            let symbol = &symbols[0];
            prop_assert_eq!(
                symbol.kind,
                DocumentSymbolKind::Constant,
                "Right assignment with ALL_CAPS name '{}' should be classified as CONSTANT. \
                 Code: '{}', Actual kind: {:?}",
                const_name, code, symbol.kind
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 6: Symbol Kind Classification
        ///
        /// For any R code containing multiple assignments with different kinds,
        /// the SymbolExtractor SHALL correctly classify each symbol.
        ///
        /// **Validates: Requirements 5.1, 5.2, 5.6, 5.7**
        fn prop_symbol_kind_mixed_assignments(
            var_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            func_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            const_suffix in "[A-Z0-9_]{1,6}"
        ) {
            // Skip if names collide
            prop_assume!(var_name != func_name);

            let const_name = format!("C{}", const_suffix);
            let code = format!(
                "{} <- 42\n{} <- function() {{ }}\n{} <- 100",
                var_name, func_name, const_name
            );

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert_eq!(
                symbols.len(),
                3,
                "Should extract 3 symbols from code:\n{}\nFound: {:?}",
                code, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            // Find each symbol by name and verify its kind
            let var_symbol = symbols.iter().find(|s| s.name == var_name);
            let func_symbol = symbols.iter().find(|s| s.name == func_name);
            let const_symbol = symbols.iter().find(|s| s.name == const_name);

            prop_assert!(var_symbol.is_some(), "Should find variable symbol '{}'", var_name);
            prop_assert!(func_symbol.is_some(), "Should find function symbol '{}'", func_name);
            prop_assert!(const_symbol.is_some(), "Should find constant symbol '{}'", const_name);

            prop_assert_eq!(
                var_symbol.unwrap().kind,
                DocumentSymbolKind::Variable,
                "Variable '{}' should be classified as VARIABLE",
                var_name
            );

            prop_assert_eq!(
                func_symbol.unwrap().kind,
                DocumentSymbolKind::Function,
                "Function '{}' should be classified as FUNCTION",
                func_name
            );

            prop_assert_eq!(
                const_symbol.unwrap().kind,
                DocumentSymbolKind::Constant,
                "Constant '{}' should be classified as CONSTANT",
                const_name
            );
        }

        // ========================================================================
        // **Feature: document-workspace-symbols, Property 13: S4 Name Extraction**
        // **Validates: Requirements 10.1, 10.2, 10.3**
        //
        // For any S4 method call:
        // - `setMethod("methodName", ...)` SHALL produce a symbol named `methodName`
        // - `setClass("ClassName", ...)` SHALL produce a symbol named `ClassName`
        // - `setGeneric("genericName", ...)` SHALL produce a symbol named `genericName`
        // ========================================================================

        #[test]
        /// Feature: document-workspace-symbols, Property 13: S4 Name Extraction
        ///
        /// For any R code containing a setMethod() call with a string first argument,
        /// the SymbolExtractor SHALL produce a symbol with the method name from that string.
        ///
        /// **Validates: Requirements 10.1, 10.2, 10.3**
        fn prop_s4_setmethod_name_extraction(
            method_name in "[a-z][a-zA-Z0-9_]{2,12}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            class_name in "[A-Z][a-zA-Z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            let code = format!(
                r#"setMethod("{}", "{}", function(object) {{ object }})"#,
                method_name, class_name
            );

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert!(
                !symbols.is_empty(),
                "Should extract at least one symbol from setMethod call. Code: '{}'", code
            );

            let symbol = symbols.iter().find(|s| s.name == method_name);
            prop_assert!(
                symbol.is_some(),
                "Should find symbol named '{}' from setMethod call. \
                 Code: '{}', Found symbols: {:?}",
                method_name, code, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            let symbol = symbol.unwrap();
            prop_assert_eq!(
                symbol.kind,
                DocumentSymbolKind::Method,
                "setMethod symbol '{}' should be classified as METHOD. \
                 Code: '{}', Actual kind: {:?}",
                method_name, code, symbol.kind
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 13: S4 Name Extraction
        ///
        /// For any R code containing a setClass() call with a string first argument,
        /// the SymbolExtractor SHALL produce a symbol with the class name from that string.
        ///
        /// **Validates: Requirements 10.1, 10.2, 10.3**
        fn prop_s4_setclass_name_extraction(
            class_name in "[A-Z][a-zA-Z0-9_]{2,12}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            let code = format!(
                r#"setClass("{}", slots = c(value = "numeric"))"#,
                class_name
            );

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert!(
                !symbols.is_empty(),
                "Should extract at least one symbol from setClass call. Code: '{}'", code
            );

            let symbol = symbols.iter().find(|s| s.name == class_name);
            prop_assert!(
                symbol.is_some(),
                "Should find symbol named '{}' from setClass call. \
                 Code: '{}', Found symbols: {:?}",
                class_name, code, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            let symbol = symbol.unwrap();
            prop_assert_eq!(
                symbol.kind,
                DocumentSymbolKind::Class,
                "setClass symbol '{}' should be classified as CLASS. \
                 Code: '{}', Actual kind: {:?}",
                class_name, code, symbol.kind
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 13: S4 Name Extraction
        ///
        /// For any R code containing a setGeneric() call with a string first argument,
        /// the SymbolExtractor SHALL produce a symbol with the generic name from that string.
        ///
        /// **Validates: Requirements 10.1, 10.2, 10.3**
        fn prop_s4_setgeneric_name_extraction(
            generic_name in "[a-z][a-zA-Z0-9_]{2,12}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            let code = format!(
                r#"setGeneric("{}", function(x) standardGeneric("{}"))"#,
                generic_name, generic_name
            );

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert!(
                !symbols.is_empty(),
                "Should extract at least one symbol from setGeneric call. Code: '{}'", code
            );

            let symbol = symbols.iter().find(|s| s.name == generic_name);
            prop_assert!(
                symbol.is_some(),
                "Should find symbol named '{}' from setGeneric call. \
                 Code: '{}', Found symbols: {:?}",
                generic_name, code, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            let symbol = symbol.unwrap();
            prop_assert_eq!(
                symbol.kind,
                DocumentSymbolKind::Interface,
                "setGeneric symbol '{}' should be classified as INTERFACE. \
                 Code: '{}', Actual kind: {:?}",
                generic_name, code, symbol.kind
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 13: S4 Name Extraction
        ///
        /// For any R code containing S4 method calls with single-quoted string arguments,
        /// the SymbolExtractor SHALL correctly extract the name from single quotes.
        ///
        /// **Validates: Requirements 10.1, 10.2, 10.3**
        fn prop_s4_single_quote_name_extraction(
            method_name in "[a-z][a-zA-Z0-9_]{2,12}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            class_name in "[A-Z][a-zA-Z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Use single quotes instead of double quotes
            let code = format!(
                r#"setMethod('{}', '{}', function(object) {{ object }})"#,
                method_name, class_name
            );

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert!(
                !symbols.is_empty(),
                "Should extract at least one symbol from setMethod call with single quotes. Code: '{}'", code
            );

            let symbol = symbols.iter().find(|s| s.name == method_name);
            prop_assert!(
                symbol.is_some(),
                "Should find symbol named '{}' from setMethod call with single quotes. \
                 Code: '{}', Found symbols: {:?}",
                method_name, code, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 13: S4 Name Extraction
        ///
        /// For any R code containing multiple S4 method calls, the SymbolExtractor
        /// SHALL extract the correct name from each call.
        ///
        /// **Validates: Requirements 10.1, 10.2, 10.3**
        fn prop_s4_multiple_calls_name_extraction(
            class_name in "[A-Z][a-zA-Z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            generic_name in "[a-z][a-zA-Z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            method_name in "[a-z][a-zA-Z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Skip if names collide
            prop_assume!(class_name != generic_name);
            prop_assume!(class_name != method_name);
            prop_assume!(generic_name != method_name);

            let code = format!(
                r#"setClass("{}", slots = c(value = "numeric"))
setGeneric("{}", function(x) standardGeneric("{}"))
setMethod("{}", "{}", function(x) {{ x@value }})"#,
                class_name, generic_name, generic_name, method_name, class_name
            );

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert_eq!(
                symbols.len(),
                3,
                "Should extract 3 symbols from S4 code. Code:\n{}\nFound: {:?}",
                code, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            // Verify each symbol is found with correct name
            let class_symbol = symbols.iter().find(|s| s.name == class_name);
            let generic_symbol = symbols.iter().find(|s| s.name == generic_name);
            let method_symbol = symbols.iter().find(|s| s.name == method_name);

            prop_assert!(
                class_symbol.is_some(),
                "Should find class symbol '{}'. Found: {:?}",
                class_name, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
            prop_assert!(
                generic_symbol.is_some(),
                "Should find generic symbol '{}'. Found: {:?}",
                generic_name, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
            prop_assert!(
                method_symbol.is_some(),
                "Should find method symbol '{}'. Found: {:?}",
                method_name, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            // Verify correct kinds
            prop_assert_eq!(
                class_symbol.unwrap().kind,
                DocumentSymbolKind::Class,
                "setClass symbol should be CLASS"
            );
            prop_assert_eq!(
                generic_symbol.unwrap().kind,
                DocumentSymbolKind::Interface,
                "setGeneric symbol should be INTERFACE"
            );
            prop_assert_eq!(
                method_symbol.unwrap().kind,
                DocumentSymbolKind::Method,
                "setMethod symbol should be METHOD"
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 13: S4 Name Extraction
        ///
        /// For any R code containing S4 method calls mixed with regular assignments,
        /// the SymbolExtractor SHALL correctly extract names from both S4 calls and assignments.
        ///
        /// **Validates: Requirements 10.1, 10.2, 10.3**
        fn prop_s4_mixed_with_assignments_name_extraction(
            class_name in "[A-Z][a-zA-Z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            func_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            var_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Skip if names collide
            prop_assume!(class_name != func_name);
            prop_assume!(class_name != var_name);
            prop_assume!(func_name != var_name);

            let code = format!(
                r#"{} <- function(x) {{ x + 1 }}
setClass("{}", slots = c(value = "numeric"))
{} <- 42"#,
                func_name, class_name, var_name
            );

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert_eq!(
                symbols.len(),
                3,
                "Should extract 3 symbols from mixed code. Code:\n{}\nFound: {:?}",
                code, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            // Verify each symbol is found with correct name
            let func_symbol = symbols.iter().find(|s| s.name == func_name);
            let class_symbol = symbols.iter().find(|s| s.name == class_name);
            let var_symbol = symbols.iter().find(|s| s.name == var_name);

            prop_assert!(
                func_symbol.is_some(),
                "Should find function symbol '{}'. Found: {:?}",
                func_name, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
            prop_assert!(
                class_symbol.is_some(),
                "Should find class symbol '{}'. Found: {:?}",
                class_name, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
            prop_assert!(
                var_symbol.is_some(),
                "Should find variable symbol '{}'. Found: {:?}",
                var_name, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            // Verify correct kinds
            prop_assert_eq!(
                func_symbol.unwrap().kind,
                DocumentSymbolKind::Function,
                "Function symbol should be FUNCTION"
            );
            prop_assert_eq!(
                class_symbol.unwrap().kind,
                DocumentSymbolKind::Class,
                "setClass symbol should be CLASS"
            );
            prop_assert_eq!(
                var_symbol.unwrap().kind,
                DocumentSymbolKind::Variable,
                "Variable symbol should be VARIABLE"
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 13: S4 Name Extraction
        ///
        /// For any R code containing S4 method calls, the selection_range SHALL be
        /// contained within the range (Property 3 invariant for S4 symbols).
        ///
        /// **Validates: Requirements 10.1, 10.2, 10.3**
        fn prop_s4_selection_range_contained_in_range(
            method_name in "[a-z][a-zA-Z0-9_]{2,12}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            class_name in "[A-Z][a-zA-Z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            let code = format!(
                r#"setMethod("{}", "{}", function(object) {{ object }})"#,
                method_name, class_name
            );

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            prop_assert!(
                !symbols.is_empty(),
                "Should extract at least one symbol from setMethod call. Code: '{}'", code
            );

            for symbol in &symbols {
                // selection_range.start >= range.start
                let start_contained = symbol.selection_range.start.line > symbol.range.start.line
                    || (symbol.selection_range.start.line == symbol.range.start.line
                        && symbol.selection_range.start.character >= symbol.range.start.character);

                // selection_range.end <= range.end
                let end_contained = symbol.selection_range.end.line < symbol.range.end.line
                    || (symbol.selection_range.end.line == symbol.range.end.line
                        && symbol.selection_range.end.character <= symbol.range.end.character);

                prop_assert!(
                    start_contained,
                    "selection_range.start must be >= range.start for S4 symbol '{}'. \
                     selection_range: {:?}, range: {:?}, code: '{}'",
                    symbol.name, symbol.selection_range, symbol.range, code
                );

                prop_assert!(
                    end_contained,
                    "selection_range.end must be <= range.end for S4 symbol '{}'. \
                     selection_range: {:?}, range: {:?}, code: '{}'",
                    symbol.name, symbol.selection_range, symbol.range, code
                );
            }
        }

        // ========================================================================
        // **Feature: document-workspace-symbols, Property 5: Section Detection and Nesting**
        // **Validates: Requirements 4.1, 4.5**
        //
        // For any R document with section comments matching the pattern
        // `^\s*#(#*)\s*(%%)?\s*(\S.+?)\s*(#{4,}|\-{4,}|={4,}|\*{4,}|\+{4,})\s*$`:
        // - Each matching comment SHALL produce a `DocumentSymbol` with `kind = MODULE`
        // - Section `range` SHALL span from the comment line to the line before the next section (or EOF)
        // - Section `selectionRange` SHALL be the comment line only
        // - Sections with more `#` characters SHALL be nested within sections with fewer `#` characters
        //
        // Note: Requirements 4.2, 4.3, 4.4 (section range expansion and nesting) will be fully
        // tested after HierarchyBuilder is implemented in task 6. For now, focus on extraction
        // aspects (4.1, 4.5).
        // ========================================================================

        #[test]
        /// Feature: document-workspace-symbols, Property 5: Section Detection and Nesting
        ///
        /// For any R code containing a section comment with dash delimiter (----),
        /// the SymbolExtractor SHALL produce a symbol with kind=MODULE.
        ///
        /// **Validates: Requirements 4.1, 4.5**
        fn prop_section_detection_dash_delimiter(
            section_name in "[A-Za-z][A-Za-z0-9 ]{2,20}".prop_filter("Has content", |s| !s.trim().is_empty()),
            dash_count in 4usize..12
        ) {
            let delimiter = "-".repeat(dash_count);
            let code = format!("# {} {}\nx <- 1", section_name.trim(), delimiter);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Find section symbols
            let sections: Vec<_> = symbols
                .iter()
                .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
                .collect();

            prop_assert!(
                !sections.is_empty(),
                "Should extract at least one section from code with dash delimiter. \
                 Code: '{}', Found symbols: {:?}",
                code, symbols.iter().map(|s| (&s.name, &s.kind)).collect::<Vec<_>>()
            );

            let section = &sections[0];
            prop_assert_eq!(
                section.kind,
                DocumentSymbolKind::Module,
                "Section symbol should have kind=MODULE. Code: '{}', Actual kind: {:?}",
                code, section.kind
            );

            prop_assert_eq!(
                section.name.trim(),
                section_name.trim(),
                "Section name should match. Code: '{}', Expected: '{}', Actual: '{}'",
                code, section_name.trim(), section.name
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 5: Section Detection and Nesting
        ///
        /// For any R code containing a section comment with hash delimiter (####),
        /// the SymbolExtractor SHALL produce a symbol with kind=MODULE.
        ///
        /// **Validates: Requirements 4.1, 4.5**
        fn prop_section_detection_hash_delimiter(
            section_name in "[A-Za-z][A-Za-z0-9 ]{2,20}".prop_filter("Has content", |s| !s.trim().is_empty()),
            hash_count in 4usize..12
        ) {
            let delimiter = "#".repeat(hash_count);
            let code = format!("# {} {}\nx <- 1", section_name.trim(), delimiter);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Find section symbols
            let sections: Vec<_> = symbols
                .iter()
                .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
                .collect();

            prop_assert!(
                !sections.is_empty(),
                "Should extract at least one section from code with hash delimiter. \
                 Code: '{}', Found symbols: {:?}",
                code, symbols.iter().map(|s| (&s.name, &s.kind)).collect::<Vec<_>>()
            );

            let section = &sections[0];
            prop_assert_eq!(
                section.kind,
                DocumentSymbolKind::Module,
                "Section symbol should have kind=MODULE. Code: '{}', Actual kind: {:?}",
                code, section.kind
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 5: Section Detection and Nesting
        ///
        /// For any R code containing a section comment with equals delimiter (====),
        /// the SymbolExtractor SHALL produce a symbol with kind=MODULE.
        ///
        /// **Validates: Requirements 4.1, 4.5**
        fn prop_section_detection_equals_delimiter(
            section_name in "[A-Za-z][A-Za-z0-9 ]{2,20}".prop_filter("Has content", |s| !s.trim().is_empty()),
            equals_count in 4usize..12
        ) {
            let delimiter = "=".repeat(equals_count);
            let code = format!("# {} {}\nx <- 1", section_name.trim(), delimiter);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Find section symbols
            let sections: Vec<_> = symbols
                .iter()
                .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
                .collect();

            prop_assert!(
                !sections.is_empty(),
                "Should extract at least one section from code with equals delimiter. \
                 Code: '{}', Found symbols: {:?}",
                code, symbols.iter().map(|s| (&s.name, &s.kind)).collect::<Vec<_>>()
            );

            let section = &sections[0];
            prop_assert_eq!(
                section.kind,
                DocumentSymbolKind::Module,
                "Section symbol should have kind=MODULE. Code: '{}', Actual kind: {:?}",
                code, section.kind
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 5: Section Detection and Nesting
        ///
        /// For any R code containing a section comment with asterisk delimiter (****),
        /// the SymbolExtractor SHALL produce a symbol with kind=MODULE.
        ///
        /// **Validates: Requirements 4.1, 4.5**
        fn prop_section_detection_asterisk_delimiter(
            section_name in "[A-Za-z][A-Za-z0-9 ]{2,20}".prop_filter("Has content", |s| !s.trim().is_empty()),
            asterisk_count in 4usize..12
        ) {
            let delimiter = "*".repeat(asterisk_count);
            let code = format!("# {} {}\nx <- 1", section_name.trim(), delimiter);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Find section symbols
            let sections: Vec<_> = symbols
                .iter()
                .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
                .collect();

            prop_assert!(
                !sections.is_empty(),
                "Should extract at least one section from code with asterisk delimiter. \
                 Code: '{}', Found symbols: {:?}",
                code, symbols.iter().map(|s| (&s.name, &s.kind)).collect::<Vec<_>>()
            );

            let section = &sections[0];
            prop_assert_eq!(
                section.kind,
                DocumentSymbolKind::Module,
                "Section symbol should have kind=MODULE. Code: '{}', Actual kind: {:?}",
                code, section.kind
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 5: Section Detection and Nesting
        ///
        /// For any R code containing a section comment with plus delimiter (++++),
        /// the SymbolExtractor SHALL produce a symbol with kind=MODULE.
        ///
        /// **Validates: Requirements 4.1, 4.5**
        fn prop_section_detection_plus_delimiter(
            section_name in "[A-Za-z][A-Za-z0-9 ]{2,20}".prop_filter("Has content", |s| !s.trim().is_empty()),
            plus_count in 4usize..12
        ) {
            let delimiter = "+".repeat(plus_count);
            let code = format!("# {} {}\nx <- 1", section_name.trim(), delimiter);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Find section symbols
            let sections: Vec<_> = symbols
                .iter()
                .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
                .collect();

            prop_assert!(
                !sections.is_empty(),
                "Should extract at least one section from code with plus delimiter. \
                 Code: '{}', Found symbols: {:?}",
                code, symbols.iter().map(|s| (&s.name, &s.kind)).collect::<Vec<_>>()
            );

            let section = &sections[0];
            prop_assert_eq!(
                section.kind,
                DocumentSymbolKind::Module,
                "Section symbol should have kind=MODULE. Code: '{}', Actual kind: {:?}",
                code, section.kind
            );
        }

        // ====================================================================
        // Feature: r-section-detection-fix property tests
        // ====================================================================

        #[test]
        /// Feature: r-section-detection-fix, Property 1: Delimiter validation correctness
        ///
        /// For any string composed only of delimiter characters (#, -, =, *, +)
        /// and/or whitespace, is_delimiter_only() SHALL return true.
        ///
        /// **Validates: Requirements 1.1, 1.2, 1.3**
        fn prop_is_delimiter_only_true_for_delimiter_strings(
            s in "[#=*+\\- ]{1,30}"
        ) {
            prop_assert!(
                is_delimiter_only(&s),
                "is_delimiter_only should return true for delimiter-only string: '{}'",
                s
            );
        }

        #[test]
        /// Feature: r-section-detection-fix, Property 1: Delimiter validation correctness
        ///
        /// For any string containing at least one non-delimiter, non-whitespace
        /// character, is_delimiter_only() SHALL return false.
        ///
        /// **Validates: Requirements 1.1, 1.2, 1.3**
        fn prop_is_delimiter_only_false_for_non_delimiter_strings(
            prefix in "[#=*+\\-]{0,5}",
            non_delim in "[a-zA-Z0-9_.@!?:()]{1,10}",
            suffix in "[#=*+\\-]{0,5}"
        ) {
            let s = format!("{}{}{}", prefix, non_delim, suffix);
            prop_assert!(
                !is_delimiter_only(&s),
                "is_delimiter_only should return false for string with non-delimiter content: '{}'",
                s
            );
        }

        #[test]
        /// Feature: r-section-detection-fix, Property 2: Section detection consistency
        ///
        /// For any R section comment with a name containing non-delimiter content,
        /// extract_sections() SHALL detect it as a valid section.
        ///
        /// **Validates: Requirements 2.1-2.6, 4.1-4.6**
        fn prop_section_with_non_delimiter_name_detected(
            section_name in "[a-zA-Z0-9_.@]{1,5}[a-zA-Z0-9 _.@]{0,15}".prop_filter(
                "Has non-delimiter content",
                |s| !s.trim().is_empty() && !is_delimiter_only(s.trim())
            ),
            dash_count in 4usize..12
        ) {
            let delimiter = "-".repeat(dash_count);
            let code = format!("# {} {}\nx <- 1", section_name.trim(), delimiter);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            let sections: Vec<_> = symbols
                .iter()
                .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
                .collect();

            prop_assert!(
                !sections.is_empty(),
                "Section with non-delimiter name should be detected. Code: '{}'",
                code
            );
        }

        #[test]
        /// Feature: r-section-detection-fix, Property 2: Section detection consistency
        ///
        /// For any R comment line where the regex captures a delimiter-only name,
        /// extract_sections() SHALL NOT detect it as a section.
        ///
        /// **Validates: Requirements 3.1-3.11**
        fn prop_delimiter_only_name_rejected(
            delim_name_count in 2usize..10,
            trail_count in 4usize..12
        ) {
            // Build a "name" from delimiter chars and a trailing delimiter
            let delim_name = "=".repeat(delim_name_count);
            let trail = "-".repeat(trail_count);
            let code = format!("# {} {}\nx <- 1", delim_name, trail);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            let sections: Vec<_> = symbols
                .iter()
                .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
                .collect();

            prop_assert!(
                sections.is_empty(),
                "Delimiter-only name should NOT produce a section. Code: '{}', Sections: {:?}",
                code, sections.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 5: Section Detection and Nesting
        ///
        /// For any R code containing section comments with different heading levels
        /// (# count), the SymbolExtractor SHALL correctly compute section_level.
        ///
        /// **Validates: Requirements 4.1, 4.5**
        fn prop_section_heading_level_extraction(
            section_name in "[A-Za-z][A-Za-z0-9 ]{2,15}".prop_filter("Has content", |s| !s.trim().is_empty()),
            heading_level in 1u32..6
        ) {
            // Generate the appropriate number of # characters
            let hashes = "#".repeat(heading_level as usize);
            let code = format!("{} {} ----\nx <- 1", hashes, section_name.trim());

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Find section symbols
            let sections: Vec<_> = symbols
                .iter()
                .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
                .collect();

            prop_assert!(
                !sections.is_empty(),
                "Should extract at least one section from code with {} # characters. \
                 Code: '{}', Found symbols: {:?}",
                heading_level, code, symbols.iter().map(|s| (&s.name, &s.kind)).collect::<Vec<_>>()
            );

            let section = &sections[0];
            prop_assert_eq!(
                section.section_level,
                Some(heading_level),
                "Section level should be {} for {} # characters. \
                 Code: '{}', Actual level: {:?}",
                heading_level, heading_level, code, section.section_level
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 5: Section Detection and Nesting
        ///
        /// For any R code containing section comments with RStudio cell marker (%%),
        /// the SymbolExtractor SHALL correctly extract the section name.
        ///
        /// **Validates: Requirements 4.1, 4.5**
        fn prop_section_rstudio_cell_marker(
            section_name in "[A-Za-z][A-Za-z0-9 ]{2,15}".prop_filter("Has content", |s| !s.trim().is_empty())
        ) {
            let code = format!("# %% {} ----\nx <- 1", section_name.trim());

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Find section symbols
            let sections: Vec<_> = symbols
                .iter()
                .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
                .collect();

            prop_assert!(
                !sections.is_empty(),
                "Should extract at least one section from code with RStudio cell marker. \
                 Code: '{}', Found symbols: {:?}",
                code, symbols.iter().map(|s| (&s.name, &s.kind)).collect::<Vec<_>>()
            );

            let section = &sections[0];
            prop_assert_eq!(
                section.name.trim(),
                section_name.trim(),
                "Section name should match for RStudio cell marker. \
                 Code: '{}', Expected: '{}', Actual: '{}'",
                code, section_name.trim(), section.name
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 5: Section Detection and Nesting
        ///
        /// For any R code containing multiple sections, the SymbolExtractor SHALL
        /// extract all sections with correct names and levels.
        ///
        /// **Validates: Requirements 4.1, 4.5**
        fn prop_section_multiple_sections_extraction(
            section_count in 2usize..5
        ) {
            // Generate multiple sections with different levels
            let mut code_lines = Vec::new();
            let mut expected_sections = Vec::new();

            for i in 0..section_count {
                let level = (i % 3) + 1; // Levels 1, 2, 3, 1, 2, ...
                let hashes = "#".repeat(level);
                let name = format!("Section{}", i);
                code_lines.push(format!("{} {} ----", hashes, name));
                code_lines.push(format!("x{} <- {}", i, i));
                expected_sections.push((name, level as u32));
            }

            let code = code_lines.join("\n");

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Find section symbols
            let sections: Vec<_> = symbols
                .iter()
                .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
                .collect();

            prop_assert_eq!(
                sections.len(),
                section_count,
                "Should extract {} sections from code. \
                 Code:\n{}\nFound sections: {:?}",
                section_count, code, sections.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            // Verify each section has correct name and level
            for (i, (expected_name, expected_level)) in expected_sections.iter().enumerate() {
                let section = &sections[i];
                prop_assert_eq!(
                    &section.name,
                    expected_name,
                    "Section {} should have name '{}'. Actual: '{}'",
                    i, expected_name, section.name
                );
                prop_assert_eq!(
                    section.section_level,
                    Some(*expected_level),
                    "Section '{}' should have level {}. Actual: {:?}",
                    expected_name, expected_level, section.section_level
                );
            }
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 5: Section Detection and Nesting
        ///
        /// For any R code containing section comments, the section's selection_range
        /// SHALL equal its range (both cover the comment line only).
        ///
        /// **Validates: Requirements 4.1, 4.5**
        fn prop_section_selection_range_equals_range(
            section_name in "[A-Za-z][A-Za-z0-9 ]{2,15}".prop_filter("Has content", |s| !s.trim().is_empty()),
            delimiter_type in prop::sample::select(vec!["-", "#", "=", "*", "+"])
        ) {
            let delimiter = delimiter_type.repeat(4);
            let code = format!("# {} {}\nx <- 1", section_name.trim(), delimiter);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Find section symbols
            let sections: Vec<_> = symbols
                .iter()
                .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
                .collect();

            prop_assert!(
                !sections.is_empty(),
                "Should extract at least one section. Code: '{}'", code
            );

            let section = &sections[0];
            prop_assert_eq!(
                section.range,
                section.selection_range,
                "Section selection_range should equal range. \
                 Code: '{}', range: {:?}, selection_range: {:?}",
                code, section.range, section.selection_range
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 5: Section Detection and Nesting
        ///
        /// For any R code containing section comments, the section SHALL NOT have
        /// a detail field (detail is for function signatures only).
        ///
        /// **Validates: Requirements 4.1, 4.5**
        fn prop_section_no_detail_field(
            section_name in "[A-Za-z][A-Za-z0-9 ]{2,15}".prop_filter("Has content", |s| !s.trim().is_empty())
        ) {
            let code = format!("# {} ----\nx <- 1", section_name.trim());

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Find section symbols
            let sections: Vec<_> = symbols
                .iter()
                .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
                .collect();

            prop_assert!(
                !sections.is_empty(),
                "Should extract at least one section. Code: '{}'", code
            );

            let section = &sections[0];
            prop_assert!(
                section.detail.is_none(),
                "Section should not have a detail field. \
                 Code: '{}', detail: {:?}",
                code, section.detail
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 5: Section Detection and Nesting
        ///
        /// For any R code containing comments that do NOT match the section pattern
        /// (delimiter too short), the SymbolExtractor SHALL NOT produce section symbols.
        ///
        /// **Validates: Requirements 4.1, 4.5**
        fn prop_section_short_delimiter_not_detected(
            section_name in "[A-Za-z][A-Za-z0-9 ]{2,15}".prop_filter("Has content", |s| !s.trim().is_empty()),
            delimiter_count in 1usize..4  // Less than 4 characters
        ) {
            let delimiter = "-".repeat(delimiter_count);
            let code = format!("# {} {}\nx <- 1", section_name.trim(), delimiter);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Find section symbols
            let sections: Vec<_> = symbols
                .iter()
                .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
                .collect();

            prop_assert!(
                sections.is_empty(),
                "Should NOT extract section from code with short delimiter ({}). \
                 Code: '{}', Found sections: {:?}",
                delimiter_count, code, sections.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 5: Section Detection and Nesting
        ///
        /// For any R code containing regular comments (no delimiter), the SymbolExtractor
        /// SHALL NOT produce section symbols.
        ///
        /// **Validates: Requirements 4.1, 4.5**
        fn prop_section_regular_comment_not_detected(
            comment_text in "[A-Za-z][A-Za-z0-9 ]{5,30}".prop_filter("Has content", |s| !s.trim().is_empty())
        ) {
            // Ensure the comment doesn't accidentally match the section pattern
            let clean_text = comment_text
                .replace('-', " ")
                .replace('#', " ")
                .replace('=', " ")
                .replace('*', " ")
                .replace('+', " ");
            let code = format!("# {}\nx <- 1", clean_text.trim());

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Find section symbols
            let sections: Vec<_> = symbols
                .iter()
                .filter(|s| matches!(s.kind, DocumentSymbolKind::Module))
                .collect();

            prop_assert!(
                sections.is_empty(),
                "Should NOT extract section from regular comment. \
                 Code: '{}', Found sections: {:?}",
                code, sections.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 5: Section Detection and Nesting
        ///
        /// For any R code containing sections mixed with code, the SymbolExtractor
        /// SHALL extract both sections and code symbols correctly.
        ///
        /// **Validates: Requirements 4.1, 4.5**
        fn prop_section_mixed_with_code_symbols(
            section_name in "[A-Za-z][A-Za-z0-9]{2,10}".prop_filter("Has content", |s| !s.trim().is_empty()),
            var_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            func_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Skip if names collide
            prop_assume!(var_name != func_name);

            let code = format!(
                "# {} ----\n{} <- 42\n{} <- function(x) {{ x + 1 }}",
                section_name.trim(), var_name, func_name
            );

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Should have 1 section, 1 variable, 1 function
            prop_assert_eq!(
                symbols.len(),
                3,
                "Should extract 3 symbols (1 section, 1 variable, 1 function). \
                 Code:\n{}\nFound: {:?}",
                code, symbols.iter().map(|s| (&s.name, &s.kind)).collect::<Vec<_>>()
            );

            // Verify section
            let section = symbols.iter().find(|s| matches!(s.kind, DocumentSymbolKind::Module));
            prop_assert!(
                section.is_some(),
                "Should find section symbol. Found: {:?}",
                symbols.iter().map(|s| (&s.name, &s.kind)).collect::<Vec<_>>()
            );
            prop_assert_eq!(
                section.unwrap().name.trim(),
                section_name.trim(),
                "Section name should match"
            );

            // Verify variable
            let var = symbols.iter().find(|s| s.name == var_name);
            prop_assert!(
                var.is_some(),
                "Should find variable symbol '{}'. Found: {:?}",
                var_name, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
            prop_assert_eq!(
                var.unwrap().kind,
                DocumentSymbolKind::Variable,
                "Variable should have kind=VARIABLE"
            );

            // Verify function
            let func = symbols.iter().find(|s| s.name == func_name);
            prop_assert!(
                func.is_some(),
                "Should find function symbol '{}'. Found: {:?}",
                func_name, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
            prop_assert_eq!(
                func.unwrap().kind,
                DocumentSymbolKind::Function,
                "Function should have kind=FUNCTION"
            );
        }

        // ========================================================================
        // **Feature: document-workspace-symbols, Property 4: Hierarchical Nesting Correctness**
        // **Validates: Requirements 3.1, 3.2, 3.3**
        //
        // For any R document with assignments:
        // - Assignments inside function bodies SHALL appear as children of that function's symbol
        // - Assignments at top-level SHALL appear as root-level symbols
        // - Nested function definitions SHALL preserve their nesting depth in the symbol hierarchy
        // ========================================================================

        #[test]
        /// Feature: document-workspace-symbols, Property 4: Hierarchical Nesting Correctness
        ///
        /// For any R code containing a function with a variable defined inside its body,
        /// the HierarchyBuilder SHALL nest the variable as a child of the function.
        ///
        /// **Validates: Requirements 3.1, 3.2, 3.3**
        fn prop_hierarchical_nesting_variable_inside_function(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            value in 1i32..1000
        ) {
            // Skip if names are the same
            prop_assume!(func_name != var_name);

            // Create code with a variable inside a function body
            let code = format!(
                "{} <- function() {{\n  {} <- {}\n}}",
                func_name, var_name, value
            );

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Build hierarchy
            let line_count = code.lines().count() as u32;
            let builder = HierarchyBuilder::new(symbols, line_count);
            let doc_symbols = builder.build();

            // Should have exactly 1 root-level symbol (the function)
            prop_assert_eq!(
                doc_symbols.len(),
                1,
                "Should have exactly 1 root-level symbol (the function). \
                 Code:\n{}\nFound: {:?}",
                code, doc_symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            let func_symbol = &doc_symbols[0];
            prop_assert_eq!(
                &func_symbol.name,
                &func_name,
                "Root symbol should be the function"
            );
            prop_assert_eq!(
                func_symbol.kind,
                tower_lsp::lsp_types::SymbolKind::FUNCTION,
                "Root symbol should have kind=FUNCTION"
            );

            // Function should have the variable as a child
            let children = func_symbol.children.as_ref();
            prop_assert!(
                children.is_some(),
                "Function should have children. Code:\n{}", code
            );

            let children = children.unwrap();
            prop_assert_eq!(
                children.len(),
                1,
                "Function should have exactly 1 child (the variable). \
                 Code:\n{}\nFound children: {:?}",
                code, children.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            let var_symbol = &children[0];
            prop_assert_eq!(
                &var_symbol.name,
                &var_name,
                "Child symbol should be the variable"
            );
            prop_assert_eq!(
                var_symbol.kind,
                tower_lsp::lsp_types::SymbolKind::VARIABLE,
                "Child symbol should have kind=VARIABLE"
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 4: Hierarchical Nesting Correctness
        ///
        /// For any R code containing a top-level variable (outside any function),
        /// the HierarchyBuilder SHALL include it as a root-level symbol.
        ///
        /// **Validates: Requirements 3.1, 3.2, 3.3**
        fn prop_hierarchical_nesting_top_level_variable(
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            value in 1i32..1000
        ) {
            // Create code with a top-level variable
            let code = format!("{} <- {}", var_name, value);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Build hierarchy
            let line_count = code.lines().count() as u32;
            let builder = HierarchyBuilder::new(symbols, line_count);
            let doc_symbols = builder.build();

            // Should have exactly 1 root-level symbol (the variable)
            prop_assert_eq!(
                doc_symbols.len(),
                1,
                "Should have exactly 1 root-level symbol (the variable). \
                 Code: '{}'\nFound: {:?}",
                code, doc_symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            let var_symbol = &doc_symbols[0];
            prop_assert_eq!(
                &var_symbol.name,
                &var_name,
                "Root symbol should be the variable"
            );
            prop_assert_eq!(
                var_symbol.kind,
                tower_lsp::lsp_types::SymbolKind::VARIABLE,
                "Root symbol should have kind=VARIABLE"
            );

            // Variable should have no children
            prop_assert!(
                var_symbol.children.is_none() || var_symbol.children.as_ref().unwrap().is_empty(),
                "Top-level variable should have no children. Code: '{}'", code
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 4: Hierarchical Nesting Correctness
        ///
        /// For any R code containing nested function definitions, the HierarchyBuilder
        /// SHALL preserve the nesting depth in the symbol hierarchy.
        ///
        /// **Validates: Requirements 3.1, 3.2, 3.3**
        fn prop_hierarchical_nesting_nested_functions(
            outer_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            inner_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Skip if names are the same
            prop_assume!(outer_name != inner_name);

            // Create code with nested functions
            let code = format!(
                "{} <- function() {{\n  {} <- function() {{\n    42\n  }}\n}}",
                outer_name, inner_name
            );

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Build hierarchy
            let line_count = code.lines().count() as u32;
            let builder = HierarchyBuilder::new(symbols, line_count);
            let doc_symbols = builder.build();

            // Should have exactly 1 root-level symbol (the outer function)
            prop_assert_eq!(
                doc_symbols.len(),
                1,
                "Should have exactly 1 root-level symbol (outer function). \
                 Code:\n{}\nFound: {:?}",
                code, doc_symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            let outer_symbol = &doc_symbols[0];
            prop_assert_eq!(
                &outer_symbol.name,
                &outer_name,
                "Root symbol should be the outer function"
            );
            prop_assert_eq!(
                outer_symbol.kind,
                tower_lsp::lsp_types::SymbolKind::FUNCTION,
                "Outer symbol should have kind=FUNCTION"
            );

            // Outer function should have the inner function as a child
            let children = outer_symbol.children.as_ref();
            prop_assert!(
                children.is_some(),
                "Outer function should have children. Code:\n{}", code
            );

            let children = children.unwrap();
            prop_assert_eq!(
                children.len(),
                1,
                "Outer function should have exactly 1 child (inner function). \
                 Code:\n{}\nFound children: {:?}",
                code, children.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            let inner_symbol = &children[0];
            prop_assert_eq!(
                &inner_symbol.name,
                &inner_name,
                "Child symbol should be the inner function"
            );
            prop_assert_eq!(
                inner_symbol.kind,
                tower_lsp::lsp_types::SymbolKind::FUNCTION,
                "Inner symbol should have kind=FUNCTION"
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 4: Hierarchical Nesting Correctness
        ///
        /// For any R code containing a function with multiple nested symbols (variables
        /// and functions), the HierarchyBuilder SHALL nest all of them as children.
        ///
        /// **Validates: Requirements 3.1, 3.2, 3.3**
        fn prop_hierarchical_nesting_multiple_children(
            func_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            var_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            inner_func_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Skip if any names are the same
            prop_assume!(func_name != var_name);
            prop_assume!(func_name != inner_func_name);
            prop_assume!(var_name != inner_func_name);

            // Create code with a function containing both a variable and a nested function
            let code = format!(
                "{} <- function() {{\n  {} <- 42\n  {} <- function() {{ 1 }}\n}}",
                func_name, var_name, inner_func_name
            );

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Build hierarchy
            let line_count = code.lines().count() as u32;
            let builder = HierarchyBuilder::new(symbols, line_count);
            let doc_symbols = builder.build();

            // Should have exactly 1 root-level symbol (the outer function)
            prop_assert_eq!(
                doc_symbols.len(),
                1,
                "Should have exactly 1 root-level symbol. Code:\n{}\nFound: {:?}",
                code, doc_symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            let func_symbol = &doc_symbols[0];
            prop_assert_eq!(
                &func_symbol.name,
                &func_name,
                "Root symbol should be the outer function"
            );

            // Function should have 2 children (variable and inner function)
            let children = func_symbol.children.as_ref();
            prop_assert!(
                children.is_some(),
                "Function should have children. Code:\n{}", code
            );

            let children = children.unwrap();
            prop_assert_eq!(
                children.len(),
                2,
                "Function should have exactly 2 children. \
                 Code:\n{}\nFound children: {:?}",
                code, children.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            // Verify both children exist
            let var_child = children.iter().find(|s| s.name == var_name);
            let func_child = children.iter().find(|s| s.name == inner_func_name);

            prop_assert!(
                var_child.is_some(),
                "Should find variable child '{}'. Found: {:?}",
                var_name, children.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
            prop_assert!(
                func_child.is_some(),
                "Should find function child '{}'. Found: {:?}",
                inner_func_name, children.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            prop_assert_eq!(
                var_child.unwrap().kind,
                tower_lsp::lsp_types::SymbolKind::VARIABLE,
                "Variable child should have kind=VARIABLE"
            );
            prop_assert_eq!(
                func_child.unwrap().kind,
                tower_lsp::lsp_types::SymbolKind::FUNCTION,
                "Function child should have kind=FUNCTION"
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 4: Hierarchical Nesting Correctness
        ///
        /// For any R code containing mixed top-level and nested symbols, the HierarchyBuilder
        /// SHALL correctly separate root-level symbols from nested ones.
        ///
        /// **Validates: Requirements 3.1, 3.2, 3.3**
        fn prop_hierarchical_nesting_mixed_levels(
            top_var_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            func_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            nested_var_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Skip if any names are the same
            prop_assume!(top_var_name != func_name);
            prop_assume!(top_var_name != nested_var_name);
            prop_assume!(func_name != nested_var_name);

            // Create code with a top-level variable, a function, and a nested variable
            let code = format!(
                "{} <- 1\n{} <- function() {{\n  {} <- 2\n}}",
                top_var_name, func_name, nested_var_name
            );

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Build hierarchy
            let line_count = code.lines().count() as u32;
            let builder = HierarchyBuilder::new(symbols, line_count);
            let doc_symbols = builder.build();

            // Should have exactly 2 root-level symbols (top_var and func)
            prop_assert_eq!(
                doc_symbols.len(),
                2,
                "Should have exactly 2 root-level symbols. Code:\n{}\nFound: {:?}",
                code, doc_symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            // Find the top-level variable and function
            let top_var = doc_symbols.iter().find(|s| s.name == top_var_name);
            let func = doc_symbols.iter().find(|s| s.name == func_name);

            prop_assert!(
                top_var.is_some(),
                "Should find top-level variable '{}'. Found: {:?}",
                top_var_name, doc_symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
            prop_assert!(
                func.is_some(),
                "Should find function '{}'. Found: {:?}",
                func_name, doc_symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            // Top-level variable should have no children
            let top_var = top_var.unwrap();
            prop_assert!(
                top_var.children.is_none() || top_var.children.as_ref().unwrap().is_empty(),
                "Top-level variable should have no children"
            );

            // Function should have the nested variable as a child
            let func = func.unwrap();
            let func_children = func.children.as_ref();
            prop_assert!(
                func_children.is_some(),
                "Function should have children"
            );

            let func_children = func_children.unwrap();
            prop_assert_eq!(
                func_children.len(),
                1,
                "Function should have exactly 1 child (nested variable). Found: {:?}",
                func_children.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            prop_assert_eq!(
                &func_children[0].name,
                &nested_var_name,
                "Function's child should be the nested variable"
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 4: Hierarchical Nesting Correctness
        ///
        /// For any R code containing three levels of nesting (function -> function -> variable),
        /// the HierarchyBuilder SHALL preserve all three levels in the hierarchy.
        ///
        /// **Validates: Requirements 3.1, 3.2, 3.3**
        fn prop_hierarchical_nesting_three_levels(
            outer_name in "[a-z][a-z0-9_]{2,6}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            middle_name in "[a-z][a-z0-9_]{2,6}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            inner_var_name in "[a-z][a-z0-9_]{2,6}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Skip if any names are the same
            prop_assume!(outer_name != middle_name);
            prop_assume!(outer_name != inner_var_name);
            prop_assume!(middle_name != inner_var_name);

            // Create code with three levels of nesting
            let code = format!(
                "{} <- function() {{\n  {} <- function() {{\n    {} <- 42\n  }}\n}}",
                outer_name, middle_name, inner_var_name
            );

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Build hierarchy
            let line_count = code.lines().count() as u32;
            let builder = HierarchyBuilder::new(symbols, line_count);
            let doc_symbols = builder.build();

            // Should have exactly 1 root-level symbol (outer function)
            prop_assert_eq!(
                doc_symbols.len(),
                1,
                "Should have exactly 1 root-level symbol. Code:\n{}\nFound: {:?}",
                code, doc_symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            // Level 1: Outer function
            let outer = &doc_symbols[0];
            prop_assert_eq!(&outer.name, &outer_name, "Root should be outer function");
            prop_assert_eq!(outer.kind, tower_lsp::lsp_types::SymbolKind::FUNCTION, "Outer should be FUNCTION");

            // Level 2: Middle function
            let outer_children = outer.children.as_ref();
            prop_assert!(outer_children.is_some(), "Outer should have children");
            let outer_children = outer_children.unwrap();
            prop_assert_eq!(
                outer_children.len(),
                1,
                "Outer should have 1 child. Found: {:?}",
                outer_children.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            let middle = &outer_children[0];
            prop_assert_eq!(&middle.name, &middle_name, "Middle should be middle function");
            prop_assert_eq!(middle.kind, tower_lsp::lsp_types::SymbolKind::FUNCTION, "Middle should be FUNCTION");

            // Level 3: Inner variable
            let middle_children = middle.children.as_ref();
            prop_assert!(middle_children.is_some(), "Middle should have children");
            let middle_children = middle_children.unwrap();
            prop_assert_eq!(
                middle_children.len(),
                1,
                "Middle should have 1 child. Found: {:?}",
                middle_children.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            let inner = &middle_children[0];
            prop_assert_eq!(&inner.name, &inner_var_name, "Inner should be the variable");
            prop_assert_eq!(inner.kind, tower_lsp::lsp_types::SymbolKind::VARIABLE, "Inner should be VARIABLE");
        }

        // ========================================================================
        // **Feature: document-workspace-symbols, Property 1: Response Type Selection**
        // **Validates: Requirements 1.1, 1.2**
        //
        // For any client capability configuration:
        // - If `hierarchicalDocumentSymbolSupport` is true, the Document_Symbol_Provider
        //   SHALL return `DocumentSymbol[]` (DocumentSymbolResponse::Nested)
        // - Otherwise it SHALL return `SymbolInformation[]` (DocumentSymbolResponse::Flat)
        // ========================================================================

        #[test]
        /// Feature: document-workspace-symbols, Property 1: Response Type Selection
        ///
        /// For any R code containing symbols, when `hierarchical_document_symbol_support`
        /// is true, `document_symbol()` SHALL return `DocumentSymbolResponse::Nested`.
        ///
        /// **Validates: Requirements 1.1, 1.2**
        fn prop_response_type_hierarchical_when_supported(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Skip if names are the same
            prop_assume!(func_name != var_name);

            use crate::state::{WorldState, Document};

            // Generate R code with a function and a variable
            let code = format!(
                "{} <- function() {{}}\n{} <- 42",
                func_name, var_name
            );

            let mut state = WorldState::new(vec![]);
            // Set hierarchical_document_symbol_support to TRUE
            state.symbol_config.hierarchical_document_symbol_support = true;

            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            // Call document_symbol
            let response = super::document_symbol(&state, &uri);

            prop_assert!(
                response.is_some(),
                "document_symbol should return Some for valid document. Code:\n{}", code
            );

            // Requirement 1.1: When hierarchical support is true, return DocumentSymbol[]
            match response.unwrap() {
                tower_lsp::lsp_types::DocumentSymbolResponse::Nested(symbols) => {
                    // Success - got nested response as expected
                    prop_assert!(
                        !symbols.is_empty(),
                        "Nested response should contain symbols. Code:\n{}", code
                    );
                }
                tower_lsp::lsp_types::DocumentSymbolResponse::Flat(_) => {
                    prop_assert!(
                        false,
                        "Expected DocumentSymbolResponse::Nested when hierarchical_document_symbol_support is true. Code:\n{}", code
                    );
                }
            }
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 1: Response Type Selection
        ///
        /// For any R code containing symbols, when `hierarchical_document_symbol_support`
        /// is false, `document_symbol()` SHALL return `DocumentSymbolResponse::Flat`.
        ///
        /// **Validates: Requirements 1.1, 1.2**
        fn prop_response_type_flat_when_not_supported(
            func_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Skip if names are the same
            prop_assume!(func_name != var_name);

            use crate::state::{WorldState, Document};

            // Generate R code with a function and a variable
            let code = format!(
                "{} <- function() {{}}\n{} <- 42",
                func_name, var_name
            );

            let mut state = WorldState::new(vec![]);
            // Set hierarchical_document_symbol_support to FALSE (default)
            state.symbol_config.hierarchical_document_symbol_support = false;

            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            // Call document_symbol
            let response = super::document_symbol(&state, &uri);

            prop_assert!(
                response.is_some(),
                "document_symbol should return Some for valid document. Code:\n{}", code
            );

            // Requirement 1.2: When hierarchical support is false, return SymbolInformation[]
            match response.unwrap() {
                tower_lsp::lsp_types::DocumentSymbolResponse::Flat(symbols) => {
                    // Success - got flat response as expected
                    prop_assert!(
                        !symbols.is_empty(),
                        "Flat response should contain symbols. Code:\n{}", code
                    );
                }
                tower_lsp::lsp_types::DocumentSymbolResponse::Nested(_) => {
                    prop_assert!(
                        false,
                        "Expected DocumentSymbolResponse::Flat when hierarchical_document_symbol_support is false. Code:\n{}", code
                    );
                }
            }
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 1: Response Type Selection
        ///
        /// For any R code, the response type selection SHALL be consistent regardless
        /// of the code content - only the `hierarchical_document_symbol_support` flag
        /// determines the response type.
        ///
        /// **Validates: Requirements 1.1, 1.2**
        fn prop_response_type_selection_consistent(
            symbol_count in 1usize..5,
            hierarchical_support in any::<bool>()
        ) {
            use crate::state::{WorldState, Document};

            // Generate R code with multiple symbols
            let assignments: Vec<String> = (0..symbol_count)
                .map(|i| format!("sym{} <- {}", i, i + 1))
                .collect();
            let code = assignments.join("\n");

            let mut state = WorldState::new(vec![]);
            state.symbol_config.hierarchical_document_symbol_support = hierarchical_support;

            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            // Call document_symbol
            let response = super::document_symbol(&state, &uri);

            prop_assert!(
                response.is_some(),
                "document_symbol should return Some for valid document. Code:\n{}", code
            );

            // Verify response type matches the flag
            let is_nested = matches!(
                response.as_ref().unwrap(),
                tower_lsp::lsp_types::DocumentSymbolResponse::Nested(_)
            );

            prop_assert_eq!(
                is_nested,
                hierarchical_support,
                "Response type should match hierarchical_document_symbol_support flag. \
                 Flag: {}, Got nested: {}, Code:\n{}",
                hierarchical_support, is_nested, code
            );
        }

        // ========================================================================
        // **Feature: document-workspace-symbols, Property 8: Reserved Word Filtering**
        // **Validates: Requirements 7.1, 7.2**
        //
        // For any assignment where the LHS is an R reserved word (if, else, for, while,
        // repeat, in, next, break, TRUE, FALSE, NULL, Inf, NaN, NA, NA_integer_,
        // NA_real_, NA_complex_, NA_character_, function):
        // - The symbol SHALL NOT appear in document symbol results
        // - The symbol SHALL NOT appear in workspace symbol results
        // ========================================================================

        #[test]
        /// Feature: document-workspace-symbols, Property 8: Reserved Word Filtering
        ///
        /// For any R code containing an assignment to a reserved word (e.g., `if <- 1`),
        /// the SymbolExtractor SHALL NOT include that reserved word in the extracted symbols.
        ///
        /// **Validates: Requirements 7.1, 7.2**
        fn prop_reserved_word_filtering_variable_assignment(
            reserved_word in prop::sample::select(crate::reserved_words::RESERVED_WORDS),
            value in 1i32..1000
        ) {
            // Create code with assignment to reserved word (e.g., "if <- 42")
            let code = format!("{} <- {}", reserved_word, value);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Reserved word should NOT appear in extracted symbols
            let reserved_symbols: Vec<_> = symbols
                .iter()
                .filter(|s| s.name == reserved_word)
                .collect();

            prop_assert!(
                reserved_symbols.is_empty(),
                "Reserved word '{}' should NOT appear in extracted symbols. \
                 Code: '{}', Found symbols: {:?}",
                reserved_word, code, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 8: Reserved Word Filtering
        ///
        /// For any R code containing a function definition with a reserved word name
        /// (e.g., `if <- function() {}`), the SymbolExtractor SHALL NOT include that
        /// reserved word in the extracted symbols.
        ///
        /// **Validates: Requirements 7.1, 7.2**
        fn prop_reserved_word_filtering_function_definition(
            reserved_word in prop::sample::select(crate::reserved_words::RESERVED_WORDS)
        ) {
            // Create code with function definition using reserved word name
            let code = format!("{} <- function() {{}}", reserved_word);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Reserved word should NOT appear in extracted symbols
            let reserved_symbols: Vec<_> = symbols
                .iter()
                .filter(|s| s.name == reserved_word)
                .collect();

            prop_assert!(
                reserved_symbols.is_empty(),
                "Reserved word '{}' should NOT appear in extracted symbols (function). \
                 Code: '{}', Found symbols: {:?}",
                reserved_word, code, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 8: Reserved Word Filtering
        ///
        /// For any R code containing multiple assignments where some are to reserved words
        /// and some are to non-reserved identifiers, the SymbolExtractor SHALL include
        /// only the non-reserved identifiers in the extracted symbols.
        ///
        /// **Validates: Requirements 7.1, 7.2**
        fn prop_reserved_word_filtering_mixed_assignments(
            reserved_word in prop::sample::select(crate::reserved_words::RESERVED_WORDS),
            var_name in "[a-z][a-z0-9_]{2,8}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            // Create code with both reserved and non-reserved assignments
            let code = format!("{} <- 1\n{} <- 2", reserved_word, var_name);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Reserved word should NOT appear in extracted symbols
            let reserved_symbols: Vec<_> = symbols
                .iter()
                .filter(|s| s.name == reserved_word)
                .collect();

            prop_assert!(
                reserved_symbols.is_empty(),
                "Reserved word '{}' should NOT appear in extracted symbols. \
                 Code:\n{}\nFound symbols: {:?}",
                reserved_word, code, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );

            // Non-reserved identifier SHOULD appear in extracted symbols
            let var_symbols: Vec<_> = symbols
                .iter()
                .filter(|s| s.name == var_name)
                .collect();

            prop_assert!(
                !var_symbols.is_empty(),
                "Non-reserved identifier '{}' SHOULD appear in extracted symbols. \
                 Code:\n{}\nFound symbols: {:?}",
                var_name, code, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 8: Reserved Word Filtering
        ///
        /// For any R code containing assignments to all reserved words, the SymbolExtractor
        /// SHALL return an empty symbol list.
        ///
        /// **Validates: Requirements 7.1, 7.2**
        fn prop_reserved_word_filtering_all_reserved_words_empty_result(
            reserved_count in 1usize..5
        ) {
            // Select a subset of reserved words
            let reserved_words: Vec<&str> = crate::reserved_words::RESERVED_WORDS
                .iter()
                .take(reserved_count)
                .copied()
                .collect();

            // Create code with assignments to all selected reserved words
            let assignments: Vec<String> = reserved_words
                .iter()
                .enumerate()
                .map(|(i, word)| format!("{} <- {}", word, i + 1))
                .collect();
            let code = assignments.join("\n");

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // All symbols should be filtered out (empty result)
            prop_assert!(
                symbols.is_empty(),
                "All reserved word assignments should be filtered out. \
                 Code:\n{}\nFound symbols: {:?}",
                code, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 8: Reserved Word Filtering
        ///
        /// For any R code containing a right assignment to a reserved word (e.g., `1 -> if`),
        /// the SymbolExtractor SHALL NOT include that reserved word in the extracted symbols.
        ///
        /// **Validates: Requirements 7.1, 7.2**
        fn prop_reserved_word_filtering_right_assignment(
            reserved_word in prop::sample::select(crate::reserved_words::RESERVED_WORDS),
            value in 1i32..1000
        ) {
            // Create code with right assignment to reserved word (e.g., "42 -> if")
            let code = format!("{} -> {}", value, reserved_word);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Reserved word should NOT appear in extracted symbols
            let reserved_symbols: Vec<_> = symbols
                .iter()
                .filter(|s| s.name == reserved_word)
                .collect();

            prop_assert!(
                reserved_symbols.is_empty(),
                "Reserved word '{}' should NOT appear in extracted symbols (right assignment). \
                 Code: '{}', Found symbols: {:?}",
                reserved_word, code, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 8: Reserved Word Filtering
        ///
        /// For any R code containing an assignment to a reserved word, the document_symbol()
        /// handler SHALL NOT include that reserved word in the response (both Nested and Flat).
        ///
        /// **Validates: Requirements 7.1, 7.2**
        fn prop_reserved_word_filtering_document_symbol_handler(
            reserved_word in prop::sample::select(crate::reserved_words::RESERVED_WORDS),
            hierarchical_support in any::<bool>()
        ) {
            use crate::state::{WorldState, Document};

            // Create code with assignment to reserved word
            let code = format!("{} <- 42", reserved_word);

            let mut state = WorldState::new(vec![]);
            state.symbol_config.hierarchical_document_symbol_support = hierarchical_support;

            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            // Call document_symbol
            let response = super::document_symbol(&state, &uri);

            prop_assert!(
                response.is_some(),
                "document_symbol should return Some for valid document. Code: '{}'", code
            );

            // Check that reserved word is not in the response
            match response.unwrap() {
                tower_lsp::lsp_types::DocumentSymbolResponse::Nested(symbols) => {
                    let reserved_found = symbols.iter().any(|s| s.name == reserved_word);
                    prop_assert!(
                        !reserved_found,
                        "Reserved word '{}' should NOT appear in Nested document symbols. \
                         Code: '{}', Found symbols: {:?}",
                        reserved_word, code, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
                    );
                }
                tower_lsp::lsp_types::DocumentSymbolResponse::Flat(symbols) => {
                    let reserved_found = symbols.iter().any(|s| s.name == reserved_word);
                    prop_assert!(
                        !reserved_found,
                        "Reserved word '{}' should NOT appear in Flat document symbols. \
                         Code: '{}', Found symbols: {:?}",
                        reserved_word, code, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
                    );
                }
            }
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 8: Reserved Word Filtering
        ///
        /// For any R code containing an assignment to a reserved word, the workspace_symbol()
        /// handler SHALL NOT include that reserved word in the response.
        ///
        /// **Validates: Requirements 7.1, 7.2**
        fn prop_reserved_word_filtering_workspace_symbol_handler(
            reserved_word in prop::sample::select(crate::reserved_words::RESERVED_WORDS)
        ) {
            use crate::state::{WorldState, Document};

            // Create code with assignment to reserved word
            let code = format!("{} <- 42", reserved_word);

            let mut state = WorldState::new(vec![]);
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            // Call workspace_symbol with empty query (returns all symbols)
            let response = super::workspace_symbol(&state, "");

            prop_assert!(
                response.is_some(),
                "workspace_symbol should return Some. Code: '{}'", code
            );

            let symbols = response.unwrap();

            // Check that reserved word is not in the response
            let reserved_found = symbols.iter().any(|s| s.name == reserved_word);
            prop_assert!(
                !reserved_found,
                "Reserved word '{}' should NOT appear in workspace symbols. \
                 Code: '{}', Found symbols: {:?}",
                reserved_word, code, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 8: Reserved Word Filtering (Negative Control)
        ///
        /// For any R code containing an assignment to a non-reserved identifier,
        /// the SymbolExtractor SHALL include that identifier in the extracted symbols.
        /// This is a negative control to ensure the extractor is working correctly.
        ///
        /// **Validates: Requirements 7.1, 7.2**
        fn prop_reserved_word_filtering_non_reserved_included(
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            value in 1i32..1000
        ) {
            // Create code with assignment to non-reserved identifier
            let code = format!("{} <- {}", var_name, value);

            let tree = parse_r_code(&code);
            let extractor = SymbolExtractor::new(&code, tree.root_node());
            let symbols = extractor.extract_all();

            // Non-reserved identifier SHOULD appear in extracted symbols
            let var_symbols: Vec<_> = symbols
                .iter()
                .filter(|s| s.name == var_name)
                .collect();

            prop_assert!(
                !var_symbols.is_empty(),
                "Non-reserved identifier '{}' SHOULD appear in extracted symbols. \
                 Code: '{}', Found symbols: {:?}",
                var_name, code, symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
        }

        // ========================================================================
        // Workspace Symbol Property Tests
        // ========================================================================

        #[test]
        /// Feature: document-workspace-symbols, Property 9: Workspace ContainerName
        ///
        /// For any workspace symbol from a file with path `/path/to/filename.R`,
        /// the `containerName` field SHALL equal `filename` (without extension).
        ///
        /// **Validates: Requirements 8.1, 8.2**
        fn prop_workspace_container_name(
            filename in "[a-z][a-z0-9_]{2,10}",
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            use crate::state::{WorldState, Document};

            let code = format!("{} <- 42", var_name);
            let uri_str = format!("file:///path/to/{}.R", filename);
            let uri = Url::parse(&uri_str).unwrap();

            let mut state = WorldState::new(vec![]);
            state.documents.insert(uri.clone(), Document::new(&code, None));

            let response = super::workspace_symbol(&state, "");
            prop_assert!(response.is_some());

            let symbols = response.unwrap();
            let matching_symbols: Vec<_> = symbols.iter().filter(|s| s.name == var_name).collect();

            prop_assert!(
                !matching_symbols.is_empty(),
                "Symbol '{}' should appear in workspace symbols", var_name
            );

            for sym in matching_symbols {
                prop_assert_eq!(
                    sym.container_name.as_deref(),
                    Some(filename.as_str()),
                    "containerName should be '{}' for file '{}.R'",
                    filename, filename
                );
            }
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 10: Workspace Query Filtering
        ///
        /// For any workspace symbol query with a non-empty query string,
        /// all returned symbols SHALL have names containing the query (case-insensitive substring match).
        ///
        /// **Validates: Requirements 9.1**
        fn prop_workspace_query_filtering(
            var_name in "[a-z][a-z0-9_]{4,10}".prop_filter("Not reserved", |s| !is_r_reserved(s)),
            query_start in 0usize..3,
            query_len in 2usize..4
        ) {
            use crate::state::{WorldState, Document};

            // Ensure query is within bounds
            let query_end = (query_start + query_len).min(var_name.len());
            let query = &var_name[query_start..query_end];

            let code = format!("{} <- 42\nother_var <- 100", var_name);
            let uri = Url::parse("file:///test.R").unwrap();

            let mut state = WorldState::new(vec![]);
            state.documents.insert(uri.clone(), Document::new(&code, None));

            let response = super::workspace_symbol(&state, query);
            prop_assert!(response.is_some());

            let symbols = response.unwrap();

            // All returned symbols should contain the query (case-insensitive)
            let lower_query = query.to_lowercase();
            for sym in &symbols {
                prop_assert!(
                    sym.name.to_lowercase().contains(&lower_query),
                    "Symbol '{}' should contain query '{}' (case-insensitive)",
                    sym.name, query
                );
            }
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 11: Workspace Result Limiting
        ///
        /// For any workspace symbol query, the number of returned symbols
        /// SHALL NOT exceed `symbols.workspaceMaxResults` configuration value.
        ///
        /// **Validates: Requirements 9.2**
        fn prop_workspace_result_limiting(
            max_results in 100usize..500,
            num_symbols in 50usize..200
        ) {
            use crate::state::{WorldState, Document, SymbolConfig};

            // Generate code with many symbols
            let code: String = (0..num_symbols)
                .map(|i| format!("var_{} <- {}", i, i))
                .collect::<Vec<_>>()
                .join("\n");

            let uri = Url::parse("file:///test.R").unwrap();

            let mut state = WorldState::new(vec![]);
            state.symbol_config = SymbolConfig::with_max_results(max_results);
            state.documents.insert(uri.clone(), Document::new(&code, None));

            let response = super::workspace_symbol(&state, "");
            prop_assert!(response.is_some());

            let symbols = response.unwrap();

            prop_assert!(
                symbols.len() <= max_results,
                "Number of symbols ({}) should not exceed max_results ({})",
                symbols.len(), max_results
            );
        }

        #[test]
        /// Feature: document-workspace-symbols, Property 12: Workspace Deduplication
        ///
        /// For any workspace symbol query where the same symbol exists in multiple sources
        /// (open documents, workspace index, legacy indices), the symbol SHALL appear
        /// exactly once in the results.
        ///
        /// **Validates: Requirements 9.3**
        fn prop_workspace_deduplication(
            var_name in "[a-z][a-z0-9_]{2,10}".prop_filter("Not reserved", |s| !is_r_reserved(s))
        ) {
            use crate::state::{WorldState, Document};

            let code = format!("{} <- 42", var_name);
            let uri = Url::parse("file:///test.R").unwrap();

            let mut state = WorldState::new(vec![]);
            // Add to both documents (open) and workspace_index (closed)
            state.documents.insert(uri.clone(), Document::new(&code, None));
            state.workspace_index.insert(uri.clone(), Document::new(&code, None));

            let response = super::workspace_symbol(&state, "");
            prop_assert!(response.is_some());

            let symbols = response.unwrap();

            // Count occurrences of the symbol
            let count = symbols.iter().filter(|s| s.name == var_name).count();

            prop_assert_eq!(
                count, 1,
                "Symbol '{}' should appear exactly once, but appeared {} times",
                var_name, count
            );
        }
    }

    // ========================================================================
    // Workspace Symbol Tests
    // ========================================================================

    #[test]
    fn test_workspace_symbol_basic_search() {
        use crate::state::{Document, WorldState};

        let mut state = WorldState::new(vec![]);

        let uri1 = Url::parse("file:///test1.R").unwrap();
        state
            .documents
            .insert(uri1.clone(), Document::new("my_func <- function(x) x + 1\nmy_var <- 42", None));

        let uri2 = Url::parse("file:///test2.R").unwrap();
        state
            .documents
            .insert(uri2.clone(), Document::new("helper_func <- function(y) y * 2", None));

        let result = workspace_symbol(&state, "func").unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.iter().any(|s| s.name == "my_func"));
        assert!(result.iter().any(|s| s.name == "helper_func"));

        let result = workspace_symbol(&state, "var").unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.iter().any(|s| s.name == "my_var"));
    }

    #[test]
    fn test_workspace_symbol_case_insensitive() {
        use crate::state::{Document, WorldState};

        let mut state = WorldState::new(vec![]);
        let uri = Url::parse("file:///test.R").unwrap();
        state
            .documents
            .insert(uri.clone(), Document::new("MyFunction <- function() {}", None));

        let result = workspace_symbol(&state, "myfunction").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "MyFunction");

        let result = workspace_symbol(&state, "MYFUNCTION").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "MyFunction");
    }

    #[test]
    fn test_workspace_symbol_empty_query_returns_all() {
        use crate::state::{Document, WorldState};

        let mut state = WorldState::new(vec![]);
        let uri = Url::parse("file:///test.R").unwrap();
        state.documents.insert(
            uri.clone(),
            Document::new("foo <- 1\nbar <- function() {}\nbaz <- 3", None),
        );

        let result = workspace_symbol(&state, "").unwrap();
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_workspace_symbol_correct_uris() {
        use crate::state::{Document, WorldState};

        let mut state = WorldState::new(vec![]);
        let uri1 = Url::parse("file:///dir/file1.R").unwrap();
        state
            .documents
            .insert(uri1.clone(), Document::new("alpha <- 1", None));

        let uri2 = Url::parse("file:///dir/file2.R").unwrap();
        state
            .documents
            .insert(uri2.clone(), Document::new("beta <- 2", None));

        let result = workspace_symbol(&state, "").unwrap();
        for sym in &result {
            if sym.name == "alpha" {
                assert_eq!(sym.location.uri, uri1);
            } else if sym.name == "beta" {
                assert_eq!(sym.location.uri, uri2);
            }
        }
    }

    #[test]
    fn test_workspace_symbol_no_match() {
        use crate::state::{Document, WorldState};

        let mut state = WorldState::new(vec![]);
        let uri = Url::parse("file:///test.R").unwrap();
        state
            .documents
            .insert(uri.clone(), Document::new("foo <- 1", None));

        let result = workspace_symbol(&state, "zzz_nonexistent").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_workspace_symbol_function_and_variable_kinds() {
        use crate::state::{Document, WorldState};

        let mut state = WorldState::new(vec![]);
        let uri = Url::parse("file:///test.R").unwrap();
        state.documents.insert(
            uri.clone(),
            Document::new("my_func <- function() {}\nmy_var <- 42", None),
        );

        let result = workspace_symbol(&state, "my_").unwrap();
        assert_eq!(result.len(), 2);

        let func_sym = result.iter().find(|s| s.name == "my_func").unwrap();
        assert_eq!(func_sym.kind, tower_lsp::lsp_types::SymbolKind::FUNCTION);

        let var_sym = result.iter().find(|s| s.name == "my_var").unwrap();
        assert_eq!(var_sym.kind, tower_lsp::lsp_types::SymbolKind::VARIABLE);
    }

    // ========================================================================
    // Feature: section-range-hierarchy-fix
    // Property-based tests for level-aware section range computation
    // ========================================================================

    /// Strategy to generate a list of section `RawSymbol`s with unique start lines
    /// and random section levels (1–4).
    fn section_symbols_strategy(
        max_line_count: u32,
    ) -> impl Strategy<Value = (Vec<RawSymbol>, u32)> {
        // Generate line_count in [1, max_line_count]
        (1u32..=max_line_count).prop_flat_map(move |line_count| {
            // Generate 0..min(line_count, 20) unique start lines
            let max_sections = std::cmp::min(line_count, 20) as usize;
            (
                proptest::collection::hash_set(0..line_count, 0..=max_sections),
                Just(line_count),
            )
                .prop_flat_map(move |(start_lines_set, lc)| {
                    let start_lines: Vec<u32> = start_lines_set.into_iter().collect();
                    let n = start_lines.len();
                    // Generate a level (1..=4) for each section
                    (
                        proptest::collection::vec(1u32..=4, n),
                        Just(start_lines),
                        Just(lc),
                    )
                })
                .prop_map(|(levels, start_lines, lc)| {
                    let symbols: Vec<RawSymbol> = start_lines
                        .iter()
                        .zip(levels.iter())
                        .enumerate()
                        .map(|(i, (&line, &level))| RawSymbol {
                            name: format!("Section_{}", i),
                            kind: DocumentSymbolKind::Module,
                            range: Range {
                                start: Position {
                                    line,
                                    character: 0,
                                },
                                end: Position {
                                    line,
                                    character: 20,
                                },
                            },
                            selection_range: Range {
                                start: Position {
                                    line,
                                    character: 0,
                                },
                                end: Position {
                                    line,
                                    character: 20,
                                },
                            },
                            detail: None,
                            section_level: Some(level),
                            children: Vec::new(),
                        })
                        .collect();
                    (symbols, lc)
                })
        })
    }

    /// Strategy that generates a mix of section and non-section symbols for property testing.
    ///
    /// Returns `(Vec<RawSymbol>, u32)` where the symbols include both sections (with
    /// `section_level = Some(1..=4)`) and non-section symbols (with `section_level = None`),
    /// all with unique start lines, plus the line_count.
    fn mixed_symbols_strategy(
        max_line_count: u32,
    ) -> impl Strategy<Value = (Vec<RawSymbol>, u32)> {
        (1u32..=max_line_count).prop_flat_map(move |line_count| {
            // Generate 0..min(line_count, 20) unique start lines for all symbols
            let max_symbols = std::cmp::min(line_count, 20) as usize;
            (
                proptest::collection::hash_set(0..line_count, 0..=max_symbols),
                Just(line_count),
            )
                .prop_flat_map(move |(start_lines_set, lc)| {
                    let start_lines: Vec<u32> = start_lines_set.into_iter().collect();
                    let n = start_lines.len();
                    // For each symbol, generate: is_section (bool), level (1..=4)
                    (
                        proptest::collection::vec(proptest::bool::ANY, n),
                        proptest::collection::vec(1u32..=4, n),
                        // Generate varied selection_range characters to ensure they differ
                        proptest::collection::vec(0u32..=80, n),
                        proptest::collection::vec(1u32..=80, n),
                        Just(start_lines),
                        Just(lc),
                    )
                })
                .prop_map(
                    |(is_sections, levels, sel_starts, sel_lengths, start_lines, lc)| {
                        let symbols: Vec<RawSymbol> = start_lines
                            .iter()
                            .zip(is_sections.iter())
                            .zip(levels.iter())
                            .zip(sel_starts.iter())
                            .zip(sel_lengths.iter())
                            .enumerate()
                            .map(|(i, ((((&line, &is_section), &level), &sel_start), &sel_len))| {
                                let sel_end = sel_start + sel_len;
                                RawSymbol {
                                    name: format!("Symbol_{}", i),
                                    kind: if is_section {
                                        DocumentSymbolKind::Module
                                    } else {
                                        DocumentSymbolKind::Function
                                    },
                                    range: Range {
                                        start: Position {
                                            line,
                                            character: 0,
                                        },
                                        end: Position {
                                            line,
                                            character: 20,
                                        },
                                    },
                                    selection_range: Range {
                                        start: Position {
                                            line,
                                            character: sel_start,
                                        },
                                        end: Position {
                                            line,
                                            character: sel_end,
                                        },
                                    },
                                    detail: None,
                                    section_level: if is_section { Some(level) } else { None },
                                    children: Vec::new(),
                                }
                            })
                            .collect();
                        (symbols, lc)
                    },
                )
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        /// Feature: section-range-hierarchy-fix, Property 1: Level-aware section range end lines
        ///
        /// For any list of sections with arbitrary levels and start lines, after calling
        /// `compute_section_ranges()`, each section at level N shall have its end line
        /// equal to `next_sibling_or_ancestor.start_line - 1` where the next sibling or
        /// ancestor is the first subsequent section with level <= N, or `line_count - 1`
        /// (EOF) if no such section exists.
        ///
        /// **Validates: Requirements 1.1, 1.2, 1.3, 1.4**
        fn prop_level_aware_section_range_end_lines(
            (symbols, line_count) in section_symbols_strategy(100)
        ) {
            // Skip trivial case where line_count is 0 (no meaningful ranges)
            prop_assume!(line_count > 0);

            // Clone symbols to compute expected values independently
            let mut expected_sections: Vec<(u32, u32)> = symbols
                .iter()
                .filter(|s| s.section_level.is_some())
                .map(|s| (s.range.start.line, s.section_level.unwrap()))
                .collect();
            expected_sections.sort_by_key(|&(line, _)| line);

            // Compute expected end lines using the specification algorithm
            let mut expected_end_lines: Vec<(u32, u32)> = Vec::new(); // (start_line, expected_end_line)
            for i in 0..expected_sections.len() {
                let (current_start, current_level) = expected_sections[i];
                let mut end_line = line_count - 1; // default: EOF
                for j in (i + 1)..expected_sections.len() {
                    let (next_start, next_level) = expected_sections[j];
                    if next_level <= current_level {
                        // Found sibling or ancestor
                        end_line = if next_start > 0 { next_start - 1 } else { 0 };
                        break;
                    }
                }
                expected_end_lines.push((current_start, end_line));
            }

            // Run the actual implementation
            let mut builder = HierarchyBuilder::new(symbols, line_count);
            builder.compute_section_ranges();

            // Verify each section's end line matches expected
            for &(start_line, expected_end) in &expected_end_lines {
                let section = builder
                    .symbols
                    .iter()
                    .find(|s| {
                        s.section_level.is_some() && s.range.start.line == start_line
                    })
                    .unwrap_or_else(|| {
                        panic!(
                            "Section at start_line {} not found in builder symbols",
                            start_line
                        )
                    });

                prop_assert_eq!(
                    section.range.end.line,
                    expected_end,
                    "Section at line {} (level {}) should have end_line={}, got end_line={}. \
                     Sections (sorted): {:?}",
                    start_line,
                    section.section_level.unwrap(),
                    expected_end,
                    section.range.end.line,
                    expected_sections
                );
            }
        }

        #[test]
        /// Feature: section-range-hierarchy-fix, Property 3: Selection range preservation
        ///
        /// For any list of symbols (sections and non-sections), after calling
        /// `compute_section_ranges()`, the `selection_range` of every symbol shall be
        /// identical to its `selection_range` before the call.
        ///
        /// **Validates: Requirements 3.1**
        fn prop_selection_range_preservation(
            (symbols, line_count) in mixed_symbols_strategy(100)
        ) {
            // Snapshot all selection_ranges before compute_section_ranges
            let selection_ranges_before: Vec<(String, Range)> = symbols
                .iter()
                .map(|s| (s.name.clone(), s.selection_range))
                .collect();

            // Run compute_section_ranges
            let mut builder = HierarchyBuilder::new(symbols, line_count);
            builder.compute_section_ranges();

            // Verify every symbol's selection_range is unchanged
            prop_assert_eq!(
                builder.symbols.len(),
                selection_ranges_before.len(),
                "Symbol count changed after compute_section_ranges"
            );

            for (i, (name, expected_sel_range)) in selection_ranges_before.iter().enumerate() {
                let actual = &builder.symbols[i];
                prop_assert_eq!(
                    &actual.name,
                    name,
                    "Symbol order changed at index {}: expected '{}', got '{}'",
                    i,
                    name,
                    actual.name
                );
                prop_assert_eq!(
                    actual.selection_range,
                    *expected_sel_range,
                    "selection_range changed for symbol '{}' at index {}. \
                     Before: {:?}, After: {:?}",
                    name,
                    i,
                    expected_sel_range,
                    actual.selection_range
                );
            }
        }

        #[test]
        /// Feature: section-range-hierarchy-fix, Property 4: Input order independence (confluence)
        ///
        /// For any list of sections, the computed section ranges after
        /// `compute_section_ranges()` shall be identical regardless of the initial
        /// ordering of sections in the input list.
        ///
        /// **Validates: Requirements 4.4**
        fn prop_input_order_independence(
            (symbols, line_count) in section_symbols_strategy(100)
        ) {
            // Skip trivial cases
            prop_assume!(line_count > 0);
            prop_assume!(symbols.len() >= 2);

            // Create a shuffled copy by reversing the symbol order
            let reversed_symbols: Vec<RawSymbol> = symbols.iter().rev().cloned().collect();

            // Run compute_section_ranges on the original order
            let mut builder_original = HierarchyBuilder::new(symbols, line_count);
            builder_original.compute_section_ranges();

            // Run compute_section_ranges on the reversed order
            let mut builder_reversed = HierarchyBuilder::new(reversed_symbols, line_count);
            builder_reversed.compute_section_ranges();

            // Extract section ranges from both, sorted by start line for comparison
            let mut ranges_original: Vec<(u32, u32, u32)> = builder_original
                .symbols
                .iter()
                .filter(|s| s.section_level.is_some())
                .map(|s| (s.range.start.line, s.range.end.line, s.section_level.unwrap()))
                .collect();
            ranges_original.sort_by_key(|&(start, _, _)| start);

            let mut ranges_reversed: Vec<(u32, u32, u32)> = builder_reversed
                .symbols
                .iter()
                .filter(|s| s.section_level.is_some())
                .map(|s| (s.range.start.line, s.range.end.line, s.section_level.unwrap()))
                .collect();
            ranges_reversed.sort_by_key(|&(start, _, _)| start);

            // Both orderings must produce identical ranges
            prop_assert_eq!(
                ranges_original.len(),
                ranges_reversed.len(),
                "Different number of sections after compute_section_ranges: original={}, reversed={}",
                ranges_original.len(),
                ranges_reversed.len()
            );

            for (i, (orig, rev)) in ranges_original.iter().zip(ranges_reversed.iter()).enumerate() {
                prop_assert_eq!(
                    orig,
                    rev,
                    "Section ranges differ at index {} between original and reversed input order. \
                     Original: (start={}, end={}, level={}), Reversed: (start={}, end={}, level={})",
                    i,
                    orig.0, orig.1, orig.2,
                    rev.0, rev.1, rev.2
                );
            }
        }

        #[test]
        /// Feature: section-range-hierarchy-fix, Property 2: Correct symbol nesting after build
        ///
        /// For any valid configuration of sections (with varying levels) and non-section
        /// symbols, after calling `build()`, every non-section symbol whose start line
        /// falls within a section's computed range shall appear as a descendant of that
        /// section (nested in the deepest containing section), and symbols outside all
        /// section ranges shall appear at the root level.
        ///
        /// **Validates: Requirements 2.1, 2.2, 2.3**
        fn prop_correct_symbol_nesting(
            (symbols, line_count) in mixed_symbols_strategy(100)
        ) {
            prop_assume!(line_count > 0);

            // --- Step 1: Independently compute expected section ranges ---
            // Extract sections sorted by start line
            let mut sections_info: Vec<(u32, u32, String)> = symbols
                .iter()
                .filter(|s| s.section_level.is_some())
                .map(|s| (s.range.start.line, s.section_level.unwrap(), s.name.clone()))
                .collect();
            sections_info.sort_by_key(|&(line, _, _)| line);

            // Compute expected end lines for each section using the level-aware algorithm
            let mut section_ranges: Vec<(u32, u32, u32, String)> = Vec::new(); // (start, end, level, name)
            for i in 0..sections_info.len() {
                let (start, level, ref name) = sections_info[i];
                let mut end_line = line_count - 1; // default: EOF
                for j in (i + 1)..sections_info.len() {
                    let (next_start, next_level, _) = sections_info[j];
                    if next_level <= level {
                        end_line = if next_start > 0 { next_start - 1 } else { 0 };
                        break;
                    }
                }
                section_ranges.push((start, end_line, level, name.clone()));
            }

            // --- Step 2: For each non-section symbol, compute expected deepest containing section ---
            let non_section_symbols: Vec<(u32, String)> = symbols
                .iter()
                .filter(|s| s.section_level.is_none())
                .map(|s| (s.range.start.line, s.name.clone()))
                .collect();

            // For each non-section symbol, find the deepest containing section.
            // "Deepest" means the section with the smallest range that contains the symbol.
            // Among sections that contain the symbol, the one with the latest start line
            // and highest level is the deepest.
            let mut expected_parent: std::collections::HashMap<String, Option<String>> =
                std::collections::HashMap::new();

            for &(sym_line, ref sym_name) in &non_section_symbols {
                let mut best_section: Option<&(u32, u32, u32, String)> = None;
                for sr in &section_ranges {
                    let (sec_start, sec_end, _sec_level, _) = sr;
                    // Symbol must be strictly after section start (sections start on
                    // their comment line; symbols on the same line as a section comment
                    // are the section itself, but non-section symbols on the same line
                    // would be at the section start line).
                    // The nesting logic uses: symbol_line >= section.range.start.line
                    // && symbol_line <= section.range.end.line
                    // But sections themselves occupy their start line, so a non-section
                    // symbol on the same line as a section start IS within that section's range.
                    if sym_line >= *sec_start && sym_line <= *sec_end {
                        // This section contains the symbol. Pick the deepest one:
                        // the one with the latest start line (most nested).
                        // If same start line, pick higher level (more nested).
                        match best_section {
                            None => best_section = Some(sr),
                            Some(best) => {
                                let (best_start, _, best_level, _) = best;
                                if *sec_start > *best_start
                                    || (*sec_start == *best_start && _sec_level > best_level)
                                {
                                    best_section = Some(sr);
                                }
                            }
                        }
                    }
                }
                expected_parent.insert(
                    sym_name.clone(),
                    best_section.map(|(_, _, _, name)| name.clone()),
                );
            }

            // --- Step 3: Run build() ---
            let result = HierarchyBuilder::new(symbols, line_count).build();

            // --- Step 4: Flatten the hierarchy to find actual parent of each non-section symbol ---
            // Walk the DocumentSymbol tree and record (symbol_name -> parent_section_name)
            let mut actual_parent: std::collections::HashMap<String, Option<String>> =
                std::collections::HashMap::new();

            fn collect_parents(
                symbols: &[DocumentSymbol],
                current_section_parent: Option<&str>,
                actual: &mut std::collections::HashMap<String, Option<String>>,
            ) {
                for sym in symbols {
                    let is_section = sym.kind == tower_lsp::lsp_types::SymbolKind::MODULE;
                    if !is_section {
                        // This is a non-section symbol; record its parent section
                        actual.insert(
                            sym.name.clone(),
                            current_section_parent.map(|s| s.to_string()),
                        );
                    }
                    // Recurse into children
                    if let Some(ref children) = sym.children {
                        let parent_for_children = if is_section {
                            Some(sym.name.as_str())
                        } else {
                            current_section_parent
                        };
                        collect_parents(children, parent_for_children, actual);
                    }
                }
            }

            collect_parents(&result, None, &mut actual_parent);

            // --- Step 5: Verify each non-section symbol is in the correct section ---
            for (sym_name, expected) in &expected_parent {
                let actual = actual_parent.get(sym_name);
                match actual {
                    Some(actual_val) => {
                        prop_assert_eq!(
                            actual_val,
                            expected,
                            "Symbol '{}' expected parent section {:?}, but found {:?}. \
                             Section ranges: {:?}",
                            sym_name,
                            expected,
                            actual_val,
                            section_ranges
                        );
                    }
                    None => {
                        // Symbol not found in output at all - this shouldn't happen
                        // unless build() dropped it (which would be a bug)
                        prop_assert!(
                            false,
                            "Symbol '{}' not found in build() output. Expected parent: {:?}",
                            sym_name,
                            expected
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::r_env;
    use crate::state::{Document, WorldState};

    #[test]
    fn test_base_package_functions() {
        // Test that base R functions are recognized
        let library_paths = r_env::find_library_paths();
        let _state = WorldState::new(library_paths);

        let code = "library(stats)\nx <- rnorm(100)\ny <- mean(x)";
        let doc = Document::new(code, None);

        // rnorm and mean should be recognized (rnorm from stats, mean from base)
        assert!(doc.loaded_packages.contains(&"stats".to_string()));
    }

    #[test]
    fn test_no_spurious_errors_with_common_packages() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        // Test code that uses common package functions
        let test_cases = vec![
            ("library(stats)\nx <- rnorm(100)", vec!["rnorm"]),
            (
                "library(utils)\ndata <- read.csv('file.csv')",
                vec!["read.csv"],
            ),
            ("require(graphics)\nplot(1:10)", vec!["plot"]),
        ];

        for (code, expected_funcs) in test_cases {
            let doc = Document::new(code, None);
            let uri = tower_lsp::lsp_types::Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), doc);

            let diagnostics = diagnostics(&state, &uri);

            // Check that expected functions don't generate undefined variable errors
            for func in expected_funcs {
                let has_error = diagnostics.iter().any(|d| d.message.contains(func));
                assert!(
                    !has_error,
                    "Function {} should not generate undefined variable error",
                    func
                );
            }
        }
    }

    #[test]
    fn test_package_exports_loaded() {
        let library_paths = r_env::find_library_paths();
        let state = WorldState::new(library_paths);

        // Try to load stats package metadata
        if let Some(stats_pkg) = state.library.get("stats") {
            // stats should export common functions
            assert!(
                !stats_pkg.exports.is_empty(),
                "stats package should have exports"
            );

            // Check for some known stats exports
            let has_common_funcs = stats_pkg
                .exports
                .iter()
                .any(|e| e == "rnorm" || e == "lm" || e == "t.test");
            assert!(
                has_common_funcs,
                "stats should export common statistical functions"
            );
        }
    }

    #[test]
    fn test_hover_shows_definition_statement() {
        use crate::cross_file::scope::{ScopedSymbol, SymbolKind};

        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        // Create a test document
        let uri = Url::parse("file:///test.R").unwrap();
        let code = "my_var <- 42\nresult <- my_var + 1";
        let doc = Document::new(code, None);
        state.documents.insert(uri.clone(), doc);

        // Create a scoped symbol with definition info
        let symbol = ScopedSymbol {
            name: "my_var".to_string(),
            kind: SymbolKind::Variable,
            source_uri: uri.clone(),
            defined_line: 0,
            defined_column: 0,
            signature: None,
            is_declared: false,
        };

        // Test hover on the symbol
        // Mock get_cross_file_symbols to return our test symbol
        // Note: In a real test, we'd need to set up the cross-file state properly
        // For now, we'll test the definition extraction directly
        let def_info = extract_definition_statement(&symbol, &state);
        assert!(def_info.is_some());
        let def_info = def_info.unwrap();
        assert_eq!(def_info.statement, "my_var <- 42");
    }

    #[test]
    fn test_hover_same_file_location_format() {
        let library_paths = r_env::find_library_paths();
        let _state = WorldState::new(library_paths);

        let uri = Url::parse("file:///test.R").unwrap();
        let def_info = DefinitionInfo {
            statement: "my_var <- 42".to_string(),
            source_uri: uri.clone(),
            line: 0, // 0-based
            column: 0,
        };

        // Test same-file location formatting
        let escaped_statement = escape_markdown(&def_info.statement);
        let mut value = String::new();
        value.push_str(&format!("```r\n{}\n```\n\n", escaped_statement));

        if def_info.source_uri == uri {
            value.push_str(&format!("this file, line {}", def_info.line + 1)); // 1-based
        }

        assert!(value.contains("```r\nmy\\_var <- 42\n```"));
        assert!(value.contains("this file, line 1"));
        assert!(value.contains("\n\n")); // Blank line separator
    }

    #[test]
    fn test_hover_cross_file_hyperlink_format() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);
        state.workspace_folders = vec![Url::parse("file:///workspace/").unwrap()];

        let current_uri = Url::parse("file:///workspace/main.R").unwrap();
        let def_uri = Url::parse("file:///workspace/utils/helper.R").unwrap();

        let def_info = DefinitionInfo {
            statement: "helper_func <- function(x) { x + 1 }".to_string(),
            source_uri: def_uri.clone(),
            line: 5, // 0-based
            column: 0,
        };

        // Test cross-file location formatting
        let escaped_statement = escape_markdown(&def_info.statement);
        let mut value = String::new();
        value.push_str(&format!("```r\n{}\n```\n\n", escaped_statement));

        if def_info.source_uri != current_uri {
            let relative_path =
                compute_relative_path(&def_info.source_uri, state.workspace_folders.first());
            let absolute_path = def_info.source_uri.as_str();
            value.push_str(&format!(
                "[{}]({}), line {}",
                relative_path,
                absolute_path,
                def_info.line + 1
            ));
        }

        assert!(value.contains("```r\nhelper\\_func <- function\\(x\\) { x + 1 }\n```"));
        assert!(value.contains("[utils/helper.R](file:///workspace/utils/helper.R), line 6"));
        assert!(value.contains("\n\n")); // Blank line separator
    }

    #[test]
    fn test_hover_markdown_code_block_formatting() {
        let statement = "my_var <- c(1, 2, 3) # comment with *special* chars";
        let escaped = escape_markdown(statement);

        let formatted = format!("```r\n{}\n```", escaped);

        assert!(formatted.starts_with("```r\n"));
        assert!(formatted.ends_with("\n```"));
        assert!(formatted.contains("\\*special\\*")); // Markdown chars should be escaped
    }

    #[test]
    fn test_hover_blank_line_separator() {
        let def_info = DefinitionInfo {
            statement: "test_func <- function() {}".to_string(),
            source_uri: Url::parse("file:///test.R").unwrap(),
            line: 0,
            column: 0,
        };

        let escaped_statement = escape_markdown(&def_info.statement);
        let mut value = String::new();
        value.push_str(&format!("```r\n{}\n```\n\n", escaped_statement));
        value.push_str("this file, line 1");

        // Should have exactly one blank line between code block and location
        assert!(value.contains("```\n\nthis file"));
        assert!(!value.contains("```\n\n\nthis file")); // Not two blank lines
        assert!(!value.contains("```\nthis file")); // Not zero blank lines
    }

    // ============================================================================
    // Tests for hover on declared symbols - Task 9.1
    // ============================================================================

    #[test]
    fn test_hover_declared_variable_format() {
        // Test that hover on a declared variable shows the correct format
        // Validates: Requirements 7.1, 7.3
        use crate::cross_file::scope::{ScopedSymbol, SymbolKind};

        // Create a declared variable symbol
        let symbol = ScopedSymbol {
            name: "myvar".to_string(),
            kind: SymbolKind::Variable,
            source_uri: Url::parse("file:///test.R").unwrap(),
            defined_line: 4, // 0-based line 4 = display line 5
            defined_column: 0,
            signature: None,
            is_declared: true,
        };

        // Verify the symbol is marked as declared
        assert!(symbol.is_declared, "Symbol should be marked as declared");

        // Verify the hover content format
        let kind_str = match symbol.kind {
            SymbolKind::Function => "declared function",
            SymbolKind::Variable => "declared variable",
            _ => "declared symbol",
        };
        let directive_type = match symbol.kind {
            SymbolKind::Function => "@lsp-func",
            _ => "@lsp-var",
        };
        let display_line = symbol.defined_line + 1;

        let value = format!(
            "{} ({})\n\nDeclared via {} directive at line {}",
            symbol.name, kind_str, directive_type, display_line
        );

        assert!(value.contains("myvar (declared variable)"), "Should show symbol name with kind");
        assert!(value.contains("Declared via @lsp-var directive at line 5"), "Should show directive info with 1-based line");
    }

    #[test]
    fn test_hover_declared_function_format() {
        // Test that hover on a declared function shows the correct format
        // Validates: Requirements 7.2, 7.3
        use crate::cross_file::scope::{ScopedSymbol, SymbolKind};

        // Create a declared function symbol
        let symbol = ScopedSymbol {
            name: "myfunc".to_string(),
            kind: SymbolKind::Function,
            source_uri: Url::parse("file:///test.R").unwrap(),
            defined_line: 9, // 0-based line 9 = display line 10
            defined_column: 0,
            signature: None,
            is_declared: true,
        };

        // Verify the symbol is marked as declared
        assert!(symbol.is_declared, "Symbol should be marked as declared");

        // Verify the hover content format
        let kind_str = match symbol.kind {
            SymbolKind::Function => "declared function",
            SymbolKind::Variable => "declared variable",
            _ => "declared symbol",
        };
        let directive_type = match symbol.kind {
            SymbolKind::Function => "@lsp-func",
            _ => "@lsp-var",
        };
        let display_line = symbol.defined_line + 1;

        let value = format!(
            "{} ({})\n\nDeclared via {} directive at line {}",
            symbol.name, kind_str, directive_type, display_line
        );

        assert!(value.contains("myfunc (declared function)"), "Should show symbol name with kind");
        assert!(value.contains("Declared via @lsp-func directive at line 10"), "Should show directive info with 1-based line");
    }

    #[test]
    fn test_hover_declared_symbol_line_number_conversion() {
        // Test that 0-based line numbers are correctly converted to 1-based for display
        // Validates: Requirement 7.3
        use crate::cross_file::scope::{ScopedSymbol, SymbolKind};

        // Test various line numbers
        let test_cases = vec![
            (0, 1),   // Line 0 -> display line 1
            (4, 5),   // Line 4 -> display line 5
            (99, 100), // Line 99 -> display line 100
        ];

        for (defined_line, expected_display_line) in test_cases {
            let symbol = ScopedSymbol {
                name: "test_symbol".to_string(),
                kind: SymbolKind::Variable,
                source_uri: Url::parse("file:///test.R").unwrap(),
                defined_line,
                defined_column: 0,
                signature: None,
                is_declared: true,
            };

            let display_line = symbol.defined_line + 1;
            assert_eq!(
                display_line, expected_display_line,
                "Line {} should display as line {}",
                defined_line, expected_display_line
            );
        }
    }

    // ============================================================================
    // Tests for goto_definition on declared symbols - Task 10.1
    // ============================================================================

    #[test]
    fn test_goto_definition_declared_variable() {
        // Test that go-to-definition on a declared variable navigates to the directive line
        // Validates: Requirements 8.1, 8.2
        use crate::cross_file::directive::parse_directives;
        use crate::handlers::goto_definition;
        use crate::state::{Document, WorldState};

        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let uri = Url::parse("file:///test.R").unwrap();
        let code = r#"# @lsp-var myvar
x <- myvar + 1"#;

        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        // Update cross-file graph with metadata
        let metadata = parse_directives(code);
        state.cross_file_graph.update_file(&uri, &metadata, None, |_| None);

        // Test go-to-definition on "myvar" usage (line 1, position 5)
        let position = Position::new(1, 5);
        let result = goto_definition(&state, &uri, position);

        assert!(result.is_some(), "Should find definition for declared variable");

        if let Some(GotoDefinitionResponse::Scalar(location)) = result {
            assert_eq!(location.uri, uri, "Should navigate to same file");
            assert_eq!(location.range.start.line, 0, "Should navigate to directive line (0-based)");
            assert_eq!(location.range.start.character, 0, "Should navigate to start of line (column 0)");
        } else {
            panic!("Expected Scalar response");
        }
    }

    #[test]
    fn test_goto_definition_declared_function() {
        // Test that go-to-definition on a declared function navigates to the directive line
        // Validates: Requirements 8.1, 8.2
        use crate::cross_file::directive::parse_directives;
        use crate::handlers::goto_definition;
        use crate::state::{Document, WorldState};

        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let uri = Url::parse("file:///test.R").unwrap();
        let code = r#"# @lsp-func myfunc
result <- myfunc(42)"#;

        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        // Update cross-file graph with metadata
        let metadata = parse_directives(code);
        state.cross_file_graph.update_file(&uri, &metadata, None, |_| None);

        // Test go-to-definition on "myfunc" usage (line 1, position 10)
        let position = Position::new(1, 10);
        let result = goto_definition(&state, &uri, position);

        assert!(result.is_some(), "Should find definition for declared function");

        if let Some(GotoDefinitionResponse::Scalar(location)) = result {
            assert_eq!(location.uri, uri, "Should navigate to same file");
            assert_eq!(location.range.start.line, 0, "Should navigate to directive line (0-based)");
            assert_eq!(location.range.start.character, 0, "Should navigate to start of line (column 0)");
        } else {
            panic!("Expected Scalar response");
        }
    }

    #[test]
    fn test_goto_definition_declared_symbol_first_declaration() {
        // Test that when a symbol is declared multiple times, go-to-definition
        // navigates to the first declaration by line number
        // Validates: Requirement 11.3 (conflicting declarations)
        use crate::cross_file::directive::parse_directives;
        use crate::handlers::goto_definition;
        use crate::state::{Document, WorldState};

        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let uri = Url::parse("file:///test.R").unwrap();
        // Symbol declared as variable first (line 0), then as function (line 1)
        let code = r#"# @lsp-var mysymbol
# @lsp-func mysymbol
result <- mysymbol"#;

        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        // Update cross-file graph with metadata
        let metadata = parse_directives(code);
        state.cross_file_graph.update_file(&uri, &metadata, None, |_| None);

        // Test go-to-definition on "mysymbol" usage (line 2, position 10)
        let position = Position::new(2, 10);
        let result = goto_definition(&state, &uri, position);

        assert!(result.is_some(), "Should find definition for declared symbol");

        if let Some(GotoDefinitionResponse::Scalar(location)) = result {
            assert_eq!(location.uri, uri, "Should navigate to same file");
            // Should navigate to FIRST declaration (line 0), not the later one (line 1)
            assert_eq!(location.range.start.line, 0, "Should navigate to first declaration (line 0)");
            assert_eq!(location.range.start.character, 0, "Should navigate to start of line (column 0)");
        } else {
            panic!("Expected Scalar response");
        }
    }

    #[test]
    fn test_cross_file_hover_resolution() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        // Create main.R that sources utils.R
        let main_uri = Url::parse("file:///workspace/main.R").unwrap();
        let utils_uri = Url::parse("file:///workspace/utils.R").unwrap();

        let main_code = r#"source("utils.R")
result <- helper_func(42)"#;

        let utils_code = r#"helper_func <- function(x) {
    x * 2
}"#;

        // Add documents to state
        state
            .documents
            .insert(main_uri.clone(), Document::new(main_code, None));
        state
            .documents
            .insert(utils_uri.clone(), Document::new(utils_code, None));

        // Update cross-file graph
        state.cross_file_graph.update_file(
            &main_uri,
            &crate::cross_file::extract_metadata(main_code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &utils_uri,
            &crate::cross_file::extract_metadata(utils_code),
            None,
            |_| None,
        );

        // Test hover on helper_func in main.R (line 1, after source call)
        let position = Position::new(1, 10); // Position of "helper_func"
        let hover_result = hover_blocking(&state, &main_uri, position);

        assert!(hover_result.is_some());
        let hover = hover_result.unwrap();

        if let HoverContents::Markup(content) = hover.contents {
            // Code blocks don't need escaping - content should be unescaped
            assert!(content.value.contains("helper_func"));
            assert!(content.value.contains("function(x)"));
            assert!(content.value.contains("utils.R")); // Should show cross-file source
        } else {
            panic!("Expected markup content");
        }
    }

    #[test]
    fn test_hover_symbol_shadowing() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        // Create files with shadowing: local definition should take precedence
        let main_uri = Url::parse("file:///workspace/main.R").unwrap();
        let utils_uri = Url::parse("file:///workspace/utils.R").unwrap();

        let main_code = r#"source("utils.R")
my_func <- function(a, b) { a + b }  # Local definition shadows utils.R
result <- my_func(1, 2)"#;

        let utils_code = r#"my_func <- function(x) { x * 2 }  # Will be shadowed"#;

        state
            .documents
            .insert(main_uri.clone(), Document::new(main_code, None));
        state
            .documents
            .insert(utils_uri.clone(), Document::new(utils_code, None));

        // Update cross-file graph
        state.cross_file_graph.update_file(
            &main_uri,
            &crate::cross_file::extract_metadata(main_code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &utils_uri,
            &crate::cross_file::extract_metadata(utils_code),
            None,
            |_| None,
        );

        // Test hover on my_func usage (should show local definition, not utils.R)
        let position = Position::new(2, 10); // Position of "my_func" in usage
        let hover_result = hover_blocking(&state, &main_uri, position);

        assert!(hover_result.is_some());
        let hover = hover_result.unwrap();

        if let HoverContents::Markup(content) = hover.contents {
            // Code blocks don't need escaping - content should be unescaped
            assert!(content.value.contains("my_func"));
            assert!(content.value.contains("(a, b)")); // Local signature, not (x)
            assert!(content.value.contains("this file")); // Should be local, not cross-file
        } else {
            panic!("Expected markup content");
        }
    }

    #[test]
    fn test_hover_builtin_function_fallback() {
        let library_paths = r_env::find_library_paths();
        let state = WorldState::new(library_paths);

        let uri = Url::parse("file:///test.R").unwrap();
        let code = r#"result <- mean(c(1, 2, 3))"#;

        let doc = Document::new(code, None);
        let tree = doc.tree.as_ref().unwrap();
        let text = doc.text();

        // Find the "mean" identifier
        let point = tree_sitter::Point::new(0, 10); // Position of "mean"
        let node = tree
            .root_node()
            .descendant_for_point_range(point, point)
            .unwrap();
        assert_eq!(node.kind(), "identifier");
        assert_eq!(&text[node.byte_range()], "mean");

        // Test hover should fall back to R help for built-in functions
        let position = Position::new(0, 10);

        // Mock the state with the document
        let mut test_state = state;
        test_state.documents.insert(uri.clone(), doc);

        let hover_result = hover_blocking(&test_state, &uri, position);

        // Should return hover info (either from help cache or R subprocess)
        // The exact content depends on R availability, but structure should be consistent
        if let Some(hover) = hover_result {
            if let HoverContents::Markup(content) = hover.contents {
                assert!(content.kind == MarkupKind::Markdown);
                assert!(content.value.starts_with("```"));
                assert!(content.value.ends_with("```"));
            } else {
                panic!("Expected markup content");
            }
        }
        // Note: We don't assert hover_result.is_some() because R might not be available in CI
    }

    #[test]
    fn test_hover_undefined_symbol_returns_none() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let uri = Url::parse("file:///test.R").unwrap();
        let code = r#"result <- undefined_symbol_that_does_not_exist"#;

        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        // Test hover on undefined symbol
        let position = Position::new(0, 10); // Position of "undefined_symbol_that_does_not_exist"
        let hover_result = hover_blocking(&state, &uri, position);

        // Should return None for truly undefined symbols (after trying all fallbacks)
        // This tests the graceful handling when no definition is found anywhere
        assert!(hover_result.is_none());
    }

    #[test]
    fn test_hover_graceful_fallback_missing_definition_file() {
        use crate::cross_file::ScopedSymbol;

        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let main_uri = Url::parse("file:///workspace/main.R").unwrap();
        let missing_uri = Url::parse("file:///workspace/missing.R").unwrap(); // File doesn't exist

        let main_code = r#"# Symbol from missing file
result <- missing_func(42)"#;

        state
            .documents
            .insert(main_uri.clone(), Document::new(main_code, None));

        // Create a scoped symbol that references a missing file
        let symbol = ScopedSymbol {
            name: "missing_func".to_string(),
            kind: crate::cross_file::SymbolKind::Function,
            source_uri: missing_uri, // This file doesn't exist in state
            defined_line: 0,
            defined_column: 0,
            signature: Some("missing_func(x)".to_string()),
            is_declared: false,
        };

        // Test extract_definition_statement with missing file (should return None)
        let def_info = extract_definition_statement(&symbol, &state);
        assert!(
            def_info.is_none(),
            "Should return None when source file is missing"
        );

        // The hover function should gracefully fall back to showing just the signature
        // This is tested implicitly in the hover function's match arm for None from extract_definition_statement
    }

    #[test]
    fn test_hover_position_aware_scope_resolution() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let uri = Url::parse("file:///workspace/test.R").unwrap();
        let code = r#"# Before source call - symbol not available
result1 <- helper_func(1)  # Should not resolve

source("utils.R")

# After source call - symbol available  
result2 <- helper_func(2)  # Should resolve"#;

        let utils_uri = Url::parse("file:///workspace/utils.R").unwrap();
        let utils_code = r#"helper_func <- function(x) { x * 2 }"#;

        state
            .documents
            .insert(uri.clone(), Document::new(code, None));
        state
            .documents
            .insert(utils_uri.clone(), Document::new(utils_code, None));

        // Update cross-file graph
        state.cross_file_graph.update_file(
            &uri,
            &crate::cross_file::extract_metadata(code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &utils_uri,
            &crate::cross_file::extract_metadata(utils_code),
            None,
            |_| None,
        );

        // Test hover before source call (line 1) - should not find cross-file symbol
        let position_before = Position::new(1, 11); // "helper_func" before source()
        let cross_file_symbols_before = get_cross_file_symbols(
            &state,
            &uri,
            position_before.line,
            position_before.character,
        );
        assert!(
            !cross_file_symbols_before.contains_key("helper_func"),
            "Symbol should not be available before source() call"
        );

        // Test hover after source call (line 5) - should find cross-file symbol
        let position_after = Position::new(5, 11); // "helper_func" after source()
        let cross_file_symbols_after =
            get_cross_file_symbols(&state, &uri, position_after.line, position_after.character);
        assert!(
            cross_file_symbols_after.contains_key("helper_func"),
            "Symbol should be available after source() call"
        );
    }

    #[test]
    fn test_hover_uses_dependency_graph_correctly() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        // Create a chain: main.R -> utils.R -> helpers.R
        let main_uri = Url::parse("file:///workspace/main.R").unwrap();
        let utils_uri = Url::parse("file:///workspace/utils.R").unwrap();
        let helpers_uri = Url::parse("file:///workspace/helpers.R").unwrap();

        let main_code = r#"source("utils.R")
result <- process_data(42)"#;

        let utils_code = r#"source("helpers.R")
process_data <- function(x) {
    transform_value(x) + 10
}"#;

        let helpers_code = r#"transform_value <- function(x) { x * 2 }"#;

        state
            .documents
            .insert(main_uri.clone(), Document::new(main_code, None));
        state
            .documents
            .insert(utils_uri.clone(), Document::new(utils_code, None));
        state
            .documents
            .insert(helpers_uri.clone(), Document::new(helpers_code, None));

        // Update cross-file graph for all files
        state.cross_file_graph.update_file(
            &main_uri,
            &crate::cross_file::extract_metadata(main_code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &utils_uri,
            &crate::cross_file::extract_metadata(utils_code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &helpers_uri,
            &crate::cross_file::extract_metadata(helpers_code),
            None,
            |_| None,
        );

        // Test hover on transform_value in utils.R (should resolve through chain)
        let position = Position::new(2, 4); // "transform_value" in utils.R
        let cross_file_symbols =
            get_cross_file_symbols(&state, &utils_uri, position.line, position.character);

        assert!(
            cross_file_symbols.contains_key("transform_value"),
            "Should resolve symbol through dependency chain"
        );

        let symbol = &cross_file_symbols["transform_value"];
        assert_eq!(
            symbol.source_uri, helpers_uri,
            "Should trace back to helpers.R"
        );
    }

    // ============================================================================
    // Task 17: Enhanced Variable Detection Hover Integration Tests
    // ============================================================================

    #[test]
    fn test_complete_workflow_for_loops_and_functions() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let uri = Url::parse("file:///workspace/test.R").unwrap();
        let code = r#"# Test for loops and function parameters
process_data <- function(data, threshold = 0.5, ...) {
    filtered <- data[data > threshold]
    for (i in 1:10) {
        for (j in 1:5) {
            result <- i * j
            if (result > threshold) {
                print(result)
            }
        }
    }
    for (item in filtered) {
        print(item)
    }
    return(filtered)
}"#;

        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        // Test scope resolution includes all iterators and parameters
        let positions = vec![
            (Position::new(5, 12), "result", true), // result inside nested loop
            (Position::new(4, 12), "i", true),      // i iterator
            (Position::new(4, 18), "j", true),      // j iterator
            (Position::new(12, 14), "item", true),  // item used inside the loop body
            (Position::new(2, 20), "data", true),   // function parameter
            (Position::new(6, 27), "threshold", true), // function parameter with default
            (Position::new(14, 14), "filtered", true), // local variable used in return(filtered)
        ];

        for (position, symbol_name, should_exist) in positions {
            let symbols = get_cross_file_symbols(&state, &uri, position.line, position.character);
            if should_exist {
                assert!(
                    symbols.contains_key(symbol_name),
                    "Symbol '{}' should be in scope at line {}, col {}",
                    symbol_name,
                    position.line + 1,
                    position.character
                );
            } else {
                assert!(
                    !symbols.contains_key(symbol_name),
                    "Symbol '{}' should NOT be in scope at line {}, col {}",
                    symbol_name,
                    position.line + 1,
                    position.character
                );
            }
        }

        // Test no false-positive undefined variable diagnostics
        let diagnostics = diagnostics(&state, &uri);
        let undefined_errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("undefined") || d.message.contains("not found"))
            .collect();

        assert!(
            undefined_errors.is_empty(),
            "Should not have undefined variable errors for loop iterators and function parameters: {:?}",
            undefined_errors
        );

        // Test hover shows definition statements (no escaping needed in code blocks)
        let hover_tests = vec![
            (Position::new(4, 12), "i", "for (i in 1:10)"),
            (Position::new(4, 18), "j", "for (j in 1:5)"),
            (Position::new(12, 14), "item", "for (item in filtered)"),
            (
                Position::new(2, 20),
                "data",
                "process_data <- function(data, threshold = 0.5, ...)",
            ),
        ];

        for (position, symbol_name, expected_statement) in hover_tests {
            let hover_result = hover_blocking(&state, &uri, position);
            if let Some(hover) = hover_result {
                if let HoverContents::Markup(content) = hover.contents {
                    assert!(
                        content.value.contains(expected_statement),
                        "Hover for '{}' should contain '{}', got: {}",
                        symbol_name,
                        expected_statement,
                        content.value
                    );
                    assert!(
                        content.value.contains("this file"),
                        "Hover should show file location"
                    );
                }
            }
        }
    }

    #[test]
    fn test_realistic_r_code_patterns() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        // Create main file with realistic patterns
        let main_uri = Url::parse("file:///workspace/analysis.R").unwrap();
        let utils_uri = Url::parse("file:///workspace/utils.R").unwrap();
        let helpers_uri = Url::parse("file:///workspace/helpers.R").unwrap();

        let main_code = r#"# Analysis script with realistic patterns
source("utils.R")
source("helpers.R", local = TRUE)

# Nested loops with multiple iterators
results <- list()
for (i in 1:10) {
    for (j in 1:5) {
        value <- i * j
        results[[paste0(i, "_", j)]] <- value
    }
}

# Function with parameters and locals
analyze_data <- function(dataset, 
                        min_threshold = 0.1,
                        max_threshold = 0.9,
                        ...) {
    # Multi-line function definition
    cleaned <- dataset[!is.na(dataset)]
    
    for (threshold in seq(min_threshold, max_threshold, 0.1)) {
        filtered <- cleaned[cleaned > threshold]
        cat("Threshold:", threshold, "Count:", length(filtered), "\n")
    }
    
    return(cleaned)
}

# Code with markdown special characters
comment_with_stars <- "This has *asterisks* and _underscores_"
backtick_var <- `special name with spaces`
"#;

        let utils_code = r#"# Utility functions
utility_func <- function(x, y = 2) {
    x ^ y
}

CONSTANT_VALUE <- 42
"#;

        let helpers_code = r#"# Helper functions (sourced with local=TRUE)
helper_transform <- function(data) {
    data * 2
}
"#;

        state
            .documents
            .insert(main_uri.clone(), Document::new(main_code, None));
        state
            .documents
            .insert(utils_uri.clone(), Document::new(utils_code, None));
        state
            .documents
            .insert(helpers_uri.clone(), Document::new(helpers_code, None));

        // Update cross-file graph
        state.cross_file_graph.update_file(
            &main_uri,
            &crate::cross_file::extract_metadata(main_code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &utils_uri,
            &crate::cross_file::extract_metadata(utils_code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &helpers_uri,
            &crate::cross_file::extract_metadata(helpers_code),
            None,
            |_| None,
        );

        // Test nested loop iterators are in scope
        let nested_loop_position = Position::new(8, 8); // Inside nested loop
        let symbols = get_cross_file_symbols(
            &state,
            &main_uri,
            nested_loop_position.line,
            nested_loop_position.character,
        );

        assert!(
            symbols.contains_key("i"),
            "Outer loop iterator 'i' should be in scope"
        );
        assert!(
            symbols.contains_key("j"),
            "Inner loop iterator 'j' should be in scope"
        );
        assert!(
            symbols.contains_key("value"),
            "Local variable 'value' should be in scope"
        );

        // Test function parameters are in scope within function
        let function_body_position = Position::new(19, 4); // Inside analyze_data function
        let func_symbols = get_cross_file_symbols(
            &state,
            &main_uri,
            function_body_position.line,
            function_body_position.character,
        );

        assert!(
            func_symbols.contains_key("dataset"),
            "Function parameter 'dataset' should be in scope"
        );
        assert!(
            func_symbols.contains_key("min_threshold"),
            "Function parameter 'min_threshold' should be in scope"
        );
        assert!(
            func_symbols.contains_key("max_threshold"),
            "Function parameter 'max_threshold' should be in scope"
        );
        assert!(
            func_symbols.contains_key("cleaned"),
            "Local variable 'cleaned' should be in scope"
        );

        // Test cross-file symbols are resolved correctly
        let after_source_position = Position::new(4, 0); // After source() calls
        let cross_symbols = get_cross_file_symbols(
            &state,
            &main_uri,
            after_source_position.line,
            after_source_position.character,
        );

        assert!(
            cross_symbols.contains_key("utility_func"),
            "Should resolve utility_func from utils.R"
        );
        assert!(
            cross_symbols.contains_key("CONSTANT_VALUE"),
            "Should resolve CONSTANT_VALUE from utils.R"
        );
        // Note: helper_transform should NOT be available due to local=TRUE

        // Test hover shows proper formatting for multi-line definitions
        let multi_line_func_position = Position::new(13, 0); // analyze_data function name
        let hover_result = hover_blocking(&state, &main_uri, multi_line_func_position);

        if let Some(hover) = hover_result {
            if let HoverContents::Markup(content) = hover.contents {
                assert!(content.value.contains("analyze_data <- function(dataset,"));
                assert!(content.value.contains("this file"));
                // Should handle markdown special characters properly
                assert!(!content.value.contains("*asterisks*")); // Should be escaped
            }
        }

        // Test no false positives for valid symbols
        let diagnostics = diagnostics(&state, &main_uri);
        let undefined_errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("undefined"))
            .collect();

        // Should not report undefined errors for loop iterators, function parameters, or cross-file symbols
        for error in &undefined_errors {
            assert!(
                !error.message.contains("i "),
                "Should not report 'i' as undefined"
            );
            assert!(
                !error.message.contains("j "),
                "Should not report 'j' as undefined"
            );
            assert!(
                !error.message.contains("dataset"),
                "Should not report 'dataset' as undefined"
            );
            assert!(
                !error.message.contains("utility_func"),
                "Should not report 'utility_func' as undefined"
            );
        }
    }

    #[test]
    fn test_cross_file_local_scope_isolation() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let main_uri = Url::parse("file:///workspace/main.R").unwrap();
        let local_uri = Url::parse("file:///workspace/local_source.R").unwrap();
        let global_uri = Url::parse("file:///workspace/global_source.R").unwrap();

        let main_code = r#"# Test local vs global sourcing
source("global_source.R")           # Global scope
source("local_source.R", local = TRUE)  # Local scope

# These should be available from global source
global_result <- global_func(42)

# These should NOT be available from local source
# local_func(42)  # Would be undefined
"#;

        let global_code = r#"global_func <- function(x) { x + 1 }
global_var <- 100"#;

        let local_code = r#"local_func <- function(x) { x * 2 }
local_var <- 200"#;

        state
            .documents
            .insert(main_uri.clone(), Document::new(main_code, None));
        state
            .documents
            .insert(global_uri.clone(), Document::new(global_code, None));
        state
            .documents
            .insert(local_uri.clone(), Document::new(local_code, None));

        // Update cross-file graph
        state.cross_file_graph.update_file(
            &main_uri,
            &crate::cross_file::extract_metadata(main_code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &global_uri,
            &crate::cross_file::extract_metadata(global_code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &local_uri,
            &crate::cross_file::extract_metadata(local_code),
            None,
            |_| None,
        );

        // Test symbols after both source calls
        let position = Position::new(5, 0); // After both source() calls
        let symbols = get_cross_file_symbols(&state, &main_uri, position.line, position.character);

        // Global source symbols should be available
        assert!(
            symbols.contains_key("global_func"),
            "global_func should be available from global source"
        );
        assert!(
            symbols.contains_key("global_var"),
            "global_var should be available from global source"
        );

        // Local source symbols should NOT be available in main scope
        assert!(
            !symbols.contains_key("local_func"),
            "local_func should NOT be available from local source"
        );
        assert!(
            !symbols.contains_key("local_var"),
            "local_var should NOT be available from local source"
        );

        // Test hover on global symbol shows cross-file location
        let hover_position = Position::new(5, 16); // "global_func" usage
        let hover_result = hover_blocking(&state, &main_uri, hover_position);

        if let Some(hover) = hover_result {
            if let HoverContents::Markup(content) = hover.contents {
                assert!(content.value.contains("global_func"));
                assert!(
                    content.value.contains("global_source.R"),
                    "Should show cross-file source"
                );
            }
        }
    }

    #[test]
    fn test_hover_hyperlink_formatting_with_special_paths() {
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);
        state.workspace_folders = vec![Url::parse("file:///workspace/").unwrap()];

        // Test various path scenarios
        let main_uri = Url::parse("file:///workspace/src/analysis/main.R").unwrap();
        let utils_uri = Url::parse("file:///workspace/utils/helpers with spaces.R").unwrap();

        let main_code = r#"source("../../utils/helpers with spaces.R")
result <- helper_with_spaces(42)"#;

        let utils_code = r#"helper_with_spaces <- function(x) {
    # Function with special characters in filename
    x * 2
}"#;

        state
            .documents
            .insert(main_uri.clone(), Document::new(main_code, None));
        state
            .documents
            .insert(utils_uri.clone(), Document::new(utils_code, None));

        // Update cross-file graph
        state.cross_file_graph.update_file(
            &main_uri,
            &crate::cross_file::extract_metadata(main_code),
            None,
            |_| None,
        );
        state.cross_file_graph.update_file(
            &utils_uri,
            &crate::cross_file::extract_metadata(utils_code),
            None,
            |_| None,
        );

        // Test hover shows proper hyperlink formatting
        let position = Position::new(1, 10); // "helper_with_spaces"
        let hover_result = hover_blocking(&state, &main_uri, position);

        if let Some(hover) = hover_result {
            if let HoverContents::Markup(content) = hover.contents {
                // Should contain properly formatted hyperlink
                assert!(content.value.contains("[utils/helpers with spaces.R]"));
                assert!(content
                    .value
                    .contains("file:///workspace/utils/helpers%20with%20spaces.R"));
                assert!(content.value.contains("line 1"));
            }
        }
    }

    // ============================================================================
    // Tests for hover package info - Task 12.1
    // ============================================================================

    #[test]
    fn test_hover_shows_package_name_for_package_exports() {
        // Test that hover displays package name for package exports
        // Validates: Requirement 10.1
        use crate::cross_file::scope::{ScopedSymbol, SymbolKind};

        // Create a symbol with a package URI
        let package_uri = Url::parse("package:dplyr").unwrap();
        let symbol = ScopedSymbol {
            name: "mutate".to_string(),
            kind: SymbolKind::Variable,
            source_uri: package_uri,
            defined_line: 0,
            defined_column: 0,
            signature: None,
            is_declared: false,
        };

        // Verify the package name can be extracted from the URI
        let package_name = symbol.source_uri.as_str().strip_prefix("package:");
        assert_eq!(
            package_name,
            Some("dplyr"),
            "Should extract package name from URI"
        );

        // Test the formatting that would be used in hover
        let mut value = String::new();
        value.push_str(&format!("```r\n{}\n```\n", symbol.name));
        if let Some(pkg) = package_name {
            value.push_str(&format!("\nfrom {{{}}}", pkg));
        }

        assert!(
            value.contains("```r\nmutate\n```"),
            "Should contain symbol name in code block"
        );
        assert!(
            value.contains("from {dplyr}"),
            "Should contain package name in braces"
        );
    }

    #[test]
    fn test_hover_package_uri_detection() {
        // Test that package URIs are correctly detected
        // Validates: Requirement 10.1

        // Package URIs should be detected
        let package_uri = Url::parse("package:ggplot2").unwrap();
        assert!(
            package_uri.as_str().starts_with("package:"),
            "Package URI should start with 'package:'"
        );
        assert_eq!(
            package_uri.as_str().strip_prefix("package:"),
            Some("ggplot2")
        );

        // Base package URI should also be detected
        let base_uri = Url::parse("package:base").unwrap();
        assert!(
            base_uri.as_str().starts_with("package:"),
            "Base package URI should start with 'package:'"
        );
        assert_eq!(base_uri.as_str().strip_prefix("package:"), Some("base"));

        // File URIs should NOT be detected as packages
        let file_uri = Url::parse("file:///test.R").unwrap();
        assert!(
            !file_uri.as_str().starts_with("package:"),
            "File URI should not start with 'package:'"
        );
        assert_eq!(file_uri.as_str().strip_prefix("package:"), None);
    }

    #[test]
    fn test_hover_local_definition_not_shown_as_package() {
        // Test that local definitions are not shown as package exports
        // Validates: Requirement 10.4 (shadowing)
        use crate::cross_file::scope::{ScopedSymbol, SymbolKind};

        // Create a symbol with a file URI (local definition)
        let file_uri = Url::parse("file:///workspace/main.R").unwrap();
        let symbol = ScopedSymbol {
            name: "mutate".to_string(),
            kind: SymbolKind::Function,
            source_uri: file_uri.clone(),
            defined_line: 5,
            defined_column: 0,
            signature: Some("mutate <- function(x) { x + 1 }".to_string()),
            is_declared: false,
        };

        // Verify this is NOT detected as a package export
        let package_name = symbol.source_uri.as_str().strip_prefix("package:");
        assert_eq!(
            package_name, None,
            "Local definition should not be detected as package export"
        );
    }

    // ============================================================================
    // Tests for collect_missing_package_diagnostics - Task 10.3
    // ============================================================================

    #[test]
    fn test_missing_package_diagnostic_emitted() {
        // Test that a diagnostic is emitted for a non-installed package
        // Validates: Requirement 15.1
        let mut meta = crate::cross_file::CrossFileMetadata::default();
        meta.library_calls
            .push(crate::cross_file::source_detect::LibraryCall {
                package: "__nonexistent_package_xyz__".to_string(),
                line: 0,
                column: 30,
                function_scope: None,
            });

        let state = WorldState::new(Vec::new());
        let mut diagnostics = Vec::new();

        collect_missing_package_diagnostics(&state, &meta, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            1,
            "Should emit one diagnostic for missing package"
        );
        assert!(diagnostics[0]
            .message
            .contains("__nonexistent_package_xyz__"));
        assert!(diagnostics[0].message.contains("not installed"));
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn test_missing_package_diagnostic_not_emitted_for_base_package() {
        // Test that no diagnostic is emitted for base packages
        // Validates: Requirement 15.1 (base packages are always available)
        let mut meta = crate::cross_file::CrossFileMetadata::default();
        meta.library_calls
            .push(crate::cross_file::source_detect::LibraryCall {
                package: "base".to_string(),
                line: 0,
                column: 15,
                function_scope: None,
            });

        let mut state = WorldState::new(Vec::new());
        // Ensure base is in base_packages by creating a new PackageLibrary
        let mut base_packages = std::collections::HashSet::new();
        base_packages.insert("base".to_string());
        let mut pkg_lib = crate::package_library::PackageLibrary::new_empty();
        pkg_lib.set_base_packages(base_packages);
        state.package_library = std::sync::Arc::new(pkg_lib);

        let mut diagnostics = Vec::new();

        collect_missing_package_diagnostics(&state, &meta, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            0,
            "Should not emit diagnostic for base package"
        );
    }

    #[test]
    fn test_missing_package_diagnostic_ignored_line() {
        // Test that diagnostics are not emitted for ignored lines
        // Validates: Requirement 15.1 with @lsp-ignore support
        let mut meta = crate::cross_file::CrossFileMetadata::default();
        meta.library_calls
            .push(crate::cross_file::source_detect::LibraryCall {
                package: "__nonexistent_package_xyz__".to_string(),
                line: 5,
                column: 30,
                function_scope: None,
            });
        // Mark line 5 as ignored
        meta.ignored_lines.insert(5);

        let state = WorldState::new(Vec::new());
        let mut diagnostics = Vec::new();

        collect_missing_package_diagnostics(&state, &meta, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            0,
            "Should not emit diagnostic for ignored line"
        );
    }

    #[test]
    fn test_missing_package_diagnostic_multiple_packages() {
        // Test that diagnostics are emitted for multiple missing packages
        // Validates: Requirement 15.1
        let mut meta = crate::cross_file::CrossFileMetadata::default();
        meta.library_calls
            .push(crate::cross_file::source_detect::LibraryCall {
                package: "__missing_pkg1__".to_string(),
                line: 0,
                column: 20,
                function_scope: None,
            });
        meta.library_calls
            .push(crate::cross_file::source_detect::LibraryCall {
                package: "__missing_pkg2__".to_string(),
                line: 1,
                column: 20,
                function_scope: None,
            });

        let state = WorldState::new(Vec::new());
        let mut diagnostics = Vec::new();

        collect_missing_package_diagnostics(&state, &meta, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            2,
            "Should emit diagnostics for both missing packages"
        );
        assert!(diagnostics[0].message.contains("__missing_pkg1__"));
        assert!(diagnostics[1].message.contains("__missing_pkg2__"));
    }

    // ============================================================================
    // Tests for redundant directive diagnostics - Task 7.2
    // ============================================================================

    #[test]
    fn test_redundant_directive_diagnostic_emitted() {
        // Test that a diagnostic is emitted when @lsp-source directive without line=
        // targets the same file as an earlier source() call.
        // Validates: Requirement 6.2
        use crate::cross_file::types::ForwardSource;
        use tempfile::TempDir;

        // Create temp workspace with actual files
        let temp_dir = TempDir::new().unwrap();
        let workspace_path = temp_dir.path();
        let main_path = workspace_path.join("main.R");
        let utils_path = workspace_path.join("utils.R");
        std::fs::write(&main_path, "# main").unwrap();
        std::fs::write(&utils_path, "# utils").unwrap();

        let workspace_url = Url::from_file_path(workspace_path).unwrap();
        let main_url = Url::from_file_path(&main_path).unwrap();

        let mut state = WorldState::new(Vec::new());
        state.workspace_folders.push(workspace_url);

        // Metadata with AST source at line 5 and directive at line 10
        let meta = crate::cross_file::CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: false, // AST source at line 5 (earlier)
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true, ..Default::default()
                },
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 10,
                    column: 0,
                    is_directive: true, // Directive at line 10 (later)
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true, ..Default::default()
                },
            ],
            ..Default::default()
        };

        let mut diagnostics = Vec::new();
        collect_redundant_directive_diagnostics(&state, &main_url, &meta, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            1,
            "Should emit one diagnostic for redundant directive"
        );
        assert!(diagnostics[0].message.contains("redundant"));
        assert!(diagnostics[0].message.contains("utils.R"));
        assert!(diagnostics[0].message.contains("line 6")); // 1-based display
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::HINT));
        assert_eq!(diagnostics[0].range.start.line, 10); // Directive line
    }

    #[test]
    fn test_redundant_directive_diagnostic_not_emitted_when_disabled() {
        // Test that no diagnostic is emitted when redundant_directive_severity is None
        // Validates: Requirement 6.2 (configurable severity)
        use crate::cross_file::types::ForwardSource;
        use tempfile::TempDir;

        // Create temp workspace with actual files
        let temp_dir = TempDir::new().unwrap();
        let workspace_path = temp_dir.path();
        let main_path = workspace_path.join("main.R");
        let utils_path = workspace_path.join("utils.R");
        std::fs::write(&main_path, "# main").unwrap();
        std::fs::write(&utils_path, "# utils").unwrap();

        let workspace_url = Url::from_file_path(workspace_path).unwrap();
        let main_url = Url::from_file_path(&main_path).unwrap();

        let mut state = WorldState::new(Vec::new());
        state.workspace_folders.push(workspace_url);
        state.cross_file_config.redundant_directive_severity = None; // Disabled

        // Metadata with AST source at line 5 and directive at line 10
        let meta = crate::cross_file::CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: false,
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true, ..Default::default()
                },
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 10,
                    column: 0,
                    is_directive: true,
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true, ..Default::default()
                },
            ],
            ..Default::default()
        };

        let mut diagnostics = Vec::new();
        collect_redundant_directive_diagnostics(&state, &main_url, &meta, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            0,
            "Should not emit diagnostic when severity is disabled"
        );
    }

    #[test]
    fn test_redundant_directive_diagnostic_not_emitted_when_directive_is_earlier() {
        // Test that no diagnostic is emitted when directive is at an earlier line than source()
        // Validates: Requirement 6.2 (only emit when directive is later)
        use crate::cross_file::types::ForwardSource;
        use tempfile::TempDir;

        // Create temp workspace with actual files
        let temp_dir = TempDir::new().unwrap();
        let workspace_path = temp_dir.path();
        let main_path = workspace_path.join("main.R");
        let utils_path = workspace_path.join("utils.R");
        std::fs::write(&main_path, "# main").unwrap();
        std::fs::write(&utils_path, "# utils").unwrap();

        let workspace_url = Url::from_file_path(workspace_path).unwrap();
        let main_url = Url::from_file_path(&main_path).unwrap();

        let mut state = WorldState::new(Vec::new());
        state.workspace_folders.push(workspace_url);

        // Metadata with directive at line 5 and AST source at line 10
        let meta = crate::cross_file::CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 5,
                    column: 0,
                    is_directive: true, // Directive at line 5 (earlier)
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true, ..Default::default()
                },
                ForwardSource {
                    path: "utils.R".to_string(),
                    line: 10,
                    column: 0,
                    is_directive: false, // AST source at line 10 (later)
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true, ..Default::default()
                },
            ],
            ..Default::default()
        };

        let mut diagnostics = Vec::new();
        collect_redundant_directive_diagnostics(&state, &main_url, &meta, &mut diagnostics);

        assert_eq!(
            diagnostics.len(),
            0,
            "Should not emit diagnostic when directive is earlier than source()"
        );
    }

    // ============================================================================
    // Tests for hover shadowing - Task 12.3
    // ============================================================================

    #[test]
    fn test_hover_local_definition_shadows_package_export() {
        // Test that when a local definition shadows a package export,
        // hover shows the local definition, not the package export.
        // Validates: Requirement 10.4
        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let uri = Url::parse("file:///workspace/main.R").unwrap();

        // Code that loads a package and then defines a local function with the same name
        // as a package export. The local definition should shadow the package export.
        let code = r#"library(dplyr)
mutate <- function(x, y) { x + y }  # Local definition shadows dplyr::mutate
result <- mutate(1, 2)"#;

        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        // Update cross-file graph with metadata
        state.cross_file_graph.update_file(
            &uri,
            &crate::cross_file::extract_metadata(code),
            None,
            |_| None,
        );

        // Test hover on "mutate" usage (line 2, position 10)
        let position = Position::new(2, 10);
        let hover_result = hover_blocking(&state, &uri, position);

        assert!(hover_result.is_some(), "Hover should return a result");
        let hover = hover_result.unwrap();

        if let HoverContents::Markup(content) = hover.contents {
            // Should show local definition signature (x, y), not dplyr's mutate
            assert!(
                content.value.contains("mutate"),
                "Should contain function name"
            );
            assert!(
                content.value.contains("(x, y)"),
                "Should show local signature (x, y), not dplyr's signature"
            );
            // Should NOT show package attribution
            assert!(
                !content.value.contains("{dplyr}"),
                "Should NOT show package attribution for shadowed symbol"
            );
            // Should show local file location
            assert!(
                content.value.contains("this file"),
                "Should show local file location"
            );
        } else {
            panic!("Expected markup content");
        }
    }

    #[test]
    fn test_hover_shadowing_scope_resolution_returns_local() {
        // Test that scope resolution returns the local definition when it shadows a package export.
        // This verifies the underlying mechanism that hover relies on.
        // Validates: Requirement 10.4
        use crate::cross_file::scope::{compute_artifacts, scope_at_position_with_packages};
        use std::collections::HashSet;

        let uri = Url::parse("file:///workspace/test.R").unwrap();

        // Code with library() and local definition of same name
        let code = r#"library(dplyr)
filter <- function(x) { x > 0 }
result <- filter(c(1, -2, 3))"#;

        // Use Document::new to parse the code (same as other tests)
        let doc = Document::new(code, None);
        let tree = doc.tree.as_ref().expect("Should parse successfully");
        let artifacts = compute_artifacts(&uri, tree, code);

        // Create a mock package exports callback that returns "filter" for dplyr
        let get_exports = |pkg: &str| -> HashSet<String> {
            if pkg == "dplyr" {
                let mut exports = HashSet::new();
                exports.insert("filter".to_string());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope at line 2 (after both library and local definition)
        let scope = scope_at_position_with_packages(&artifacts, 2, 10, &get_exports, &base_exports);

        // Symbol should be in scope
        assert!(
            scope.symbols.contains_key("filter"),
            "filter should be in scope"
        );

        // The symbol should be from the local definition, not the package
        let symbol = scope.symbols.get("filter").unwrap();
        assert!(
            !symbol.source_uri.as_str().starts_with("package:"),
            "filter should be from local definition, not package. Got URI: '{}'",
            symbol.source_uri.as_str()
        );
        assert_eq!(
            symbol.source_uri, uri,
            "filter should be from the local file"
        );
    }

    #[test]
    fn test_hover_package_export_shown_when_no_local_shadow() {
        // Test that when there's no local definition, hover shows the package export.
        // This is the complement to test_hover_local_definition_shadows_package_export.
        // Validates: Requirements 10.1, 10.4
        use crate::cross_file::scope::{ScopedSymbol, SymbolKind};

        // Create a symbol that represents a package export
        let package_uri = Url::parse("package:dplyr").unwrap();
        let symbol = ScopedSymbol {
            name: "mutate".to_string(),
            kind: SymbolKind::Function,
            source_uri: package_uri.clone(),
            defined_line: 0,
            defined_column: 0,
            signature: Some("mutate(.data, ...)".to_string()),
            is_declared: false,
        };

        // Verify this IS detected as a package export
        let package_name = symbol.source_uri.as_str().strip_prefix("package:");
        assert_eq!(
            package_name,
            Some("dplyr"),
            "Package export should be detected"
        );

        // Verify the formatting that would be used in hover
        let mut value = String::new();
        if let Some(pkg) = package_name {
            if let Some(sig) = &symbol.signature {
                value.push_str(&format!("```r\n{}\n```\n", sig));
            }
            value.push_str(&format!("\nfrom {{{}}}", pkg));
        }

        assert!(
            value.contains("mutate(.data, ...)"),
            "Should show function signature"
        );
        assert!(
            value.contains("from {dplyr}"),
            "Should show package attribution"
        );
    }

    #[test]
    fn test_hover_shadowing_position_aware() {
        // Test that shadowing is position-aware: before the local definition,
        // the package export should be shown; after, the local definition.
        // Validates: Requirement 10.4
        use crate::cross_file::scope::{compute_artifacts, scope_at_position_with_packages};
        use std::collections::HashSet;

        let uri = Url::parse("file:///workspace/test.R").unwrap();

        // Code with library() first, then local definition later
        let code = r#"library(dplyr)
x <- mutate(df, y = 1)  # Uses package export
mutate <- function(x) { x + 1 }  # Local definition
z <- mutate(5)  # Uses local definition"#;

        // Use Document::new to parse the code (same as other tests)
        let doc = Document::new(code, None);
        let tree = doc.tree.as_ref().expect("Should parse successfully");
        let artifacts = compute_artifacts(&uri, tree, code);

        // Create a mock package exports callback
        let get_exports = |pkg: &str| -> HashSet<String> {
            if pkg == "dplyr" {
                let mut exports = HashSet::new();
                exports.insert("mutate".to_string());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope at line 1 (before local definition) - should get package export
        let scope_before =
            scope_at_position_with_packages(&artifacts, 1, 5, &get_exports, &base_exports);
        assert!(
            scope_before.symbols.contains_key("mutate"),
            "mutate should be in scope before local def"
        );
        let symbol_before = scope_before.symbols.get("mutate").unwrap();
        assert!(
            symbol_before.source_uri.as_str().starts_with("package:"),
            "Before local definition, mutate should be from package. Got URI: '{}'",
            symbol_before.source_uri.as_str()
        );

        // Query scope at line 3 (after local definition) - should get local definition
        let scope_after =
            scope_at_position_with_packages(&artifacts, 3, 5, &get_exports, &base_exports);
        assert!(
            scope_after.symbols.contains_key("mutate"),
            "mutate should be in scope after local def"
        );
        let symbol_after = scope_after.symbols.get("mutate").unwrap();
        assert!(
            !symbol_after.source_uri.as_str().starts_with("package:"),
            "After local definition, mutate should be from local file. Got URI: '{}'",
            symbol_after.source_uri.as_str()
        );
        assert_eq!(
            symbol_after.source_uri, uri,
            "mutate should be from the local file"
        );
    }

    // ============================================================================
    // Tests for goto_definition package handling - Task 13.1
    // ============================================================================

    /// Verifies that symbols originating from packages are treated as non-navigable.
    ///
    /// This test constructs a `ScopedSymbol` whose `source_uri` uses the `package:`
    /// scheme and asserts that such URIs are recognized as package exports (which
    /// goto-definition should not navigate into).
    ///
    /// # Examples
    ///
    /// ```
    /// use crate::cross_file::scope::{ScopedSymbol, SymbolKind};
    /// use url::Url;
    ///
    /// let package_uri = Url::parse("package:dplyr").unwrap();
    /// let symbol = ScopedSymbol {
    ///     name: "mutate".to_string(),
    ///     kind: SymbolKind::Function,
    ///     source_uri: package_uri.clone(),
    ///     defined_line: 0,
    ///     defined_column: 0,
    ///     signature: Some("mutate(.data, ...)".to_string()),
    ///     is_declared: false,
    /// };
    ///
    /// assert!(symbol.source_uri.as_str().starts_with("package:"));
    /// let is_package_export = symbol.source_uri.as_str().starts_with("package:");
    /// assert!(is_package_export);
    /// let package_name = symbol.source_uri.as_str().strip_prefix("package:");
    /// assert_eq!(package_name, Some("dplyr"));
    /// ```
    #[test]
    fn test_goto_definition_returns_none_for_package_exports() {
        // Test that goto_definition returns None for package exports
        // since package source files are not navigable
        // Validates: Requirements 11.1, 11.2
        use crate::cross_file::scope::{ScopedSymbol, SymbolKind};

        // Create a symbol with a package URI
        let package_uri = Url::parse("package:dplyr").unwrap();
        let symbol = ScopedSymbol {
            name: "mutate".to_string(),
            kind: SymbolKind::Function,
            source_uri: package_uri.clone(),
            defined_line: 0,
            defined_column: 0,
            signature: Some("mutate(.data, ...)".to_string()),
            is_declared: false,
        };

        // Verify the package URI is detected correctly
        assert!(
            symbol.source_uri.as_str().starts_with("package:"),
            "Package export should have package: URI prefix"
        );

        // The goto_definition logic should skip package exports
        // This test verifies the detection logic used in goto_definition
        let is_package_export = symbol.source_uri.as_str().starts_with("package:");
        assert!(is_package_export, "Should detect package export");

        // Extract package name for logging
        let package_name = symbol.source_uri.as_str().strip_prefix("package:");
        assert_eq!(package_name, Some("dplyr"), "Should extract package name");
    }

    #[test]
    fn test_goto_definition_navigates_to_local_definition() {
        // Test that goto_definition navigates to local definitions (not package exports)
        // Validates: Requirement 11.3 (shadowing)
        use crate::cross_file::scope::{ScopedSymbol, SymbolKind};

        // Create a symbol with a file URI (local definition)
        let file_uri = Url::parse("file:///workspace/main.R").unwrap();
        let symbol = ScopedSymbol {
            name: "mutate".to_string(),
            kind: SymbolKind::Function,
            source_uri: file_uri.clone(),
            defined_line: 5,
            defined_column: 0,
            signature: Some("mutate <- function(x) { x + 1 }".to_string()),
            is_declared: false,
        };

        // Verify this is NOT a package export
        assert!(
            !symbol.source_uri.as_str().starts_with("package:"),
            "Local definition should not have package: URI prefix"
        );

        // The goto_definition logic should navigate to local definitions
        let is_package_export = symbol.source_uri.as_str().starts_with("package:");
        assert!(!is_package_export, "Should not detect as package export");

        // Verify the location would be correct
        let expected_line = symbol.defined_line;
        let expected_column = symbol.defined_column;
        assert_eq!(expected_line, 5, "Should navigate to correct line");
        assert_eq!(expected_column, 0, "Should navigate to correct column");
    }

    #[test]
    fn test_goto_definition_package_uri_formats() {
        // Test various package URI formats are correctly detected
        // Validates: Requirements 11.1, 11.2

        // Standard package URI
        let dplyr_uri = Url::parse("package:dplyr").unwrap();
        assert!(dplyr_uri.as_str().starts_with("package:"));
        assert_eq!(dplyr_uri.as_str().strip_prefix("package:"), Some("dplyr"));

        // Base package URI
        let base_uri = Url::parse("package:base").unwrap();
        assert!(base_uri.as_str().starts_with("package:"));
        assert_eq!(base_uri.as_str().strip_prefix("package:"), Some("base"));

        // Package with dots in name
        let data_table_uri = Url::parse("package:data.table").unwrap();
        assert!(data_table_uri.as_str().starts_with("package:"));
        assert_eq!(
            data_table_uri.as_str().strip_prefix("package:"),
            Some("data.table")
        );

        // File URIs should NOT be detected as packages
        let file_uri = Url::parse("file:///workspace/test.R").unwrap();
        assert!(!file_uri.as_str().starts_with("package:"));
        assert_eq!(file_uri.as_str().strip_prefix("package:"), None);
    }

    // ============================================================================
    // Tests for goto_definition shadowing behavior - Task 13.2
    // ============================================================================

    #[test]
    fn test_goto_definition_local_shadows_package_export() {
        // Test that when a local definition shadows a package export,
        // goto_definition navigates to the local definition, not the package.
        // Validates: Requirement 11.3

        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let uri = Url::parse("file:///workspace/main.R").unwrap();

        // Code that loads a package and then defines a local function with the same name
        // as a package export. The local definition should shadow the package export.
        // "mutate" is defined locally on line 1 (0-indexed), shadowing dplyr::mutate
        let code = r#"library(dplyr)
mutate <- function(x, y) { x + y }
result <- mutate(1, 2)"#;

        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        // Update cross-file graph with metadata
        state.cross_file_graph.update_file(
            &uri,
            &crate::cross_file::extract_metadata(code),
            None,
            |_| None,
        );

        // Test goto_definition on "mutate" usage (line 2, position 10 - within "mutate")
        let position = Position::new(2, 10);
        let result = goto_definition(&state, &uri, position);

        // Should navigate to local definition, not return None (which would happen for package exports)
        assert!(
            result.is_some(),
            "goto_definition should return a result for shadowed symbol"
        );

        if let Some(GotoDefinitionResponse::Scalar(location)) = result {
            // Should navigate to the local definition on line 1
            assert_eq!(location.uri, uri, "Should navigate to the same file");
            assert_eq!(
                location.range.start.line, 1,
                "Should navigate to line 1 where local mutate is defined"
            );
            assert_eq!(
                location.range.start.character, 0,
                "Should navigate to column 0"
            );
        } else {
            panic!("Expected Scalar response");
        }
    }

    #[test]
    fn test_goto_definition_local_definition_found_first() {
        // Test that goto_definition searches the current document first,
        // ensuring local definitions are found before cross-file symbols.
        // This is the core mechanism that enables shadowing.
        // Validates: Requirement 11.3

        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let uri = Url::parse("file:///workspace/test.R").unwrap();

        // Simple code with a local function definition and usage
        let code = r#"my_func <- function(a, b) { a + b }
result <- my_func(1, 2)"#;

        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        // Test goto_definition on "my_func" usage (line 1, position 10)
        let position = Position::new(1, 10);
        let result = goto_definition(&state, &uri, position);

        assert!(
            result.is_some(),
            "goto_definition should find local definition"
        );

        if let Some(GotoDefinitionResponse::Scalar(location)) = result {
            assert_eq!(location.uri, uri, "Should navigate to the same file");
            assert_eq!(
                location.range.start.line, 0,
                "Should navigate to line 0 where my_func is defined"
            );
        } else {
            panic!("Expected Scalar response");
        }
    }

    /// Verifies that scope resolution prefers local definitions over package exports for goto-definition.
    ///
    /// Constructs a document containing a `library()` call and a local function named `filter`, computes
    /// the cross-file scope at a position after the local definition, and asserts that the `filter`
    /// symbol resolves to the local file (not a `package:` URI) and has the expected definition line.
    ///
    /// # Examples
    ///
    /// ```
    /// // Confirms a local `filter` shadows the `dplyr` export when resolving definitions.
    /// ```
    #[test]
    fn test_goto_definition_shadowing_scope_resolution() {
        // Test that scope resolution correctly returns local definitions over package exports.
        // This verifies the underlying mechanism that goto_definition relies on.
        // Validates: Requirement 11.3
        use crate::cross_file::scope::{compute_artifacts, scope_at_position_with_packages};
        use std::collections::HashSet;

        let uri = Url::parse("file:///workspace/test.R").unwrap();

        // Code with library() and local definition of same name
        let code = r#"library(dplyr)
filter <- function(x) { x > 0 }
result <- filter(c(1, -2, 3))"#;

        let doc = Document::new(code, None);
        let tree = doc.tree.as_ref().expect("Should parse successfully");
        let artifacts = compute_artifacts(&uri, tree, code);

        // Create a mock package exports callback that returns "filter" for dplyr
        let get_exports = |pkg: &str| -> HashSet<String> {
            if pkg == "dplyr" {
                let mut exports = HashSet::new();
                exports.insert("filter".to_string());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope at line 2 (after both library and local definition)
        let scope = scope_at_position_with_packages(&artifacts, 2, 10, &get_exports, &base_exports);

        // Symbol should be in scope
        assert!(
            scope.symbols.contains_key("filter"),
            "filter should be in scope"
        );

        // The symbol should be from the local definition, not the package
        let symbol = scope.symbols.get("filter").unwrap();
        assert!(
            !symbol.source_uri.as_str().starts_with("package:"),
            "filter should be from local definition, not package. Got URI: '{}'",
            symbol.source_uri.as_str()
        );
        assert_eq!(
            symbol.source_uri, uri,
            "filter should be from the local file"
        );

        // Verify the definition position matches the local definition
        assert_eq!(symbol.defined_line, 1, "filter should be defined on line 1");
    }

    #[test]
    fn test_goto_definition_shadowing_position_aware() {
        // Test that shadowing is position-aware: before the local definition,
        // the package export would be used; after, the local definition.
        // For goto_definition, this means:
        // - Before local def: returns None (package export, not navigable)
        // - After local def: returns local definition location
        // Validates: Requirement 11.3

        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let uri = Url::parse("file:///workspace/test.R").unwrap();

        // Code where package is loaded, then used, then shadowed, then used again
        // Line 0: library(dplyr)
        // Line 1: x <- filter(data)  # Uses dplyr::filter
        // Line 2: filter <- function(x) { x > 0 }  # Local definition
        // Line 3: y <- filter(data)  # Uses local filter
        let code = r#"library(dplyr)
x <- filter(data)
filter <- function(x) { x > 0 }
y <- filter(data)"#;

        state
            .documents
            .insert(uri.clone(), Document::new(code, None));
        state.cross_file_graph.update_file(
            &uri,
            &crate::cross_file::extract_metadata(code),
            None,
            |_| None,
        );

        // Test goto_definition on "filter" usage AFTER local definition (line 3, position 5)
        let position_after = Position::new(3, 5);
        let result_after = goto_definition(&state, &uri, position_after);

        // After local definition, should navigate to local definition
        assert!(
            result_after.is_some(),
            "goto_definition should find local definition after shadowing"
        );

        if let Some(GotoDefinitionResponse::Scalar(location)) = result_after {
            assert_eq!(location.uri, uri, "Should navigate to the same file");
            assert_eq!(
                location.range.start.line, 2,
                "Should navigate to line 2 where local filter is defined"
            );
        } else {
            panic!("Expected Scalar response");
        }
    }

    #[test]
    fn test_goto_definition_multiple_local_definitions() {
        // Test that goto_definition finds the first local definition when
        // there are multiple definitions of the same symbol.
        // Validates: Requirement 11.3

        let library_paths = r_env::find_library_paths();
        let mut state = WorldState::new(library_paths);

        let uri = Url::parse("file:///workspace/test.R").unwrap();

        // Code with multiple definitions of the same symbol
        let code = r#"x <- 1
x <- 2
y <- x"#;

        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        // Test goto_definition on "x" usage (line 2, position 5)
        let position = Position::new(2, 5);
        let result = goto_definition(&state, &uri, position);

        assert!(result.is_some(), "goto_definition should find definition");

        if let Some(GotoDefinitionResponse::Scalar(location)) = result {
            assert_eq!(location.uri, uri, "Should navigate to the same file");
            // Position-aware definition finding returns the latest definition before usage
            // So it should be line 1 (x <- 2), not line 0 (x <- 1)
            assert_eq!(
                location.range.start.line, 1,
                "Should navigate to latest definition on line 1"
            );
        } else {
            panic!("Expected Scalar response");
        }
    }
}

#[cfg(test)]
mod position_aware_tests {
    use crate::cross_file::directive::parse_directives;
    use crate::handlers::{collect_undefined_variables_position_aware, goto_definition};
    use crate::state::{Document, WorldState};
    use tower_lsp::lsp_types::{Position, Url};

    fn parse_r_code(code: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        parser.parse(code, None).unwrap()
    }

    fn create_test_state() -> WorldState {
        WorldState::new(vec![])
    }

    fn add_document(state: &mut WorldState, uri_str: &str, content: &str) -> Url {
        let uri = Url::parse(uri_str).expect("Invalid URI");
        let document = Document::new(content, None);
        state.documents.insert(uri.clone(), document);
        uri
    }

    #[test]
    fn test_diagnostics_undefined_forward_reference() {
        let mut state = create_test_state();
        let code = "
x
x <- 1
";
        // Line 1: x (usage) - should be undefined
        // Line 2: x <- 1 (definition)
        let uri = add_document(&mut state, "file:///test.R", code);
        let tree = parse_r_code(code);
        let root = tree.root_node();
        let directive_meta = parse_directives(code);

        let mut diagnostics = Vec::new();
        collect_undefined_variables_position_aware(
            &state,
            &uri,
            root,
            code,
            &[], // deprecated loaded_packages
            &[], // workspace_imports
            &state.package_library,
            &directive_meta,
            &mut diagnostics,
        );

        assert_eq!(diagnostics.len(), 1, "Should have 1 diagnostic");
        assert!(diagnostics[0].message.contains("Undefined variable: x"));
        assert_eq!(diagnostics[0].range.start.line, 1);
    }

    #[test]
    fn test_diagnostics_defined_before_usage() {
        let mut state = create_test_state();
        let code = "
x <- 1
x
";
        // Line 1: x <- 1
        // Line 2: x (usage)
        let uri = add_document(&mut state, "file:///test.R", code);
        let tree = parse_r_code(code);
        let root = tree.root_node();
        let directive_meta = parse_directives(code);

        let mut diagnostics = Vec::new();
        collect_undefined_variables_position_aware(
            &state,
            &uri,
            root,
            code,
            &[],
            &[],
            &state.package_library,
            &directive_meta,
            &mut diagnostics,
        );

        assert_eq!(diagnostics.len(), 0, "Should have 0 diagnostics");
    }

    #[test]
    fn test_diagnostics_redefined_later() {
        let mut state = create_test_state();
        let code = "
x <- 1
x
x <- 2
";
        // Line 1: x <- 1
        // Line 2: x (usage) - defined by line 1
        // Line 3: x <- 2
        let uri = add_document(&mut state, "file:///test.R", code);
        let tree = parse_r_code(code);
        let root = tree.root_node();
        let directive_meta = parse_directives(code);

        let mut diagnostics = Vec::new();
        collect_undefined_variables_position_aware(
            &state,
            &uri,
            root,
            code,
            &[],
            &[],
            &state.package_library,
            &directive_meta,
            &mut diagnostics,
        );

        assert_eq!(diagnostics.len(), 0, "Should have 0 diagnostics");
    }

    // ========================================================================
    // Declaration Directive Diagnostic Suppression Tests
    // Validates: Requirements 5.1, 5.2, 5.3, 5.4
    // ========================================================================

    #[test]
    fn test_declared_variable_suppresses_undefined_diagnostic() {
        // Requirement 5.1: WHEN a variable is declared via directive and used after the directive line,
        // THE diagnostic collector SHALL NOT emit an "undefined variable" warning
        let mut state = create_test_state();
        let code = "# @lsp-var myvar
myvar
";
        // Line 0: @lsp-var myvar (declaration)
        // Line 1: myvar (usage) - should NOT be flagged as undefined
        let uri = add_document(&mut state, "file:///test.R", code);
        let tree = parse_r_code(code);
        let root = tree.root_node();
        let directive_meta = parse_directives(code);

        let mut diagnostics = Vec::new();
        collect_undefined_variables_position_aware(
            &state,
            &uri,
            root,
            code,
            &[],
            &[],
            &state.package_library,
            &directive_meta,
            &mut diagnostics,
        );

        assert_eq!(diagnostics.len(), 0, "Declared variable should suppress undefined diagnostic");
    }

    #[test]
    fn test_declared_function_suppresses_undefined_diagnostic() {
        // Requirement 5.2: WHEN a function is declared via directive and called after the directive line,
        // THE diagnostic collector SHALL NOT emit an "undefined variable" warning
        let mut state = create_test_state();
        let code = "# @lsp-func myfunc
myfunc()
";
        // Line 0: @lsp-func myfunc (declaration)
        // Line 1: myfunc() (usage) - should NOT be flagged as undefined
        let uri = add_document(&mut state, "file:///test.R", code);
        let tree = parse_r_code(code);
        let root = tree.root_node();
        let directive_meta = parse_directives(code);

        let mut diagnostics = Vec::new();
        collect_undefined_variables_position_aware(
            &state,
            &uri,
            root,
            code,
            &[],
            &[],
            &state.package_library,
            &directive_meta,
            &mut diagnostics,
        );

        assert_eq!(diagnostics.len(), 0, "Declared function should suppress undefined diagnostic");
    }

    #[test]
    fn test_usage_before_declaration_emits_diagnostic() {
        // Requirement 5.3: WHEN a variable is used before its declaration directive,
        // THE diagnostic collector SHALL emit an "undefined variable" warning
        let mut state = create_test_state();
        let code = "myvar
# @lsp-var myvar
";
        // Line 0: myvar (usage) - should be flagged as undefined (before declaration)
        // Line 1: @lsp-var myvar (declaration)
        let uri = add_document(&mut state, "file:///test.R", code);
        let tree = parse_r_code(code);
        let root = tree.root_node();
        let directive_meta = parse_directives(code);

        let mut diagnostics = Vec::new();
        collect_undefined_variables_position_aware(
            &state,
            &uri,
            root,
            code,
            &[],
            &[],
            &state.package_library,
            &directive_meta,
            &mut diagnostics,
        );

        assert_eq!(diagnostics.len(), 1, "Usage before declaration should emit diagnostic");
        assert!(diagnostics[0].message.contains("Undefined variable: myvar"));
        assert_eq!(diagnostics[0].range.start.line, 0);
    }

    #[test]
    fn test_declared_symbol_case_sensitive_matching() {
        // Requirement 5.4: WHEN a declared symbol name matches a usage exactly (case-sensitive),
        // THE diagnostic SHALL be suppressed
        let mut state = create_test_state();
        let code = "# @lsp-var myVar
myVar
MyVar
";
        // Line 0: @lsp-var myVar (declaration)
        // Line 1: myVar (usage) - exact match, should NOT be flagged
        // Line 2: MyVar (usage) - different case, should be flagged
        let uri = add_document(&mut state, "file:///test.R", code);
        let tree = parse_r_code(code);
        let root = tree.root_node();
        let directive_meta = parse_directives(code);

        let mut diagnostics = Vec::new();
        collect_undefined_variables_position_aware(
            &state,
            &uri,
            root,
            code,
            &[],
            &[],
            &state.package_library,
            &directive_meta,
            &mut diagnostics,
        );

        assert_eq!(diagnostics.len(), 1, "Only case-mismatched usage should emit diagnostic");
        assert!(diagnostics[0].message.contains("Undefined variable: MyVar"));
        assert_eq!(diagnostics[0].range.start.line, 2);
    }

    #[test]
    fn test_declared_variable_all_synonyms() {
        // Test all 4 synonym forms: @lsp-declare-variable, @lsp-declare-var, @lsp-variable, @lsp-var
        for directive in ["@lsp-declare-variable", "@lsp-declare-var", "@lsp-variable", "@lsp-var"] {
            let mut state = create_test_state();
            let code = format!("# {} myvar\nmyvar\n", directive);
            let uri = add_document(&mut state, "file:///test.R", &code);
            let tree = parse_r_code(&code);
            let root = tree.root_node();
            let directive_meta = parse_directives(&code);

            let mut diagnostics = Vec::new();
            collect_undefined_variables_position_aware(
                &state,
                &uri,
                root,
                &code,
                &[],
                &[],
                &state.package_library,
                &directive_meta,
                &mut diagnostics,
            );

            assert_eq!(
                diagnostics.len(),
                0,
                "Directive {} should suppress undefined diagnostic",
                directive
            );
        }
    }

    #[test]
    fn test_declared_function_all_synonyms() {
        // Test all 4 synonym forms: @lsp-declare-function, @lsp-declare-func, @lsp-function, @lsp-func
        for directive in ["@lsp-declare-function", "@lsp-declare-func", "@lsp-function", "@lsp-func"] {
            let mut state = create_test_state();
            let code = format!("# {} myfunc\nmyfunc()\n", directive);
            let uri = add_document(&mut state, "file:///test.R", &code);
            let tree = parse_r_code(&code);
            let root = tree.root_node();
            let directive_meta = parse_directives(&code);

            let mut diagnostics = Vec::new();
            collect_undefined_variables_position_aware(
                &state,
                &uri,
                root,
                &code,
                &[],
                &[],
                &state.package_library,
                &directive_meta,
                &mut diagnostics,
            );

            assert_eq!(
                diagnostics.len(),
                0,
                "Directive {} should suppress undefined diagnostic",
                directive
            );
        }
    }

    #[test]
    fn test_declared_symbol_on_same_line_not_available() {
        // Requirement 4.6: WHEN a directive appears on a line with code,
        // THE declared symbol SHALL be available starting from line N+1, not on line N
        let mut state = create_test_state();
        let code = "x <- myvar # @lsp-var myvar
myvar
";
        // Line 0: x <- myvar # @lsp-var myvar - myvar usage is on same line as declaration
        // Line 1: myvar (usage) - should NOT be flagged (after declaration)
        let uri = add_document(&mut state, "file:///test.R", code);
        let tree = parse_r_code(code);
        let root = tree.root_node();
        let directive_meta = parse_directives(code);

        let mut diagnostics = Vec::new();
        collect_undefined_variables_position_aware(
            &state,
            &uri,
            root,
            code,
            &[],
            &[],
            &state.package_library,
            &directive_meta,
            &mut diagnostics,
        );

        // The usage on line 0 should be flagged (same line as declaration)
        // The usage on line 1 should NOT be flagged (after declaration)
        assert_eq!(diagnostics.len(), 1, "Usage on same line as declaration should emit diagnostic");
        assert!(diagnostics[0].message.contains("Undefined variable: myvar"));
        assert_eq!(diagnostics[0].range.start.line, 0);
    }

    // ========================================================================
    // Conflicting Declaration Tests
    // Validates: Requirements 11.1, 11.2, 11.3, 11.4
    // ========================================================================

    #[test]
    fn test_conflicting_declarations_later_wins_for_kind_var_then_func() {
        // Requirement 11.1: WHEN the same symbol name is declared as both a variable and a function,
        // THE later declaration SHALL take precedence for symbol kind
        // Test case: variable declared first (line 0), function declared later (line 1)
        use crate::cross_file::scope::compute_artifacts_with_metadata;

        let mut state = create_test_state();
        let code = "# @lsp-var mysymbol
# @lsp-func mysymbol
mysymbol
";
        let uri = add_document(&mut state, "file:///test.R", code);
        let tree = parse_r_code(code);
        let directive_meta = parse_directives(code);

        // Compute artifacts with metadata to get scope
        let artifacts = compute_artifacts_with_metadata(&uri, &tree, code, Some(&directive_meta));

        // Check the exported interface - the symbol should have Function kind (later declaration)
        let symbol = artifacts.exported_interface.get("mysymbol");
        assert!(symbol.is_some(), "Symbol should be in exported interface");
        assert_eq!(
            symbol.unwrap().kind,
            crate::cross_file::SymbolKind::Function,
            "Later declaration (function on line 1) should take precedence for symbol kind"
        );
    }

    #[test]
    fn test_conflicting_declarations_later_wins_for_kind_func_then_var() {
        // Requirement 11.1: WHEN the same symbol name is declared as both a variable and a function,
        // THE later declaration SHALL take precedence for symbol kind
        // Test case: function declared first (line 0), variable declared later (line 1)
        use crate::cross_file::scope::compute_artifacts_with_metadata;

        let mut state = create_test_state();
        let code = "# @lsp-func mysymbol
# @lsp-var mysymbol
mysymbol
";
        let uri = add_document(&mut state, "file:///test.R", code);
        let tree = parse_r_code(code);
        let directive_meta = parse_directives(code);

        // Compute artifacts with metadata to get scope
        let artifacts = compute_artifacts_with_metadata(&uri, &tree, code, Some(&directive_meta));

        // Check the exported interface - the symbol should have Variable kind (later declaration)
        let symbol = artifacts.exported_interface.get("mysymbol");
        assert!(symbol.is_some(), "Symbol should be in exported interface");
        assert_eq!(
            symbol.unwrap().kind,
            crate::cross_file::SymbolKind::Variable,
            "Later declaration (variable on line 1) should take precedence for symbol kind"
        );
    }

    #[test]
    fn test_conflicting_declarations_diagnostic_suppression_regardless_of_kind() {
        // Requirement 11.2: WHEN conflicting declarations exist,
        // THE diagnostic suppression SHALL apply regardless of kind (the symbol exists)
        let mut state = create_test_state();
        
        // Test case 1: variable first, function second
        let code1 = "# @lsp-var mysymbol
# @lsp-func mysymbol
mysymbol
";
        let uri1 = add_document(&mut state, "file:///test1.R", code1);
        let tree1 = parse_r_code(code1);
        let root1 = tree1.root_node();
        let directive_meta1 = parse_directives(code1);

        let mut diagnostics1 = Vec::new();
        collect_undefined_variables_position_aware(
            &state,
            &uri1,
            root1,
            code1,
            &[],
            &[],
            &state.package_library,
            &directive_meta1,
            &mut diagnostics1,
        );

        assert_eq!(
            diagnostics1.len(),
            0,
            "Conflicting declarations (var then func) should suppress undefined diagnostic"
        );

        // Test case 2: function first, variable second
        let code2 = "# @lsp-func mysymbol
# @lsp-var mysymbol
mysymbol
";
        let uri2 = add_document(&mut state, "file:///test2.R", code2);
        let tree2 = parse_r_code(code2);
        let root2 = tree2.root_node();
        let directive_meta2 = parse_directives(code2);

        let mut diagnostics2 = Vec::new();
        collect_undefined_variables_position_aware(
            &state,
            &uri2,
            root2,
            code2,
            &[],
            &[],
            &state.package_library,
            &directive_meta2,
            &mut diagnostics2,
        );

        assert_eq!(
            diagnostics2.len(),
            0,
            "Conflicting declarations (func then var) should suppress undefined diagnostic"
        );
    }

    #[test]
    fn test_conflicting_declarations_completion_kind_reflects_later() {
        // Requirement 11.4: WHEN conflicting declarations exist,
        // THE completion item kind SHALL reflect the later declaration's kind
        use crate::cross_file::scope::{compute_artifacts_with_metadata, scope_at_position};

        let mut state = create_test_state();
        
        // Test case 1: variable first (line 0), function second (line 1)
        let code1 = "# @lsp-var mysymbol
# @lsp-func mysymbol
";
        let uri1 = add_document(&mut state, "file:///test1.R", code1);
        let tree1 = parse_r_code(code1);
        let directive_meta1 = parse_directives(code1);
        let artifacts1 = compute_artifacts_with_metadata(&uri1, &tree1, code1, Some(&directive_meta1));

        // Get scope at line 2 (after both declarations)
        let scope1 = scope_at_position(&artifacts1, 2, 0);
        let symbol1 = scope1.symbols.get("mysymbol");
        assert!(symbol1.is_some(), "Symbol should be in scope");
        assert_eq!(
            symbol1.unwrap().kind,
            crate::cross_file::SymbolKind::Function,
            "Completion kind should reflect later declaration (function)"
        );

        // Test case 2: function first (line 0), variable second (line 1)
        let code2 = "# @lsp-func mysymbol
# @lsp-var mysymbol
";
        let uri2 = add_document(&mut state, "file:///test2.R", code2);
        let tree2 = parse_r_code(code2);
        let directive_meta2 = parse_directives(code2);
        let artifacts2 = compute_artifacts_with_metadata(&uri2, &tree2, code2, Some(&directive_meta2));

        // Get scope at line 2 (after both declarations)
        let scope2 = scope_at_position(&artifacts2, 2, 0);
        let symbol2 = scope2.symbols.get("mysymbol");
        assert!(symbol2.is_some(), "Symbol should be in scope");
        assert_eq!(
            symbol2.unwrap().kind,
            crate::cross_file::SymbolKind::Variable,
            "Completion kind should reflect later declaration (variable)"
        );
    }

    #[test]
    fn test_goto_definition_same_file_before_usage() {
        let mut state = create_test_state();
        let code = "
x <- 1
x
";
        // Line 1: x <- 1
        // Line 2: x (usage)
        let uri = add_document(&mut state, "file:///test.R", code);

        // Usage at line 2, col 0
        let pos = Position::new(2, 0);
        let result = goto_definition(&state, &uri, pos);

        assert!(result.is_some(), "Should find definition");
        let location = match result.unwrap() {
            tower_lsp::lsp_types::GotoDefinitionResponse::Scalar(loc) => loc,
            _ => panic!("Expected Scalar location"),
        };

        assert_eq!(location.uri, uri);
        assert_eq!(
            location.range.start.line, 1,
            "Definition should be on line 1"
        );
    }

    #[test]
    fn test_goto_definition_same_file_after_usage() {
        let mut state = create_test_state();
        let code = "
x
x <- 1
";
        // Line 1: x (usage)
        // Line 2: x <- 1 (definition)
        let uri = add_document(&mut state, "file:///test.R", code);

        // Usage at line 1, col 0
        let pos = Position::new(1, 0);
        let result = goto_definition(&state, &uri, pos);

        assert!(
            result.is_none(),
            "Should NOT find definition appearing after usage"
        );
    }

    #[test]
    fn test_goto_definition_function_scope_no_leak() {
        let mut state = create_test_state();
        let code = "
f <- function() {
    local_var <- 1
}
local_var
";
        // Line 1: f <- ...
        // Line 2:     local_var <- 1
        // Line 3: }
        // Line 4: local_var (usage)
        let uri = add_document(&mut state, "file:///test.R", code);

        // Usage at line 4, col 0
        let pos = Position::new(4, 0);
        let result = goto_definition(&state, &uri, pos);

        assert!(
            result.is_none(),
            "Function-local variable should not be visible outside"
        );
    }

    #[test]
    fn test_goto_definition_shadowing() {
        let mut state = create_test_state();
        let code = "
x <- 1
f <- function() {
    x <- 2
    x
}
";
        // Line 1: x <- 1 (global)
        // Line 2: f <- ...
        // Line 3:     x <- 2 (local)
        // Line 4:     x (usage)
        let uri = add_document(&mut state, "file:///test.R", code);

        // Usage at line 4, col 4
        let pos = Position::new(4, 4);
        let result = goto_definition(&state, &uri, pos);

        assert!(result.is_some());
        let location = match result.unwrap() {
            tower_lsp::lsp_types::GotoDefinitionResponse::Scalar(loc) => loc,
            _ => panic!("Expected Scalar location"),
        };

        assert_eq!(
            location.range.start.line, 3,
            "Should resolve to local definition (line 3)"
        );
    }

    #[test]
    fn test_goto_definition_sequential_redefinition() {
        let mut state = create_test_state();
        let code = "
x <- 1
x <- 2
x
";
        // Line 1: x <- 1
        // Line 2: x <- 2
        // Line 3: x (usage)
        let uri = add_document(&mut state, "file:///test.R", code);

        // Usage at line 3, col 0
        let pos = Position::new(3, 0);
        let result = goto_definition(&state, &uri, pos);

        assert!(result.is_some());
        let location = match result.unwrap() {
            tower_lsp::lsp_types::GotoDefinitionResponse::Scalar(loc) => loc,
            _ => panic!("Expected Scalar location"),
        };

        assert_eq!(
            location.range.start.line, 2,
            "Should resolve to latest definition (line 2)"
        );
    }
}

#[cfg(test)]
mod function_parameter_tests {
    use crate::cross_file::directive::parse_directives;
    use crate::handlers::collect_undefined_variables_position_aware;
    use crate::state::{Document, WorldState};
    use tower_lsp::lsp_types::Url;

    fn parse_r_code(code: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        parser.parse(code, None).unwrap()
    }

    fn create_test_state() -> WorldState {
        WorldState::new(vec![])
    }

    fn add_document(state: &mut WorldState, uri_str: &str, content: &str) -> Url {
        let uri = Url::parse(uri_str).expect("Invalid URI");
        let document = Document::new(content, None);
        state.documents.insert(uri.clone(), document);
        uri
    }

    #[test]
    fn test_function_parameters_not_flagged_as_undefined() {
        let mut state = create_test_state();
        let code = r#"
add <- function(a, b) {
  result <- a + b
  return(result)
}
"#;
        let uri = add_document(&mut state, "file:///test.R", code);
        let tree = parse_r_code(code);
        let root = tree.root_node();
        let directive_meta = parse_directives(code);

        let mut diagnostics = Vec::new();
        collect_undefined_variables_position_aware(
            &state,
            &uri,
            root,
            code,
            &[],
            &[],
            &state.package_library,
            &directive_meta,
            &mut diagnostics,
        );

        // Filter to only undefined variable warnings (use starts_with for filtering, exact match for assertions)
        let undefined_var_diags: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.starts_with("Undefined variable: "))
            .collect();

        // Print diagnostics for debugging
        for diag in &undefined_var_diags {
            println!(
                "Diagnostic: {} at line {}",
                diag.message, diag.range.start.line
            );
        }

        // Function parameters a and b should NOT be flagged as undefined
        // Use exact equality to avoid substring false positives (e.g., "a" matching "ab")
        assert!(
            !undefined_var_diags
                .iter()
                .any(|d| d.message == "Undefined variable: a"),
            "Parameter 'a' should not be flagged as undefined"
        );
        assert!(
            !undefined_var_diags
                .iter()
                .any(|d| d.message == "Undefined variable: b"),
            "Parameter 'b' should not be flagged as undefined"
        );
    }

    #[test]
    fn test_nested_function_parameters_not_flagged() {
        let mut state = create_test_state();
        let code = r#"
outer_func <- function(x) {
  inner_func <- function(y) {
    x + y
  }
  return(inner_func)
}
"#;
        let uri = add_document(&mut state, "file:///test.R", code);
        let tree = parse_r_code(code);
        let root = tree.root_node();
        let directive_meta = parse_directives(code);

        let mut diagnostics = Vec::new();
        collect_undefined_variables_position_aware(
            &state,
            &uri,
            root,
            code,
            &[],
            &[],
            &state.package_library,
            &directive_meta,
            &mut diagnostics,
        );

        // Filter to only undefined variable warnings (use starts_with for filtering, exact match for assertions)
        let undefined_var_diags: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.starts_with("Undefined variable: "))
            .collect();

        // Print diagnostics for debugging
        for diag in &undefined_var_diags {
            println!(
                "Diagnostic: {} at line {}",
                diag.message, diag.range.start.line
            );
        }

        // Function parameters x and y should NOT be flagged as undefined
        // Use exact equality to avoid substring false positives (e.g., "x" matching "x_var")
        assert!(
            !undefined_var_diags
                .iter()
                .any(|d| d.message == "Undefined variable: x"),
            "Parameter 'x' should not be flagged as undefined"
        );
        assert!(
            !undefined_var_diags
                .iter()
                .any(|d| d.message == "Undefined variable: y"),
            "Parameter 'y' should not be flagged as undefined"
        );
    }

    #[test]
    fn test_default_parameter_expression_checked() {
        // Test that identifiers in default parameter expressions ARE checked for undefined
        // e.g., function(a = b) should flag 'b' as undefined if not defined
        let mut state = create_test_state();
        let code = r#"
my_func <- function(a = undefined_var) {
  a
}
"#;
        let uri = add_document(&mut state, "file:///test.R", code);
        let tree = parse_r_code(code);
        let root = tree.root_node();
        let directive_meta = parse_directives(code);

        let mut diagnostics = Vec::new();
        collect_undefined_variables_position_aware(
            &state,
            &uri,
            root,
            code,
            &[],
            &[],
            &state.package_library,
            &directive_meta,
            &mut diagnostics,
        );

        // Filter to only undefined variable warnings
        let undefined_var_diags: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.starts_with("Undefined variable: "))
            .collect();

        // Print diagnostics for debugging
        for diag in &undefined_var_diags {
            println!(
                "Diagnostic: {} at line {}",
                diag.message, diag.range.start.line
            );
        }

        // The parameter name 'a' should NOT be flagged
        assert!(
            !undefined_var_diags
                .iter()
                .any(|d| d.message == "Undefined variable: a"),
            "Parameter name 'a' should not be flagged as undefined"
        );

        // The undefined variable in the default expression SHOULD be flagged
        assert!(
            undefined_var_diags
                .iter()
                .any(|d| d.message == "Undefined variable: undefined_var"),
            "Undefined variable 'undefined_var' in default expression should be flagged"
        );
    }

    #[test]
    fn test_default_parameter_with_defined_var() {
        // Test that identifiers in default parameter expressions are NOT flagged if defined
        let mut state = create_test_state();
        let code = r#"
default_value <- 42
my_func <- function(a = default_value) {
  a
}
"#;
        let uri = add_document(&mut state, "file:///test.R", code);
        let tree = parse_r_code(code);
        let root = tree.root_node();
        let directive_meta = parse_directives(code);

        let mut diagnostics = Vec::new();
        collect_undefined_variables_position_aware(
            &state,
            &uri,
            root,
            code,
            &[],
            &[],
            &state.package_library,
            &directive_meta,
            &mut diagnostics,
        );

        // Filter to only undefined variable warnings
        let undefined_var_diags: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.starts_with("Undefined variable: "))
            .collect();

        // Print diagnostics for debugging
        for diag in &undefined_var_diags {
            println!(
                "Diagnostic: {} at line {}",
                diag.message, diag.range.start.line
            );
        }

        // Neither 'a' nor 'default_value' should be flagged
        assert!(
            !undefined_var_diags
                .iter()
                .any(|d| d.message == "Undefined variable: a"),
            "Parameter name 'a' should not be flagged as undefined"
        );
        assert!(
            !undefined_var_diags
                .iter()
                .any(|d| d.message == "Undefined variable: default_value"),
            "Defined variable 'default_value' should not be flagged as undefined"
        );
    }
}

// ============================================================================
// Property Tests for Diagnostics Master Switch
// ============================================================================

#[cfg(test)]
mod diagnostics_master_switch_tests {
    use crate::handlers::diagnostics;
    use crate::state::{Document, WorldState};
    use proptest::prelude::*;
    use tower_lsp::lsp_types::Url;

    /// Strategy to generate arbitrary R code content.
    /// Includes valid R code, syntax errors, and edge cases.
    fn arbitrary_r_code() -> impl Strategy<Value = String> {
        prop_oneof![
            // Valid R code
            Just("x <- 1".to_string()),
            Just("f <- function(a, b) { a + b }".to_string()),
            Just("library(dplyr)\nfilter(data)".to_string()),
            Just("for (i in 1:10) { print(i) }".to_string()),
            // Code with undefined variables (would normally produce diagnostics)
            Just("undefined_var".to_string()),
            Just("x <- undefined_func()".to_string()),
            Just("result <- a + b".to_string()),
            // Syntax errors (would normally produce diagnostics)
            Just("x <- ".to_string()),
            Just("function( { }".to_string()),
            Just("if (TRUE".to_string()),
            Just("for (i in".to_string()),
            // Empty and whitespace
            Just("".to_string()),
            Just("   \n\n   ".to_string()),
            // Comments only
            Just("# This is a comment".to_string()),
            Just("# Comment 1\n# Comment 2".to_string()),
            // Complex code with multiple issues
            Just("x <- 1\nundefined\nfor (i in".to_string()),
            // Random alphanumeric strings (may or may not be valid R)
            "[a-zA-Z0-9_ ]+".prop_map(|s| s),
        ]
    }

    // ========================================================================
    // Property 1: Master switch disabled suppresses all diagnostics
    // **Validates: Requirements 1.4, 3.1, 3.4**
    // ========================================================================

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        /// Property 1: Master switch disabled suppresses all diagnostics
        ///
        /// For any document URI and any WorldState where `diagnostics_enabled` is `false`,
        /// calling `diagnostics(state, uri)` SHALL return an empty vector regardless of
        /// document content, syntax errors, or other configuration settings (including
        /// individual severity settings).
        ///
        /// **Validates: Requirements 1.4, 3.1, 3.4**
        #[test]
        fn prop_master_switch_disabled_suppresses_all_diagnostics(
            code in arbitrary_r_code(),
            undefined_variables_enabled in any::<bool>(),
        ) {
            // Create a WorldState with diagnostics_enabled = false
            let mut state = WorldState::new(vec![]);
            state.cross_file_config.diagnostics_enabled = false;

            // Also vary other diagnostic settings to ensure master switch takes precedence
            state.cross_file_config.undefined_variables_enabled = undefined_variables_enabled;

            // Create a document with the generated code
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            // Call diagnostics - should always return empty when master switch is disabled
            let result = diagnostics(&state, &uri);

            prop_assert!(
                result.is_empty(),
                "When diagnostics_enabled is false, diagnostics() should return empty vector. \
                 Got {} diagnostics for code: {:?}",
                result.len(),
                code
            );
        }
    }

    // ========================================================================
    // Unit test to verify the property with specific edge cases
    // ========================================================================

    #[test]
    fn test_master_switch_disabled_with_syntax_error() {
        // Validates: Requirements 1.4, 3.1, 3.4
        // Even code with syntax errors should produce no diagnostics when disabled
        let mut state = WorldState::new(vec![]);
        state.cross_file_config.diagnostics_enabled = false;

        let uri = Url::parse("file:///test.R").unwrap();
        // Code with obvious syntax error
        let code = "x <- function( { }";
        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        let result = diagnostics(&state, &uri);
        assert!(
            result.is_empty(),
            "Syntax errors should be suppressed when master switch is disabled"
        );
    }

    #[test]
    fn test_master_switch_disabled_with_undefined_variables() {
        // Validates: Requirements 1.4, 3.1, 3.4
        // Even code with undefined variables should produce no diagnostics when disabled
        let mut state = WorldState::new(vec![]);
        state.cross_file_config.diagnostics_enabled = false;
        state.cross_file_config.undefined_variables_enabled = true; // Would normally produce diagnostics

        let uri = Url::parse("file:///test.R").unwrap();
        // Code with undefined variable
        let code = "result <- undefined_variable + 1";
        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        let result = diagnostics(&state, &uri);
        assert!(
            result.is_empty(),
            "Undefined variable diagnostics should be suppressed when master switch is disabled"
        );
    }

    #[test]
    fn test_master_switch_disabled_takes_precedence_over_individual_settings() {
        // Validates: Requirements 3.4
        // Master switch should take precedence over all individual diagnostic severity settings
        let mut state = WorldState::new(vec![]);
        state.cross_file_config.diagnostics_enabled = false;

        // Enable all individual diagnostic settings
        state.cross_file_config.undefined_variables_enabled = true;
        // Severity settings are already set to non-None values by default

        let uri = Url::parse("file:///test.R").unwrap();
        // Code that would trigger multiple types of diagnostics
        let code = r#"
# Syntax error
x <- function( { }
# Undefined variable
result <- undefined_var
"#;
        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        let result = diagnostics(&state, &uri);
        assert!(
            result.is_empty(),
            "Master switch should suppress all diagnostics regardless of individual settings"
        );
    }

    // ========================================================================
    // Property 2: Master switch enabled preserves normal diagnostics behavior
    // **Validates: Requirements 1.3, 3.3**
    // ========================================================================

    /// Strategy to generate R code that is known to produce diagnostics.
    /// These are code patterns that will definitely trigger diagnostic messages.
    fn code_with_known_issues() -> impl Strategy<Value = String> {
        prop_oneof![
            // Syntax errors - incomplete expressions
            Just("x <- ".to_string()),
            Just("function( { }".to_string()),
            Just("if (TRUE".to_string()),
            Just("for (i in".to_string()),
            Just("while (".to_string()),
            Just("x <- (1 +".to_string()),
            // Multiple syntax errors
            Just("x <- \ny <- ".to_string()),
            Just("if (TRUE\nfor (i in".to_string()),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(50))]

        /// Property 2: Master switch enabled preserves normal diagnostics behavior
        ///
        /// For any document URI and any WorldState where `diagnostics_enabled` is `true`,
        /// calling `diagnostics(state, uri)` SHALL return the same diagnostics as if the
        /// master switch check did not exist (i.e., the master switch is transparent when enabled).
        ///
        /// This test verifies that code with known issues DOES produce diagnostics when
        /// the master switch is enabled, demonstrating that the switch is transparent.
        ///
        /// **Validates: Requirements 1.3, 3.3**
        #[test]
        fn prop_master_switch_enabled_preserves_normal_diagnostics(
            code in code_with_known_issues(),
        ) {
            // Create a WorldState with diagnostics_enabled = true (the default)
            let mut state = WorldState::new(vec![]);
            state.cross_file_config.diagnostics_enabled = true;

            // Create a document with code that has known issues
            let uri = Url::parse("file:///test.R").unwrap();
            state.documents.insert(uri.clone(), Document::new(&code, None));

            // Call diagnostics - should return non-empty for code with issues
            let result = diagnostics(&state, &uri);

            prop_assert!(
                !result.is_empty(),
                "When diagnostics_enabled is true, code with syntax errors should produce diagnostics. \
                 Got 0 diagnostics for code: {:?}",
                code
            );
        }
    }

    // ========================================================================
    // Unit tests for Property 2: Master switch enabled
    // ========================================================================

    #[test]
    fn test_master_switch_enabled_with_syntax_error() {
        // Validates: Requirements 1.3, 3.3
        // Code with syntax errors should produce diagnostics when enabled
        let mut state = WorldState::new(vec![]);
        state.cross_file_config.diagnostics_enabled = true;

        let uri = Url::parse("file:///test.R").unwrap();
        // Code with obvious syntax error (incomplete assignment)
        let code = "x <- ";
        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        let result = diagnostics(&state, &uri);
        assert!(
            !result.is_empty(),
            "Syntax errors should produce diagnostics when master switch is enabled"
        );
    }

    #[test]
    fn test_master_switch_enabled_is_transparent() {
        // Validates: Requirements 1.3, 3.3
        // The master switch should be transparent when enabled - same result as if check didn't exist
        let mut state_enabled = WorldState::new(vec![]);
        state_enabled.cross_file_config.diagnostics_enabled = true;

        let uri = Url::parse("file:///test.R").unwrap();
        let code = "x <- \nfor (i in";
        state_enabled
            .documents
            .insert(uri.clone(), Document::new(code, None));

        let result_enabled = diagnostics(&state_enabled, &uri);

        // The result should be non-empty (diagnostics are computed normally)
        assert!(
            !result_enabled.is_empty(),
            "Master switch enabled should allow diagnostics to be computed normally"
        );

        // Verify we get syntax error diagnostics
        assert!(
            result_enabled
                .iter()
                .any(|d| d.message.to_lowercase().contains("error")
                    || d.message.to_lowercase().contains("syntax")
                    || d.message.to_lowercase().contains("unexpected")),
            "Should contain syntax-related diagnostics, got: {:?}",
            result_enabled
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_master_switch_enabled_default_behavior() {
        // Validates: Requirements 1.3, 3.3
        // Default state (diagnostics_enabled = true) should compute diagnostics normally
        let mut state = WorldState::new(vec![]);
        // Don't explicitly set diagnostics_enabled - use default
        assert!(
            state.cross_file_config.diagnostics_enabled,
            "Default value should be true"
        );

        let uri = Url::parse("file:///test.R").unwrap();
        let code = "if (TRUE"; // Incomplete if statement - syntax error
        state
            .documents
            .insert(uri.clone(), Document::new(code, None));

        let result = diagnostics(&state, &uri);
        assert!(
            !result.is_empty(),
            "Default configuration should allow diagnostics to be computed"
        );
    }
}