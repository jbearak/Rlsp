//! Cross-rule contract matrix adapted from lintr's upstream default tests.
//!
//! The cases below were audited against r-lib/lintr commit
//! `603ab79e6db25d380c5ee96f35ffd6ba16d223aa` (3.3.0.9000, 2026-07-07)
//! and spot-checked with the 3.3.0.1 release. They intentionally exercise
//! default behavior that Raven advertises, not lintr options Raven does not
//! expose (for example `allow_multiple_spaces = FALSE`) or package-aware
//! namespace analysis. Focused rule tests remain responsible for diagnostic
//! ranges, messages, suppressions, and Raven-specific extensions. See the
//! repository `NOTICE` for lintr's MIT attribution.

use std::collections::BTreeSet;

use tower_lsp::lsp_types::{DiagnosticSeverity, NumberOrString};

use super::{LintConfig, rule_ids, run_lints};
use crate::parser_pool::with_parser;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Rule {
    LineLength,
    TrailingWhitespace,
    NoTab,
    TrailingBlankLines,
    AssignmentOperator,
    ObjectName,
    InfixSpaces,
    CommentedCode,
    Quotes,
    Commas,
    TAndF,
    Semicolon,
    EqualsNa,
    ObjectLength,
    VectorLogic,
    FunctionLeftParentheses,
    SpacesInside,
    Indentation,
}

const ALL_RULES: &[Rule] = &[
    Rule::LineLength,
    Rule::TrailingWhitespace,
    Rule::NoTab,
    Rule::TrailingBlankLines,
    Rule::AssignmentOperator,
    Rule::ObjectName,
    Rule::InfixSpaces,
    Rule::CommentedCode,
    Rule::Quotes,
    Rule::Commas,
    Rule::TAndF,
    Rule::Semicolon,
    Rule::EqualsNa,
    Rule::ObjectLength,
    Rule::VectorLogic,
    Rule::FunctionLeftParentheses,
    Rule::SpacesInside,
    Rule::Indentation,
];

#[derive(Clone, Copy)]
struct Case {
    rule: Rule,
    label: &'static str,
    source: &'static str,
    should_lint: bool,
}

