# Requirements Document: Q Bot PR Review Fixes

## Introduction

This specification addresses critical issues identified by Q Bot in PR #2 for the Rlsp project. The issues include integer overflow risks, performance regressions from parser recreation and O(n²) complexity, potential deadlock risks, and sequential file I/O inefficiencies. These fixes are essential for maintaining code correctness, performance, and reliability of the cross-file awareness and background indexing features.

## Glossary

- **Parser**: tree-sitter Parser instance used for parsing R source code
- **Background_Indexer**: Component that asynchronously indexes files not currently open in the editor
- **Priority_Score**: Numeric value used to order file revalidation based on user activity
- **Affected_Files**: Collection of files that need diagnostics revalidation after a change
- **Thread_Local**: Storage mechanism that provides separate instances per thread
- **Saturating_Add**: Arithmetic operation that caps at maximum value instead of overflowing
- **HashSet**: Data structure providing O(1) membership testing
- **Async_Read_Lock**: Tokio RwLock read guard that allows concurrent reads

## Requirements

### Requirement 1: Integer Overflow Prevention

**User Story:** As a developer, I want arithmetic operations to handle edge cases safely, so that the system remains stable under extreme conditions.

#### Acceptance Criteria

1. WHEN computing priority scores in activity tracking, THE System SHALL use saturating addition to prevent integer overflow
2. WHEN incrementing depth counters in background indexing, THE System SHALL use saturating addition to prevent integer overflow
3. WHEN any arithmetic operation could overflow, THE System SHALL use saturating arithmetic instead of unchecked operations
4. THE System SHALL maintain correct behavior even when counters approach maximum values

### Requirement 2: Parser Instance Reuse

**User Story:** As a system operator, I want parser instances to be reused efficiently, so that metadata extraction and file indexing have minimal allocation overhead.

#### Acceptance Criteria

1. WHEN extracting metadata from R source code, THE System SHALL reuse a thread-local Parser instance
2. WHEN indexing files in the background, THE System SHALL reuse a thread-local Parser instance
3. THE System SHALL avoid creating new Parser instances on every function call
4. WHEN multiple operations occur on the same thread, THE System SHALL share the same Parser instance

### Requirement 3: Efficient Affected Files Collection

**User Story:** As a developer, I want file change processing to scale efficiently, so that the system performs well with large dependency graphs.

#### Acceptance Criteria

1. WHEN collecting affected files for revalidation, THE System SHALL use a HashSet for O(1) membership testing
2. WHEN checking if a file is already in the affected collection, THE System SHALL complete the check in constant time
3. WHEN adding files to the affected collection, THE System SHALL use HashSet insert which returns a boolean indicating if the item was newly inserted
4. THE System SHALL avoid O(n²) complexity from repeated linear searches in the affected files collection

### Requirement 4: Deadlock Risk Analysis

**User Story:** As a system architect, I want to understand potential deadlock scenarios, so that I can ensure the system remains responsive under concurrent operations.

#### Acceptance Criteria

1. WHEN the did_open handler holds an async read lock, THE System SHALL document any nested lock acquisition patterns
2. WHEN background indexer needs state access, THE System SHALL avoid holding locks during blocking operations
3. THE System SHALL identify and document any potential deadlock scenarios in the codebase
4. WHEN concurrent operations access shared state, THE System SHALL follow a consistent lock ordering strategy

### Requirement 5: Concurrent File I/O Operations

**User Story:** As a system operator, I want file indexing operations to execute concurrently when possible, so that background indexing completes faster.

#### Acceptance Criteria

1. WHEN indexing multiple files on-demand, THE System SHALL evaluate if concurrent execution is beneficial
2. WHEN file I/O operations are independent, THE System SHALL consider parallel execution strategies
3. THE System SHALL document the rationale for sequential vs concurrent file operations
4. WHEN performance bottlenecks exist in file I/O, THE System SHALL provide optimization opportunities
