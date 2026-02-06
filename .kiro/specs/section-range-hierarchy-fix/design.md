# Design Document: Section Range Hierarchy Fix

## Overview

The `HierarchyBuilder::compute_section_ranges()` method in `crates/raven/src/handlers.rs` currently treats all sections as flat siblings when computing their end-line ranges. Each section's range ends at `next_section_start_line - 1`, regardless of heading level. This causes parent sections to be truncated at the start of their first child subsection, resulting in symbols after subsections being orphaned to the root level.

The fix modifies `compute_section_ranges()` to be level-aware: a section at level N ends at the line before the next section at level ≤ N (a sibling or ancestor), not at the next section of any level. Subsections (level > N) are contained within the parent section's range.

## Architecture

The fix is localized to a single method: `HierarchyBuilder::compute_section_ranges()`. No other methods need modification because:

- `build_section_hierarchy()` already correctly builds nesting from section levels
- `try_insert_into_section()` already correctly uses range containment to nest symbols
- `insert_symbol_into_hierarchy()` already correctly traverses the hierarchy

The bug is purely in range computation. Once ranges are correct, the downstream nesting logic works as intended.

### Current Flow (Buggy)

```text
compute_section_ranges() → flat sibling ranges → nest_in_sections() → incorrect nesting
```

### Fixed Flow

```text
compute_section_ranges() → level-aware ranges → nest_in_sections() → correct nesting
```

## Components and Interfaces

### Modified Component: `HierarchyBuilder::compute_section_ranges()`

The method signature remains unchanged (`pub fn compute_section_ranges(&mut self)`). Only the internal logic for computing `end_line` changes.

#### Current Algorithm (Buggy)

```text
for each section i (sorted by start line):
    end_line = next_section[i+1].start_line - 1   // or EOF if last
```

#### Fixed Algorithm

```rust
for each section i (sorted by start line):
    let current_level = sections[i].section_level
    // Scan forward for the next section at level <= current_level
    let end_line = scan_forward_for_sibling_or_ancestor(i, current_level)
    // If none found, extend to EOF
```

Specifically, for each section at index `i` with level `current_level`:
1. Iterate through sections `j = i+1, i+2, ...` (sorted by start line)
2. If `sections[j].section_level <= current_level`, then `end_line = sections[j].start_line - 1`
3. If no such section is found, `end_line = line_count - 1` (EOF)

### Unchanged Components

- `RawSymbol` struct — no changes needed
- `build_section_hierarchy()` — already level-aware
- `try_insert_into_section()` — already uses range containment
- `insert_symbol_into_hierarchy()` — already traverses hierarchy correctly
- `build()` — orchestration unchanged

## Data Models

No data model changes. The existing `RawSymbol` struct with its `section_level: Option<u32>` field and `range: Range` field are sufficient.

### Example: Before and After

Given sections (sorted by line):

| Index | Name | Level | Start Line |
|-------|------|-------|------------|
| 0 | Section A | 1 | 0 |
| 1 | Subsection B | 2 | 2 |
| 2 | Section C | 1 | 5 |

**Before (buggy):**

| Section | End Line | Reason |
|---------|----------|--------|
| Section A | 1 | Next section (Subsection B) at line 2 → 2-1=1 |
| Subsection B | 4 | Next section (Section C) at line 5 → 5-1=4 |
| Section C | EOF | Last section |

**After (fixed):**

| Section | End Line | Reason |
|---------|----------|--------|
| Section A | 4 | Next section at level ≤ 1 is Section C at line 5 → 5-1=4 |
| Subsection B | 4 | Next section at level ≤ 2 is Section C at line 5 → 5-1=4 |
| Section C | EOF | Last section |

Now a symbol at line 4 falls within both Section A's range (0–4) and Subsection B's range (2–4). The `try_insert_into_section()` method correctly nests it in Subsection B first (deepest match), which is itself a child of Section A.


## Correctness Properties

*A property is a characteristic or behavior that should hold true across all valid executions of a system — essentially, a formal statement about what the system should do. Properties serve as the bridge between human-readable specifications and machine-verifiable correctness guarantees.*

### Property 1: Level-aware section range end lines

*For any* list of sections with arbitrary levels and start lines (sorted by start line), after calling `compute_section_ranges()`, each section at level N shall have its end line equal to `next_sibling_or_ancestor.start_line - 1` where the next sibling or ancestor is the first subsequent section with level ≤ N, or `line_count - 1` (EOF) if no such section exists.

**Validates: Requirements 1.1, 1.2, 1.3, 1.4**

### Property 2: Correct symbol nesting after build

*For any* valid configuration of sections (with varying levels) and non-section symbols, after calling `build()`, every non-section symbol whose start line falls within a section's computed range shall appear as a descendant of that section (nested in the deepest containing section), and symbols outside all section ranges shall appear at the root level.

**Validates: Requirements 2.1, 2.2, 2.3**

### Property 3: Selection range preservation

*For any* list of symbols (sections and non-sections), after calling `compute_section_ranges()`, the `selection_range` of every symbol shall be identical to its `selection_range` before the call.

**Validates: Requirements 3.1**

### Property 4: Input order independence (confluence)

*For any* list of sections, the computed section ranges after `compute_section_ranges()` shall be identical regardless of the initial ordering of sections in the input list.

**Validates: Requirements 4.4**

## Error Handling

This fix does not introduce new error conditions. The existing error handling in `compute_section_ranges()` is preserved:

- **Empty symbol list**: Returns immediately (no-op). No change needed.
- **No sections in symbol list**: Returns immediately (no-op). No change needed.
- **Section at line 0 with next section at line 0**: The `if next_start_line > 0` guard prevents underflow. This guard is preserved in the fixed algorithm.
- **`line_count` of 0**: The `if self.line_count > 0` guard handles this. Preserved.

## Testing Strategy

### Property-Based Tests (proptest)

Use the existing `proptest` crate (already a dev-dependency). Each property test runs a minimum of 100 iterations.

**Generators needed:**
- Random section lists: generate `Vec<RawSymbol>` where each entry has `kind = Module`, `section_level = Some(1..=4)`, and unique start lines within `0..line_count`
- Random non-section symbols: generate `Vec<RawSymbol>` with `section_level = None` and start lines within `0..line_count`
- Random `line_count` values (e.g., `1..=100`)

**Edge cases to cover in generators:**
- Sections on consecutive lines with different levels
- Symbols before any section (should remain at root)
- Level-2+ sections without preceding level-1 sections
- Documents with no sections
- Single section documents

**Property test tags:**
- `Feature: section-range-hierarchy-fix, Property 1: Level-aware section range end lines`
- `Feature: section-range-hierarchy-fix, Property 2: Correct symbol nesting after build`
- `Feature: section-range-hierarchy-fix, Property 3: Selection range preservation`
- `Feature: section-range-hierarchy-fix, Property 4: Input order independence`

### Unit Tests

Unit tests complement property tests by covering specific, readable scenarios:

- **Core bug scenario**: Parent section with child subsection, symbol after subsection — verify symbol nests under parent
- **Three-level nesting**: Level 1 > Level 2 > Level 3 with symbols at each level
- **Multiple subsections under one parent**: Verify parent range spans all subsections
- **Existing test updates**: Update existing `compute_section_ranges` tests that assumed flat sibling behavior to reflect level-aware behavior (tests with mixed levels will have different expected end lines)
