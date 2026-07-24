// This grammar is an independently authored description of black-box behavior
// observed from the public JAGS 4.3.2 command-line parser. It contains no JAGS
// source or manual prose.
//
// Structure and small implementation idioms were informed by the MIT-licensed
// tree-sitter-r revision 8ac99ed1e7ad319737fc11dde20c07d1e1942383
// and tree-sitter-stan revision 86544507c3600d5c4719d98ada477123fee81983.
// See ATTRIBUTION.md and oracle/SYNTAX_FINDINGS.md.

const PREC = {
  OR: 1,
  AND: 2,
  COMPARISON: 3,
  ADD: 4,
  MULTIPLY: 5,
  COLON: 6,
  UNARY: 7,
  POWER: 8,
  POSTFIX: 9,
};

module.exports = grammar({
  name: 'jags',

  extras: $ => [
    /[ \t\r\n\f]/,
    $.comment,
  ],

  word: $ => $.identifier,

  rules: {
    program: $ => seq(
      optional($.variable_declaration),
      optional($.data_block),
      $.model_block,
    ),

    variable_declaration: $ => seq(
      'var',
      commaSep1($.declared_variable),
      optional(';'),
    ),

    declared_variable: $ => seq(
      field('name', $.identifier),
      optional(field('dimensions', $.dimensions)),
    ),

    dimensions: $ => seq(
      '[',
      commaSep1(field('dimension', $._expression)),
      ']',
    ),

    data_block: $ => seq(
      'data',
      field('body', $.block_statement),
    ),

    model_block: $ => seq(
      'model',
      field('body', $.block_statement),
    ),

    block_statement: $ => seq(
      '{',
      repeat1($._statement),
      '}',
    ),

    _statement: $ => choice(
      $.deterministic_relation,
      $.stochastic_relation,
      $.for_statement,
    ),

    deterministic_relation: $ => seq(
      field('lhs', $._deterministic_lhs),
      field('operator', choice('<-', '=')),
      field('rhs', $._expression),
      optional(';'),
    ),

    _deterministic_lhs: $ => choice(
      $.identifier,
      $.subset,
      alias($.call, $.link_call),
    ),

    stochastic_relation: $ => seq(
      field('lhs', choice($.identifier, $.subset)),
      '~',
      field('distribution', $.call),
      optional(field('bounds', $.bounds_clause)),
      optional(';'),
    ),

    bounds_clause: $ => seq(
      field('kind', choice('T', 'I')),
      '(',
      optional(field('lower', $._expression)),
      ',',
      optional(field('upper', $._expression)),
      ')',
    ),

    for_statement: $ => seq(
      'for',
      '(',
      field('variable', $.identifier),
      'in',
      field('sequence', $._expression),
      ')',
      field('body', $.block_statement),
      optional(';'),
    ),

    call: $ => prec.left(PREC.POSTFIX, seq(
      field('function', $.identifier),
      field('arguments', $.call_arguments),
    )),

    call_arguments: $ => seq(
      '(',
      commaSep1(field('argument', $._expression)),
      ')',
    ),

    subset: $ => prec.left(PREC.POSTFIX, seq(
      field('function', choice($.identifier, $.subset)),
      field('arguments', $.subset_arguments),
    )),

    subset_arguments: $ => seq(
      '[',
      optional(seq(
        optional(field('argument', $._expression)),
        repeat(seq(',', optional(field('argument', $._expression)))),
      )),
      ']',
    ),

    parenthesized_expression: $ => seq(
      '(',
      field('body', $._expression),
      ')',
    ),

    unary_operator: $ => prec.right(PREC.UNARY, seq(
      field('operator', choice('+', '-')),
      field('rhs', $._expression),
    )),

    binary_operator: $ => choice(
      prec.left(PREC.OR, binary($, '||')),
      prec.left(PREC.AND, binary($, '&&')),
      prec.left(PREC.COMPARISON, binary($, choice('<', '<=', '>', '>=', '==', '!='))),
      prec.left(PREC.ADD, binary($, choice('+', '-'))),
      prec.left(PREC.MULTIPLY, binary($, choice('*', '/', '%%', '%/%'))),
      prec.left(PREC.COLON, binary($, ':')),
      prec.right(PREC.POWER, binary($, choice('^', '**'))),
    ),

    _expression: $ => choice(
      $.number,
      $.identifier,
      $.call,
      $.subset,
      $.parenthesized_expression,
      $.unary_operator,
      $.binary_operator,
    ),

    number: _ => token(/(?:(?:[0-9]+(?:\.[0-9]*)?)|(?:\.[0-9]+))(?:[eE][+-]?[0-9]+)?/),

    identifier: _ => /[A-Za-z][A-Za-z0-9_.]*/,

    comment: _ => token(choice(
      /#[^\r\n]*/,
      seq('/*', /[^*]*\*+([^/*][^*]*\*+)*/, '/'),
    )),
  },
});

function binary($, operator) {
  return seq(
    field('lhs', $._expression),
    field('operator', operator),
    field('rhs', $._expression),
  );
}

function commaSep1(rule) {
  return seq(rule, repeat(seq(',', rule)));
}
