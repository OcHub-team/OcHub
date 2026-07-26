#!/usr/bin/env bash
#
# Compile `crates/app/assets/app-icons/OcHub.icon` into an `Assets.car`.
#
# macOS 26 resolves an app icon through `CFBundleIconName` + `Assets.car`, and
# that is the only route that carries per-appearance artwork: a `.appiconset`
# silently drops every image tagged `luminosity/dark` (verified -- none of them
# reach the compiled catalog), so the light/dark pair has to come from an Icon
# Composer `.icon` package.
#
# The car has to be built *before* `cargo packager` assembles the bundle, not
# injected afterwards: editing a bundle invalidates its signature, and the
# release only signs once.
#
# The icon is deliberately a single appearance — the dark plate, matching what
# shipped before it. A light/dark pair was built and dropped, because macOS
# never showed the dark half: an icon's appearance is governed by the Icon &
# Widget Style setting in System Settings, not by Dark Mode, so on a default
# system both halves resolve to the same one and the second is dead weight.
#
# If that is ever revisited, two findings from the attempt are worth keeping.
# `hidden-specializations` is honoured and validated — a wrong value type or
# an unknown appearance name aborts the compile. `fill-specializations` is
# not: present, absent, or set to a wholly different colour, the compiled car
# is byte-identical, so the plate has to live in the layer artwork rather than
# in `fill`. Getting that backwards renders a white robot on a white plate.
#
# Everything here fails loudly on purpose. An app icon that quietly falls back
# is exactly the class of bug `packaging/README.md` records for signing -- the
# release goes green and the user is the one who finds out.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"
icon_path="${repo_root}/crates/app/assets/app-icons/OcHub.icon"

out_dir="${1:-}"
if [[ -z "${out_dir}" ]]; then
    printf 'usage: %s <output-dir>\n' "$0" >&2
    exit 2
fi

if [[ ! -d "${icon_path}" ]]; then
    printf 'missing icon source: %s\n' "${icon_path}" >&2
    exit 1
fi

# `actool` ships only with full Xcode. A machine whose `xcode-select` points at
# the Command Line Tools has `xcrun` but not this, and the error it produces
# ("unable to find utility") is easy to mistake for a broken checkout.
if ! actool="$(xcrun --find actool 2>/dev/null)"; then
    printf 'actool not found.\n' >&2
    printf 'It ships with full Xcode; `xcode-select -p` currently points at:\n  %s\n' \
        "$(xcode-select -p 2>/dev/null || printf '<unset>')" >&2
    printf 'Set DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer (the justfile\n' >&2
    printf 'already does) or run `sudo xcode-select -s /Applications/Xcode.app`.\n' >&2
    exit 1
fi

mkdir -p "${out_dir}"
out_dir="$(cd "${out_dir}" && pwd)"

# The deployment target does not change the output here (11.0 and 26.0 compile
# byte-identical catalogs), so it tracks the bundle's own floor rather than the
# release that introduced `.icon`.
"${actool}" \
    --compile "${out_dir}" \
    --app-icon OcHub \
    --platform macosx \
    --minimum-deployment-target 11.0 \
    --output-partial-info-plist "${out_dir}/icon-partial-info.plist" \
    --errors --warnings --notices \
    --output-format human-readable-text \
    "${icon_path}"

# actool reports failures in its plist/text output and still exits 0 for some of
# them, so the artifact itself is the check.
if [[ ! -f "${out_dir}/Assets.car" ]]; then
    printf 'actool produced no Assets.car in %s\n' "${out_dir}" >&2
    exit 1
fi

printf 'app icon catalog: %s (%s bytes)\n' \
    "${out_dir}/Assets.car" "$(stat -f%z "${out_dir}/Assets.car")"
