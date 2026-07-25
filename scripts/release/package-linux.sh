#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"
target="${CARGO_BUILD_TARGET:-x86_64-unknown-linux-gnu}"
out_dir="${1:-dist}"

cd "${repo_root}"
mkdir -p "${out_dir}"

cargo build --release --locked --target "${target}" -p ochub-app
"target/${target}/release/ochub" --version
cargo packager \
    --release \
    --packages ochub-app \
    --formats deb,appimage \
    --out-dir "${out_dir}" \
    --target "${target}"
