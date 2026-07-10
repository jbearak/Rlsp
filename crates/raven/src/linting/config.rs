//! Configuration for the lint subsystem.
//!
//! Defaults follow `lintr`'s most common settings: 80-character lines, `<-` for
//! assignment, all rules disabled by default at the master switch so the
//! feature stays opt-in until it stabilizes (per upstream issue #211).

use std::fmt;

use tower_lsp::lsp_types::DiagnosticSeverity;

/// Preferred assignment operator. Mirrors `lintr::assignment_linter`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AssignmentOperatorStyle {
    /// Require `<-` for assignment; flag top-level `=` assignments.
    #[default]
    LeftArrow,
    /// Require `=` for assignment; flag top-level `<-` assignments.
    Equals,
}

/// Preferred string-literal delimiter. Mirrors `lintr::quotes_linter` /
/// `lintr::single_quotes_linter` — the two map to the two enum variants here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StringDelimiter {
    /// Require `"..."` for string literals; flag `'...'`. Default.
    #[default]
    Double,
    /// Require `'...'` for string literals; flag `"..."`.
    Single,
}

/// Naming scheme used by the `object_name` lint.
///
/// Mirrors `lintr::object_name_linter` named styles. `Any` disables the check
/// for a given symbol kind without disabling the rule entirely — useful when
/// only one of function/variable/argument naming should be enforced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ObjectNameStyle {
    /// `snake_case` — lowercase with underscores (e.g. `my_function`).
    #[default]
    SnakeCase,
    /// `camelCase` — first letter lowercase, subsequent words capitalized.
    CamelCase,
    /// `dotted.case` — historical R convention (e.g. `my.function`).
    DottedCase,
    /// `UPPER_CASE` — typically reserved for constants.
    UpperCase,
    /// `lowercase` — a single all-lowercase word with no separators.
    Lowercase,
    /// `symbols` — a name containing *only* non-alphanumeric characters,
    /// e.g. an operator overload like `%+%` or `!=`. Part of lintr's default
    /// styles so operator definitions pass under the default configuration.
    Symbols,
    /// `any` — disable the check for this kind of symbol.
    Any,
}

/// The `(config name, variant)` table backing [`ObjectNameStyle::from_config_name`]
/// and [`ObjectNameStyle::config_names`], so the recognized set and the
/// human-visible name list cannot drift apart.
const OBJECT_NAME_STYLE_NAMES: &[(&str, ObjectNameStyle)] = &[
    ("snake_case", ObjectNameStyle::SnakeCase),
    ("camelCase", ObjectNameStyle::CamelCase),
    ("dotted.case", ObjectNameStyle::DottedCase),
    ("UPPER_CASE", ObjectNameStyle::UpperCase),
    ("lowercase", ObjectNameStyle::Lowercase),
    ("symbols", ObjectNameStyle::Symbols),
    ("any", ObjectNameStyle::Any),
];

impl ObjectNameStyle {
    /// Parse an object-name style name (as written in `.lintr` or
    /// `raven.toml`) into the enum, returning `None` for any value Raven
    /// does not recognize as a named style.
    ///
    /// This is the **single source of truth** for the set of style names
    /// Raven understands. The JSON/TOML parser skips unknown style elements;
    /// the `.lintr` loader deliberately treats non-empty unknown `styles`
    /// elements as regexes, which is a lenient extension over lintr's own
    /// style-name validation.
    pub fn from_config_name(value: &str) -> Option<Self> {
        OBJECT_NAME_STYLE_NAMES
            .iter()
            .find(|(name, _)| *name == value)
            .map(|&(_, style)| style)
    }

    /// All recognized config-name strings, in canonical order. Reused by the
    /// `.lintr` loader's did-you-mean hint so its name list cannot drift from
    /// [`Self::from_config_name`].
    pub fn config_names() -> impl Iterator<Item = &'static str> {
        OBJECT_NAME_STYLE_NAMES.iter().map(|&(name, _)| name)
    }
}

/// Regex accepted by the `object_name` lint.
///
/// The compiled form is stored in [`LintConfig`] so lint passes can test names
/// without recompiling user patterns on every debounced edit. `Debug` and
/// equality intentionally use only the original source string (via
/// `Regex::as_str`) because `regex::Regex` does not implement equality and
/// the source is the stable configuration value users supplied.
#[derive(Clone)]
pub struct CompiledRegex {
    regex: regex::Regex,
}

