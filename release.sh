#!/usr/bin/env bash
# Usage: ./release.sh 0.2.0   (needs `gh auth login`)
# Bumps Cargo.toml, builds, tags v<version>, pushes to Codeberg and GitHub, and
# publishes a GitHub release with the binary that the app's updater downloads.
set -euo pipefail

version=${1:?usage: release.sh <version, e.g. 0.2.0>}
repo=eurofutur/deezer-tui # must match update::API
asset=deezer-tui-x86_64-linux # must match update::ASSET
tag=v$version

cd "$(dirname "$0")"
[[ -z $(git status --porcelain) ]] || { echo "working tree not clean" >&2; exit 1; }
git rev-parse -q --verify "refs/tags/$tag" >/dev/null && { echo "$tag already exists" >&2; exit 1; }

sed -i "0,/^version = .*/s//version = \"$version\"/" Cargo.toml
cargo build --release
git commit -qam "Release $tag"
git tag -a "$tag" -m "$tag"
git push -q origin HEAD "$tag"
git push -q github HEAD "$tag"

notes=$(git log --format='- %s' "$(git describe --tags --abbrev=0 "$tag^" 2>/dev/null || git rev-list --max-parents=0 HEAD)..$tag^")
cp target/release/deezer-tui "target/release/$asset"
gh release create "$tag" "target/release/$asset" -R "$repo" -t "$tag" -n "$notes"
echo "Published $tag"
