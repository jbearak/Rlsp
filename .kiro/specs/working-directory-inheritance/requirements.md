# Requirements Document

## Introduction

This feature enables working directory inheritance for files that declare themselves as being sourced by a parent file via backward directives (`@lsp-sourced-by`, `@lsp-run-by`, `@lsp-included-by`). Currently, working directory inheritance only occurs during forward traversal when processing `source()` calls. This feature extends that behavior to backward directive scenarios, allowing child files to inherit the parent's working directory context for path resolution of their own `source()` calls.

## Glossary

- **Backward_Directive**: A directive in a child file declaring it is sourced by a parent file (e.g., `@lsp-sourced-by: ../parent.R`)
- **Working_Directory**: The directory used as the base for resolving relative paths in `source()` calls
- **Explicit_Working_Directory**: A working directory set via `@lsp-cd` directive in the file itself
- **Inherited_Working_Directory**: A working directory inherited from a parent file
- **Effective_Working_Directory**: The resolved working directory used for path resolution (priority: explicit > inherited > file's directory)
- **Parent_File**: A file that sources another file (the caller)
- **Child_File**: A file that is sourced by another file (the callee)
- **PathContext**: The context object containing file path, working directories, and workspace root for path resolution
- **Forward_Traversal**: Processing `source()` calls from parent to child during scope resolution
- **Backward_Traversal**: Processing backward directives to establish parent-child relationships

## Requirements

### Requirement 1: Inherit Explicit Working Directory from Parent

**User Story:** As a developer, I want a child file with a backward directive to inherit the parent's explicit `@lsp-cd` working directory, so that my `source()` calls resolve paths correctly relative to the parent's context.

#### Acceptance Criteria

1. WHEN a Child_File has a Backward_Directive pointing to a Parent_File AND the Parent_File has an Explicit_Working_Directory via `@lsp-cd` AND the Child_File has no Explicit_Working_Directory, THEN the Path_Resolver SHALL use the Parent_File's Explicit_Working_Directory as the Child_File's Inherited_Working_Directory
2. WHEN resolving paths for `source()` calls in the Child_File, THE Path_Resolver SHALL use the Inherited_Working_Directory from the Parent_File
3. WHEN the Parent_File's `@lsp-cd` specifies a workspace-relative path (starting with `/`), THEN the Path_Resolver SHALL resolve it relative to the workspace root before inheritance

### Requirement 2: Inherit Implicit Working Directory from Parent

**User Story:** As a developer, I want a child file with a backward directive to inherit the parent's implicit working directory (parent's file directory), so that path resolution behaves consistently whether or not the parent has an explicit `@lsp-cd`.

#### Acceptance Criteria

1. WHEN a Child_File has a Backward_Directive pointing to a Parent_File AND the Parent_File has no Explicit_Working_Directory AND the Child_File has no Explicit_Working_Directory, THEN the Path_Resolver SHALL use the Parent_File's directory as the Child_File's Inherited_Working_Directory
2. WHEN the Parent_File is in a different directory than the Child_File, THE Path_Resolver SHALL correctly resolve the Parent_File's directory path for inheritance

### Requirement 3: Child's Explicit Working Directory Takes Precedence

**User Story:** As a developer, I want my child file's explicit `@lsp-cd` directive to override any inherited working directory, so that I can customize path resolution when needed.

#### Acceptance Criteria

1. WHEN a Child_File has both a Backward_Directive AND its own Explicit_Working_Directory via `@lsp-cd`, THEN the Path_Resolver SHALL use the Child_File's Explicit_Working_Directory for path resolution
2. THE Path_Resolver SHALL NOT use the Parent_File's working directory when the Child_File has an Explicit_Working_Directory
3. WHEN the Child_File's `@lsp-cd` is removed, THEN the Path_Resolver SHALL fall back to the Inherited_Working_Directory from the Parent_File

### Requirement 4: Backward Directive Path Resolution Unchanged

**User Story:** As a developer, I want backward directive paths to continue resolving relative to the child file's directory, so that existing behavior is preserved.

#### Acceptance Criteria

1. WHEN resolving the path in a Backward_Directive (e.g., `@lsp-sourced-by: ../parent.R`), THE Path_Resolver SHALL resolve it relative to the Child_File's directory
2. THE Path_Resolver SHALL NOT use the Inherited_Working_Directory when resolving Backward_Directive paths
3. THE Path_Resolver SHALL NOT use the Child_File's Explicit_Working_Directory when resolving Backward_Directive paths

### Requirement 5: Parent Metadata Retrieval

**User Story:** As a developer, I want the system to retrieve parent file metadata to determine its working directory, so that inheritance can work correctly.

#### Acceptance Criteria

1. WHEN processing a Backward_Directive, THE Dependency_Graph SHALL retrieve the Parent_File's metadata to determine its Effective_Working_Directory
2. WHEN the Parent_File is not open in the editor, THE System SHALL use the workspace index or file cache to retrieve metadata
3. IF the Parent_File's metadata cannot be retrieved, THEN the System SHALL fall back to using the Parent_File's directory as the Inherited_Working_Directory

### Requirement 6: Working Directory Storage in Metadata

**User Story:** As a developer, I want the inherited working directory to be stored in the child file's metadata, so that it can be used during path resolution.

#### Acceptance Criteria

1. WHEN a Child_File's metadata is computed AND it has a Backward_Directive, THEN the CrossFileMetadata SHALL include an `inherited_working_directory` field
2. THE PathContext SHALL be constructable from CrossFileMetadata including the inherited working directory
3. WHEN building PathContext from metadata, THE System SHALL populate `inherited_working_directory` from the parent's Effective_Working_Directory

### Requirement 7: Multiple Parent Handling

**User Story:** As a developer, I want consistent behavior when a child file has multiple backward directives pointing to different parents, so that path resolution is predictable.

#### Acceptance Criteria

1. WHEN a Child_File has multiple Backward_Directives pointing to different Parent_Files, THEN the Path_Resolver SHALL use the first parent's Effective_Working_Directory for inheritance
2. THE System SHALL process Backward_Directives in document order (top to bottom)
3. WHEN multiple parents have different working directories, THE System SHALL log a trace message indicating which parent's working directory was used

### Requirement 8: Cache Invalidation

**User Story:** As a developer, I want the system to invalidate cached data when a parent's working directory changes, so that child files use the updated working directory.

#### Acceptance Criteria

1. WHEN a Parent_File's `@lsp-cd` directive is added, changed, or removed, THEN the Cache SHALL invalidate entries for all Child_Files that have Backward_Directives pointing to that Parent_File
2. THE Revalidation_System SHALL trigger re-computation of affected Child_File metadata
3. WHEN the Parent_File's working directory changes, THE System SHALL update the Inherited_Working_Directory in all affected Child_Files

### Requirement 9: Transitive Inheritance

**User Story:** As a developer, I want working directory inheritance to work transitively through chains of backward directives, so that deeply nested file structures work correctly.

#### Acceptance Criteria

1. WHEN File_A sources File_B (via backward directive) AND File_B sources File_C (via backward directive) AND only File_A has an Explicit_Working_Directory, THEN File_C SHALL inherit File_A's working directory through File_B
2. THE System SHALL respect the max_chain_depth configuration to prevent infinite inheritance chains
3. WHEN a cycle is detected in the backward directive chain, THE System SHALL stop inheritance and use the file's own directory
