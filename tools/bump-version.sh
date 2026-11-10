#!/usr/bin/env bash
# Bump the workspace version (Cargo.toml root, `version.workspace = true`)
# and cut a `vX.Y.Z` tag. Mirrors a minimal `bumpver` for this repo.
#
# Usage:
#   tools/bump-version.sh [patch|minor|major] [--push]
#
# Examples:
#   tools/bump-version.sh           # 0.1.0 -> 0.1.1
#   tools/bump-version.sh minor     # 0.1.0 -> 0.2.0
#   tools/bump-version.sh major --push  # 0.1.0 -> 1.0.0 + git push
set -euo pipefail

cd "$(dirname "$0")/.."

level="${1:-patch}"
push=false
[[ "${2:-}" == "--push" ]] && push=true

case "$level" in
  patch|minor|major) ;;
  *) echo "usage: $0 [patch|minor|major] [--push]" >&2; exit 2 ;;
esac

# Source version from the workspace root.
version=$(grep -m1 -E '^version = ' Cargo.toml | sed -E 's/^version = "(.*)"/\1/')
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "unexpected version '$version' in Cargo.toml" >&2
  exit 1
fi
IFS='.' read -r major minor patch <<< "$version"

case "$level" in
  patch) patch=$((patch + 1)) ;;
  minor) minor=$((minor + 1)); patch=0 ;;
  major) major=$((major + 1)); minor=0; patch=0 ;;
esac
new="$major.$minor.$patch"

# Refuse to muddle an uncommitted tree unless forced.
if [[ -n "$(git status --porcelain)" ]]; then
  echo "working tree is dirty; commit or stash first (or bump manually)" >&2
  exit 1
fi

sed -i "s/^version = \"$version\"/version = \"$new\"/" Cargo.toml
echo "version: $version -> $new"

# The crate versions are all `version.workspace = true`, so the root bump is
# the only edit. `cargo metadata` also rewrites the workspace members' versions in
# Cargo.lock, so commit the lock too (v0.1.1 missed it).
cargo metadata --no-deps --format-version 1 >/dev/null

git add Cargo.toml Cargo.lock
git commit -q -m "chore(release): v$new"
git tag "v$new"

echo "tagged v$new"
if $push; then
  git push origin main
  git push origin "v$new"
  echo "pushed"
else
  echo "not pushed (add --push to push branch and tag)"
fi
