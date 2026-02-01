#!/bin/bash
set -e

echo "Building Raven..."
cargo build --release -p raven

echo "Installing binary to ~/bin..."
mkdir -p ~/bin
cp target/release/raven ~/bin/raven
chmod +x ~/bin/raven
echo "✓ Binary installed to ~/bin/raven"

echo "Building VS Code extension..."
cd editors/vscode

echo "Copying binary to extension..."
mkdir -p bin
cp ../../target/release/raven bin/raven

echo "Installing npm dependencies..."
npm install

echo "Compiling TypeScript..."
npm run compile

echo "Packaging extension..."
npm run package

echo "Installing extension to VS Code..."
VSIX_FILE=$(ls raven-*.vsix | head -n 1)
if [ -n "$VSIX_FILE" ]; then
    code --install-extension "$VSIX_FILE"
    echo "✓ Extension installed: $VSIX_FILE"
else
    echo "✗ No .vsix file found"
    exit 1
fi

echo ""
echo "✅ Setup complete!"
echo "   - Binary: ~/bin/raven"
echo "   - Extension: $VSIX_FILE"