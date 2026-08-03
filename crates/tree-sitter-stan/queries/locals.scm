; scopes
[
    (function_definition)
    (block_statement)
    (profile_statement)
    (for_statement)
    (while_statement)
    (if_statement)
] @local.scope

; definitions

(parameter_declaration
parameter: (identifier) @local.definition
)
(for_statement
loopvar: (identifier) @local.definition
)
(var_decl name: (identifier) @local.definition)
; BEGIN_STAN_ONLY
(top_var_decl name: (identifier) @local.definition)
(top_var_decl_no_assign name: (identifier) @local.definition)
; END_STAN_ONLY
; references
(variable_expression (identifier) @local.reference)
