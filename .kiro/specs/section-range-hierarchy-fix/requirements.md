# Requirements Document

## Introduction

Fix a bug in Raven's `HierarchyBuilder::compute_section_ranges()` method where section ranges are computed without considering heading levels. Currently, all sections are treated as flat siblings when computing end-line ranges, causing a parent section's range to be truncated at the start of its first child subsection. This results in symbols that appear after a subsection (but still logically within the parent section) being placed at the root level of the document outline instead of nested under the parent section.

## Glossary

- **HierarchyBuilder**: The component in `handlers.rs` that builds a hierarchical `DocumentSymbol` tree from a flat list of `RawSymbol` entries for the LSP document outline.
- **Section**: An R code section comment (e.g., `# Section ----`) represented as a `RawSymbol` with `kind = Module` and a non-None `section_level`.
- **Section_Level**: An integer (1, 2, 3, ...) representing the heading depth of a section. Level 1 corresponds to `#`, level 2 to `##`, etc.
- **Section_Range**: The LSP `Range` of a section symbol, spanning from the section comment line to the computed end line. Used to determine which symbols are contained within a section.
- **Sibling_Section**: A section at the same or lower (numerically smaller) heading level as the current section, indicating the start of a new peer or ancestor section.
- **Child_Section**: A section at a higher (numerically larger) heading level than the current section, indicating a subsection contained within the current section.
- **Symbol**: A code construct (function, variable, constant, class, method, or section) extracted from an R document for the document outline.

## Requirements

### Requirement 1: Level-Aware Section Range Computation

**User Story:** As a developer using Raven's document outline, I want parent sections to span over their child subsections, so that symbols appearing after a subsection are correctly nested under the parent section.

#### Acceptance Criteria

1. WHEN computing the end line for a section at Section_Level N, THE HierarchyBuilder SHALL set the end line to the line before the next section at Section_Level less than or equal to N.
2. WHEN a section at Section_Level N is followed only by Child_Sections (Section_Level greater than N) and no Sibling_Section before end of file, THE HierarchyBuilder SHALL set the end line of that section to the last line of the document.
3. WHEN a section at Section_Level N is the last section in the document, THE HierarchyBuilder SHALL set the end line to the last line of the document.
4. THE HierarchyBuilder SHALL preserve the existing behavior for same-level sections: a section's range ends at the line before the next section of the same level.

### Requirement 2: Correct Symbol Nesting After Subsections

**User Story:** As a developer using Raven's document outline, I want symbols that appear after a subsection but before the next sibling section to be nested under the correct parent section, so that the outline accurately reflects the logical structure of my code.

#### Acceptance Criteria

1. WHEN a Symbol appears on a line after a Child_Section but within the Section_Range of a parent section, THE HierarchyBuilder SHALL nest that Symbol under the parent section (or the deepest containing section).
2. WHEN a Symbol appears on a line within a Child_Section's range, THE HierarchyBuilder SHALL nest that Symbol under the Child_Section.
3. WHEN a Symbol appears on a line before any section, THE HierarchyBuilder SHALL place that Symbol at the root level of the outline.

### Requirement 3: Selection Range Preservation

**User Story:** As a developer, I want the selection range of section symbols to remain unchanged after the fix, so that clicking on a section in the outline still navigates to the section comment line.

#### Acceptance Criteria

1. THE HierarchyBuilder SHALL preserve the `selection_range` of each section symbol unchanged during section range computation.

### Requirement 4: Edge Case Handling

**User Story:** As a developer, I want the section range computation to handle edge cases correctly, so that the outline remains stable for unusual section arrangements.

#### Acceptance Criteria

1. WHEN a document contains no sections, THE HierarchyBuilder SHALL leave all symbol ranges unchanged.
2. WHEN a document contains only a single section, THE HierarchyBuilder SHALL set that section's range from its start line to the last line of the document.
3. WHEN two sections of different levels start on consecutive lines, THE HierarchyBuilder SHALL compute ranges correctly without overlap or gaps.
4. WHEN sections are provided in unsorted order, THE HierarchyBuilder SHALL sort sections by start line before computing ranges and produce correct results.
5. WHEN a level-2 section appears without a preceding level-1 section, THE HierarchyBuilder SHALL treat the level-2 section as a root-level section with its range extending to the next section of level less than or equal to 2 or end of file.
