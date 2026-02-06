# Implementation Plan: Section Range Hierarchy Fix

## Overview

Fix `HierarchyBuilder::compute_section_ranges()` in `crates/raven/src/handlers.rs` to be level-aware, then update existing tests and add property-based tests to validate correctness.

## Tasks

- [x] 1. Fix `compute_section_ranges()` to be level-aware
  - [x] 1.1 Modify the end-line computation loop in `HierarchyBuilder::compute_section_ranges()`
    - For each section at index `i` with `section_level = N`, scan forward through subsequent sections (sorted by start line) to find the first section with `section_level <= N`
    - If found, set `end_line = that_section.start_line - 1`; if not found, set `end_line = line_count - 1` (EOF)
    - Preserve the existing `next_start_line > 0` underflow guard and `line_count > 0` guard
    - _Requirements: 1.1, 1.2, 1.3, 1.4_

  - [x] 1.2 Write property test for level-aware section range end lines
    - **Property 1: Level-aware section range end lines**
    - Generate random section lists with levels 1–4 and unique start lines; verify each section's end line matches the expected value based on the next sibling-or-ancestor section
    - **Validates: Requirements 1.1, 1.2, 1.3, 1.4**

  - [x] 1.3 Write property test for selection range preservation
    - **Property 3: Selection range preservation**
    - Generate random symbol lists, snapshot selection_ranges before `compute_section_ranges()`, verify they are unchanged after
    - **Validates: Requirements 3.1**

  - [x] 1.4 Write property test for input order independence
    - **Property 4: Input order independence (confluence)**
    - Generate random section lists, run `compute_section_ranges()` on two copies with different input orderings, verify identical output ranges
    - **Validates: Requirements 4.4**

- [x] 2. Update existing tests and add unit tests for the bug scenario
  - [x] 2.1 Update existing `compute_section_ranges` unit tests
    - Review and update any existing tests that assumed flat sibling behavior for mixed-level sections
    - Add a new unit test for the core bug scenario: level-1 section, level-2 subsection, symbol after subsection — verify parent section range spans over the subsection
    - _Requirements: 1.1, 2.1_

  - [x] 2.2 Add unit test for three-level nesting with symbols
    - Test level 1 > level 2 > level 3 sections with symbols at each level, verify correct nesting after `build()`
    - _Requirements: 2.1, 2.2_

  - [x] 2.3 Write property test for correct symbol nesting after build
    - **Property 2: Correct symbol nesting after build**
    - Generate sections with varying levels and non-section symbols at random lines; after `build()`, verify every symbol is nested in the deepest containing section, and symbols outside all sections are at root
    - **Validates: Requirements 2.1, 2.2, 2.3**

- [x] 3. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- The fix is localized to a single method; no interface or data model changes needed
- Existing tests for same-level sections should continue to pass since level-aware logic degenerates to the current behavior when all sections are at the same level
- Property tests use the existing `proptest` crate (already in dev-dependencies)