const CASES: &[Case] = &[
    case(Rule::LineLength, "at limit", "1234567890\n", false),
    case(Rule::LineLength, "over limit", "12345678901\n", true),
    case(Rule::TrailingWhitespace, "ordinary", "x <- 1  \n", true),
    case(
        Rule::TrailingWhitespace,
        "empty line",
        "x <- 1\n  \ny <- 2\n",
        true,
    ),
    case(
        Rule::TrailingWhitespace,
        "inside multiline string",
        "x <- 'a  \n'\n",
        false,
    ),
    case(Rule::NoTab, "tab indentation", "\tx <- 1\n", true),
    case(Rule::NoTab, "space indentation", "  x <- 1\n", false),
    case(Rule::NoTab, "tab after comment marker", "#\tnote\n", false),
    case(Rule::NoTab, "tab in string", "x <- \"a\tb\"\n", false),
    case(
        Rule::TrailingBlankLines,
        "single terminal newline",
        "x <- 1\n",
        false,
    ),
    case(
        Rule::TrailingBlankLines,
        "trailing blank",
        "x <- 1\n\n",
        true,
    ),
    case(
        Rule::TrailingBlankLines,
        "missing terminal newline",
        "x <- 1",
        true,
    ),
    case(Rule::AssignmentOperator, "left arrow", "x <- 1\n", false),
    case(
        Rule::AssignmentOperator,
        "equals assignment",
        "x = 1\n",
        true,
    ),
    case(
        Rule::AssignmentOperator,
        "named argument",
        "f(x = 1)\n",
        false,
    ),
    case(Rule::AssignmentOperator, "right arrow", "1 -> x\n", true),
    case(
        Rule::AssignmentOperator,
        "superassignment",
        "x <<- 1\n",
        true,
    ),
    case(Rule::ObjectName, "snake case", "good_name <- 1\n", false),
    case(Rule::ObjectName, "camel case", "badName <- 1\n", true),
    case(
        Rule::ObjectName,
        "backtick camel case",
        "`badName` <- 1\n",
        true,
    ),
    case(
        Rule::ObjectName,
        "compound good base",
        "good_name$BAD_FIELD <- 1\n",
        false,
    ),
    case(
        Rule::ObjectName,
        "compound bad base",
        "badName$field <- 1\n",
        true,
    ),
    case(
        Rule::ObjectName,
        "S3 method",
        "print.my_class <- function(x) x\n",
        false,
    ),
    case(
        Rule::ObjectName,
        "namespace hook",
        ".onLoad <- function(lib, pkg) NULL\n",
        false,
    ),
    case(
        Rule::ObjectName,
        "assign literal",
        "assign('badName', 1)\n",
        true,
    ),
    case(Rule::InfixSpaces, "spaced addition", "x <- 1 + 2\n", false),
    case(Rule::InfixSpaces, "tight addition", "x <- 1+2\n", true),
    case(Rule::InfixSpaces, "tight exponent", "x <- x^2\n", false),
    case(Rule::InfixSpaces, "spaced exponent", "x <- x ^ 2\n", false),
    case(Rule::InfixSpaces, "tight named equals", "f(x=1)\n", true),
    case(
        Rule::InfixSpaces,
        "spaced named equals",
        "f(x = 1)\n",
        false,
    ),
    case(Rule::InfixSpaces, "scientific sign", "x <- 2e-4\n", false),
    case(Rule::CommentedCode, "prose", "# explanatory prose\n", false),
    case(Rule::CommentedCode, "roxygen", "#' f(x)\n", false),
    case(
        Rule::CommentedCode,
        "standalone assignment",
        "# x <- 1\n",
        true,
    ),
    case(
        Rule::CommentedCode,
        "inline call",
        "x <- 1 # other_call()\n",
        true,
    ),
    case(
        Rule::CommentedCode,
        "inline prose",
        "x <- 1 # for example only\n",
        false,
    ),
    case(Rule::Quotes, "double quoted", "x <- \"plain\"\n", false),
    case(Rule::Quotes, "single quoted", "x <- 'plain'\n", true),
    case(Rule::Quotes, "avoid escaping", "x <- '\"quoted\"'\n", false),
    case(
        Rule::Quotes,
        "raw double quoted",
        "x <- R\"(plain)\"\n",
        false,
    ),
    case(Rule::Quotes, "raw single quoted", "x <- R'(plain)'\n", true),
    case(Rule::Commas, "ordinary comma", "f(1, 2)\n", false),
    case(Rule::Commas, "tight comma", "f(1,2)\n", true),
    case(Rule::Commas, "space before", "f(1 , 2)\n", true),
    case(Rule::Commas, "newline after", "f(1,\n2)\n", false),
    case(
        Rule::Commas,
        "comma starts continuation line",
        "f(1\n  ,\n2)\n",
        false,
    ),
    case(Rule::Commas, "trailing subset comma", "x[1,]\n", true),
    case(
        Rule::TAndF,
        "reserved literals",
        "x <- c(TRUE, FALSE)\n",
        false,
    ),
    case(Rule::TAndF, "bare alias", "x <- T\n", true),
    case(Rule::TAndF, "alias assignment", "T <- 1\n", true),
    case(Rule::TAndF, "formula terms", "y ~ T + F\n", false),
    case(
        Rule::TAndF,
        "nested named-argument formula term",
        "y ~ foo(arg = T + 1)\n",
        false,
    ),
    case(
        Rule::TAndF,
        "direct named-argument formula value",
        "y ~ foo(arg = T)\n",
        true,
    ),
    case(Rule::TAndF, "subset object", "T[1]\n", false),
    case(Rule::TAndF, "named argument value", "f(na.rm = T)\n", true),
    case(Rule::Semicolon, "no separator", "x <- 1\n", false),
    case(
        Rule::Semicolon,
        "compound separator",
        "x <- 1; y <- 2\n",
        true,
    ),
    case(Rule::Semicolon, "trailing separator", "x <- 1;\n", true),
    case(Rule::Semicolon, "inside string", "x <- \"a;b\"\n", false),
    case(Rule::EqualsNa, "is.na", "is.na(x)\n", false),
    case(Rule::EqualsNa, "equals NA", "x == NA\n", true),
    case(Rule::EqualsNa, "typed NA", "x != NA_real_\n", true),
    case(Rule::EqualsNa, "membership RHS", "x %in% NA\n", true),
    case(Rule::EqualsNa, "membership LHS", "NA %in% x\n", false),
    case(Rule::ObjectLength, "at limit", "abcde <- 1\n", false),
    case(Rule::ObjectLength, "over limit", "abcdef <- 1\n", true),
    case(
        Rule::ObjectLength,
        "quoted over limit",
        "`abcdef` <- 1\n",
        true,
    ),
    case(
        Rule::ObjectLength,
        "compound base",
        "abcdef$field <- 1\n",
        true,
    ),
    case(
        Rule::ObjectLength,
        "Unicode characters",
        "résumé <- 1\n",
        true,
    ),
    case(
        Rule::ObjectLength,
        "assign literal",
        "assign('abcdef', 1)\n",
        true,
    ),
    case(
        Rule::VectorLogic,
        "scalar conditional",
        "if (x && y) 1\n",
        false,
    ),
    case(
        Rule::VectorLogic,
        "vector conditional",
        "if (x & y) 1\n",
        true,
    ),
    case(
        Rule::VectorLogic,
        "aggregator boundary",
        "if (any(x & y)) 1\n",
        false,
    ),
    case(
        Rule::VectorLogic,
        "testthat assertion",
        "expect_true(x | y)\n",
        true,
    ),
    case(
        Rule::VectorLogic,
        "filter scalar op",
        "filter(data, x && y)\n",
        true,
    ),
    case(
        Rule::VectorLogic,
        "filter vector op",
        "filter(data, x & y)\n",
        false,
    ),
    case(
        Rule::VectorLogic,
        "magrittr-piped filter predicate",
        "data %>% filter(x && y)\n",
        true,
    ),
    case(
        Rule::VectorLogic,
        "native-piped filter predicate",
        "data |> filter(x && y)\n",
        true,
    ),
    case(
        Rule::FunctionLeftParentheses,
        "tight definition",
        "f <- function(x) x\n",
        false,
    ),
    case(
        Rule::FunctionLeftParentheses,
        "spaced definition",
        "f <- function (x) x\n",
        true,
    ),
    case(
        Rule::FunctionLeftParentheses,
        "tight call",
        "mean(x)\n",
        false,
    ),
    case(
        Rule::FunctionLeftParentheses,
        "spaced call",
        "mean (x)\n",
        true,
    ),
    case(
        Rule::FunctionLeftParentheses,
        "split call",
        "if (x > mean\n(y)) x\n",
        true,
    ),
    case(Rule::SpacesInside, "tight call", "f(x)\n", false),
    case(Rule::SpacesInside, "padded call", "f( x )\n", true),
    case(Rule::SpacesInside, "empty padded call", "f( )\n", true),
    case(
        Rule::SpacesInside,
        "multiline call",
        "f(\n    x\n)\n",
        false,
    ),
    case(Rule::SpacesInside, "padded subset", "x[ 1 ]\n", true),
    case(
        Rule::SpacesInside,
        "padded formal parameters",
        "function( x ) x\n",
        true,
    ),
    case(Rule::SpacesInside, "padded condition", "if ( x ) x\n", true),
    case(
        Rule::Indentation,
        "braced block",
        "if (x) {\n    y <- 1\n}\n",
        false,
    ),
    case(
        Rule::Indentation,
        "underindented block",
        "if (x) {\ny <- 1\n}\n",
        true,
    ),
    case(
        Rule::Indentation,
        "aligned parenthesized Boolean clauses",
        "changed <- !(\n    (is.na(old) & is.na(new)) |\n    (!is.na(old) & !is.na(new))\n)\n",
        false,
    ),
];

