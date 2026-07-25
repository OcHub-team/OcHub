#!/usr/bin/env bash
set -euo pipefail

tag="${1:-${GITHUB_REF_NAME:-}}"
if [[ -z "${tag}" ]]; then
    printf 'Usage: %s v<workspace-version>\n' "$0" >&2
    exit 2
fi

version="$(
    cargo metadata --no-deps --format-version 1 |
        jq -r '.packages[] | select(.name == "ochub-app") | .version'
)"
expected="v${version}"

if [[ "${tag}" != "${expected}" ]]; then
    printf 'Release tag %s does not match workspace version %s.\n' "${tag}" "${expected}" >&2
    exit 1
fi

printf '%s\n' "${version}"