/// Cap on [`compile_cached`]'s process-wide pattern cache. Real configurations
/// hold a handful of patterns; the cap only guards against pathological churn
/// (e.g. a config file rewritten with generated patterns).
const REGEX_CACHE_CAP: usize = 512;

/// Compile `source`, memoizing successes in a process-wide LRU cache.
///
/// Some callers re-parse lint configuration repeatedly — per-document
/// `[[linting.overrides]]` resolution re-runs on every debounced edit, and the
/// `.lintr` loader validates each pattern before the backend compiles it again
/// — so compilation must be cheap on repeat. `regex::Regex` is internally
/// reference-counted, making the cached clone trivial. Errors are not cached:
/// they are already rare, warned about once per parse, and re-erroring is
/// cheap relative to a successful compile. The lock is exclusive but the
/// critical sections are tiny and only config parsing takes it — the
/// per-name `is_match` hot loop never touches the cache.
fn compile_cached(source: &str) -> Result<regex::Regex, regex::Error> {
    use std::num::NonZeroUsize;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<lru::LruCache<String, regex::Regex>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        Mutex::new(lru::LruCache::new(
            NonZeroUsize::new(REGEX_CACHE_CAP).expect("cap is nonzero"),
        ))
    });
    if let Ok(mut map) = cache.lock()
        && let Some(regex) = map.get(source)
    {
        return Ok(regex.clone());
    }
    let regex = regex::Regex::new(source)?;
    if let Ok(mut map) = cache.lock() {
        map.push(source.to_string(), regex.clone());
    }
    Ok(regex)
}

impl CompiledRegex {
    /// Compile a user-provided regex source string (memoized; see
    /// `compile_cached`).
    pub fn new(source: &str) -> Result<Self, regex::Error> {
        Ok(Self {
            regex: compile_cached(source)?,
        })
    }

    /// Original pattern text from the user's configuration.
    pub fn source(&self) -> &str {
        self.regex.as_str()
    }

    /// Return whether this pattern matches `name`.
    pub(crate) fn is_match(&self, name: &str) -> bool {
        self.regex.is_match(name)
    }
}

impl fmt::Debug for CompiledRegex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CompiledRegex")
            .field(&self.source())
            .finish()
    }
}

impl PartialEq for CompiledRegex {
    fn eq(&self, other: &Self) -> bool {
        self.source() == other.source()
    }
}