const fn case(rule: Rule, label: &'static str, source: &'static str, should_lint: bool) -> Case {
    Case {
        rule,
        label,
        source,
        should_lint,
    }
}

#[test]
fn lintr_default_contract_matrix_covers_every_supported_rule() {
    let covered: BTreeSet<_> = CASES.iter().map(|case| case.rule).collect();
    let expected: BTreeSet<_> = ALL_RULES.iter().copied().collect();
    assert_eq!(covered, expected, "the parity matrix must cover every rule");

    for case in CASES {
        let config = config_for(case.rule);
        let tree = with_parser(|parser| parser.parse(case.source, None)).expect("source parses");
        let diagnostics = run_lints(case.source, tree.root_node(), &config);
        let rule_id = rule_id(case.rule);
        let matching: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.code.as_ref() == Some(&NumberOrString::String(rule_id.to_string()))
            })
            .collect();
        assert_eq!(
            !matching.is_empty(),
            case.should_lint,
            "{:?} / {}: source {:?}, diagnostics {:?}",
            case.rule,
            case.label,
            case.source,
            diagnostics
        );
    }
}

fn config_for(rule: Rule) -> LintConfig {
    let mut config = LintConfig {
        enabled: true,
        line_length_severity: None,
        trailing_whitespace_severity: None,
        no_tab_severity: None,
        trailing_blank_lines_severity: None,
        assignment_operator_severity: None,
        object_name_severity: None,
        infix_spaces_severity: None,
        commented_code_severity: None,
        quotes_severity: None,
        commas_severity: None,
        t_and_f_symbol_severity: None,
        semicolon_severity: None,
        equals_na_severity: None,
        object_length_severity: None,
        vector_logic_severity: None,
        function_left_parentheses_severity: None,
        spaces_inside_severity: None,
        indentation_severity: None,
        ..LintConfig::default()
    };
    let severity = Some(DiagnosticSeverity::HINT);
    match rule {
        Rule::LineLength => {
            config.line_length = 10;
            config.line_length_severity = severity;
        }
        Rule::TrailingWhitespace => config.trailing_whitespace_severity = severity,
        Rule::NoTab => config.no_tab_severity = severity,
        Rule::TrailingBlankLines => config.trailing_blank_lines_severity = severity,
        Rule::AssignmentOperator => config.assignment_operator_severity = severity,
        Rule::ObjectName => config.object_name_severity = severity,
        Rule::InfixSpaces => config.infix_spaces_severity = severity,
        Rule::CommentedCode => config.commented_code_severity = severity,
        Rule::Quotes => config.quotes_severity = severity,
        Rule::Commas => config.commas_severity = severity,
        Rule::TAndF => config.t_and_f_symbol_severity = severity,
        Rule::Semicolon => config.semicolon_severity = severity,
        Rule::EqualsNa => config.equals_na_severity = severity,
        Rule::ObjectLength => {
            config.object_length = 5;
            config.object_length_severity = severity;
        }
        Rule::VectorLogic => config.vector_logic_severity = severity,
        Rule::FunctionLeftParentheses => config.function_left_parentheses_severity = severity,
        Rule::SpacesInside => config.spaces_inside_severity = severity,
        Rule::Indentation => {
            config.indentation_unit = 4;
            config.indentation_severity = severity;
        }
    }
    config
}

