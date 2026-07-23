//! Built-in types, keywords, distributions, and callables for Stan.
//!
//! The callable and distribution catalogs are generated from the compiler
//! metadata exposed by the pinned `stanc3` development dependency. This keeps
//! completion and hover aligned with a concrete Stan release without starting
//! a subprocess or using the network while Raven is running.

/// Built-in Stan types.
pub static STAN_TYPES: &[&str] = &[
    "int",
    "real",
    "vector",
    "row_vector",
    "matrix",
    "simplex",
    "unit_vector",
    "ordered",
    "positive_ordered",
    "corr_matrix",
    "cov_matrix",
    "cholesky_factor_corr",
    "cholesky_factor_cov",
    "sum_to_zero_vector",
    "row_stochastic_matrix",
    "column_stochastic_matrix",
    "sum_to_zero_matrix",
    "void",
    "array",
    "complex",
    "complex_vector",
    "complex_row_vector",
    "complex_matrix",
    "tuple",
];

/// Stan program block keywords.
pub static STAN_BLOCK_KEYWORDS: &[&str] = &[
    "functions",
    "data",
    "transformed data",
    "parameters",
    "transformed parameters",
    "model",
    "generated quantities",
];

/// Stan control-flow and statement keywords.
pub static STAN_CONTROL_FLOW: &[&str] = &[
    "for", "in", "while", "if", "else", "return", "break", "continue", "print", "reject", "profile",
];

/// A compiler-known Stan callable and a bounded sample of its overloads.
pub struct StanCallable {
    /// The identifier used to call the function.
    pub name: &'static str,
    /// Representative compiler signatures displayed by hover.
    pub signatures: &'static [&'static str],
    /// The total number of compiler signatures, including omitted overloads.
    pub signature_count: usize,
}

/// A Stan distribution accepted by sampling-statement syntax.
pub struct StanDistribution {
    /// The bare distribution name used after `~`.
    pub name: &'static str,
    /// The normalized density or mass function used to describe the family.
    pub canonical_function: &'static str,
}

include!("stan_builtins_generated.rs");

/// Looks up a compiler-known callable by its exact, case-sensitive name.
pub fn callable(name: &str) -> Option<&'static StanCallable> {
    STAN_CALLABLES
        .binary_search_by(|entry| entry.name.cmp(name))
        .ok()
        .map(|index| &STAN_CALLABLES[index])
}

/// Looks up a sampling-statement distribution by its exact name.
pub fn distribution(name: &str) -> Option<&'static StanDistribution> {
    STAN_DISTRIBUTIONS
        .binary_search_by(|entry| entry.name.cmp(name))
        .ok()
        .map(|index| &STAN_DISTRIBUTIONS[index])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_catalogs_are_sorted_and_unique() {
        assert!(
            STAN_CALLABLES
                .windows(2)
                .all(|pair| pair[0].name < pair[1].name)
        );
        assert!(
            STAN_DISTRIBUTIONS
                .windows(2)
                .all(|pair| pair[0].name < pair[1].name)
        );
    }

    #[test]
    fn every_distribution_has_a_canonical_callable() {
        for distribution in STAN_DISTRIBUTIONS {
            assert!(
                callable(distribution.canonical_function).is_some(),
                "missing {} for {}",
                distribution.canonical_function,
                distribution.name
            );
        }
    }

    #[test]
    fn generated_catalog_includes_unnormalized_probability_aliases() {
        assert!(callable("normal_lupdf").is_some());
        assert!(callable("poisson_lupmf").is_some());
    }

    #[test]
    fn generated_catalog_has_pinned_provenance() {
        assert_eq!(STAN_COMPILER_VERSION, "2.39.1");
        assert_eq!(STAN_DOCS_VERSION, "2_39");
    }
}
