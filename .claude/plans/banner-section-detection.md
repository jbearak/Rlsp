# Plan: Banner-Style Section Detection

## Summary

Add detection of multi-line "banner style" R code sections to the existing single-line section detection in `extract_sections()`. Banner sections have delimiter lines **both above and below** a comment name line:

```r
# ================       ################
# Section Name           # Section Name #
# ================       ################
```

Supported delimiter characters: `#`, `=`, `*`, `-`, `+` (matching existing single-line support). Delimiters above and below must use the same character type but don't need to be the same length.

## Files to modify

- `crates/raven/src/handlers.rs` — helper functions, extract_sections() changes, tests
- `AGENTS.md` — documentation update (CLAUDE.md is a symlink to this)

## Implementation

### Step 1: Add helper functions (after `is_delimiter_only()`, ~line 72)

**`DelimiterKind` enum:**
```rust
enum DelimiterKind { Hash, Dash, Equals, Asterisk, Plus }
```

**`classify_delimiter_line(line: &str) -> Option<DelimiterKind>`:**
- Determines if a line is a "delimiter line" — a comment line consisting entirely of a single repeated delimiter character (4+).
- Handles two forms:
  - `################` (all hashes, no space needed)
  - `# ================` (leading `#` + space + 4+ of one delimiter type)
- Returns `Some(kind)` if it's a delimiter line, `None` otherwise.

**`extract_banner_name(line: &str) -> Option<String>`:**
- Extracts the section name from the middle line of a banner.
- Strips leading `#` characters and whitespace, strips trailing delimiter chars and whitespace.
- Returns `None` if the result is empty or delimiter-only.
- Examples:
  - `# Section Name #` → `"Section Name"`
  - `# Section Name` → `"Section Name"`
  - `#  My Analysis  ` → `"My Analysis"`

### Step 2: Modify `extract_sections()` (~line 870)

Change from single-pass to two-phase approach:

**Phase 1: Single-line detection (existing logic, unchanged)**
- Iterate lines, match against `section_pattern()`, filter with `is_delimiter_only()`.
- Collect results into `sections` vec.
- Also record which lines were consumed as single-line sections in a `HashSet<usize>`.

**Phase 2: Banner detection (new)**
- Iterate lines again looking for 3-line banner patterns.
- For each line `i` (from 1 to len-2), check:
  1. Line `i-1`: `classify_delimiter_line()` returns `Some(kind_top)`
  2. Line `i+1`: `classify_delimiter_line()` returns `Some(kind_bottom)`
  3. `kind_top == kind_bottom` (delimiter types match)
  4. Line `i`: `extract_banner_name()` returns `Some(name)` with non-empty, non-delimiter-only content
  5. Lines `i-1`, `i`, `i+1` are not already consumed by single-line detection
- If all conditions pass: create a `RawSymbol` with:
  - `name`: extracted banner name
  - `kind`: `DocumentSymbolKind::Module`
  - `range`: spans all 3 lines (i-1 to i+1)
  - `selection_range`: just the name line (line i)
  - `section_level`: `Some(1)` (banner sections are always top-level)

**Phase 3: Merge and sort**
- Combine single-line and banner sections, sort by start line.

### Step 3: Add tests

Add a new test section after the existing `is_delimiter_only()` tests (~line 9298):

1. Basic banner with each delimiter type (`=`, `#`, `*`, `-`, `+`)
2. Banner with decorative inner chars (`# Name #`)
3. Mismatched delimiter lengths (still detected)
4. Mismatched delimiter types (NOT detected)
5. Banner only above / only below (NOT detected)
6. Banner at start/end of file
7. Banner coexisting with single-line sections
8. Banner name is delimiter-only (NOT detected)
9. Banner section_level is always 1
10. Banner range spans 3 lines, selection_range is name line only
11. Unit tests for `classify_delimiter_line()` and `extract_banner_name()`

### Step 4: Update AGENTS.md

Update the "Symbol Provider Architecture" section to document banner section support alongside the existing single-line pattern. Note that banner sections are always heading level 1.

## Verification

1. `cargo test -p raven` — all existing tests pass, new tests pass
2. `cargo clippy -p raven` — no warnings