const fn rule_id(rule: Rule) -> &'static str {
    match rule {
        Rule::LineLength => rule_ids::LINE_LENGTH,
        Rule::TrailingWhitespace => rule_ids::TRAILING_WHITESPACE,
        Rule::NoTab => rule_ids::NO_TAB,
        Rule::TrailingBlankLines => rule_ids::TRAILING_BLANK_LINES,
        Rule::AssignmentOperator => rule_ids::ASSIGNMENT_OPERATOR,
        Rule::ObjectName => rule_ids::OBJECT_NAME,
        Rule::InfixSpaces => rule_ids::INFIX_SPACES,
        Rule::CommentedCode => rule_ids::COMMENTED_CODE,
        Rule::Quotes => rule_ids::QUOTES,
        Rule::Commas => rule_ids::COMMAS,
        Rule::TAndF => rule_ids::T_AND_F_SYMBOL,
        Rule::Semicolon => rule_ids::SEMICOLON,
        Rule::EqualsNa => rule_ids::EQUALS_NA,
        Rule::ObjectLength => rule_ids::OBJECT_LENGTH,
        Rule::VectorLogic => rule_ids::VECTOR_LOGIC,
        Rule::FunctionLeftParentheses => rule_ids::FUNCTION_LEFT_PARENTHESES,
        Rule::SpacesInside => rule_ids::SPACES_INSIDE,
        Rule::Indentation => rule_ids::INDENTATION,
    }
}
