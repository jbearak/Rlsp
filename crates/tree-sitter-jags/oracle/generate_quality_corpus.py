#!/usr/bin/env python3
"""Generate the deterministic, structurally varied JAGS quality corpus.

The sources are independently authored and their expected parse/compile phases
are verified through the public JAGS command-line interface by jags_oracle.py.
This generator contains no JAGS source-derived rules or manual prose.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys


OUTPUT = Path(__file__).with_name("quality-corpus.json")


def build_cases() -> list[dict[str, object]]:
    cases: list[dict[str, object]] = []

    def add(
        group: str,
        family: str,
        template: str,
        sources: list[str],
        *,
        expect_parse: str,
        compile_model: bool = False,
        expect_semantic_error: bool | None = None,
    ) -> None:
        first_index = 1 + sum(
            case["group"] == group and case["family"] == family for case in cases
        )
        for index, source in enumerate(sources, start=1):
            record: dict[str, object] = {
                "id": f"{group}-{family}-{first_index + index - 1:02d}",
                "group": group,
                "family": family,
                "template": template,
                "source": source,
                "expect_parse": expect_parse,
            }
            if compile_model:
                record["compile"] = True
            if expect_semantic_error is not None:
                record["expect_semantic_error"] = expect_semantic_error
            cases.append(record)

    valid = "syntax-valid"
    accepted = "accepted"
    add(valid, "empty-program", "empty or comment-only source", [
        "",
        "# only\n",
    ], expect_parse=accepted)
    add(valid, "minimal-relation", "single deterministic relation", [
        "model { x <- 1 }\n",
        "model { x = 1 }\n",
    ], expect_parse=accepted)
    add(valid, "multiple-relations", "multiple relation separators", [
        "model { x <- 1\ny <- 2\nz <- x + y }\n",
        "model { x <- 1; y <- 2; z <- x + y; }\n",
    ], expect_parse=accepted)
    add(valid, "program-blocks", "ordered program blocks", [
        "data { n <- 3 } model { x <- n }\n",
        "var x[3]; model { x[1] <- 1 }\n",
        "var x[2], y[2,3]; data { n <- 2 } model { x[n] <- y[n,1] }\n",
    ], expect_parse=accepted)
    add(valid, "declarations", "declaration dimensions", [
        "var scalar; model { scalar <- 1 }\n",
        "var vector[3]; model { vector[1] <- 1 }\n",
        "var matrix[2,3], cube[2,3,4]; model { matrix[1,2] <- cube[1,2,3] }\n",
        "var dynamic[n,m+1]; model { dynamic[1,1] <- 1 }\n",
    ], expect_parse=accepted)
    add(valid, "stochastic", "distribution arity", [
        "model { x ~ dfoo() }\n",
        "model { x ~ dfoo(theta) }\n",
        "model { x ~ dfoo(mu, tau) }\n",
        "model { x[i] ~ dfoo(mu[i], tau) }\n",
    ], expect_parse=accepted)
    add(valid, "bounds", "stochastic bounds", [
        "model { x ~ dfoo(0,1) T(0,1) }\n",
        "model { x ~ dfoo(0,1) T(,1) }\n",
        "model { x ~ dfoo(0,1) T(0,) }\n",
        "model { x ~ dfoo(0,1) T(,) }\n",
        "model { x ~ dfoo(0,1) I(lower,upper) }\n",
        "model { x ~ dfoo(0,1) I(,upper) }\n",
        "model { x ~ dfoo(0,1) I(lower,) }\n",
    ], expect_parse=accepted)
    add(valid, "loops", "loop nesting and sequences", [
        "model { for (i in 1:n) { x[i] <- i } }\n",
        "model { for (i in indexes) { x[i] ~ dfoo(mu[i]) } }\n",
        "model { for (i in 1:n) { for (j in 1:m) { x[i,j] <- i+j } } }\n",
        "model { for (for in 1:n) { x[for] <- for } }\n",
    ], expect_parse=accepted)
    add(valid, "link-relations", "link lhs shapes", [
        "model { logit(p) <- eta }\n",
        "model { logit(p[i]) <- alpha + beta*x[i] }\n",
        "model { cloglog(p[,j]) <- eta[j] }\n",
        "model { model(p[]) <- eta }\n",
    ], expect_parse=accepted)
    add(valid, "calls", "nested nonempty calls", [
        "model { x <- f(1) }\n",
        "model { x <- outer(inner(1), other(2,3)) }\n",
        "model { x <- model(data(1), var(2)) }\n",
        "model { x <- f(a[i], (b + c), -d) }\n",
    ], expect_parse=accepted)
    add(valid, "subsets", "omitted subset dimensions", [
        "model { x <- a[] }\n",
        "model { x <- a[,] }\n",
        "model { x <- a[i,] }\n",
        "model { x <- a[,j] }\n",
        "model { x <- a[i,,k] }\n",
        "model { a[] <- x }\n",
    ], expect_parse=accepted)
    add(valid, "arithmetic", "arithmetic precedence", [
        "model { x <- a + b - c }\n",
        "model { x <- a * b / c }\n",
        "model { x <- a %% b + c %/% d }\n",
        "model { x <- -2^2 + 2^-3 }\n",
        "model { x <- 2^3^4 }\n",
        "model { x <- (2^3)^4 }\n",
    ], expect_parse=accepted)
    add(valid, "colon", "nonassociative colon", [
        "model { x <- 1:n }\n",
        "model { x <- (1:2):3 }\n",
        "model { x <- 1:(2:3) }\n",
        "model { x <- (1:2):(3:4) }\n",
    ], expect_parse=accepted)
    add(valid, "special-infix", "special infix operators", [
        "model { x <- a %*% b }\n",
        "model { x <- a %in% b }\n",
        "model { x <- a %custom% b }\n",
        "model { x <- a %left% b %right% c }\n",
    ], expect_parse=accepted)
    add(valid, "comparisons", "nonassociative comparisons", [
        "model { x <- a < b }\n",
        "model { x <- a <= b }\n",
        "model { x <- a > b }\n",
        "model { x <- a >= b }\n",
        "model { x <- a == b }\n",
        "model { x <- a != b }\n",
        "model { x <- (a < b) == c }\n",
        "model { x <- a < (b == c) }\n",
    ], expect_parse=accepted)
    add(valid, "logical", "logical precedence", [
        "model { x <- a && b && c }\n",
        "model { x <- a || b || c }\n",
        "model { x <- a || b && c }\n",
        "model { x <- (a || b) && c }\n",
    ], expect_parse=accepted)
    add(valid, "numbers", "numeric token forms", [
        "model { x <- 0 }\n",
        "model { x <- 12. }\n",
        "model { x <- .25 }\n",
        "model { x <- 1e9 }\n",
        "model { x <- 2.5E-3 }\n",
    ], expect_parse=accepted)
    add(valid, "comments", "comment placements", [
        "# heading\nmodel { x <- 1 }\n",
        "model { /* before */ x <- 1 /* after */ }\n",
        "model { x <- 1 # relation\n y <- 2 }\n",
        "model { x <- 1 }\n# terminal\n",
        "/* λ 💥 */\r\nmodel {\r\n x <- 1\r\n}\r\n",
    ], expect_parse=accepted)
    add(valid, "contextual-names", "contextual keyword names", [
        "model { for <- 1; x <- for + 1 }\n",
        "model { x <- model(1) }\n",
        "model { x <- data(1) }\n",
        "model { x <- var(1) }\n",
    ], expect_parse=accepted)

    # Every unordered pair of these independently meaningful syntax features is
    # exercised in one model body.  The fragments deliberately have different
    # recursive parse shapes; the Rust quality gate verifies both that property
    # and that all 24 choose 2 generated programs remain structurally unique.
    pairwise_features = [
        ("deterministic-arrow", "a <- 1"),
        ("deterministic-equals", "b = 2"),
        ("subset-lhs", "values[i] <- i"),
        ("link-lhs", "logit(probability[i]) <- eta[i]"),
        ("stochastic-empty-call", "sample_a ~ distribution_a()"),
        ("stochastic-two-args", "sample_b ~ distribution_b(mu, tau)"),
        ("truncation-both", "sample_c ~ distribution_c(0, 1) T(lower, upper)"),
        ("interval-omitted", "sample_d ~ distribution_d(0, 1) I(, upper)"),
        ("simple-loop", "for (i in 1:n) { loop_value[i] <- i }"),
        ("nested-loop", "for (j in 1:n) { for (k in 1:m) { matrix[j,k] <- j+k } }"),
        ("unary-power", "power_value <- -base^2"),
        ("multiplicative", "product_value <- left * middle / right"),
        ("modulo", "modulo_value <- left %% right + left %/% right"),
        ("comparison", "comparison_value <- left <= right"),
        ("parenthesized-comparison", "nested_comparison <- (left < right) == flag"),
        ("logical", "logical_value <- left || middle && right"),
        ("colon", "sequence_value <- start:(stop + 1)"),
        ("special-infix", "special_value <- left %pairwise% right"),
        ("nested-call", "call_value <- outer(inner(1), other(2, 3))"),
        ("empty-subset", "empty_subset <- values[]"),
        ("omitted-subset-dimension", "slice_value <- values[row,,column]"),
        ("contextual-call", "contextual_value <- model(data(1), var(2))"),
        ("unary-parenthesized", "negated_value <- -(left + right)"),
        ("commented-relation", "/* pairwise */ commented_value <- 1"),
    ]
    for left_index, (left_name, left_source) in enumerate(pairwise_features):
        for right_name, right_source in pairwise_features[left_index + 1:]:
            add(
                valid,
                "pairwise-features",
                f"pairwise {left_name} + {right_name}",
                [f"model {{\n {left_source}\n {right_source}\n}}\n"],
                expect_parse=accepted,
            )

    semantic = "semantic-invalid"
    semantic_options = {
        "expect_parse": accepted,
        "compile_model": True,
        "expect_semantic_error": True,
    }
    add(semantic, "unknown-function", "unknown function name", [
        "model { x <- definitely_unknown_function(1) }\n",
        "model { x <- module_specific_transform(theta, 2) }\n",
        "model { x <- absent_scalar_transform(alpha + beta) }\n",
        "model { x <- unavailable_vector_transform(values[]) }\n",
        "model { for (i in 1:n) { x[i] <- unknown_index_transform(i, n) } }\n",
    ], **semantic_options)
    add(semantic, "unknown-distribution", "unknown distribution name", [
        "model { x ~ definitely_unknown_distribution(1) }\n",
        "model { x ~ module_specific_distribution(mu, tau) }\n",
        "model { x ~ absent_distribution() }\n",
        "model { x[i] ~ unavailable_distribution(mu[i]) }\n",
        "model { for (i in 1:n) { x[i] ~ unknown_vector_distribution(mu[], tau) } }\n",
    ], **semantic_options)
    add(semantic, "function-arity", "known function wrong arity", [
        "model { x <- log(1, 2) }\n",
        "model { x <- sqrt(1, 2) }\n",
        "model { x <- abs(1, 2) }\n",
        "model { x <- sin(1, 2) }\n",
        "model { x <- pow(1) }\n",
    ], **semantic_options)
    add(semantic, "distribution-arity", "known distribution wrong arity", [
        "model { x ~ dnorm(0) }\n",
        "model { x ~ dunif(0) }\n",
        "model { x ~ dbern() }\n",
        "model { x ~ dpois() }\n",
        "model { x ~ dbin(1) }\n",
    ], **semantic_options)
    add(semantic, "undefined-data", "unresolved data dependency", [
        "model { x <- missing_data + 1 }\n",
        "model { x ~ dnorm(missing_mean, 1) }\n",
        "model { x <- f(missing_vector[]) }\n",
        "model { for (i in 1:n) { x[i] <- absent_matrix[i, j] } }\n",
        "model { x ~ dnorm(0, missing_precision) T(missing_lower, ) }\n",
    ], **semantic_options)
    add(semantic, "direct-cycle", "directed cycle", [
        "model { x <- y; y <- x }\n",
        "model { a <- b; b <- c; c <- a }\n",
        "model { x <- x + 1 }\n",
        "model { x <- y + z; y <- z; z <- x }\n",
        "model { for (i in 1:n) { x[i] <- y[i]; y[i] <- x[i] } }\n",
    ], **semantic_options)
    add(semantic, "duplicate-node", "duplicate node definition", [
        "model { x <- 1; x <- 2 }\n",
        "model { x[1] <- 1; x[1] ~ dnorm(0,1) }\n",
        "model { x ~ dnorm(0,1); x <- 1 }\n",
        "model { for (i in 1:n) { x[i] <- i; x[i] <- i + 1 } }\n",
        "model { logit(p) <- eta; p <- 0.5 }\n",
    ], **semantic_options)
    add(semantic, "dimension-conflict", "declared dimension conflict", [
        "var x[2]; model { x[1:3] <- 1 }\n",
        "var x[2,2]; model { x[1] <- 1 }\n",
        "var x[2]; model { x[1,1] <- 1 }\n",
        "var x[2,2]; model { x[1,1,1] <- 1 }\n",
        "var x[2,2,2]; model { x[1,1] <- 1 }\n",
    ], **semantic_options)
    add(semantic, "invalid-link", "unknown link function", [
        "model { mystery_link(p) <- eta }\n",
        "model { module_link(p[i]) <- eta[i] }\n",
        "model { absent_link(p[]) <- eta }\n",
        "model { for (i in 1:n) { unavailable_link(p[i]) <- eta[i] } }\n",
        "model { unknown_link(probability[row,column]) <- eta[row,column] }\n",
    ], **semantic_options)
    add(semantic, "invalid-index", "invalid fixed index", [
        "model { x[0] <- 1 }\n",
        "model { x[-1] <- 1 }\n",
        "model { x[0,1] <- 1 }\n",
        "model { x[1,0] ~ dnorm(0,1) }\n",
        "model { for (i in 1:n) { x[i,0] <- i } }\n",
    ], **semantic_options)

    invalid = "syntax-invalid"
    rejected = "rejected"
    invalid_families: list[tuple[str, str, list[str]]] = [
        ("empty-model", "empty model body", ["model {}\n", "model { # no relation\n}\n"]),
        ("missing-model", "program without model", ["data { x <- 1 }\n", "var x;\n"]),
        ("block-order", "invalid block order", ["model { x <- 1 } data { y <- 2 }\n", "data { y <- 2 } var x; model { x <- y }\n"]),
        ("duplicate-block", "duplicate top-level block", ["model { x <- 1 } model { y <- 2 }\n", "data { x <- 1 } data { y <- 2 } model { z <- 3 }\n"]),
        ("missing-relation", "missing relation operator", ["model { x 1 }\n", "model { x dnorm(0,1) }\n"]),
        ("missing-operand", "missing expression operand", ["model { x <- * 1 }\n", "model { x <- 1 + }\n"]),
        ("bad-relation-operator", "unsupported relation operator", ["model { x -> 1 }\n", "model { x := 1 }\n"]),
        ("unclosed-block", "unclosed program block", ["model { x <- 1\n", "data { n <- 1 } model { x <- n\n"]),
        ("unclosed-call", "unclosed call", ["model { x <- f(1,2 }\n", "model { x ~ dfoo(0,1 }\n"]),
        ("call-separator", "invalid call separators", ["model { x <- f(1 2) }\n", "model { x <- f(1,) }\n", "model { x <- f(,1) }\n"]),
        ("empty-call", "empty deterministic call", ["model { x <- f() }\n", "model { x <- model() }\n"]),
        ("unclosed-subset", "unclosed subset", ["model { x <- a[i }\n", "model { x[i,j <- 1 }\n"]),
        ("chained-subset", "chained postfix subset", ["model { x <- a[i][j] }\n", "model { x[i][j] <- 1 }\n"]),
        ("postfix-target", "nonidentifier subset target", ["model { x <- f(i)[j] }\n", "model { x <- (a)[i] }\n"]),
        ("loop-header", "malformed loop header", ["model { for (i 1:n) { x[i] <- 1 } }\n", "model { for i in 1:n { x[i] <- 1 } }\n"]),
        ("loop-body", "invalid loop body", ["model { for (i in 1:n) x[i] <- 1 }\n", "model { for (i in 1:n) {} }\n"]),
        ("loop-semicolon", "semicolon after loop body", ["model { for (i in 1:n) { x[i] <- 1 }; y <- 2 }\n"]),
        ("comparison-chain", "unparenthesized comparison chain", ["model { x <- a < b < c }\n", "model { x <- a <= b == c }\n"]),
        ("colon-chain", "unparenthesized colon chain", ["model { x <- 1:2:3 }\n", "model { x <- a:b:c }\n"]),
        ("special-operator", "malformed special operator", ["model { x <- a %foo b }\n", "model { x <- a %foo bar% b }\n"]),
        ("link-arity", "invalid link arity", ["model { logit() <- x }\n", "model { logit(p,q) <- x }\n"]),
        ("link-shape", "invalid link argument shape", ["model { logit(1) <- x }\n", "model { logit(p+q) <- x }\n", "model { logit(f(p)) <- x }\n"]),
        ("bare-distribution", "distribution without call", ["model { x ~ dfoo }\n", "model { x ~ dfoo T(0,1) }\n"]),
        ("bad-bounds", "malformed bounds clause", ["model { x ~ dfoo(1) T(0) }\n", "model { x ~ dfoo(1) T(0,1,2) }\n"]),
        ("reserved-name", "reserved bare name", ["model { model <- 1 }\n", "model { data <- 1 }\n", "model { var <- 1 }\n", "model { in <- 1 }\n"]),
        ("reserved-call", "reserved callable name", ["model { x <- for(1) }\n", "model { x <- in(1) }\n"]),
        ("r-string", "unsupported string expression", ["model { x <- \"text\" }\n", "model { x <- 'text' }\n"]),
        ("r-control", "unsupported R control syntax", ["model { if (x) { y <- 1 } }\n", "model { while (x) { y <- 1 } }\n"]),
        ("r-function", "unsupported R function syntax", ["model { f <- function(x) x }\n", "model { x <- function(y) { y } }\n"]),
        ("r-call-argument", "unsupported R call arguments", ["model { x <- f(value=1) }\n", "model { x <- f(1, ...) }\n"]),
        ("r-operator", "unsupported R operator", ["model { x <- a |> f() }\n", "model { x <- a $ b }\n", "model { x <- pkg::f(1) }\n"]),
        ("stan-syntax", "unsupported Stan syntax", ["model { real x; }\n", "model { target += x; }\n"]),
        ("line-comment-eof", "line comment without terminator", ["model { x <- 1 }\n# tail", "model { x <- 1 }\r\n#"]),
        ("identifier-shape", "invalid identifier token", ["model { _x <- 1 }\n", "model { .x <- 1 }\n", "model { λ <- 1 }\n", "model { 2x <- 1 }\n"]),
        ("bom", "leading UTF-8 BOM", ["\ufeffmodel { x <- 1 }\n"]),
    ]
    for family, template, sources in invalid_families:
        add(invalid, family, template, sources, expect_parse=rejected)

    mutation = "mutation"
    mutation_contexts: dict[str, list[str]] = {
        "deleted-relation-operator": [
            "model { x 1 }\n",
            "data { n 3 } model { x <- n }\n",
            "model { x[i] value[i] }\n",
            "model { for (i in 1:n) { x[i] i } }\n",
            "model { link(p) eta }\n",
            "var x[2]; model { x[1] f(1) }\n",
            "model { x outer(inner(1), other(2)) }\n",
            "model { x dfoo(0, 1) }\n",
            "model { x dfoo(0, 1) T(0, 1) }\n",
            "model { /* gap */ x 1 }\n",
            "model { before <- 1; broken 2; after <- 3 }\n",
            "model {\r\n x 1\r\n}\r\n",
            "data { n 3 } model { x <- 1 }\n",
            "model { logit(p[i]) eta[i] }\n",
            "model { values[] source[] }\n",
            "model { for (i in 1:n) { for (j in 1:m) { x[i,j] i+j } } }\n",
            "model { x 1; y = 2 }\n",
            "model { contextual model(data(1), var(2)) }\n",
            "model { sequence start:(stop + 1) }\n",
            "model { transformed left %custom% right }\n",
        ],
        "missing-operand": [
            "model { x <- + }\n",
            "model { x <- 1 * }\n",
            "model { x <- / y }\n",
            "model { x <- a && }\n",
            "model { x <- < b }\n",
            "model { x <- a %foo% }\n",
            "model { x <- - }\n",
            "model { x <- base ^ }\n",
            "model { x <- start: }\n",
            "model { x <- || right }\n",
            "model { x <- left <= }\n",
            "model { x <- () }\n",
            "model { x <- f(1 +) }\n",
            "model { x <- values[i +] }\n",
            "model { for (i in 1:) { x[i] <- i } }\n",
            "data { n <- 1 + } model { x <- 1 }\n",
            "model { x ~ dfoo(0 +, 1) }\n",
            "model { x ~ dfoo(0, 1) T(lower +, upper) }\n",
            "model { x <- outer(inner(1) +) }\n",
            "model { x <- %custom% right }\n",
        ],
        "unclosed-call": [
            "model { x <- f(1 }\n",
            "model { x <- f(1, g(2) }\n",
            "model { x ~ dfoo(0,1 }\n",
            "model { link(p <- eta }\n",
            "data { n <- f(1 } model { x <- n }\n",
            "model { for (i in f(1) { x[i] <- 1 } }\n",
            "model { x <- outer(1, 2 }\n",
            "model { x <- outer(inner(1), other(2) }\n",
            "model { x <- model(data(1), var(2) }\n",
            "model { x ~ dfoo() T(0, 1 }\n",
            "model { x ~ dfoo(0, 1) I(, upper }\n",
            "model { /* call */ x <- f(1, 2 }\n",
            "model {\r\n x <- f(1\r\n}\r\n",
            "data { n <- f(1, 2 } model { x <- 1 }\n",
            "model { for (i in outer(1, n) { x[i] <- i } }\n",
            "model { logit(p[i] <- eta[i] }\n",
            "model { x <- f(values[i], values[j] }\n",
            "model { x <- f((a + b), -c }\n",
            "model { x <- f(1 /* tail */ }\n",
            "model { before <- 1; x <- f(2; after <- 3 }\n",
        ],
        "missing-call-comma": [
            "model { x <- f(1 2) }\n",
            "model { x <- f(a b, c) }\n",
            "model { x ~ dfoo(0 tau) }\n",
            "model { x <- outer(inner(1) other(2)) }\n",
            "data { n <- f(1 2) } model { x <- n }\n",
            "model { x <- f(a[i] b[j]) }\n",
            "model { x <- f(1 2 3) }\n",
            "model { x <- f((a + b) c) }\n",
            "model { x <- f((-a) (b)) }\n",
            "model { x <- outer(inner(1) other(2), third(3)) }\n",
            "model { x <- model(data(1) var(2)) }\n",
            "model { x ~ dfoo(mu[i] tau[i]) }\n",
            "model { x ~ dfoo(0, 1) T(lower upper) }\n",
            "model { x ~ dfoo(0, 1) I(lower upper) }\n",
            "model { for (i in f(1 n)) { x[i] <- i } }\n",
            "model { logit(p[i,j]) <- f(alpha beta) }\n",
            "model { x <- f(values[] other[]) }\n",
            "model { x <- f(a %foo% b c) }\n",
            "model {\r\n x <- f(1\r\n 2)\r\n}\r\n",
            "model { /* comma */ x <- f(1 2) }\n",
        ],
        "unclosed-subset": [
            "model { x <- a[i }\n",
            "model { x[i <- 1 }\n",
            "model { x <- a[i, }\n",
            "model { x <- a[,j }\n",
            "model { for (i in a[) { x[i] <- 1 } }\n",
            "var x[2; model { x[1] <- 1 }\n",
            "model { x <- values[i,j }\n",
            "model { x <- values[i,,j }\n",
            "model { x <- values[,j }\n",
            "model { x <- values[] + other[i }\n",
            "model { values[i,j <- source[i,j] }\n",
            "model { logit(p[i] <- eta[i] }\n",
            "model { x ~ dfoo(mu[i], tau[i) }\n",
            "model { for (i in values[) { x[i] <- 1 } }\n",
            "model { for (i in 1:n) { x[i <- i } }\n",
            "model { for (i in 1:n) { for (j in 1:m) { x[i,j <- i+j } } }\n",
            "var matrix[2,3; model { matrix[1,2] <- 1 }\n",
            "var cube[2,3,4; model { cube[1,2,3] <- 1 }\n",
            "data { n <- values[1 } model { x <- n }\n",
            "model {\r\n x <- values[i\r\n}\r\n",
        ],
        "missing-loop-in": [
            "model { for (i 1:n) { x[i] <- 1 } }\n",
            "model { for (j indexes) { x[j] <- 1 } }\n",
            "model { for (k f(1)) { x[k] <- 1 } }\n",
            "model { for (i 1:n) { for (j in 1:m) { x[i,j] <- 1 } } }\n",
            "model { for (for 1:n) { x[for] <- 1 } }\n",
            "model { for (i a:b) { x[i] ~ dfoo(1) } }\n",
            "model { for (i 1:n) { x[i] <- f(i) } }\n",
            "model { for (row rows) { x[row] <- row } }\n",
            "model { for (column 1:m) { x[column] ~ dfoo(mu[column]) } }\n",
            "model { for (i values[]) { x[i] <- 1 } }\n",
            "model { for (i (1:n)) { x[i] <- i } }\n",
            "model { for (i start:(stop + 1)) { x[i] <- i } }\n",
            "model { for (i f(a, b)) { x[i] <- i } }\n",
            "model { for (i a %sequence% b) { x[i] <- i } }\n",
            "model { for (i 1:n) { for (j 1:m) { x[i,j] <- 1 } } }\n",
            "model { for (i 1:n) { logit(p[i]) <- eta[i] } }\n",
            "model { for (for 1:n) { x[for] <- for } }\n",
            "model { /* header */ for (i 1:n) { x[i] <- i } }\n",
            "model {\r\n for (i 1:n) { x[i] <- i }\r\n}\r\n",
            "data { n <- 3 } model { for (i 1:n) { x[i] <- i } }\n",
        ],
        "unbraced-loop": [
            "model { for (i in 1:n) x[i] <- 1 }\n",
            "model { for (j in indexes) x[j] ~ dfoo(1) }\n",
            "model { for (i in 1:n) for (j in 1:m) { x[i,j] <- 1 } }\n",
            "model { for (i in f(1)) link(p[i]) <- eta[i] }\n",
            "model { for (i in a:b) x[i] = i }\n",
            "model { for (for in 1:n) x[for] <- for }\n",
            "model { for (i in 1:n) x[i] <- f(i) }\n",
            "model { for (row in rows) x[row] ~ dfoo(mu[row]) }\n",
            "model { for (i in values[]) x[i] = i }\n",
            "model { for (i in start:(stop + 1)) x[i] <- i }\n",
            "model { for (i in 1:n) logit(p[i]) <- eta[i] }\n",
            "model { for (i in 1:n) /* body */ x[i] <- i }\n",
            "model { before <- 1; for (i in 1:n) x[i] <- i }\n",
            "model { for (i in 1:n) x[i] <- i; after <- 2 }\n",
            "model { for (i in 1:n) for (j in 1:m) x[i,j] <- i+j }\n",
            "model { for (for in 1:n) model[for] <- for }\n",
            "model {\r\n for (i in 1:n) x[i] <- i\r\n}\r\n",
            "data { n <- 3 } model { for (i in 1:n) x[i] <- i }\n",
            "model { for (i in f(1, n)) x[i] <- outer(i) }\n",
            "model { for (i in a %sequence% b) x[i] <- i }\n",
        ],
        "r-string": [
            "model { x <- \"text\" }\n",
            "model { x <- 'text' }\n",
            "model { x ~ dfoo(\"x\") }\n",
            "model { x[\"name\"] <- 1 }\n",
            "data { x <- \"text\" } model { y <- 1 }\n",
            "model { for (i in \"abc\") { x[i] <- 1 } }\n",
            "model { x <- \"\" }\n",
            "model { x <- 'single quoted value' }\n",
            "model { x <- f(\"argument\") }\n",
            "model { x <- f(1, 'argument') }\n",
            "model { x ~ dfoo('parameter', 1) }\n",
            "model { logit(p[\"row\"]) <- eta }\n",
            "model { x <- values['row', i] }\n",
            "model { x <- \"a\" + \"b\" }\n",
            "model { x <- (\"group\") }\n",
            "model { before <- 1; x <- \"text\"; after <- 2 }\n",
            "model { /* string */ x <- \"text\" }\n",
            "model {\r\n x <- \"text\"\r\n}\r\n",
            "data { label <- 'name' } model { x <- 1 }\n",
            "model { for (i in values[]) { x[i] <- \"text\" } }\n",
        ],
        "empty-call": [
            "model { x <- f() }\n",
            "model { x <- model() }\n",
            "model { x <- outer(f()) }\n",
            "model { for (i in f()) { x[i] <- 1 } }\n",
            "data { n <- f() } model { x <- n }\n",
            "model { x <- f() + 1 }\n",
            "model { x <- inner() }\n",
            "model { x <- f() * g(1) }\n",
            "model { x <- f() %custom% right }\n",
            "model { x <- (f()) }\n",
            "model { x <- f(g()) }\n",
            "model { x <- outer(1, inner()) }\n",
            "model { x <- values[f()] }\n",
            "model { logit(p[f()]) <- eta }\n",
            "model { for (i in outer(f())) { x[i] <- i } }\n",
            "model { before <- 1; x <- f(); after <- 2 }\n",
            "model { /* empty */ x <- f() }\n",
            "model {\r\n x <- f()\r\n}\r\n",
            "data { n <- outer(f()) } model { x <- n }\n",
            "model { x <- data() + var() }\n",
        ],
        "bare-distribution": [
            "model { x ~ dfoo }\n",
            "model { x[i] ~ dfoo }\n",
            "model { x ~ dfoo T(0,1) }\n",
            "model { for (i in 1:n) { x[i] ~ dfoo } }\n",
            "model { x ~ model }\n",
            "model { x ~ dfoo; y <- 1 }\n",
            "model { x ~ dnorm }\n",
            "model { x[row,column] ~ dfoo }\n",
            "model { x ~ dfoo I(,upper) }\n",
            "model { x ~ dfoo T(lower,) }\n",
            "model { for (row in rows) { x[row] ~ dfoo } }\n",
            "model { for (i in 1:n) { for (j in 1:m) { x[i,j] ~ dfoo } } }\n",
            "model { before <- 1; x ~ dfoo; after <- 2 }\n",
            "model { /* bare */ x ~ dfoo }\n",
            "model {\r\n x ~ dfoo\r\n}\r\n",
            "model { x ~ data }\n",
            "model { x ~ var }\n",
            "model { x ~ dfoo; y ~ dbar(1) }\n",
            "model { x ~ dfoo T(0,1); y <- f(1) }\n",
            "model { x[] ~ dfoo }\n",
        ],
    }
    if any(len(sources) != 20 for sources in mutation_contexts.values()):
        raise AssertionError("each mutation category must have exactly 20 contexts")
    for family, sources in mutation_contexts.items():
        for index, source in enumerate(sources, start=1):
            add(
                mutation,
                family,
                f"{family} structural context {index}",
                [source],
                expect_parse=rejected,
            )

    ids = [str(case["id"]) for case in cases]
    if len(ids) != len(set(ids)):
        raise AssertionError("quality corpus ids are not unique")
    return cases


def rendered() -> str:
    groups: dict[str, dict[str, int]] = {}
    cases = build_cases()
    for case in cases:
        group = str(case["group"])
        stats = groups.setdefault(group, {"total": 0, "authored_templates": 0})
        stats["total"] += 1
    for group, stats in groups.items():
        stats["authored_templates"] = len({
            str(case["template"]) for case in cases if case["group"] == group
        })
    return json.dumps(
        {"schema_version": 1, "counts": groups, "cases": cases},
        indent=2,
        ensure_ascii=False,
        sort_keys=True,
    ) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--output", type=Path, default=OUTPUT)
    args = parser.parse_args()
    expected = rendered()
    if args.check:
        actual = args.output.read_text(encoding="utf-8")
        if actual != expected:
            print(f"generated corpus drift: {args.output}", file=sys.stderr)
            return 1
        return 0
    args.output.write_text(expected, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
