# Implementation Plan: Priority 2 and 3 On-Demand Indexing

## Overview

Implement background indexing for Priority 2 (backward directive targets) and Priority 3 (transitive dependencies) files using a dedicated BackgroundIndexer component with a priority queue and single worker thread.

## Tasks

- [x] 1. Implement BackgroundIndexer core infrastructure
  - [x] 1.1 Create BackgroundIndexer struct
    - Add fields: state, queue, worker_handle, cancellation_token
    - Implement new() constructor that starts worker thread
    - Implement Drop trait to cancel worker on cleanup
    - _Requirements: 4.1, 4.3, 4.4_
  
  - [x] 1.2 Create IndexTask struct
    - Add fields: uri, priority, depth, submitted_at
    - Implement ordering by priority (lower number = higher priority)
    - Add helper methods for task comparison
    - _Requirements: 4.1_
  
  - [x] 1.3 Implement submit() method
    - Check if task already queued (avoid duplicates)
    - Check queue size limit and drop if full
    - Insert task in priority order
    - Add logging for queued tasks
    - _Requirements: 3.1, 3.2, 3.4, 8.1_
  
  - [x] 1.4 Implement worker thread
    - Use tokio::spawn with cancellation support
    - Poll queue every 100ms for new tasks
    - Process tasks sequentially (one at a time)
    - Handle cancellation gracefully
    - _Requirements: 3.2, 3.3, 4.1_

- [x] 2. Implement file indexing logic
  - [x] 2.1 Implement index_file() static method
    - Read file content asynchronously
    - Extract cross-file metadata
    - Compute scope artifacts
    - Update file cache and workspace index
    - Update dependency graph
    - Return metadata for transitive processing
    - _Requirements: 4.2, 5.3, 5.4_
  
  - [x] 2.2 Implement process_task() static method
    - Check if file needs indexing (not open, not in index)
    - Call index_file() to perform indexing
    - Log success with timing and symbol count
    - Log errors without crashing worker
    - Queue transitive dependencies if applicable
    - _Requirements: 5.4, 8.2, 8.3, 9.1, 9.2, 9.3_
  
  - [x] 2.3 Implement queue_transitive_deps() static method
    - Check if Priority 3 is enabled
    - Check if depth limit allows more indexing
    - Extract source() calls from metadata
    - Resolve paths and check if files need indexing
    - Submit Priority 3 tasks with incremented depth
    - _Requirements: 6.1, 6.2, 6.3, 6.4_

- [x] 3. Integrate BackgroundIndexer with Backend
  - [x] 3.1 Add background_indexer field to Backend
    - Add Arc<BackgroundIndexer> field
    - Initialize in Backend::new()
    - Pass state Arc to BackgroundIndexer
    - _Requirements: 4.4_
  
  - [x] 3.2 Modify did_open() to submit Priority 2 tasks
    - Filter files_to_index for priority == 2
    - Check if Priority 2 indexing is enabled
    - Submit each Priority 2 file to background_indexer
    - Add logging for submitted tasks
    - _Requirements: 5.1, 5.2, 5.3, 8.1_
  
  - [x] 3.3 Keep existing Priority 1 synchronous indexing
    - No changes to Priority 1 logic
    - Ensure Priority 1 completes before diagnostics
    - _Requirements: 5.4_

- [x] 4. Add configuration options
  - [x] 4.1 Add on-demand indexing fields to CrossFileConfig
    - on_demand_indexing_enabled: bool (default: true)
    - on_demand_indexing_max_transitive_depth: usize (default: 2)
    - on_demand_indexing_max_queue_size: usize (default: 50)
    - on_demand_indexing_priority_2_enabled: bool (default: true)
    - on_demand_indexing_priority_3_enabled: bool (default: true)
    - _Requirements: 7.1, 7.2, 7.3, 7.4, 7.5_
  
  - [x] 4.2 Parse configuration from LSP settings
    - Add parsing in parse_cross_file_config()
    - Handle missing/invalid values with defaults
    - Log configuration at startup
    - _Requirements: 7.1, 7.2, 7.3, 7.4, 7.5_
  
  - [x] 4.3 Use configuration in BackgroundIndexer
    - Check enabled flags before processing
    - Respect max_transitive_depth limit
    - Respect max_queue_size limit
    - _Requirements: 7.1, 7.2, 7.3, 7.4, 7.5_

- [x] 5. Add comprehensive logging
  - [x] 5.1 Log queue operations
    - Log when tasks are submitted with priority and depth
    - Log when tasks are skipped (already queued, queue full)
    - Log queue size after each operation
    - _Requirements: 8.1, 8.4_
  
  - [x] 5.2 Log indexing operations
    - Log when indexing starts for each file
    - Log when indexing completes with timing
    - Log symbol count after indexing
    - Log errors with file path and error message
    - _Requirements: 8.2, 8.3_
  
  - [x] 5.3 Log worker lifecycle
    - Log when worker starts
    - Log when worker stops (cancellation)
    - Log when worker encounters errors
    - _Requirements: 8.2_

