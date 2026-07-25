#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"
target="${CARGO_BUILD_TARGET:-$(rustc -vV | sed -n 's/^host: //p')}"
out_dir="${1:-dist}"

cd "${repo_root}"
mkdir -p "${out_dir}"
out_dir="$(cd "${out_dir}" && pwd)"

version="$(
    cargo metadata --no-deps --format-version 1 |
        jq -r '.packages[] | select(.name == "ochub-app") | .version'
)"

cargo build --release --locked --target "${target}" -p ochub-app
"target/${target}/release/ochub" --version

# The updater payload is a tarred .app rather than the .dmg: mounting a disk
# image to update means hdiutil, an extra failure mode, and a copy of a copy.
# `tar` also preserves the symlinks and extended attributes that a signed
# bundle needs to stay verifiable. The .dmg stays the download for humans.
archive_app_bundle() {
    local app_path
    app_path="$(find "${out_dir}" -maxdepth 1 -type d -name '*.app' -print -quit)"
    if [[ -z "${app_path}" ]]; then
        printf 'no .app produced in %s; cannot build updater artifact\n' "${out_dir}" >&2
        exit 1
    fi

    local arch
    case "${target}" in
    aarch64-*) arch="aarch64" ;;
    x86_64-*) arch="x86_64" ;;
    *)
        printf 'unsupported target for updater artifact: %s\n' "${target}" >&2
        exit 1
        ;;
    esac

    local tarball="${out_dir}/OcHub_${version}_macos_${arch}.app.tar.gz"
    tar -czf "${tarball}" -C "${out_dir}" "$(basename "${app_path}")"
    printf 'updater artifact: %s\n' "${tarball}"

    # The .app itself is not a release asset; leaving it behind would be
    # uploaded alongside the tarball it duplicates.
    rm -rf "${app_path}"
}

if [[ -z "${APPLE_SIGNING_IDENTITY:-}" ]]; then
    cargo packager \
        --release \
        --packages ochub-app \
        --formats app,dmg \
        --out-dir "${out_dir}" \
        --target "${target}"
    archive_app_bundle
    exit 0
fi

required_signing_vars=(
    APPLE_CERTIFICATE
    APPLE_CERTIFICATE_PASSWORD
    APPLE_ID
    APPLE_PASSWORD
    APPLE_TEAM_ID
)
for variable in "${required_signing_vars[@]}"; do
    if [[ -z "${!variable:-}" ]]; then
        printf '%s is required when APPLE_SIGNING_IDENTITY is set.\n' "${variable}" >&2
        exit 1
    fi
done

icons=()
while IFS= read -r icon; do
    icons+=("${icon}")
done < <(find "${repo_root}/crates/app/assets/app-icons" -maxdepth 1 -type f \
    \( -name '*.icns' -o -name '*.png' \) -print | sort)

config_json="$(
    jq -cn \
        --arg version "${version}" \
        --arg target "${target}" \
        --arg binaries_dir "${repo_root}/target/${target}/release" \
        --arg out_dir "${out_dir}" \
        --arg assets "${repo_root}/crates/app/assets" \
        --arg license "${repo_root}/LICENSE" \
        --arg entitlements "${repo_root}/packaging/macos/entitlements.plist" \
        --arg info_plist "${repo_root}/packaging/macos/Info.plist" \
        --arg identity "${APPLE_SIGNING_IDENTITY}" \
        --argjson icons "$(printf '%s\n' "${icons[@]}" | jq -R . | jq -s .)" \
        '{
            productName: "OcHub",
            version: $version,
            identifier: "io.github.sleepstars.ochub",
            category: "DeveloperTool",
            description: "Native desktop manager for AI coding tools",
            authors: ["OcHub contributors"],
            publisher: "OcHub contributors",
            binaries: [{ path: "ochub", main: true }],
            binariesDir: $binaries_dir,
            outDir: $out_dir,
            targetTriple: $target,
            icons: $icons,
            resources: [
                { src: $assets, target: "assets" },
                { src: $license, target: "LICENSE" }
            ],
            macos: {
                minimumSystemVersion: "11.0",
                signingIdentity: $identity,
                entitlements: $entitlements,
                infoPlistPath: $info_plist
            },
            dmg: {
                windowSize: { width: 660, height: 420 },
                appPosition: { x: 180, y: 210 },
                appFolderPosition: { x: 480, y: 210 }
            }
        }'
)"

cargo packager --config "${config_json}" --formats app,dmg
archive_app_bundle
