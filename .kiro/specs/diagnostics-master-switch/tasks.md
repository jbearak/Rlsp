# Implementation Plan: Diagnostics Master Switch

## Overview

This implementation adds a master switch configuration option (`raven.diagnostics.enabled`) that allows users to completely enable or disable all diagnostics. The implementation touches four files: VS Code extension configuration, Rust configuration struct, configuration parser, and diagnostics handler.

## Tasks

- [x] 1. Add VS Code extension configuration
  - [x] 1.1 Add `raven.diagnostics.enabled` setting to package.json
    - Add boolean property with default `true`
    - Add description explaining the master switch behavior
    - Place it before `raven.diagnostics.undefinedVariables` for logical grouping
    - _Requirements: 1.1, 1.2_

- [x] 2. Add server-side configuration field
  - [x] 2.1 Add `diagnostics_enabled` field to CrossFileConfig struct
    - Add `pub diagnostics_enabled: bool` field to the struct
    - Add default value `true` in `Default` impl
    - _Requirements: 2.1, 2.2_
  
  - [x] 2.2 Write unit test for default value
    - Add test asserting `CrossFileConfig::default().diagnostics_enabled == true`
    - _Requirements: 2.2_

- [x] 3. Update configuration parser
  - [x] 3.1 Parse `diagnostics.enabled` in `parse_cross_file_config`
    - Read from `diagnostics.enabled` in the settings JSON
    - Apply to `config.diagnostics_enabled`
    - Handle missing value by keeping default
    - _Requirements: 2.3, 2.4_
  
  - [x] 3.2 Add logging for the new setting
    - Log `diagnostics_enabled` value with other config values
    - _Requirements: 5.1_
  
  - [x] 3.3 Write property test for configuration parsing
    - **Property 3: Configuration parsing round-trip for explicit boolean**
    - **Validates: Requirements 2.3**
  
  - [x] 3.4 Write unit test for missing setting default
    - Test that missing `diagnostics.enabled` results in `true`
    - **Property 4: Configuration parsing defaults to enabled when absent**
    - **Validates: Requirements 2.4**

- [x] 4. Implement diagnostics suppression
  - [x] 4.1 Add master switch check to `diagnostics` function
    - Add early return of empty Vec if `diagnostics_enabled` is false
    - Place check at the very start of the function
    - _Requirements: 3.1, 3.4_
  
  - [x] 4.2 Write property test for disabled master switch
    - **Property 1: Master switch disabled suppresses all diagnostics**
    - **Validates: Requirements 1.4, 3.1, 3.4**
  
  - [x] 4.3 Write property test for enabled master switch
    - **Property 2: Master switch enabled preserves normal diagnostics behavior**
    - **Validates: Requirements 1.3, 3.3**

- [x] 5. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 6. Verify runtime configuration changes
  - [x] 6.1 Verify existing `did_change_configuration` handles the new setting
    - The existing handler should already recompute diagnostics for open documents
    - When disabled, `diagnostics()` returns empty, clearing existing diagnostics
    - When enabled, `diagnostics()` computes normally
    - No code changes expected, just verification
    - _Requirements: 4.1, 4.2, 4.3, 4.4_

- [x] 7. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- The implementation leverages existing configuration infrastructure
- Runtime configuration changes are handled by existing `did_change_configuration` handler
- Property tests validate universal correctness properties
- Unit tests validate specific examples and edge cases
