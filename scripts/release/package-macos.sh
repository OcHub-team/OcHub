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

# Compile the app icon catalog before packaging. It ships as a bundle resource
# rather than being copied in afterwards: `cargo packager` signs the bundle it
# assembles, and adding a file to a signed bundle breaks the seal.
icon_build_dir="$(mktemp -d)"
trap 'rm -rf "${icon_build_dir}"' EXIT
"${script_dir}/../build-app-icon.sh" "${icon_build_dir}"
icon_catalog="${icon_build_dir}/Assets.car"

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

# Fail the release rather than ship a bundle that only *looks* signed.
#
# This is the check that would have caught the v0.1.0/v0.2.0 problem: those
# builds packaged and published successfully while the bundle carried nothing
# but the linker's automatic ad-hoc signature on the inner binary. `codesign`
# then reports "code has no resources but signature indicates they must be
# present", and macOS shows the user "已损坏 / is damaged" -- which reads as a
# corrupt download, so people delete the app instead of reporting it. The
# failure was invisible in CI because packaging itself succeeded.
verify_signed() {
    local path="$1"
    printf 'verifying signature: %s\n' "${path}"
    if ! codesign --verify --deep --strict --verbose=2 "${path}"; then
        printf 'signature verification failed for %s\n' "${path}" >&2
        exit 1
    fi
    codesign -dv --verbose=2 "${path}" 2>&1 | grep -E '^(Authority|TeamIdentifier|CodeDirectory)' || true
}

verify_signed_outputs() {
    local app_path dmg_path
    app_path="$(find "${out_dir}" -maxdepth 1 -type d -name '*.app' -print -quit)"
    [[ -n "${app_path}" ]] && verify_signed "${app_path}"
    while IFS= read -r dmg_path; do
        verify_signed "${dmg_path}"
    done < <(find "${out_dir}" -maxdepth 1 -type f -name '*.dmg')
}

# Three signing paths, in descending order of what the user experiences:
#
#   1. Developer ID + notarization -- the app opens with no warning at all.
#      The only option that lets a new user install by double-clicking.
#   2. Self-signed certificate -- Gatekeeper still refuses (its trust anchor is
#      Apple's root, and a self-signed cert has no chain to it), so the user
#      must approve once in System Settings > Privacy & Security. What this
#      does buy is a *valid seal*, which turns "damaged" into "unverified
#      developer", and a stable Designated Requirement, so per-app approvals
#      survive an update instead of being re-asked on every new build.
#   3. Unsigned -- the state that produces "damaged". Kept only so that a fork
#      without any credentials can still build.
signing_identity=""
notarize=false
signed=true
if [[ -n "${APPLE_SIGNING_IDENTITY:-}" ]]; then
    signing_identity="${APPLE_SIGNING_IDENTITY}"
    notarize=true
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
elif [[ -n "${MACOS_SELFSIGN_CERTIFICATE:-}" ]]; then
    if [[ -z "${MACOS_SELFSIGN_IDENTITY:-}" ]]; then
        printf 'MACOS_SELFSIGN_IDENTITY is required alongside MACOS_SELFSIGN_CERTIFICATE.\n' >&2
        exit 1
    fi
    # A self-signed certificate is never a *valid* identity to macOS
    # (`security find-identity -v` reports zero), so `codesign -s "<common
    # name>"` cannot resolve it and fails with "no identity found". Referring to
    # it by SHA-1 fingerprint is what works, which is why MACOS_SELFSIGN_IDENTITY
    # holds a hash rather than a name.
    signing_identity="${MACOS_SELFSIGN_IDENTITY}"
    # cargo-packager imports APPLE_CERTIFICATE into a throwaway keychain and adds
    # it to the search list, so reusing those variable names hands it the whole
    # job. Notarization stays off: it is only attempted when APPLE_ID and
    # friends are set, and Apple would reject a self-signed submission anyway.
    export APPLE_CERTIFICATE="${MACOS_SELFSIGN_CERTIFICATE}"
    export APPLE_CERTIFICATE_PASSWORD="${MACOS_SELFSIGN_CERTIFICATE_PASSWORD:-}"
    # These must be *absent*, not merely empty. cargo-packager decides whether
    # to notarize with `env::var_os(...)`, which returns Some("") for a variable
    # that is set but blank -- and GitHub Actions sets every `secrets.X` it is
    # handed to the empty string when the secret does not exist. Leaving them in
    # the environment makes it submit to notarytool with blank credentials and
    # fail the build with "Team ID must be at least 3 characters".
    unset APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID APPLE_KEYCHAIN_PROFILE
    unset APPLE_API_KEY APPLE_API_ISSUER APPLE_API_KEY_PATH
    printf 'signing with a self-signed certificate; Gatekeeper will still ask the user to approve once\n'
else
    printf 'no signing credentials; producing an UNSIGNED build\n' >&2
    printf 'macOS will report it as damaged when downloaded with quarantine set\n' >&2
    # Falls through to the same packager invocation as the signed paths rather
    # than running its own. It used to package straight from the Cargo.toml
    # metadata, which meant any resource added to the config below -- the icon
    # catalog, most recently -- silently missed this build.
    signing_identity=""
    signed=false
fi

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
        --arg icon_catalog "${icon_catalog}" \
        --arg license "${repo_root}/LICENSE" \
        --arg entitlements "${repo_root}/packaging/macos/entitlements.plist" \
        --arg info_plist "${repo_root}/packaging/macos/Info.plist" \
        --arg identity "${signing_identity}" \
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
                { src: $icon_catalog, target: "Assets.car" },
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

# Before archiving, while the .app is still on disk. `archive_app_bundle`
# removes it, and the tarball is what the in-app updater installs -- so an
# unsigned .app here would be pushed to every existing install.
if [[ "${signed}" == true ]]; then
    verify_signed_outputs
fi

if [[ "${notarize}" == true ]]; then
    # cargo-packager notarizes and staples the .app but only *signs* the .dmg
    # (src/package/dmg/mod.rs). A signed-but-un-notarized disk image is still
    # blocked when downloaded from a browser, so the DMG needs its own trip
    # through notarytool. cc-switch hit the same gap and does exactly this.
    while IFS= read -r dmg_path; do
        printf 'notarizing %s\n' "${dmg_path}"
        xcrun notarytool submit "${dmg_path}" \
            --apple-id "${APPLE_ID}" \
            --password "${APPLE_PASSWORD}" \
            --team-id "${APPLE_TEAM_ID}" \
            --wait
        # Staple, or Gatekeeper has to reach Apple to confirm the ticket and
        # an offline user is warned anyway.
        xcrun stapler staple "${dmg_path}"
        xcrun stapler validate "${dmg_path}"
    done < <(find "${out_dir}" -maxdepth 1 -type f -name '*.dmg')
fi

archive_app_bundle
