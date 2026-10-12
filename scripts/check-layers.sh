#!/usr/bin/env bash
# Checks that crate dependencies only point downwards: cli -> scenario -> core.
# Dependencies within a layer are allowed. Each crate sets its layer in
# [package.metadata.orgpunk] layer.
set -euo pipefail

violations=$(cargo metadata --format-version 1 --no-deps | jq -r '
  def rank: {"cli": 2, "scenario": 1, "core": 0}[.];
  (.packages | map({key: .name, value: .metadata.orgpunk.layer}) | from_entries) as $layer
  | .packages[]
  | .name as $from
  | if ($layer[$from] | rank) == null then "\($from): package.metadata.orgpunk.layer is missing or unknown"
    else
      ( .dependencies[]
        | select($layer[.name] != null)
        | select(($layer[.name] | rank) > ($layer[$from] | rank))
        | "\($from) (\($layer[$from])) -> \(.name) (\($layer[.name]))" )
    end
')

if [ -n "$violations" ]; then
  echo "Dependency layer violations:" >&2
  echo "$violations" >&2
  exit 1
fi
echo "Dependency layers are OK."
