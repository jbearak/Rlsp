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
  SPECIAL: 6,
  COLON: 7,
  UNARY: 8,
  POWER: 9,
  POSTFIX: 10,
};

module.exports = grammar({
  name: 'jags',

  extras: $ => [
    /[ \t\r\n\f]/,
    $.comment,
  ],

  word: $ => $.identifier,

  // JAGS reserves these words as bare names, but the public parser accepts
  // model/data/var in callable positions. `for` is the inverse edge case: it
  // is a valid bare name but not a callable name outside loop syntax.
  reserved: {
    bare: _ => ['model', 'data', 'var', 'in'],
    ordinary: _ => ['model', 'data', 'var', 'for', 'in'],
  },

  rules: {
    program: $ => optional(seq(
      optional($.variable_declaration),
      optional($.data_block),
      $.model_block,
    )),

    variable_declaration: $ => seq(
      'var',
      commaSep1($.declared_variable),
      optional(';'),
    ),

    declared_variable: $ => seq(
      field('name', $._bare_identifier),
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
      $._bare_identifier,
      $.subset,
      $.link_call,
    ),

    stochastic_relation: $ => seq(
      field('lhs', choice($._bare_identifier, $.subset)),
      '~',
      field('distribution', alias($._distribution_call, $.call)),
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
      field('variable', $._bare_identifier),
      'in',
      field('sequence', $._expression),
      ')',
      field('body', $.block_statement),
    ),

    call: $ => prec.left(PREC.POSTFIX, seq(
      field('function', $._callable_identifier),
      field('arguments', $.call_arguments),
    )),

    _distribution_call: $ => prec.left(PREC.POSTFIX, seq(
      field('function', $._callable_identifier),
      field('arguments', alias($._distribution_call_arguments, $.call_arguments)),
    )),

    _distribution_call_arguments: $ => seq(
      '(',
      optional(commaSep1(field('argument', $._expression))),
      ')',
    ),

    call_arguments: $ => seq(
      '(',
      commaSep1(field('argument', $._expression)),
      ')',
    ),

    subset: $ => prec.left(PREC.POSTFIX, seq(
      field('function', $._bare_identifier),
      field('arguments', $.subset_arguments),
    )),

    link_call: $ => prec.left(PREC.POSTFIX, seq(
      field('function', $._callable_identifier),
      field('arguments', alias($._link_call_arguments, $.call_arguments)),
    )),

    _link_call_arguments: $ => seq(
      '(',
      field('argument', choice($._bare_identifier, $.subset)),
      ')',
    ),

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
      field('rhs', $._unary_expression),
    )),

    _primary_expression: $ => choice(
      $.number,
      $._bare_identifier,
      $.call,
      $.subset,
      $.parenthesized_expression,
    ),

    _power_expression: $ => choice(
      $._primary_expression,
      alias($._power_operator, $.binary_operator),
    ),

    _power_operator: $ => prec.right(PREC.POWER, seq(
      field('lhs', $._primary_expression),
      field('operator', choice('^', '**')),
      field('rhs', $._unary_expression),
    )),

    _unary_expression: $ => choice(
      $._power_expression,
      $.unary_operator,
    ),

    _colon_expression: $ => choice(
      $._unary_expression,
      alias($._colon_operator, $.binary_operator),
    ),

    _colon_operator: $ => prec(PREC.COLON, binary($, $._unary_expression, ':')),

    _special_expression: $ => choice(
      $._colon_expression,
      alias($._special_binary_operator, $.binary_operator),
    ),

    _special_binary_operator: $ => prec.left(PREC.SPECIAL, binary(
      $,
      $._special_expression,
      $.special_operator,
      $._colon_expression,
    )),

    _multiplicative_expression: $ => choice(
      $._special_expression,
      alias($._multiplicative_operator, $.binary_operator),
    ),

    _multiplicative_operator: $ => prec.left(PREC.MULTIPLY, binary(
      $,
      $._multiplicative_expression,
      choice('*', '/', '%%', '%/%'),
      $._special_expression,
    )),

    _additive_expression: $ => choice(
      $._multiplicative_expression,
      alias($._additive_operator, $.binary_operator),
    ),

    _additive_operator: $ => prec.left(PREC.ADD, binary(
      $,
      $._additive_expression,
      choice('+', '-'),
      $._multiplicative_expression,
    )),

    _comparison_expression: $ => choice(
      $._additive_expression,
      alias($._comparison_operator, $.binary_operator),
    ),

    _comparison_operator: $ => prec(PREC.COMPARISON, binary(
      $,
      $._additive_expression,
      choice('<', '<=', '>', '>=', '==', '!='),
      $._additive_expression,
    )),

    _and_expression: $ => choice(
      $._comparison_expression,
      alias($._and_operator, $.binary_operator),
    ),

    _and_operator: $ => prec.left(PREC.AND, binary(
      $,
      $._and_expression,
      '&&',
      $._comparison_expression,
    )),

    _or_expression: $ => choice(
      $._and_expression,
      alias($._or_operator, $.binary_operator),
    ),

    _or_operator: $ => prec.left(PREC.OR, binary(
      $,
      $._or_expression,
      '||',
      $._and_expression,
    )),

    _expression: $ => $._or_expression,

    number: _ => token(/(?:(?:[0-9]+(?:\.[0-9]*)?)|(?:\.[0-9]+))(?:[eE][+-]?[0-9]+)?/),

    identifier: _ => /[A-Za-z][A-Za-z0-9_.]*/,

    _bare_identifier: $ => choice(
      reserved('bare', $.identifier),
      alias('for', $.identifier),
    ),

    _callable_identifier: $ => choice(
      reserved('ordinary', $.identifier),
      alias('model', $.identifier),
      alias('data', $.identifier),
      alias('var', $.identifier),
    ),

    special_operator: _ => token(/%[^%\s]+%/),

    comment: _ => token(choice(
      /#[^\r\n]*(?:\r\n|\r|\n)/,
      seq('/*', /[^*]*\*+([^/*][^*]*\*+)*/, '/'),
    )),
  },
});

function binary($, lhs, operator, rhs = lhs) {
  return seq(
    field('lhs', lhs),
    field('operator', operator),
    field('rhs', rhs),
  );
}

function commaSep1(rule) {
  return seq(rule, repeat(seq(',', rule)));
}
