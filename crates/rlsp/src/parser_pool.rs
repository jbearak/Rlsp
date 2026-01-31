//
// parser_pool.rs
//
// Thread-local parser pool for efficient parser reuse
//

use std::cell::RefCell;
use tree_sitter::Parser;

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new({
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_r::LANGUAGE.into())
            .expect("Failed to set R language");
        parser
    });
}

/// Execute a function with a thread-local parser instance.
/// The parser is reused across calls on the same thread.
pub fn with_parser<F, R>(f: F) -> R
where
    F: FnOnce(&mut Parser) -> R,
{
    PARSER.with(|parser| f(&mut parser.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parser_initialized_with_r_language() {
        // Parser should be able to parse R code
        let result = with_parser(|parser| {
            parser.parse("x <- 1", None).is_some()
        });
        assert!(result, "Parser should successfully parse R code");
    }

    #[test]
    fn test_parser_reuse_on_same_thread() {
        // Multiple calls should succeed (reusing same parser)
        let result1 = with_parser(|parser| parser.parse("a <- 1", None).is_some());
        let result2 = with_parser(|parser| parser.parse("b <- 2", None).is_some());
        let result3 = with_parser(|parser| parser.parse("c <- 3", None).is_some());
        
        assert!(result1 && result2 && result3, "All parses should succeed");
    }

    #[test]
    fn test_parser_state_reset_between_uses() {
        // Parse a complete program
        let tree1 = with_parser(|parser| parser.parse("function(x) { x + 1 }", None));
        assert!(tree1.is_some());
        
        // Parse a different program - should work independently
        let tree2 = with_parser(|parser| parser.parse("y <- 42", None));
        assert!(tree2.is_some());
        
        // Verify trees are independent by checking their structure differs
        let tree1 = tree1.unwrap();
        let tree2 = tree2.unwrap();
        let root1 = tree1.root_node();
        let root2 = tree2.root_node();
        // Both roots are "program", but their children should differ
        assert_eq!(root1.kind(), "program");
        assert_eq!(root2.kind(), "program");
        // First child of tree1 is a function_definition, tree2 is a binary_operator (assignment)
        let child1 = root1.child(0).map(|n| n.kind());
        let child2 = root2.child(0).map(|n| n.kind());
        assert_ne!(child1, child2, "Trees should have different structure");
    }
}

// ============================================================================
// Property Tests for Parser Instance Reuse
// Property 3: Parser Instance Reuse - validates Requirements 2.1, 2.2
// ============================================================================

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;

    /// Generate valid R code snippets for parsing
    fn r_code_snippet() -> impl Strategy<Value = String> {
        prop_oneof![
            "[a-z][a-z0-9_]{0,5}".prop_map(|name| format!("{} <- 1", name)),
            "[a-z][a-z0-9_]{0,5}".prop_map(|name| format!("{} <- function(x) x + 1", name)),
            "[a-z][a-z0-9_]{0,5}".prop_map(|name| format!("print({})", name)),
            Just("x <- 1\ny <- 2".to_string()),
            Just("for (i in 1:10) print(i)".to_string()),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        /// Property 3: For any sequence of R code snippets parsed on the same thread,
        /// the parser instance should be reused and all parses should succeed.
        #[test]
        fn prop_parser_instance_reuse(
            snippets in prop::collection::vec(r_code_snippet(), 1..10)
        ) {
            // All parses should succeed using the same thread-local parser
            for snippet in &snippets {
                let result = with_parser(|parser| parser.parse(snippet, None));
                prop_assert!(result.is_some(), "Parser should successfully parse: {}", snippet);
            }
        }

        /// Property 3 extended: Parser should handle varied code complexity
        #[test]
        fn prop_parser_handles_varied_complexity(
            simple in r_code_snippet(),
            complex in r_code_snippet()
        ) {
            // Parse simple code first
            let result1 = with_parser(|parser| parser.parse(&simple, None));
            prop_assert!(result1.is_some());

            // Parse complex code - should work independently
            let result2 = with_parser(|parser| parser.parse(&complex, None));
            prop_assert!(result2.is_some());

            // Parse simple again - parser state should not affect result
            let result3 = with_parser(|parser| parser.parse(&simple, None));
            prop_assert!(result3.is_some());
        }
    }
}
