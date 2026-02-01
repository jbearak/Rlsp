#!/bin/bash
# Test workspace indexing manually

set -e

TEST_DIR=$(mktemp -d)
echo "Test directory: $TEST_DIR"

# Create test workspace
mkdir -p "$TEST_DIR/subdir"

cat > "$TEST_DIR/main.R" << 'EOF'
# Main file
result <- helper_func(10)
EOF

cat > "$TEST_DIR/subdir/utils.R" << 'EOF'
# Utils file
helper_func <- function(x) {
    x * 2
}
EOF

echo "Created test workspace:"
find "$TEST_DIR" -name "*.R" -exec echo "  {}" \;

echo ""
echo "To test:"
echo "1. Open VS Code in: $TEST_DIR"
echo "2. Open main.R"
echo "3. Right-click on 'helper_func' and select 'Go to Definition'"
echo "4. It should navigate to subdir/utils.R"
echo ""
echo "Check LSP logs in VS Code: Output > Ark R"