/// Lint configuration.
///
/// `enabled` is the master switch (default off). Each rule has its own
/// `Option<DiagnosticSeverity>` so individual rules can also be disabled by
/// setting their severity to "off". `line_length` controls the threshold used
/// by the line-length rule.
#[derive(Debug, Clone, PartialEq)]
pub struct LintConfig {
    /// Master switch. When `false`, [`crate::linting::run_lints`] returns an
    /// empty vector regardless of per-rule severities.
    pub enabled: bool,
    /// Maximum allowed line length, measured in characters (matching
    /// lintr's `nchar()`).
    pub line_length: u32,
    /// Maximum allowed identifier length (object-length rule). Identifiers
    /// longer than this are flagged. Measured in characters of the name.
    pub object_length: u32,
    /// Number of spaces per indentation level used by the indentation rule
    /// (`lintr::indentation_linter`). Defaults to 2 to match `lintr`.
    pub indentation_unit: u32,
    /// Preferred assignment operator style.
    pub assignment_operator_style: AssignmentOperatorStyle,
    /// Preferred string-literal delimiter (used by the `quotes` rule).
    pub string_delimiter: StringDelimiter,
    /// Accepted named styles for top-level functions (assignments whose RHS is
    /// a `function() ...` expression). The name passes if it matches any named
    /// style here or any regex in [`Self::object_name_regexes_function`].
    ///
    /// `Any` anywhere in this list disables the whole function-name check and
    /// short-circuits regex matching. When this list and the regex list are
    /// both empty, the function-name check is disabled; an explicit empty
    /// styles list with regexes is regex-only mode.
    pub object_name_style_function: Vec<ObjectNameStyle>,
    /// Accepted named styles for variable assignments (assignments whose RHS
    /// is not a function definition). See
    /// [`Self::object_name_style_function`] for `Any`, empty-list, and regex
    /// combination semantics.
    pub object_name_style_variable: Vec<ObjectNameStyle>,
    /// Accepted named styles for function formal arguments. Applies to all
    /// `function(...)` definitions, whether anonymous or assigned. See
    /// [`Self::object_name_style_function`] for `Any`, empty-list, and regex
    /// combination semantics.
    pub object_name_style_argument: Vec<ObjectNameStyle>,
    /// Accepted regexes for top-level function names. Regexes are partial
    /// matches against the full identifier, including a leading dot when one
    /// is present. They are ignored when the corresponding style list contains
    /// [`ObjectNameStyle::Any`].
    pub object_name_regexes_function: Vec<CompiledRegex>,
    /// Accepted regexes for variable names. See
    /// [`Self::object_name_regexes_function`] for matching semantics.
    pub object_name_regexes_variable: Vec<CompiledRegex>,
    /// Accepted regexes for argument names. See
    /// [`Self::object_name_regexes_function`] for matching semantics.
    pub object_name_regexes_argument: Vec<CompiledRegex>,
    /// Severity for the line-length rule. `None` disables the rule.
    pub line_length_severity: Option<DiagnosticSeverity>,
    /// Severity for the trailing-whitespace rule. `None` disables the rule.
    pub trailing_whitespace_severity: Option<DiagnosticSeverity>,
    /// Severity for the no-tab rule. `None` disables the rule.
    pub no_tab_severity: Option<DiagnosticSeverity>,
    /// Severity for the trailing-blank-lines rule. `None` disables the rule.
    pub trailing_blank_lines_severity: Option<DiagnosticSeverity>,
    /// Severity for the assignment-operator rule. `None` disables the rule.
    pub assignment_operator_severity: Option<DiagnosticSeverity>,
    /// Severity for the object-name rule. `None` disables the rule entirely;
    /// per-kind `Any` styles or empty styles+regexes disable individual
    /// checks while still running other object-name checks.
    pub object_name_severity: Option<DiagnosticSeverity>,
    /// Severity for the infix-spaces rule. `None` disables the rule.
    pub infix_spaces_severity: Option<DiagnosticSeverity>,
    /// Severity for the commented-code rule. `None` disables the rule.
    pub commented_code_severity: Option<DiagnosticSeverity>,
    /// Severity for the quotes rule (`lintr::quotes_linter`). `None` disables.
    pub quotes_severity: Option<DiagnosticSeverity>,
    /// Severity for the commas rule (`lintr::commas_linter`). `None` disables.
    pub commas_severity: Option<DiagnosticSeverity>,
    /// Severity for the `T`/`F` symbol rule (`lintr::T_and_F_symbol_linter`).
    pub t_and_f_symbol_severity: Option<DiagnosticSeverity>,
    /// Severity for the semicolon rule (`lintr::semicolon_linter`).
    pub semicolon_severity: Option<DiagnosticSeverity>,
    /// Severity for the `== NA` / `!= NA` rule (`lintr::equals_na_linter`).
    pub equals_na_severity: Option<DiagnosticSeverity>,
    /// Severity for the object-length rule (`lintr::object_length_linter`).
    pub object_length_severity: Option<DiagnosticSeverity>,
    /// Severity for the vector-logic rule (`lintr::vector_logic_linter`).
    pub vector_logic_severity: Option<DiagnosticSeverity>,
    /// Severity for the function-left-parentheses rule
    /// (`lintr::function_left_parentheses_linter`).
    pub function_left_parentheses_severity: Option<DiagnosticSeverity>,
    /// Severity for the spaces-inside rule (`lintr::spaces_inside_linter`).
    pub spaces_inside_severity: Option<DiagnosticSeverity>,
    /// Severity for the indentation rule (`lintr::indentation_linter`).
    pub indentation_severity: Option<DiagnosticSeverity>,
}

