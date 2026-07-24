//! Clean-room JAGS 4.3.2 grammar for the tree-sitter parsing library.
//!
//! The grammar is based on independently authored black-box probes of the JAGS
//! command-line parser. It is not linked to JAGS and contains no JAGS source.

use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    fn tree_sitter_jags() -> *const ();
}

/// Tree-sitter language function for the JAGS grammar.
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_jags) };

/// Generated node type metadata for the JAGS grammar.
pub const NODE_TYPES: &str = include_str!("../../src/node-types.json");

#[cfg(test)]
mod tests {
    #[test]
    fn loads_language() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&super::LANGUAGE.into())
            .expect("load JAGS grammar");
    }
}
