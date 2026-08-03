(function_declarator
    name: (identifier) @name
) @definition.function


(function_expression
    name: (identifier) @name
) @reference.call

(distr_expression
    name: (identifier) @name
) @reference.call

(print_statement
"print" @name) @reference.call

(reject_statement
"reject" @name) @reference.call

(fatal_error_statement
"fatal_error" @name) @reference.call
