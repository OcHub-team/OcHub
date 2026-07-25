#!/usr/bin/env bash
# Sign the packages the in-app updater downloads.
#
# Produces a `<file>.sig` next to each updater payload, holding a
# base64-wrapped minisign signature. The client verifies it against the public
# key compiled into the binary (OCHUB_UPDATER_PUBKEY) before installing
# anything, so this signature -- not HTTPS and not SHA256SUMS -- is what stops
# a tampered release from being installed.
#
# Only formats the updater can actually apply are signed. A `.deb` is owned by
# the package manager and a Windows portable zip has no installer to re-run;
# both are check-only in the client, so a signature for them would imply a
# capability that does not exist.
#
# No key configured is not an error: the release still builds, and the client
# degrades to "open the release page".
set -euo pipefail

out_dir="${1:-dist}"

if [[ -z "${OCHUB_SIGNING_PRIVATE_KEY:-}" ]]; then
    printf 'OCHUB_SIGNING_PRIVATE_KEY is not set; publishing without updater signatures.\n' >&2
    printf 'The in-app updater will report that this build cannot self-install.\n' >&2
    exit 0
fi

# `cargo packager signer sign` reads both of these from the environment.
export CARGO_PACKAGER_SIGN_PRIVATE_KEY="${OCHUB_SIGNING_PRIVATE_KEY}"
export CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD="${OCHUB_SIGNING_PRIVATE_KEY_PASSWORD:-}"

signed=0
while IFS= read -r artifact; do
    printf 'signing %s\n' "${artifact}"
    cargo packager signer sign "${artifact}"
    signed=$((signed + 1))
done < <(
    find "${out_dir}" -maxdepth 1 -type f \
        \( -name '*.app.tar.gz' -o -name '*-setup.exe' -o -name '*.AppImage' \) |
        sort
)

if [[ "${signed}" -eq 0 ]]; then
    printf 'no updater payloads found in %s; nothing was signed\n' "${out_dir}" >&2
    exit 1
fi

printf 'signed %d updater payload(s)\n' "${signed}"
