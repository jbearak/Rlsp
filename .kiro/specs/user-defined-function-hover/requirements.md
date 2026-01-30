# Requirements Document

## Introduction

This feature adds hover information support for user-defined functions in the rlsp R Language Server. Currently, the LSP only provides hover information for R built-in functions by calling R's help system as a subprocess. User-defined functions (e.g., `add <- function(a, b) { a + b }`) return no hover information, causing a failing VS Code extension test.

The feature will display function signatures (name and parameters) when hovering over user-defined functions, while maintaining the existing behavior of showing R help documentation for built-in functions.

## Glossary

- **Hover_Handler**: The LSP handler function that responds to textDocument/hover requests
- **User_Defined_Function**: A function defined in the user's R code using the `function` keyword and assignment
- **Built_In_Function**: A function that is part of R's base packages or installed packages
- **Function_Signature**: A string representation of a function showing its name and parameters
- **Symbol_Index**: The collection of symbols (functions, variables) extracted from parsed R documents
- **WorldState**: The global LSP state containing open documents, workspace index, and caches

## Requirements

### Requirement 1: Display Hover Information for User-Defined Functions

**User Story:** As a developer, I want to see hover information for user-defined functions, so that I can quickly understand what parameters a function expects without navigating to its definition.

#### Acceptance Criteria

1. WHEN a user hovers over an identifier that is a user-defined function, THE Hover_Handler SHALL return a hover response containing the function signature
2. WHEN displaying a user-defined function signature, THE Hover_Handler SHALL include the function name and all parameter names in the format `function_name(param1, param2, ...)`
3. WHEN a user-defined function has no parameters, THE Hover_Handler SHALL display the signature as `function_name()`
4. WHEN a user-defined function has default parameter values, THE Hover_Handler SHALL include the default values in the signature format `function_name(param1 = default1, param2)`

### Requirement 2: Search Multiple Sources for Function Definitions

**User Story:** As a developer, I want hover information to work for functions defined anywhere in my workspace, so that I can get information regardless of where the function is defined.

#### Acceptance Criteria

1. WHEN searching for a user-defined function, THE Hover_Handler SHALL first search the current document
2. WHEN the function is not found in the current document, THE Hover_Handler SHALL search all open documents
3. WHEN the function is not found in open documents, THE Hover_Handler SHALL search the workspace index
4. THE Hover_Handler SHALL return the first matching function definition found

### Requirement 3: Maintain Built-in Function Hover Behavior

**User Story:** As a developer, I want hover information for built-in R functions to continue working, so that I can still access R documentation.

#### Acceptance Criteria

1. WHEN a user hovers over a built-in function identifier, THE Hover_Handler SHALL return R help documentation (existing behavior)
2. WHEN a user-defined function shadows a built-in function, THE Hover_Handler SHALL prioritize the user-defined function
3. IF the R help subprocess fails or returns no documentation, THEN THE Hover_Handler SHALL return no hover information for built-in functions

### Requirement 4: Extract Function Signature from AST

**User Story:** As a developer, I want accurate function signatures extracted from the code, so that the hover information reflects the actual function definition.

#### Acceptance Criteria

1. WHEN extracting a function signature, THE System SHALL parse the function definition node from the tree-sitter AST
2. WHEN a function parameter has a default value, THE System SHALL extract both the parameter name and its default value expression
3. WHEN a function uses the `...` (dots) parameter, THE System SHALL include `...` in the signature
4. THE System SHALL handle all R assignment operators (`<-`, `=`, `<<-`) when identifying function definitions
