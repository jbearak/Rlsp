# Declaration Directives

Declaration directives allow you to declare symbols (variables and functions) that are created dynamically and cannot be statically detected by the parser. This suppresses false-positive "undefined variable" diagnostics for symbols from `eval()`, `assign()`, `load()`, or external data loading.

These directives work in any R file, whether or not it participates in cross-file `source()` chains.

## Variable Declarations

```r
# @lsp-var myvar
# @lsp-variable myvar           # synonym
# @lsp-declare-var myvar        # synonym
# @lsp-declare-variable myvar   # synonym
```

## Function Declarations

```r
# @lsp-func myfunc
# @lsp-function myfunc          # synonym
# @lsp-declare-func myfunc      # synonym
# @lsp-declare-function myfunc  # synonym
```

## Syntax Variations

All syntax variations are supported:
- With or without colon: `@lsp-var: myvar` or `@lsp-var myvar`
- With quotes for special characters: `@lsp-var "my.var"` or `@lsp-var 'my.var'`

**Important:** Unlike source directives, declaration directives require the `@` prefix. `# lsp-var myvar` (without `@`) is NOT recognized.

## Position-Aware Behavior

Declared symbols are available starting from the beginning of the next line (line N+1), matching `source()` semantics:

```r
# @lsp-var data_from_api
x <- data_from_api  # OK: data_from_api is in scope (next line after directive)
```

The symbol is NOT available on the same line as the directive:

```r
x <- data_from_api  # ERROR: used before declaration
# @lsp-var data_from_api
```

## Cross-File Inheritance

Declarations propagate to sourced child files when declared before the `source()` call:

```r
# parent.R
# @lsp-var shared_data
source("child.R")  # child.R can use shared_data
```

## Use Cases

```r
# Dynamic assignment via assign()
assign(paste0("var_", i), value)
# @lsp-var var_1
# @lsp-var var_2

# Loading data from external sources
load("data.RData")  # Creates objects dynamically
# @lsp-var model_fit
# @lsp-var training_data

# eval() with constructed expressions
eval(parse(text = code_string))
# @lsp-func dynamic_function
```

## LSP Features

- **Completions**: Declared symbols appear in completion lists with appropriate kind (variable/function)
- **Hover**: Shows "Declared via @lsp-var directive at line N"
- **Go-to-definition**: Navigates to the directive line
- **Diagnostics**: Suppresses "undefined variable" warnings for declared symbols
