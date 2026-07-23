//! Built-in types, keywords, distributions, and functions for the Stan language.
//!
//! These tables target Stan approximately version 2.35 and are used by the
//! completion handler for `.stan` files. Bare distribution names serve sampling
//! statements such as `y ~ normal(mu, sigma)`. Distribution suffixes are listed
//! explicitly per family because Stan's availability is irregular: for example,
//! `poisson_lpdf` and `categorical_cdf` do not exist.

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

/// A Stan distribution and the probability-function suffixes it supports.
pub struct StanDistribution {
    /// The bare distribution name used in sampling statements.
    pub name: &'static str,
    /// Valid suffixes that form callable probability functions for this family.
    pub suffixes: &'static [&'static str],
}

const CONTINUOUS_FULL: &[&str] = &["_lpdf", "_cdf", "_lcdf", "_lccdf", "_rng"];
const DISCRETE_FULL: &[&str] = &["_lpmf", "_cdf", "_lcdf", "_lccdf", "_rng"];
const CONTINUOUS_LOG_CDF_RNG: &[&str] = &["_lpdf", "_lcdf", "_lccdf", "_rng"];
const CONTINUOUS_CDF_RNG: &[&str] = &["_lpdf", "_cdf", "_rng"];
const CONTINUOUS_RNG: &[&str] = &["_lpdf", "_rng"];
const DISCRETE_RNG: &[&str] = &["_lpmf", "_rng"];
const CONTINUOUS_DENSITY: &[&str] = &["_lpdf"];
const DISCRETE_MASS: &[&str] = &["_lpmf"];

/// Stan distributions and the valid generated probability-function variants.
pub static STAN_DISTRIBUTIONS: &[StanDistribution] = &[
    StanDistribution {
        name: "bernoulli",
        suffixes: DISCRETE_FULL,
    },
    StanDistribution {
        name: "beta_binomial",
        suffixes: DISCRETE_FULL,
    },
    StanDistribution {
        name: "binomial",
        suffixes: DISCRETE_FULL,
    },
    StanDistribution {
        name: "discrete_range",
        suffixes: DISCRETE_FULL,
    },
    StanDistribution {
        name: "neg_binomial",
        suffixes: DISCRETE_FULL,
    },
    StanDistribution {
        name: "neg_binomial_2",
        suffixes: DISCRETE_FULL,
    },
    StanDistribution {
        name: "poisson",
        suffixes: DISCRETE_FULL,
    },
    StanDistribution {
        name: "bernoulli_logit",
        suffixes: DISCRETE_RNG,
    },
    StanDistribution {
        name: "bernoulli_logit_glm",
        suffixes: DISCRETE_RNG,
    },
    StanDistribution {
        name: "categorical",
        suffixes: DISCRETE_RNG,
    },
    StanDistribution {
        name: "categorical_logit",
        suffixes: DISCRETE_RNG,
    },
    StanDistribution {
        name: "dirichlet_multinomial",
        suffixes: DISCRETE_RNG,
    },
    StanDistribution {
        name: "hypergeometric",
        suffixes: DISCRETE_RNG,
    },
    StanDistribution {
        name: "multinomial",
        suffixes: DISCRETE_RNG,
    },
    StanDistribution {
        name: "multinomial_logit",
        suffixes: DISCRETE_RNG,
    },
    StanDistribution {
        name: "neg_binomial_2_log",
        suffixes: DISCRETE_RNG,
    },
    StanDistribution {
        name: "ordered_logistic",
        suffixes: DISCRETE_RNG,
    },
    StanDistribution {
        name: "ordered_probit",
        suffixes: DISCRETE_RNG,
    },
    StanDistribution {
        name: "poisson_log",
        suffixes: DISCRETE_RNG,
    },
    StanDistribution {
        name: "binomial_logit",
        suffixes: DISCRETE_MASS,
    },
    StanDistribution {
        name: "binomial_logit_glm",
        suffixes: DISCRETE_MASS,
    },
    StanDistribution {
        name: "categorical_logit_glm",
        suffixes: DISCRETE_MASS,
    },
    StanDistribution {
        name: "neg_binomial_2_log_glm",
        suffixes: DISCRETE_MASS,
    },
    StanDistribution {
        name: "ordered_logistic_glm",
        suffixes: DISCRETE_MASS,
    },
    StanDistribution {
        name: "poisson_log_glm",
        suffixes: DISCRETE_MASS,
    },
    StanDistribution {
        name: "beta",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "cauchy",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "chi_square",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "double_exponential",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "exp_mod_normal",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "exponential",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "frechet",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "gamma",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "gumbel",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "inv_chi_square",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "inv_gamma",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "logistic",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "lognormal",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "normal",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "pareto",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "pareto_type_2",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "rayleigh",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "scaled_inv_chi_square",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "skew_double_exponential",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "skew_normal",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "std_normal",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "student_t",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "uniform",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "von_mises",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "weibull",
        suffixes: CONTINUOUS_FULL,
    },
    StanDistribution {
        name: "beta_proportion",
        suffixes: CONTINUOUS_LOG_CDF_RNG,
    },
    StanDistribution {
        name: "loglogistic",
        suffixes: CONTINUOUS_CDF_RNG,
    },
    StanDistribution {
        name: "dirichlet",
        suffixes: CONTINUOUS_RNG,
    },
    StanDistribution {
        name: "inv_wishart",
        suffixes: CONTINUOUS_RNG,
    },
    StanDistribution {
        name: "inv_wishart_cholesky",
        suffixes: CONTINUOUS_RNG,
    },
    StanDistribution {
        name: "lkj_corr",
        suffixes: CONTINUOUS_RNG,
    },
    StanDistribution {
        name: "lkj_corr_cholesky",
        suffixes: CONTINUOUS_RNG,
    },
    StanDistribution {
        name: "multi_normal",
        suffixes: CONTINUOUS_RNG,
    },
    StanDistribution {
        name: "multi_normal_cholesky",
        suffixes: CONTINUOUS_RNG,
    },
    StanDistribution {
        name: "multi_student_t",
        suffixes: CONTINUOUS_RNG,
    },
    StanDistribution {
        name: "multi_student_t_cholesky",
        suffixes: CONTINUOUS_RNG,
    },
    StanDistribution {
        name: "wishart",
        suffixes: CONTINUOUS_RNG,
    },
    StanDistribution {
        name: "wishart_cholesky",
        suffixes: CONTINUOUS_RNG,
    },
    StanDistribution {
        name: "gaussian_dlm_obs",
        suffixes: CONTINUOUS_DENSITY,
    },
    StanDistribution {
        name: "multi_gp",
        suffixes: CONTINUOUS_DENSITY,
    },
    StanDistribution {
        name: "multi_gp_cholesky",
        suffixes: CONTINUOUS_DENSITY,
    },
    StanDistribution {
        name: "multi_normal_prec",
        suffixes: CONTINUOUS_DENSITY,
    },
    StanDistribution {
        name: "normal_id_glm",
        suffixes: CONTINUOUS_DENSITY,
    },
    StanDistribution {
        name: "wiener",
        suffixes: CONTINUOUS_DENSITY,
    },
];

