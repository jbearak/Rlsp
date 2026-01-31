# Requirements Document

## Introduction

This document specifies the requirements for consolidating the workspace indexes in the rlsp crate and introducing a proper document management architecture following Sight's patterns. Currently, `WorldState` maintains:

1. **`documents`**: A `HashMap<Url, Document>` for open documents
2. **Legacy `workspace_index`**: A `HashMap<Url, Document>` storing full `Document` objects
3. **Cross-file `cross_file_workspace_index`**: A `CrossFileWorkspaceIndex` storing `IndexEntry` objects

This consolidation will:
- Introduce a `DocumentStore` for open documents with LRU eviction and memory limits
- Consolidate legacy and cross-file workspace indexes into a unified `WorkspaceIndex`
- Add a `ContentProvider` abstraction for file access
- Add debounced updates for workspace file changes

## Glossary

- **Document_Store**: Component managing open documents with LRU eviction and memory limits
- **Workspace_Index**: Unified index for closed workspace files (consolidates legacy and cross-file indexes)
- **Content_Provider**: Abstraction for accessing file content, metadata, and artifacts
- **Index_Entry**: A single entry in the Workspace_Index containing all data for a file
- **Document_State**: A single entry in the Document_Store for an open document
- **File_Snapshot**: Metadata for freshness checking (mtime, size, content_hash)
- **Cross_File_Metadata**: Extracted metadata for cross-file features (source calls, directives)
- **Scope_Artifacts**: Computed scope information (exported interface, timeline, function scopes)
- **LRU_Eviction**: Least-Recently-Used eviction policy for memory management
- **Debounced_Update**: Delayed update that batches rapid changes

## Requirements

### Requirement 1: Document Store for Open Documents

**User Story:** As a developer, I want open documents managed separately with proper memory limits, so that the LSP server remains responsive even with many open files.

#### Acceptance Criteria

1. THE Document_Store SHALL store open documents with content, parsed tree, metadata, and artifacts
2. THE Document_Store SHALL track document version and revision for change detection
3. WHEN a document is opened, THE Document_Store SHALL parse and compute all derived data
4. WHEN a document is updated, THE Document_Store SHALL reparse and update derived data
5. WHEN a document is closed, THE Document_Store SHALL remove it from storage

### Requirement 2: LRU Eviction and Memory Limits

**User Story:** As a developer, I want the document store to have memory limits, so that large workspaces don't exhaust system memory.

#### Acceptance Criteria

1. THE Document_Store SHALL enforce a maximum document count (configurable, default 50)
2. THE Document_Store SHALL enforce a maximum memory usage (configurable, default 100MB)
3. WHEN limits are exceeded, THE Document_Store SHALL evict least-recently-accessed documents
4. THE Document_Store SHALL track access order for LRU eviction
5. WHEN a document is accessed, THE Document_Store SHALL update its access timestamp

### Requirement 3: Open Document Authority

**User Story:** As a developer, I want open documents to always take precedence over indexed data, so that the editor's view is authoritative.

#### Acceptance Criteria

1. WHEN a document is open, THE Content_Provider SHALL return data from Document_Store
2. WHEN updating workspace index, THE Workspace_Index SHALL skip files that are open
3. THE Content_Provider SHALL provide a method to check if a URI is currently open
4. WHEN a document is closed, THE Workspace_Index SHALL be eligible to provide data for that URI

### Requirement 4: Unified Workspace Index

**User Story:** As a developer, I want a single workspace index for closed files, so that I don't need to maintain duplicate storage.

#### Acceptance Criteria

1. THE Workspace_Index SHALL store entries containing: content, parsed tree, packages, snapshot, metadata, and artifacts
2. WHEN an Index_Entry is created, THE Workspace_Index SHALL compute all derived data in a single operation
3. THE Workspace_Index SHALL use interior mutability (RwLock) for concurrent access
4. THE Workspace_Index SHALL maintain a monotonic version counter

### Requirement 5: Debounced Updates

**User Story:** As a developer, I want file updates to be debounced, so that rapid changes don't cause excessive re-indexing.

#### Acceptance Criteria