impl Default for LintConfig {
    fn default() -> Self {
        Self {
            // Conservative default: feature is opt-in until it stabilizes.
            enabled: false,
            line_length: 80,
            object_length: 30,
            indentation_unit: 2,
            assignment_operator_style: AssignmentOperatorStyle::default(),
            string_delimiter: StringDelimiter::default(),
            // lintr's default is `styles = c("snake_case", "symbols")`; the
            // `symbols` member is what lets operator definitions like
            // `` `%+%` <- ... `` pass under the default configuration.
            object_name_style_function: vec![ObjectNameStyle::SnakeCase, ObjectNameStyle::Symbols],
            object_name_style_variable: vec![ObjectNameStyle::SnakeCase, ObjectNameStyle::Symbols],
            object_name_style_argument: vec![ObjectNameStyle::SnakeCase, ObjectNameStyle::Symbols],
            object_name_regexes_function: Vec::new(),
            object_name_regexes_variable: Vec::new(),
            object_name_regexes_argument: Vec::new(),
            // Default severities mirror lintr's "style" tier and surface as
            // LSP Information, matching REditorSupport's languageserver.
            line_length_severity: Some(DiagnosticSeverity::INFORMATION),
            trailing_whitespace_severity: Some(DiagnosticSeverity::INFORMATION),
            no_tab_severity: Some(DiagnosticSeverity::INFORMATION),
            trailing_blank_lines_severity: Some(DiagnosticSeverity::INFORMATION),
            assignment_operator_severity: Some(DiagnosticSeverity::INFORMATION),
            object_name_severity: Some(DiagnosticSeverity::INFORMATION),
            infix_spaces_severity: Some(DiagnosticSeverity::INFORMATION),
            commented_code_severity: Some(DiagnosticSeverity::INFORMATION),
            quotes_severity: Some(DiagnosticSeverity::INFORMATION),
            commas_severity: Some(DiagnosticSeverity::INFORMATION),
            t_and_f_symbol_severity: Some(DiagnosticSeverity::INFORMATION),
            semicolon_severity: Some(DiagnosticSeverity::INFORMATION),
            equals_na_severity: Some(DiagnosticSeverity::INFORMATION),
            object_length_severity: Some(DiagnosticSeverity::INFORMATION),
            vector_logic_severity: Some(DiagnosticSeverity::INFORMATION),
            function_left_parentheses_severity: Some(DiagnosticSeverity::INFORMATION),
            spaces_inside_severity: Some(DiagnosticSeverity::INFORMATION),
            indentation_severity: Some(DiagnosticSeverity::INFORMATION),
        }
    }
}

/// Master-switch tri-state. Parsed from `raven.linting.enabled` (and the
/// `[linting] enabled` field in `raven.toml`).
///
/// `Auto` resolves to `true` when a `.lintr` is the discovered project config
/// (preserving the implicit opt-in users had before #281), `false` otherwise.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum LintEnabled {
    #[default]
    Auto,
    On,
    Off,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_name_maps_known_styles() {
        assert_eq!(
            ObjectNameStyle::from_config_name("snake_case"),
            Some(ObjectNameStyle::SnakeCase)
        );
        assert_eq!(
            ObjectNameStyle::from_config_name("camelCase"),
            Some(ObjectNameStyle::CamelCase)
        );
        assert_eq!(
            ObjectNameStyle::from_config_name("dotted.case"),
            Some(ObjectNameStyle::DottedCase)
        );
        assert_eq!(
            ObjectNameStyle::from_config_name("UPPER_CASE"),
            Some(ObjectNameStyle::UpperCase)
        );
        assert_eq!(
            ObjectNameStyle::from_config_name("lowercase"),
            Some(ObjectNameStyle::Lowercase)
        );
        assert_eq!(
            ObjectNameStyle::from_config_name("any"),
            Some(ObjectNameStyle::Any)
        );
    }

    #[test]
    fn from_config_name_rejects_unknown_and_regex() {
        assert_eq!(ObjectNameStyle::from_config_name("kebab-case"), None);
        assert_eq!(ObjectNameStyle::from_config_name("^[a-z][a-z0-9_]*$"), None);
        assert_eq!(ObjectNameStyle::from_config_name(""), None);
    }

    #[test]
    fn compiled_regex_debug_clone_and_eq_use_source() {
        let regex = CompiledRegex::new("^x").unwrap();
        let cloned = regex.clone();

        assert_eq!(regex, cloned);
        assert_eq!(regex.source(), "^x");
        assert_eq!(format!("{regex:?}"), "CompiledRegex(\"^x\")");
        assert!(regex.is_match("xyz"));
    }
}
