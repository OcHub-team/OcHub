#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"
target="${CARGO_BUILD_TARGET:-x86_64-unknown-linux-gnu}"
out_dir="${1:-dist}"

cd "${repo_root}"
mkdir -p "${out_dir}"

version="$(
    cargo metadata --no-deps --format-version 1 |
        jq -r '.packages[] | select(.name == "ochub-app") | .version'
)"

cargo build --release --locked --target "${target}" -p ochub-app -p ochcli
"target/${target}/release/ochub" --version
"target/${target}/release/ochcli" version
cargo packager \
    --release \
    --packages ochub-app \
    --formats deb,appimage \
    --out-dir "${out_dir}" \
    --target "${target}"

staging="$(mktemp -d)"
trap 'rm -rf "${staging}"' EXIT
mkdir -p "${staging}/ochcli"
cp "target/${target}/release/ochcli" "${staging}/ochcli/"
cp "${repo_root}/LICENSE" "${staging}/ochcli/"
cp "${repo_root}/docs/CLI-INSTALL.md" "${staging}/ochcli/README.md"
tar -czf "${out_dir}/OcHub_${version}_linux_x86_64_cli.tar.gz" \
    -C "${staging}" ochcli
cp "target/${target}/release/ochcli" \
    "${out_dir}/OcHub_${version}_linux_x86_64_ochcli"