1. THE Workspace_Index SHALL support scheduling debounced updates
2. WHEN multiple updates are scheduled for the same URI within the debounce period, THE Workspace_Index SHALL batch them into one update
3. THE Workspace_Index SHALL have a configurable debounce delay (default 200ms)
4. WHEN processing the update queue, THE Workspace_Index SHALL skip URIs that are currently open

### Requirement 6: Async Update Coordination

**User Story:** As a developer, I want to wait for pending updates to complete, so that handlers see consistent data.

#### Acceptance Criteria

1. THE Document_Store SHALL track active async updates
2. THE Document_Store SHALL provide a method to wait for a specific update to complete
3. WHEN an update is in progress, THE Document_Store SHALL queue subsequent updates
4. WHEN an update completes, THE Document_Store SHALL notify waiters

### Requirement 7: Content Provider Abstraction

**User Story:** As a developer, I want a unified interface for accessing file content, so that handlers don't need fallback logic.

#### Acceptance Criteria

1. THE Content_Provider SHALL provide methods for: get_content, get_metadata, get_artifacts, exists, is_open
2. WHEN getting content, THE Content_Provider SHALL check Document_Store first, then Workspace_Index, then file cache
3. THE Content_Provider SHALL return consistent data across all accessor methods
4. THE Content_Provider trait SHALL be implementable for testing with mock data

### Requirement 8: Freshness Checking

**User Story:** As a developer, I want the index to track file freshness, so that stale data can be detected.

#### Acceptance Criteria

1. THE Workspace_Index SHALL store File_Snapshot data for each entry
2. WHEN checking freshness, THE Workspace_Index SHALL compare stored snapshot against provided snapshot
3. THE Workspace_Index SHALL provide a method to get an entry only if it matches a given snapshot

### Requirement 9: Invalidation Operations

**User Story:** As a developer, I want to invalidate index entries when files change, so that stale data is not used.

#### Acceptance Criteria

1. THE Workspace_Index SHALL provide a method to invalidate a single entry by URI
2. THE Workspace_Index SHALL provide a method to invalidate all entries
3. WHEN an entry is invalidated, THE Workspace_Index SHALL increment its version counter

### Requirement 10: Iteration Support

**User Story:** As a developer, I want to iterate over all indexed files, so that workspace-wide operations work correctly.

#### Acceptance Criteria

1. THE Workspace_Index SHALL provide a method to get all indexed URIs
2. THE Workspace_Index SHALL provide a method to iterate over all entries
3. WHEN iterating, THE Workspace_Index SHALL provide access to full entry data

### Requirement 11: Workspace Scanning

**User Story:** As a developer, I want workspace scanning to populate the unified index efficiently.

#### Acceptance Criteria

1. WHEN scanning workspace folders, THE scan SHALL populate the Workspace_Index with all R files
2. THE scan SHALL compute all derived data during the scan
3. THE scan SHALL operate without holding locks on WorldState
4. THE scan SHALL respect max file size limits (configurable, default 512KB)
5. THE scan SHALL respect max file count limits (configurable, default 1000)

### Requirement 12: On-Demand Indexing

**User Story:** As a developer, I want the index to support on-demand indexing of files discovered after initial scan.

#### Acceptance Criteria

1. THE Workspace_Index SHALL support inserting entries for files discovered after initial scan
2. THE Workspace_Index SHALL track the version at which each entry was indexed
3. WHEN on-demand indexing completes, THE Workspace_Index SHALL increment its version counter

### Requirement 13: Migration Compatibility

**User Story:** As a developer, I want the migration to be incremental, so that existing functionality is not broken.

#### Acceptance Criteria

1. THE new components SHALL support all operations currently performed on the old components
2. WHEN migrating, THE system SHALL maintain backward compatibility with existing handler code
3. THE migration SHALL be performed in phases to minimize risk

### Requirement 14: Non-Blocking File Existence Checks

**User Story:** As a developer, I want file existence checks to be non-blocking, so that the LSP main thread is not blocked by filesystem I/O.

#### Acceptance Criteria

1. THE Content_Provider SHALL provide an async method for batched file existence checking
2. WHEN checking file existence for diagnostics, THE system SHALL use the async batch method
3. THE async existence check SHALL first check cached sources (no I/O) before checking disk
4. WHEN files are not in cache, THE system SHALL use spawn_blocking to check disk in batch
5. THE diagnostic collection SHALL handle the async nature without blocking the main thread
