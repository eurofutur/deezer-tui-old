#!/usr/bin/env bash
# Usage: CODEBERG_TOKEN=... ./release.sh 0.2.0
# Bumps Cargo.toml, builds, tags v<version>, pushes, and publishes a Codeberg
# release with the binary that the app's updater downloads.
set -euo pipefail

version=${1:?usage: release.sh <version, e.g. 0.2.0>}
: "${CODEBERG_TOKEN:?export CODEBERG_TOKEN (Codeberg > Settings > Applications, scope write:repository)}"
api=https://codeberg.org/api/v1/repos/pyratt/deezer-tui
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

notes=$(git log --format='- %s' "$(git describe --tags --abbrev=0 "$tag^" 2>/dev/null || git rev-list --max-parents=0 HEAD)..$tag^")
auth=(-H "Authorization: token $CODEBERG_TOKEN")
id=$(jq -n --arg t "$tag" --arg b "$notes" '{tag_name:$t,name:$t,body:$b}' |
  curl -fsS "${auth[@]}" -H 'Content-Type: application/json' -d @- "$api/releases" | jq -r .id)
curl -fsS "${auth[@]}" -F "attachment=@target/release/deezer-tui;filename=$asset" \
  "$api/releases/$id/assets?name=$asset" >/dev/null
echo "Published $tag (release $id)"
