# Production mapping

JAGS 4.3.2 black-box behavior is the syntax authority. The two pinned MIT
grammars are implementation references only; neither reference's accepted
language is inherited wholesale.

| JAGS production | Black-box evidence | R reference | Stan reference |
|---|---|---|---|
| `program` | empty/comment-only input, or `var` → optional `data` → required `model`; order and multiplicity contrasts | none | ordered top-level block pattern |
| `data_block`, `model_block`, `block_statement` | non-empty block probes | brace/block node idiom | named top-level/body fields |
| `variable_declaration`, `declared_variable`, `dimensions` | optional separator and multidimensional declaration probes | comma-separated rule idiom | declaration field layout |
| `deterministic_relation` | `<-`, `=`, link-left-hand-side controls | expression field naming | statement-level relation structure |
| `stochastic_relation`, `bounds_clause` | `~`, required distribution call, `T`/`I`, omitted-bound controls | call shape | structural sampling/bounds idea only; delimiters and language differ |
| `for_statement` | `for`, `in`, mandatory braced body | compatible variable/sequence/body fields | block-statement organization |
| `call`, `call_arguments` | nested calls; empty distribution call only; named/missing/trailing controls reject | compatible call/argument fields | none |
| `subset`, `subset_arguments` | empty and omitted dimensions, nesting | compatible subset/argument fields | none |
| unary/binary/parenthesized expressions, `special_operator` | operator-by-operator and associativity contrasts, including `%name%` | precedence implementation idiom | comparison with a block language |
| `number`, `identifier` | decimal/exponent and ASCII-name contrasts | token organization | ASCII identifier restriction |
| `comment` | terminated `#` and `/* */` accept; EOF `#` and `//` reject | `#` token pattern | C-style block-comment token pattern |

The grammar has no external scanner. All 23 named productions and all 35
literal tokens are exercised by a checked Rust coverage test whose source is
also accepted by the pinned real-JAGS oracle.

Deliberately unsupported constructs include tested R functions/control flow,
strings, named arguments, extraction/namespace/pipe/right-assignment syntax,
and tested Stan declarations and target accumulation. Unknown callable names
remain syntax-clean because they are semantic, not syntactic.
