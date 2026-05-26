#!/usr/bin/env bash

set -euo pipefail

PREFIX="${HOME}/.local"
INSTALL_DEPS=1
BUILD_PROFILE="release"

usage() {
    cat <<'EOF'
Usage: ./install.sh [options]

Options:
  --prefix PATH   Install under PATH instead of ~/.local
  --system        Install under /usr/local
  --skip-deps     Skip OS package installation
  --debug         Build the debug profile instead of release
  -h, --help      Show this help message

Supported distributions:
  - Fedora
  - Arch Linux
EOF
}

log() {
    printf '[papdieo-install] %s\n' "$*"
}

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        printf 'Missing required command: %s\n' "$1" >&2
        exit 1
    }
}

sudo_cmd() {
    if [[ ${EUID} -eq 0 ]]; then
        "$@"
    else
        need_cmd sudo
        sudo "$@"
    fi
}

detect_distro() {
    if [[ -r /etc/os-release ]]; then
        . /etc/os-release
        case "${ID:-}" in
            fedora)
                printf 'fedora\n'
                return
                ;;
            arch)
                printf 'arch\n'
                return
                ;;
        esac

        case "${ID_LIKE:-}" in
            *rhel*|*fedora*)
                printf 'fedora\n'
                return
                ;;
            *arch*)
                printf 'arch\n'
                return
                ;;
        esac
    fi

    printf 'unknown\n'
}

install_deps() {
    local distro="$1"

    case "$distro" in
        fedora)
            need_cmd dnf
            sudo_cmd dnf install -y \
                rust \
                cargo \
                gcc \
                pkgconf-pkg-config \
                gstreamer1-devel \
                gstreamer1-plugins-base-devel \
                gstreamer1 \
                gstreamer1-plugins-good \
                gstreamer1-plugins-bad-free \
                gstreamer1-plugins-ugly-free \
                gstreamer1-plugin-libav
            ;;
        arch)
            need_cmd pacman
            sudo_cmd pacman -S --needed --noconfirm \
                rust \
                base-devel \
                pkgconf \
                wayland \
                gstreamer \
                gst-plugins-base \
                gst-plugins-good \
                gst-plugins-bad \
                gst-plugins-ugly \
                gst-libav
            ;;
        *)
            printf 'Unsupported distribution. Install dependencies manually, then rerun with --skip-deps.\n' >&2
            exit 1
            ;;
    esac
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix)
            [[ $# -ge 2 ]] || {
                printf '--prefix requires a path\n' >&2
                exit 1
            }
            PREFIX="$2"
            shift 2
            ;;
        --system)
            PREFIX="/usr/local"
            shift
            ;;
        --skip-deps)
            INSTALL_DEPS=0
            shift
            ;;
        --debug)
            BUILD_PROFILE="debug"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'Unknown option: %s\n' "$1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

if [[ $INSTALL_DEPS -eq 1 ]]; then
    DISTRO="$(detect_distro)"
    log "installing dependencies for ${DISTRO}"
    install_deps "$DISTRO"
fi

need_cmd cargo
log "building papdieo (${BUILD_PROFILE})"

if [[ "$BUILD_PROFILE" == "release" ]]; then
    cargo build --release
    BUILD_OUTPUT="target/release/papdieo"
else
    cargo build
    BUILD_OUTPUT="target/debug/papdieo"
fi

INSTALL_BIN_DIR="${PREFIX}/bin"
INSTALL_TARGET="${INSTALL_BIN_DIR}/papdieo"

if [[ "$PREFIX" == "/usr/local" ]]; then
    log "installing binary to ${INSTALL_TARGET}"
    sudo_cmd install -Dm755 "$BUILD_OUTPUT" "$INSTALL_TARGET"
else
    log "installing binary to ${INSTALL_TARGET}"
    install -Dm755 "$BUILD_OUTPUT" "$INSTALL_TARGET"
fi

log "installation complete"
if [[ ":${PATH}:" != *":${INSTALL_BIN_DIR}:"* ]]; then
    log "${INSTALL_BIN_DIR} is not in PATH; add it to run papdieo directly"
fi