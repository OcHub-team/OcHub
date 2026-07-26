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
    printf 'OcHub-QA is still running. Quit it before rebuilding %s.\n' "${app_path}" >&2
    exit 1
fi

cd "${repo_root}"
cargo build --profile qa -p ochub-app

# The QA bundle carries the compiled icon catalog too, so the macOS 26
# light/dark app icon is verifiable here rather than only in a release build.
# That artwork cannot be checked from a screenshot of the app: it is drawn by
# the Dock and Finder, so it needs a real bundle on disk.
icon_build_dir="$(mktemp -d)"
trap 'rm -rf "${icon_build_dir}"' EXIT
"${script_dir}/build-app-icon.sh" "${icon_build_dir}"

mkdir -p "${macos_dir}" "${resources_dir}/assets"
install -m 755 "target/qa/ochub" "${executable_path}"
install -m 644 "scripts/qa/Info.plist" "${contents_dir}/Info.plist"
install -m 644 "crates/app/assets/app-icons/ochub.icns" "${resources_dir}/ochub.icns"
install -m 644 "${icon_build_dir}/Assets.car" "${resources_dir}/Assets.car"
rsync -a --delete "crates/app/assets/" "${resources_dir}/assets/"
touch "${app_path}"

printf 'QA app ready: %s\n' "${app_path}"
