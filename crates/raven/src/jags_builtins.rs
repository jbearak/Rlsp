//! Versioned built-in catalog for the JAGS language.
//!
//! The generated data contains factual names, aliases, arities, and providing
//! modules observed in JAGS 4.3.2. Raven generates it from a checked-in,
//! network-free manifest; no JAGS code, prose, subprocess, or runtime library
//! is bundled or loaded.

/// An ordinary callable accepted by JAGS.
pub struct JagsCallable {
    /// Identifier used at the call site.
    pub name: &'static str,
    /// Registry name when this entry is an alias.
    pub canonical_name: &'static str,
    /// Automatically loaded module or compiler component providing the name.
    pub module: &'static str,
    /// Stable display labels derived from the cataloged arity.
    pub parameters: &'static [&'static str],
    /// Whether the callable accepts one or more values rather than a fixed arity.
    pub variadic: bool,
}

impl JagsCallable {
    /// Formats a compact JAGS call signature.
    pub fn signature(&self) -> String {
        format_signature(self.name, self.parameters, self.variadic)
    }
}

/// A distribution accepted on the right side of a stochastic relation.
pub struct JagsDistribution {
    /// Name used after `~`, including accepted aliases.
    pub name: &'static str,
    /// Registry name when this entry is an alias.
    pub canonical_name: &'static str,
    /// Automatically loaded module providing the distribution.
    pub module: &'static str,
    /// Distribution parameters, excluding the stochastic node on the left.
    pub parameters: &'static [&'static str],
    /// Whether the distribution accepts one or more parameters rather than a fixed arity.
    pub variadic: bool,
}

impl JagsDistribution {
    /// Formats a compact JAGS distribution signature.
    pub fn signature(&self) -> String {
        format_signature(self.name, self.parameters, self.variadic)
    }
}

fn format_signature(name: &str, parameters: &[&str], variadic: bool) -> String {
    let mut labels = parameters.to_vec();
    if variadic {
        labels.push("...");
    }
    format!("{name}({})", labels.join(", "))
}

include!("jags_builtins_generated.rs");

/// Looks up an ordinary callable by its exact, case-sensitive name.
pub fn callable(name: &str) -> Option<&'static JagsCallable> {
    JAGS_CALLABLES
        .binary_search_by(|entry| entry.name.cmp(name))
        .ok()
        .map(|index| &JAGS_CALLABLES[index])
}

/// Looks up a distribution by its exact, case-sensitive name.
pub fn distribution(name: &str) -> Option<&'static JagsDistribution> {
    JAGS_DISTRIBUTIONS
        .binary_search_by(|entry| entry.name.cmp(name))
        .ok()
        .map(|index| &JAGS_DISTRIBUTIONS[index])
}

/// Returns whether `name` is syntax or a built-in callable/distribution.
pub fn is_builtin_name(name: &str) -> bool {
    JAGS_KEYWORDS.contains(&name)
        || JAGS_CONTEXTUAL_SYNTAX.contains(&name)
        || callable(name).is_some()
        || distribution(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_catalogs_are_sorted_and_unique() {
        assert!(
            JAGS_CALLABLES
                .windows(2)
                .all(|pair| pair[0].name < pair[1].name)
        );
        assert!(
            JAGS_DISTRIBUTIONS
                .windows(2)
                .all(|pair| pair[0].name < pair[1].name)
        );
    }

    #[test]
    fn generated_catalog_has_pinned_provenance_and_modules() {
        assert_eq!(JAGS_VERSION, "4.3.2");
        assert_eq!(
            JAGS_SOURCE_SHA256,
            "871f556af403a7c2ce6a0f02f15cf85a572763e093d26658ebac55c4ab472fc8"
        );
        assert_eq!(JAGS_AUTOMATIC_MODULES, ["basemod", "bugs"]);
        assert!(
            JAGS_CALLABLES
                .iter()
                .all(|entry| matches!(entry.module, "basemod" | "bugs" | "compiler"))
        );
        assert!(
            JAGS_DISTRIBUTIONS
                .iter()
                .all(|entry| entry.module == "bugs")
        );
    }

    #[test]
    fn syntax_classification_matches_jags() {
        assert_eq!(JAGS_KEYWORDS, ["data", "for", "in", "model", "var"]);
        assert_eq!(JAGS_CONTEXTUAL_SYNTAX, ["I", "T"]);
        assert!(!JAGS_KEYWORDS.contains(&"if"));
        assert!(!JAGS_KEYWORDS.contains(&"else"));
        assert!(callable("T").is_none());
        assert!(callable("I").is_none());
    }

    #[test]
    fn compiler_specials_aliases_and_dual_roles_are_present() {
        assert_eq!(callable("length").unwrap().signature(), "length(variable)");
        assert_eq!(callable("dim").unwrap().signature(), "dim(variable)");
        assert_eq!(callable("pow").unwrap().canonical_name, "^");
        assert_eq!(callable("acos").unwrap().canonical_name, "arccos");
        assert_eq!(distribution("dbinom").unwrap().canonical_name, "dbin");
        assert!(callable("dnorm").is_some());
        assert!(distribution("dnorm").is_some());
    }

    #[test]
    fn only_verified_generated_probability_forms_are_present() {
        for present in [
            "dbeta",
            "pbeta",
            "qbeta",
            "logdensity.beta",
            "dpois",
            "ppois",
            "qpois",
            "logdensity.pois",
        ] {
            assert!(callable(present).is_some(), "missing {present}");
        }
        assert!(callable("dbern").is_none());
        assert!(callable("pbern").is_none());
        assert!(callable("qbern").is_none());
    }

    #[test]
    fn optional_module_names_are_absent() {
        for absent in ["mexp", "dscaled.gamma", "dordered.logit"] {
            assert!(callable(absent).is_none(), "unexpected callable {absent}");
            assert!(
                distribution(absent).is_none(),
                "unexpected distribution {absent}"
            );
        }
    }
}
