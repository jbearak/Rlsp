//! Configuration types shared by on-type indentation and its formatter.

/// Configuration for indentation calculation.
#[derive(Debug, Clone, PartialEq)]
pub struct IndentationConfig {
    /// Number of spaces per indentation level.
    pub tab_size: u32,
    /// Whether to use spaces (true) or tabs (false) for indentation.
    pub insert_spaces: bool,
    /// Whether Tier 2 AST-aware indentation is enabled.
    pub enabled: bool,
    /// How Tier 2 formats parenthesized argument continuations.
    pub argument_style: IndentationStyle,
    /// How Tier 2 formats infix-operator continuations.
    pub infix_continuation_style: IndentationStyle,
}

impl Default for IndentationConfig {
    fn default() -> Self {
        Self {
            tab_size: 2,
            insert_spaces: true,
            enabled: true,
            argument_style: IndentationStyle::Aligned,
            infix_continuation_style: IndentationStyle::Aligned,
        }
    }
}

/// Producer style for one configurable indentation axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndentationStyle {
    /// Align continuations to the syntactic anchor, with the infix axis's
    /// one-level floor applied by the shared expectation engine.
    #[default]
    Aligned,
    /// Indent continuations one level from the construct's owning line.
    Indented,
    /// Let Tier 1/native indentation stand for this construct.
    Off,
}
