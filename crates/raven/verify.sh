#!/bin/bash
# Quick verification script for workspace indexing

set -e

echo "=== Building ark-lsp ==="
cd /Users/jmb/repos/ark/crates/ark-lsp
cargo build --release

echo ""
echo "=== Running unit tests ==="
cargo test

echo ""
echo "=== Summary ==="
echo "✓ ark-lsp builds successfully"
echo "✓ Unit tests pass"
echo ""
echo "To test workspace indexing in VS Code:"
echo "1. Open editors/vscode in VS Code"
echo "2. Run 'npm test' to execute integration tests"
echo "3. The new tests verify workspace-wide go-to-definition and find-references"
echo ""
echo "Test files created:"
echo "  - src/test/fixtures/workspace/utils.R"
echo "  - src/test/fixtures/workspace_main.R"
echo ""
echo "Changes made:"
echo "  - state.rs: +31 lines (workspace index)"
echo "  - handlers.rs: +24 lines (workspace search)"
echo "  - backend.rs: +4 lines (initialization)"
echo "  - lsp.test.ts: +28 lines (new tests)"
