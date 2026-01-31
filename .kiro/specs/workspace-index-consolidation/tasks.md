# Implementation Plan: Workspace Index Consolidation

## Overview

This implementation plan consolidates the workspace indexes in rlsp and introduces a proper document management architecture with DocumentStore, unified WorkspaceIndex, ContentProvider abstraction, and async file existence checking.

## Tasks

- [ ] 1. Create DocumentStore with LRU eviction
  - [ ] 1.1 Create `crates/rlsp/src/document_store.rs` with DocumentState and DocumentStore structs
    - Define DocumentStoreConfig with max_documents and max_memory_bytes
    - Define DocumentState with uri, version, contents, tree, loaded_packages, metadata, artifacts, revision
    - Define DocumentStoreMetrics for tracking cache hits/misses/evictions
    - Implement LRU tracking using IndexSet for O(1) access order updates
    - _Requirements: 1.1, 1.2, 1.3, 1.4, 1.5, 2.1, 2.2, 2.3, 2.4, 2.5_
  
  - [ ] 1.2 Write property test for LRU eviction correctness
    - **Property 2: LRU Eviction Correctness**
    - **Validates: Requirements 2.1, 2.2, 2.3, 2.4, 2.5**
  
  - [ ] 1.3 Write property test for memory limit enforcement
    - **Property 3: Memory Limit Enforcement**
    - **Validates: Requirements 2.1, 2.2**

- [ ] 2. Create unified WorkspaceIndex with debouncing
  - [ ] 2.1 Create `crates/rlsp/src/workspace_index.rs` with IndexEntry and WorkspaceIndex structs
    - Define WorkspaceIndexConfig with debounce_ms, max_files, max_file_size_bytes
    - Define IndexEntry with contents, tree, loaded_packages, snapshot, metadata, artifacts, indexed_at_version
    - Implement RwLock-based interior mutability for concurrent access
    - Implement monotonic version counter with AtomicU64
    - _Requirements: 4.1, 4.2, 4.3, 4.4_
  
  - [ ] 2.2 Implement debounced update scheduling
    - Add pending_updates HashMap with timestamps
    - Add update_queue HashSet for batched processing
    - Implement schedule_update() that resets debounce timer
    - Implement process_update_queue() that skips open URIs
    - _Requirements: 5.1, 5.2, 5.3, 5.4_
  
  - [ ] 2.3 Write property test for version monotonicity
    - **Property 4: Version Monotonicity**
    - **Validates: Requirements 4.4, 9.3, 12.3**
  
  - [ ] 2.4 Write property test for debounce batching
    - **Property 5: Debounce Batching**
    - **Validates: Requirements 5.1, 5.2, 5.3**

- [ ] 3. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 4. Create ContentProvider abstraction
  - [ ] 4.1 Create `crates/rlsp/src/content_provider.rs` with ContentProvider trait
    - Define sync methods: get_content, get_metadata, get_artifacts, exists_cached, is_open
    - Define AsyncContentProvider trait with check_existence_batch
    - _Requirements: 7.1, 7.2, 7.3, 7.4_
  
  - [ ] 4.2 Implement DefaultContentProvider
    - Implement ContentProvider for DefaultContentProvider
    - Check DocumentStore first, then WorkspaceIndex, then file cache
    - Implement AsyncContentProvider with spawn_blocking for disk I/O
    - _Requirements: 7.2, 14.1, 14.2, 14.3, 14.4_
  
  - [ ] 4.3 Write property test for open documents authority
    - **Property 1: Open Documents Are Authoritative**
    - **Validates: Requirements 3.1, 3.2, 3.4**
  
  - [ ] 4.4 Write property test for content provider consistency
    - **Property 8: Content Provider Consistency**
    - **Validates: Requirements 7.1, 7.2, 7.3**

- [ ] 5. Implement async update coordination
  - [ ] 5.1 Add active_updates tracking to DocumentStore
    - Track in-flight updates with oneshot channels
    - Implement wait_for_update() method
    - Queue updates when one is in progress
    - _Requirements: 6.1, 6.2, 6.3, 6.4_
  
  - [ ] 5.2 Write property test for async update coordination
    - **Property 9: Async Update Coordination**
    - **Validates: Requirements 6.1, 6.2, 6.3, 6.4**

