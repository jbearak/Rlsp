# Requirements Document

## Introduction

This feature introduces an interval tree data structure to optimize function scope lookups in the rlsp cross-file awareness module. Currently, determining which function scope contains a given position requires O(n) linear scans over all function scopes. With an interval tree, these point queries become O(log n), significantly improving performance for files with many nested functions.

## Glossary

- **Interval_Tree**: A balanced binary search tree data structure optimized for storing intervals and efficiently answering point queries (which intervals contain a given point) and range queries (which intervals overlap a given range)
- **Function_Scope**: A region in an R file defined by a function body's start and end positions, within which function parameters and local variables are in scope
- **Point_Query**: A query to find all intervals (function scopes) that contain a specific (line, column) position
- **Scope_Artifacts**: Per-file computed data including exported symbols, timeline of scope events, and function scope boundaries
- **UTF16_Position**: A position in a document using 0-based line numbers and UTF-16 code unit columns, as required by the LSP specification

## Requirements

### Requirement 1: Interval Tree Data Structure

**User Story:** As a developer, I want an interval tree data structure for storing function scopes, so that point queries can be answered in O(log n) time instead of O(n).

#### Acceptance Criteria

1. THE Interval_Tree SHALL store intervals defined by (start_line, start_column, end_line, end_column) tuples
2. THE Interval_Tree SHALL support construction from a list of intervals via from_scopes in O(n log n) time
3. THE Interval_Tree SHALL support point queries that return all intervals containing a given (line, column) position
4. WHEN multiple intervals contain the query point, THE Interval_Tree SHALL return all containing intervals
5. THE Interval_Tree SHALL handle intervals with identical start positions correctly
6. THE Interval_Tree SHALL handle empty trees gracefully, returning an empty result for queries

### Requirement 2: Innermost Scope Selection

**User Story:** As a developer, I want to efficiently find the innermost function scope containing a position, so that local variable scoping rules are correctly applied.

#### Acceptance Criteria

1. WHEN querying for the innermost scope, THE Scope_Resolver SHALL select the interval with the latest (largest) start position among all containing intervals
2. WHEN no intervals contain the query point, THE Scope_Resolver SHALL return None
3. THE Scope_Resolver SHALL use the interval tree for O(log n) query time plus O(k) selection time where k is the number of containing intervals
4. WHEN the query position is at a function boundary, THE Scope_Resolver SHALL include that function's scope (inclusive boundaries)

### Requirement 3: Integration with Scope Artifacts

**User Story:** As a developer, I want the interval tree to be integrated into ScopeArtifacts, so that existing scope resolution code benefits from the optimization.

#### Acceptance Criteria

1. THE Scope_Artifacts SHALL contain an Interval_Tree field for function scopes instead of a Vec
2. WHEN computing artifacts, THE System SHALL build the interval tree from FunctionScope events
3. THE System SHALL replace all linear scans over function_scopes with interval tree queries
4. THE System SHALL maintain backward compatibility with existing scope resolution behavior

### Requirement 4: Position Comparison Semantics

**User Story:** As a developer, I want consistent position comparison semantics, so that interval containment checks are correct across all use cases.

#### Acceptance Criteria

1. THE Interval_Tree SHALL use lexicographic ordering for positions: (line, column) pairs compared by line first, then column
2. WHEN checking containment, THE System SHALL use inclusive boundaries: start <= point <= end
3. THE System SHALL handle UTF-16 column values correctly without overflow
4. WHEN positions use sentinel values (u32::MAX), THE System SHALL treat them as "end of file" positions

### Requirement 5: Performance Characteristics

**User Story:** As a developer, I want the interval tree to provide measurable performance improvements, so that scope resolution scales well with file complexity.

#### Acceptance Criteria

1. THE Interval_Tree point query SHALL complete in O(log n + k) time where n is the number of intervals and k is the number of results
2. THE Interval_Tree construction SHALL complete in O(n log n) time from a list of n intervals
3. THE System SHALL not regress performance for files with few function scopes (< 10)
4. THE System SHALL improve performance for files with many function scopes (> 100)
