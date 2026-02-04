/// Reserved words module for R language.
///
/// This module provides a centralized list of R reserved words and a function
/// to check if a given name is a reserved word. Reserved words cannot be used
/// as user-defined identifiers in R.

/// Complete list of R reserved words for this feature.
/// These words cannot be used as user-defined identifiers.
pub const RESERVED_WORDS: &[&str] = &[
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
];

/// Check if a name is an R reserved word.
///
/// Returns `true` if `name` matches any of the R reserved words,
/// `false` otherwise. The check is case-sensitive.
///
/// # Examples
///
/// ```
/// use raven::reserved_words::is_reserved_word;
///
/// assert!(is_reserved_word("if"));
/// assert!(is_reserved_word("else"));
/// assert!(is_reserved_word("TRUE"));
/// assert!(is_reserved_word("NA_integer_"));
///
/// assert!(!is_reserved_word("myvar"));
/// assert!(!is_reserved_word("true"));  // Case-sensitive: "true" is not reserved
/// assert!(!is_reserved_word("ELSE"));  // Case-sensitive: "ELSE" is not reserved
/// ```
pub fn is_reserved_word(name: &str) -> bool {
    matches!(
        name,
        "if" | "else"
            | "repeat"
            | "while"
            | "function"
            | "for"
            | "in"
            | "next"
            | "break"
            | "TRUE"
            | "FALSE"
            | "NULL"
            | "Inf"
            | "NaN"
            | "NA"
            | "NA_integer_"
            | "NA_real_"
            | "NA_complex_"
            | "NA_character_"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ========================================================================
    // Property-Based Tests
    // ========================================================================

    /// Strategy to generate a reserved word from the set.
    fn reserved_word_strategy() -> impl Strategy<Value = &'static str> {
        prop::sample::select(RESERVED_WORDS)
    }

    /// Strategy to generate valid R identifiers that are NOT reserved words.
    /// R identifiers must start with a letter or dot (if not followed by digit),
    /// and can contain letters, digits, dots, and underscores.
    fn non_reserved_identifier_strategy() -> impl Strategy<Value = String> {
        // Generate identifiers that:
        // 1. Start with a lowercase letter (to avoid case-sensitivity edge cases)
        // 2. Contain only valid R identifier characters
        // 3. Are NOT in the reserved word set
        "[a-z][a-z0-9_.]{0,15}"
            .prop_filter("Must not be a reserved word", |s| !is_reserved_word(s))
    }

    /// Strategy to generate arbitrary strings (including edge cases).
    fn arbitrary_string_strategy() -> impl Strategy<Value = String> {
        prop::string::string_regex("[a-zA-Z0-9_. ]{0,20}")
            .unwrap()
            .prop_filter("Must not be a reserved word", |s| !is_reserved_word(s))
    }

    // ========================================================================
    // **Feature: reserved-keyword-handling, Property 1: Reserved Word Identification**
    // **Validates: Requirements 1.1, 1.2**
    //
    // For any string that is in the set of R reserved words, is_reserved_word()
    // SHALL return true. For any string that is NOT in this set, is_reserved_word()
    // SHALL return false.
    // ========================================================================

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        /// Property 1a: All reserved words are correctly identified as reserved.
        ///
        /// For any string selected from the RESERVED_WORDS set, is_reserved_word()
        /// SHALL return true.
        #[test]
        fn prop_reserved_words_return_true(word in reserved_word_strategy()) {
            prop_assert!(
                is_reserved_word(word),
                "Expected is_reserved_word('{}') to return true, but got false",
                word
            );
        }

        /// Property 1b: Non-reserved identifiers are correctly identified as not reserved.
        ///
        /// For any valid R identifier that is NOT in the reserved word set,
        /// is_reserved_word() SHALL return false.
        #[test]
        fn prop_non_reserved_identifiers_return_false(ident in non_reserved_identifier_strategy()) {
            prop_assert!(
                !is_reserved_word(&ident),
                "Expected is_reserved_word('{}') to return false, but got true",
                ident
            );
        }

        /// Property 1c: Arbitrary non-reserved strings return false.
        ///
        /// For any arbitrary string that is NOT in the reserved word set,
        /// is_reserved_word() SHALL return false.
        #[test]
        fn prop_arbitrary_non_reserved_strings_return_false(s in arbitrary_string_strategy()) {
            prop_assert!(
                !is_reserved_word(&s),
                "Expected is_reserved_word('{}') to return false, but got true",
                s
            );
        }
    }

    // ========================================================================
    // Unit Tests
    // ========================================================================

    #[test]
    fn test_control_flow_reserved_words() {
        assert!(is_reserved_word("if"));
        assert!(is_reserved_word("else"));
        assert!(is_reserved_word("repeat"));
        assert!(is_reserved_word("while"));
        assert!(is_reserved_word("function"));
        assert!(is_reserved_word("for"));
        assert!(is_reserved_word("in"));
        assert!(is_reserved_word("next"));
        assert!(is_reserved_word("break"));
    }

    #[test]
    fn test_logical_constants() {
        assert!(is_reserved_word("TRUE"));
        assert!(is_reserved_word("FALSE"));
    }

    #[test]
    fn test_null() {
        assert!(is_reserved_word("NULL"));
    }

    #[test]
    fn test_special_numeric() {
        assert!(is_reserved_word("Inf"));
        assert!(is_reserved_word("NaN"));
    }

    #[test]
    fn test_na_variants() {
        assert!(is_reserved_word("NA"));
        assert!(is_reserved_word("NA_integer_"));
        assert!(is_reserved_word("NA_real_"));
        assert!(is_reserved_word("NA_complex_"));
        assert!(is_reserved_word("NA_character_"));
    }

    #[test]
    fn test_non_reserved_words() {
        assert!(!is_reserved_word("myvar"));
        assert!(!is_reserved_word("x"));
        assert!(!is_reserved_word("data"));
        assert!(!is_reserved_word("library")); // Not a reserved word, just a function
        assert!(!is_reserved_word("require")); // Not a reserved word, just a function
        assert!(!is_reserved_word("return")); // Not a reserved word, just a function
        assert!(!is_reserved_word("print")); // Not a reserved word, just a function
    }

    #[test]
    fn test_case_sensitivity() {
        // Reserved words are case-sensitive
        assert!(!is_reserved_word("IF"));
        assert!(!is_reserved_word("ELSE"));
        assert!(!is_reserved_word("true"));
        assert!(!is_reserved_word("false"));
        assert!(!is_reserved_word("null"));
        assert!(!is_reserved_word("inf"));
        assert!(!is_reserved_word("nan"));
        assert!(!is_reserved_word("na"));
    }

    #[test]
    fn test_edge_cases() {
        assert!(!is_reserved_word("")); // Empty string
        assert!(!is_reserved_word(" ")); // Whitespace
        assert!(!is_reserved_word("if ")); // Trailing space
        assert!(!is_reserved_word(" if")); // Leading space
        assert!(!is_reserved_word("myelse")); // Contains reserved word but is not one
        assert!(!is_reserved_word("elseif")); // Contains reserved word but is not one
        assert!(!is_reserved_word("NA_")); // Partial NA variant
        assert!(!is_reserved_word("NA_string_")); // Invalid NA variant
    }

    #[test]
    fn test_reserved_words_constant_count() {
        // Verify we have exactly 19 reserved words as specified
        assert_eq!(RESERVED_WORDS.len(), 19);
    }

    #[test]
    fn test_reserved_words_constant_matches_function() {
        // Verify that all words in the constant are recognized by the function
        for word in RESERVED_WORDS {
            assert!(
                is_reserved_word(word),
                "RESERVED_WORDS contains '{}' but is_reserved_word() returns false",
                word
            );
        }
    }
}
