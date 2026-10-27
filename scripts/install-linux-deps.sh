#!/usr/bin/env bash
set -euo pipefail

if [[ ${EUID} -ne 0 ]]; then
    printf '%s\n' 'Run this script with sudo.' >&2
    exit 1
fi

apt-get update
apt-get install --no-install-recommends --no-remove -y \
    build-essential \
    pkg-config \
    curl \
    wget \
    file \
    libwebkit2gtk-4.1-dev webkit2gtk-driver \
    libxdo-dev \
    libssl-dev \
    openssl \
    libayatana-appindicator3-dev \
    librsvg2-dev \
    sqlite3 \
    xvfb \
    xauth
