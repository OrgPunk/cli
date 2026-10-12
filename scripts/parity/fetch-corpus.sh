#!/usr/bin/env bash
# Fetches the corpus at its pinned commits into DIR (default target/corpus).
# Existing checkouts at the right commit are kept.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
dir=${1:-$here/../../target/corpus}
mkdir -p "$dir"

grep -v '^#' "$here/corpus.txt" | while read -r repo sha; do
  [ -n "$repo" ] || continue
  dst="$dir/${repo//\//__}"
  if [ "$(git -C "$dst" rev-parse HEAD 2>/dev/null)" = "$sha" ]; then
    continue
  fi
  echo "fetching $repo@${sha:0:12}"
  rm -rf "$dst"
  git init -q "$dst"
  git -C "$dst" fetch -q --depth 1 "https://github.com/$repo" "$sha"
  git -C "$dst" -c advice.detachedHead=false checkout -q FETCH_HEAD
done
