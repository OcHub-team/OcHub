#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"
app_path="/tmp/OCHUB-QA.app"
contents_dir="${app_path}/Contents"
macos_dir="${contents_dir}/MacOS"
resources_dir="${contents_dir}/Resources"
executable_path="${macos_dir}/ochub"

if pgrep -f "${executable_path}" >/dev/null 2>&1; then
    printf 'OCHUB-QA is still running. Quit it before rebuilding %s.\n' "${app_path}" >&2
    exit 1
fi

cd "${repo_root}"
cargo build -p ochub-app

mkdir -p "${macos_dir}" "${resources_dir}/assets"
install -m 755 "target/debug/ochub" "${executable_path}"
install -m 644 "scripts/qa/Info.plist" "${contents_dir}/Info.plist"
rsync -a --delete "crates/app/assets/" "${resources_dir}/assets/"
touch "${app_path}"

printf 'QA app ready: %s\n' "${app_path}"