- [ ] 6. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 7. Update WorldState to use new components
  - [ ] 7.1 Update WorldState struct definition
    - Replace `documents: HashMap<Url, Document>` with `document_store: DocumentStore`
    - Replace `workspace_index: HashMap<Url, Document>` and `cross_file_workspace_index` with `workspace_index: WorkspaceIndex`
    - Add content_provider() method
    - _Requirements: 4.1, 13.1_
  
  - [ ] 7.2 Update WorldState::new() initialization
    - Initialize DocumentStore with default config
    - Initialize WorkspaceIndex with default config
    - _Requirements: 4.1, 13.1_
  
  - [ ] 7.3 Update scan_workspace() to produce IndexEntry
    - Modify scan_directory() to create IndexEntry instead of Document
    - Compute all derived data during scan
    - _Requirements: 11.1, 11.2, 11.3, 11.4, 11.5_
  
  - [ ] 7.4 Update apply_workspace_index() to use new WorkspaceIndex
    - Accept HashMap<Url, IndexEntry> instead of separate maps
    - _Requirements: 11.1, 13.1_

- [ ] 8. Update handlers to use ContentProvider
  - [ ] 8.1 Update get_cross_file_symbols() in handlers.rs
    - Replace fallback pattern with ContentProvider calls
    - Use content_provider.get_artifacts() and content_provider.get_metadata()
    - _Requirements: 7.2, 13.2_
  
  - [ ] 8.2 Update collect_missing_file_diagnostics() to use async
    - Collect all URIs to check
    - Use check_existence_batch() for non-blocking I/O
    - Generate diagnostics from results
    - _Requirements: 14.2, 14.5_
  
  - [ ] 8.3 Update other handlers using workspace_index
    - Update goto_definition() to use ContentProvider
    - Update references() to use ContentProvider
    - Update workspace symbol search to use WorkspaceIndex.iter()
    - _Requirements: 10.1, 10.2, 10.3, 13.2_

- [ ] 9. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 10. Update backend.rs for new architecture
  - [ ] 10.1 Update document lifecycle methods
    - Update textDocument/didOpen to use document_store.open()
    - Update textDocument/didChange to use document_store.update()
    - Update textDocument/didClose to use document_store.close()
    - _Requirements: 1.3, 1.4, 1.5_
  
  - [ ] 10.2 Update file watcher handlers
    - Use workspace_index.schedule_update() for file changes
    - Periodically call process_update_queue()
    - _Requirements: 5.1, 5.4_
  
  - [ ] 10.3 Update on-demand indexing
    - Use workspace_index.insert() for on-demand indexed files
    - _Requirements: 12.1, 12.2, 12.3_

- [ ] 11. Remove deprecated code
  - [ ] 11.1 Remove old Document struct usage for workspace index
    - Keep Document struct for DocumentState compatibility if needed
    - Remove workspace_index: HashMap<Url, Document> field
    - _Requirements: 13.1_
  
  - [ ] 11.2 Remove CrossFileWorkspaceIndex
    - Remove cross_file_workspace_index field from WorldState
    - Remove crates/rlsp/src/cross_file/workspace_index.rs (or repurpose)
    - _Requirements: 13.1_
  
  - [ ] 11.3 Remove fallback patterns from handlers
    - Remove "Fallback to legacy workspace index" code paths
    - _Requirements: 13.2_

- [ ] 12. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 13. Write integration tests
  - [ ] 13.1 Write integration test for full document lifecycle
    - Test open → edit → close → workspace index update flow
    - _Requirements: 1.3, 1.4, 1.5, 3.4_
  
  - [ ] 13.2 Write integration test for cross-file resolution
    - Test that cross-file features work with new architecture
    - _Requirements: 7.2, 13.2_
  
  - [ ] 13.3 Write integration test for async diagnostics
    - Test that missing file diagnostics work with async existence checks
    - _Requirements: 14.2, 14.5_

## Notes

- All tasks are required for comprehensive implementation
- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation
- Property tests validate universal correctness properties
- The migration is designed to be incremental - each phase can be tested independently
