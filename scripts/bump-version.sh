#!/bin/bash
set -e

# Release script: bumps version, commits, tags, and pushes.
#
# Usage:
#   ./scripts/bump-version.sh <version>
#
# Examples:
#   ./scripts/bump-version.sh 0.2.0
#   ./scripts/bump-version.sh 1.0.0-beta.1
#
# This script will:
#   1. Validate the working directory is clean
#   2. Update version in Cargo.toml and editors/vscode/package.json
#   3. Commit the version bump
#   4. Create and push a git tag (triggers release-build.yml)
#
# After this completes, go to GitHub Actions and manually run
# release-publish.yml with the tag to create the GitHub Release
# (and optionally publish to VS Code Marketplace).

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

usage() {
    echo "Usage: $0 <version>"
    echo ""
    echo "  version   Semantic version (e.g., 0.2.0 or 1.0.0-beta.1)"
    echo ""
    echo "Examples:"
    echo "  $0 0.2.0"
    echo "  $0 1.0.0"
    exit 1
}

VERSION="$1"
if [ -z "$VERSION" ]; then
    usage
fi

# Validate version format
if ! echo "$VERSION" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9.]+)?$'; then
    echo "ERROR: Invalid version format: $VERSION"
    echo "Expected format: X.Y.Z or X.Y.Z-suffix"
    exit 1
fi

TAG="v$VERSION"

# Check for clean working directory
if [ -n "$(git -C "$REPO_ROOT" status --porcelain)" ]; then
    echo "ERROR: Working directory is not clean. Commit or stash changes first."
    git -C "$REPO_ROOT" status --short
    exit 1
fi

# Check tag doesn't already exist
if git -C "$REPO_ROOT" rev-parse -q --verify "refs/tags/$TAG" >/dev/null 2>&1; then
    echo "ERROR: Tag $TAG already exists."
    exit 1
fi

echo "Bumping version to $VERSION..."

# Update workspace Cargo.toml
sed -i '' "s/^version = \".*\"/version = \"$VERSION\"/" "$REPO_ROOT/Cargo.toml"

# Update VS Code extension package.json
cd "$REPO_ROOT/editors/vscode"
npm version "$VERSION" --no-git-tag-version --allow-same-version >/dev/null
cd "$REPO_ROOT"

echo "Updated Cargo.toml and editors/vscode/package.json"

# Commit, tag, and push
git -C "$REPO_ROOT" add Cargo.toml editors/vscode/package.json
if [ -f "$REPO_ROOT/editors/vscode/package-lock.json" ]; then
  git -C "$REPO_ROOT" add editors/vscode/package-lock.json
fi
git -C "$REPO_ROOT" commit -m "chore: bump version to $VERSION"

echo "Creating tag $TAG..."
git -C "$REPO_ROOT" tag "$TAG"

echo "Pushing commit and tag..."
git -C "$REPO_ROOT" push origin
git -C "$REPO_ROOT" push origin "$TAG"

echo ""
echo "Release $TAG initiated!"
echo ""
echo "Next steps:"
echo "  1. Wait for release-build.yml to finish: https://github.com/jbearak/raven/actions"
echo "  2. Run release-publish.yml manually with tag=$TAG"
echo "     (check 'Publish to VS Code Marketplace' if ready)"
