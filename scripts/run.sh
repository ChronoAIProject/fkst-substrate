#!/usr/bin/env bash
# Repository-local command wrapper.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

case "${1:-test}" in
  test)
    shift || true
    if (($#)); then
      echo "scripts/run.sh test: unexpected arguments: $*" >&2
      exit 2
    fi
    "$repo/scripts/verify.sh"
    ;;
  *)
    echo "usage: scripts/run.sh test" >&2
    exit 2
    ;;
esac
