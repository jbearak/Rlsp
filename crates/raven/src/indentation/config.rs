//! Configuration types shared by on-type indentation and its formatter.

/// Configuration for indentation calculation.
#[derive(Debug, Clone, PartialEq)]
pub struct IndentationConfig {
    /// Number of spaces per indentation level.
    pub tab_size: u32,
    /// Whether to use spaces (true) or tabs (false) for indentation.
    pub insert_spaces: bool,
    /// The indentation style to use.
    pub style: IndentationStyle,
}

impl Default for IndentationConfig {
    fn default() -> Self {
        Self {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        }
    }
}

/// Indentation style variants.
///
/// These correspond to common R coding conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndentationStyle {
    /// RStudio style: same-line arguments align to the opening parenthesis,
    /// while next-line arguments indent from the function line.
    #[default]
    RStudio,
    /// RStudio-minus style: arguments indent from the opener line, ignoring
    /// the parenthesis column.
    RStudioMinus,
    /// Disable Tier 2 AST-aware indentation.
    ///
    /// The on-type formatting handler returns no edits, leaving Tier 1
    /// declarative rules and the editor's native indentation unchanged.
    Off,
}
