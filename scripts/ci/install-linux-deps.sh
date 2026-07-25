#!/usr/bin/env bash
set -euo pipefail

if ! command -v apt-get >/dev/null 2>&1; then
    printf 'This helper currently supports Debian/Ubuntu runners only.\n' >&2
    exit 1
fi

sudo apt-get update
sudo apt-get install --no-install-recommends -y \
    build-essential \
    clang \
    cmake \
    curl \
    file \
    jq \
    libasound2-dev \
    libfontconfig-dev \
    libfuse2 \
    libglib2.0-dev \
    libssl-dev \
    libvulkan1 \
    libwayland-dev \
    libx11-xcb-dev \
    libxkbcommon-x11-dev \
    patchelf \
    pkg-config
