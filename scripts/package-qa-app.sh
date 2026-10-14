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

# Reuse the fixed QA bundle's icon catalog when its source has not changed.
# UI-only iterations should not need another Icon Composer service session.
# Keep the builder alongside it so later changes to compiler flags invalidate
# the cache too. Older QA bundles predate that marker; accept those only when
# the builder still matches the repository's committed version.
icon_build_dir=""
trap 'if [[ -n "${icon_build_dir}" ]]; then rm -rf "${icon_build_dir}"; fi' EXIT
cached_icon_builder="${resources_dir}/.qa-icon-builder.sh"
icon_builder_matches=false
if [[ -f "${cached_icon_builder}" ]]; then
    if cmp -s "${script_dir}/build-app-icon.sh" "${cached_icon_builder}"; then
        icon_builder_matches=true
    fi
elif git -C "${repo_root}" diff --quiet HEAD -- scripts/build-app-icon.sh; then
    icon_builder_matches=true
fi

if [[ "${OCHUB_QA_REBUILD_ICON:-0}" != "1" \
      && "${icon_builder_matches}" == "true" \
      && -s "${resources_dir}/Assets.car" ]] \
    && diff -rq "${repo_root}/crates/app/assets/app-icons/OcHub.icon" \
        "${resources_dir}/assets/app-icons/OcHub.icon" >/dev/null 2>&1; then
    printf 'Reusing QA icon catalog (unchanged source).\n'
else
    icon_build_dir="$(mktemp -d)"
    "${script_dir}/build-app-icon.sh" "${icon_build_dir}"
fi

mkdir -p "${macos_dir}" "${resources_dir}/assets"
install -m 755 "target/qa/ochub" "${executable_path}"
install -m 644 "scripts/qa/Info.plist" "${contents_dir}/Info.plist"
install -m 644 "crates/app/assets/app-icons/ochub.icns" "${resources_dir}/ochub.icns"
if [[ -n "${icon_build_dir}" ]]; then
    install -m 644 "${icon_build_dir}/Assets.car" "${resources_dir}/Assets.car"
fi
install -m 644 "${script_dir}/build-app-icon.sh" "${cached_icon_builder}"
rsync -a --delete "crates/app/assets/" "${resources_dir}/assets/"
codesign --force --deep --sign - "${app_path}"
touch "${app_path}"

printf 'QA app ready: %s\n' "${app_path}"