/// Ordinary Stan built-in functions that are not distribution variants.
///
/// The scalar math function `beta(a, b)` shares its label with the `beta`
/// distribution and is therefore covered by `STAN_DISTRIBUTIONS`.
pub static STAN_FUNCTIONS: &[&str] = &[
    // Scalar and math functions.
    "abs",
    "fabs",
    "fdim",
    "fmin",
    "fmax",
    "fmod",
    "floor",
    "ceil",
    "round",
    "trunc",
    "int_step",
    "step",
    "is_inf",
    "is_nan",
    "sqrt",
    "cbrt",
    "square",
    "exp",
    "exp2",
    "expm1",
    "log",
    "log2",
    "log10",
    "log1p",
    "log1m",
    "log1p_exp",
    "log1m_exp",
    "log_diff_exp",
    "log_mix",
    "log_sum_exp",
    "log_inv_logit",
    "log_inv_logit_diff",
    "log1m_inv_logit",
    "pow",
    "inv",
    "inv_sqrt",
    "inv_square",
    "hypot",
    "cos",
    "sin",
    "tan",
    "acos",
    "asin",
    "atan",
    "atan2",
    "cosh",
    "sinh",
    "tanh",
    "acosh",
    "asinh",
    "atanh",
    "logit",
    "inv_logit",
    "inv_cloglog",
    "erf",
    "erfc",
    "inv_erfc",
    "Phi",
    "inv_Phi",
    "Phi_approx",
    "binary_log_loss",
    "owens_t",
    "inc_beta",
    "inv_inc_beta",
    "lbeta",
    "tgamma",
    "lgamma",
    "digamma",
    "trigamma",
    "lmgamma",
    "gamma_p",
    "gamma_q",
    "choose",
    "lchoose",
    "lambert_w0",
    "lambert_wm1",
    "std_normal_qf",
    "std_normal_log_qf",
    // Reductions and arrays.
    "min",
    "max",
    "sum",
    "prod",
    "mean",
    "variance",
    "sd",
    "norm1",
    "norm2",
    "distance",
    "squared_distance",
    "quantile",
    "dims",
    "num_elements",
    "size",
    "rows",
    "cols",
    "rep_array",
    "append_array",
    "sort_asc",
    "sort_desc",
    "sort_indices_asc",
    "sort_indices_desc",
    "rank",
    "reverse",
    "cumulative_sum",
    // Conversions and constructors.
    "to_vector",
    "to_row_vector",
    "to_matrix",
    "to_array_1d",
    "to_array_2d",
    "rep_vector",
    "rep_row_vector",
    "rep_matrix",
    "identity_matrix",
    "linspaced_vector",
    "linspaced_row_vector",
    "zeros_vector",
    "zeros_row_vector",
    "ones_vector",
    "ones_row_vector",
    // Matrix and linear algebra.
    "dot_product",
    "columns_dot_product",
    "rows_dot_product",
    "dot_self",
    "columns_dot_self",
    "rows_dot_self",
    "crossprod",
    "tcrossprod",
    "quad_form",
    "quad_form_diag",
    "quad_form_sym",
    "trace_quad_form",
    "trace_gen_quad_form",
    "multiply_lower_tri_self_transpose",
    "diag_pre_multiply",
    "diag_post_multiply",
    "add_diag",
    "diagonal",
    "diag_matrix",
    "col",
    "row",
    "block",
    "sub_col",
    "sub_row",
    "head",
    "tail",
    "segment",
    "append_col",
    "append_row",
    "softmax",
    "log_softmax",
    "trace",
    "determinant",
    "log_determinant",
    "log_determinant_spd",
    "inverse",
    "inverse_spd",
    "chol2inv",
    "generalized_inverse",
    "cholesky_decompose",
    "eigenvalues_sym",
    "eigenvectors_sym",
    "eigendecompose_sym",
    "qr_thin_Q",
    "qr_thin_R",
    "singular_values",
    "svd_U",
    "svd_V",
    "mdivide_left_tri_low",
    "mdivide_right_tri_low",
    "mdivide_left_spd",
    "mdivide_right_spd",
    "matrix_exp",
    "matrix_exp_multiply",
    "scale_matrix_exp_multiply",
    "matrix_power",
];

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn stan_distribution_table_has_unique_generated_labels() {
        let mut labels = HashSet::new();

        for distribution in STAN_DISTRIBUTIONS {
            assert!(
                labels.insert(distribution.name.to_string()),
                "duplicate Stan distribution label: {}",
                distribution.name
            );
            for suffix in distribution.suffixes {
                let label = format!("{}{}", distribution.name, suffix);
                assert!(
                    labels.insert(label.clone()),
                    "duplicate Stan distribution label: {label}"
                );
            }
        }
    }

    #[test]
    fn stan_functions_do_not_overlap_distribution_labels() {
        let mut distribution_labels = HashSet::new();
        for distribution in STAN_DISTRIBUTIONS {
            distribution_labels.insert(distribution.name.to_string());
            for suffix in distribution.suffixes {
                distribution_labels.insert(format!("{}{}", distribution.name, suffix));
            }
        }

        for function in STAN_FUNCTIONS {
            assert!(
                !distribution_labels.contains(*function),
                "Stan function overlaps distribution label: {function}"
            );
        }
    }

    #[test]
    fn stan_distribution_suffixes_are_well_formed() {
        const SUPPORTED_SUFFIXES: &[&str] = &["_lpdf", "_lpmf", "_cdf", "_lcdf", "_lccdf", "_rng"];

        for distribution in STAN_DISTRIBUTIONS {
            for suffix in distribution.suffixes {
                assert!(
                    suffix.starts_with('_'),
                    "Stan suffix lacks underscore: {suffix}"
                );
                assert!(
                    SUPPORTED_SUFFIXES.contains(suffix),
                    "unsupported Stan suffix: {suffix}"
                );
            }
        }
    }

    #[test]
    fn stan_functions_are_unique() {
        let mut functions = HashSet::new();
        for function in STAN_FUNCTIONS {
            assert!(
                functions.insert(*function),
                "duplicate Stan function: {function}"
            );
        }
    }
}