- [x] 6. Add unit tests
  - [x] 6.1 Test BackgroundIndexer::submit()
    - Test priority ordering in queue
    - Test duplicate detection
    - Test queue size limiting
    - Test task insertion at correct position
    - _Requirements: 3.1, 3.4_
  
  - [x] 6.2 Test IndexTask ordering
    - Test priority comparison
    - Test tasks with same priority (FIFO)
    - Test depth tracking
    - _Requirements: 3.1_
  
  - [x] 6.3 Test queue_transitive_deps()
    - Test depth limiting
    - Test circular dependency detection
    - Test enabled/disabled flag
    - Test path resolution
    - _Requirements: 6.3, 6.4_

- [x] 7. Add integration tests
  - [x] 7.1 Test Priority 2 indexing end-to-end
    - Create file with @lsp-run-by directive
    - Open file and verify parent is queued
    - Wait for background indexing to complete
    - Verify parent file is in workspace index
    - Verify parent symbols are available
    - _Requirements: 1.1, 1.2, 1.3, 1.4, 5.1, 5.2, 5.3, 5.4_
  
  - [x] 7.2 Test Priority 3 indexing end-to-end
    - Create chain: A sources B, B sources C
    - Open A and verify B is indexed synchronously
    - Verify C is queued for background indexing
    - Wait for background indexing to complete
    - Verify C is in workspace index
    - _Requirements: 2.1, 2.2, 2.3, 2.4, 6.1, 6.2, 6.3_
  
  - [x] 7.3 Test depth limiting
    - Create deep chain: A -> B -> C -> D -> E
    - Set max_transitive_depth to 2
    - Open A and verify only B and C are indexed
    - Verify D and E are not indexed
    - _Requirements: 2.4, 6.3_
  
  - [x] 7.4 Test circular dependencies
    - Create cycle: A sources B, B sources A
    - Open A and verify no infinite loop
    - Verify both files are indexed once
    - _Requirements: 2.5, 6.4_
  
  - [x] 7.5 Test queue size limiting
    - Submit more tasks than max_queue_size
    - Verify excess tasks are dropped
    - Verify warning is logged
    - _Requirements: 3.4, 8.4_
  
  - [x] 7.6 Test cancellation
    - Start background indexing
    - Drop BackgroundIndexer
    - Verify worker stops cleanly
    - Verify no panics or hangs
    - _Requirements: 3.3_
  
  - [x] 7.7 Test error handling
    - Submit task for non-existent file
    - Submit task for unparseable file
    - Verify errors are logged
    - Verify worker continues processing
    - _Requirements: 9.1, 9.2, 9.3_

- [x] 8. Add property-based tests
  - [x] 8.1 Write property test for queue ordering
    - **Property 1: Queue ordering**
    - **Validates: Requirements 3.1**
    - Generate random tasks with various priorities
    - Verify tasks are processed in priority order
  
  - [x] 8.2 Write property test for no duplicate indexing
    - **Property 2: No duplicate indexing**
    - **Validates: Requirements 3.2**
    - Submit duplicate tasks
    - Verify each file is indexed at most once
  
  - [x] 8.3 Write property test for depth limiting
    - **Property 3: Depth limiting**
    - **Validates: Requirements 6.3**
    - Generate random dependency chains
    - Verify depth limit is never exceeded
  
  - [x] 8.4 Write property test for queue size limiting
    - **Property 4: Queue size limiting**
    - **Validates: Requirements 3.4**
    - Submit many tasks
    - Verify queue size never exceeds limit

- [x] 9. Documentation and cleanup
  - [x] 9.1 Add module documentation
    - Document BackgroundIndexer purpose and usage
    - Document priority levels and their meanings
    - Document configuration options
    - Add examples
    - _Requirements: All_
  
  - [x] 9.2 Update AGENTS.md
    - Document BackgroundIndexer architecture
    - Document priority queue design
    - Document configuration options
    - Add troubleshooting guide
    - _Requirements: All_
  
  - [x] 9.3 Add inline code comments
    - Comment complex logic in process_task()
    - Comment queue ordering algorithm
    - Comment depth tracking logic
    - _Requirements: All_

- [x] 10. Final validation and testing
  - [x] 10.1 Run full test suite
    - Run: cargo test -p rlsp
    - Verify all tests pass
    - Fix any regressions
    - _Requirements: All_
  
  - [x] 10.2 Test with real VS Code extension
    - Build and install: ./setup.sh
    - Test Priority 2: Open file with @lsp-run-by
    - Test Priority 3: Open file with transitive sources
    - Verify symbols are available after indexing
    - Check logs for correct behavior
    - _Requirements: 1.1, 1.2, 1.3, 1.4, 2.1, 2.2, 2.3_
  
  - [x] 10.3 Performance testing
    - Test with large workspace (100+ files)
    - Test with deep dependency chains (5+ levels)
    - Verify no performance degradation
    - Verify queue doesn't grow unbounded
    - _Requirements: 3.1, 3.2, 3.4_

## Notes

- All tasks build on completed task 20.2 (Priority 1 synchronous indexing)
- BackgroundIndexer is designed to be independent and testable
- Single worker thread keeps implementation simple and avoids race conditions
- Priority queue ensures important files are indexed first
- Depth limiting prevents excessive indexing of deep chains
- Configuration allows users to tune behavior for their needs
