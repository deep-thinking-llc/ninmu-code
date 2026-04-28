#!/usr/bin/env bash
# Ninmu Code — pre-built binary installer
#
# Downloads the latest ninmu binary for your platform and installs it
# to /usr/local/bin (or $NINMU_INSTALL_DIR if set).
#
# Usage:
#   curl -sSf https://ninmu.dev/install.sh | sh
#   curl -sSf https://ninmu.dev/install.sh | sh -s -- --version v0.1.0
#
# Environment overrides:
#   NINMU_INSTALL_DIR    target directory (default: /usr/local/bin)
#   NINMU_VERSION        version to install (default: latest release)
#   NINMU_SKIP_VERIFY    set to 1 to skip signature/checksum verification

set -euo pipefail

REPO="deep-thinking-llc/claw-code"
INSTALL_DIR="${NINMU_INSTALL_DIR:-/usr/local/bin}"
VERSION="${NINMU_VERSION:-latest}"
SKIP_VERIFY="${NINMU_SKIP_VERIFY:-0}"

# ---- pretty printing ----
if [ -t 1 ] && command -v tput >/dev/null 2>&1 && [ "$(tput colors 2>/dev/null || echo 0)" -ge 8 ]; then
    R="$(tput sgr0)"
    B="$(tput bold)"
    D="$(tput dim)"
    G="$(tput setaf 2)"
    C="$(tput setaf 6)"
    Y="$(tput setaf 3)"
else
    R=""; B=""; D=""; G=""; C=""; Y=""
fi

info()  { printf "  %s->%s %s\n" "${C}" "${R}" "$1"; }
ok()    { printf "  %sok%s %s\n" "${G}" "${R}" "$1"; }
warn()  { printf "  %swarn%s %s\n" "${Y}" "${R}" "$1"; }
die()   { printf "  %serror%s %s\n" "$(tput setaf 1 2>/dev/null || echo '')" "${R}" "$1" >&2; exit 1; }

# ---- detect platform ----
UNAME_S="$(uname -s)"
UNAME_M="$(uname -m)"

case "${UNAME_S}" in
    Darwin)  OS="macos"  ;;
    Linux)   OS="linux"  ;;
    *)       die "unsupported OS: ${UNAME_S}" ;;
esac

case "${UNAME_M}" in
    x86_64|amd64) ARCH="x64"  ;;
    aarch64|arm64) ARCH="arm64" ;;
    *)            die "unsupported arch: ${UNAME_M}" ;;
esac

ARTIFACT="ninmu-${OS}-${ARCH}"
info "detected: ${OS} ${ARCH} → ${ARTIFACT}"

# ---- resolve version ----
if [ "${VERSION}" = "latest" ]; then
    info "resolving latest release..."
    TAG="$(curl -sfL "https://api.github.com/repos/${REPO}/releases/latest" | python3 -c "import sys,json; print(json.load(sys.stdin)['tag_name'])" 2>/dev/null)" || die "could not resolve latest version"
else
    TAG="${VERSION}"
fi
info "installing ${TAG}"

# ---- download ----
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${TAG}/${ARTIFACT}"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "${TMPDIR}"' EXIT

info "downloading ${DOWNLOAD_URL}..."
curl -sfL "${DOWNLOAD_URL}" -o "${TMPDIR}/ninmu" || die "download failed (check version/network)"
chmod +x "${TMPDIR}/ninmu"

# ---- verify ----
if [ "${SKIP_VERIFY}" != "1" ]; then
    info "verifying checksum..."
    CHECKSUM_URL="https://github.com/${REPO}/releases/download/${TAG}/checksums.txt"
    curl -sfL "${CHECKSUM_URL}" -o "${TMPDIR}/checksums.txt" 2>/dev/null || true
    if [ -f "${TMPDIR}/checksums.txt" ]; then
        if ! (cd "${TMPDIR}" && grep "${ARTIFACT}" checksums.txt | sha256sum -c - >/dev/null 2&1); then
            die "checksum verification failed for ${ARTIFACT}"
        fi
        ok "checksum verified"
    else
        info "no checksums.txt available, skipping checksum verification"
        info "verifying binary..."
        "${TMPDIR}/ninmu" --version >/dev/null 2>&1 || die "verification failed"
    fi
else
    info "verifying binary..."
    "${TMPDIR}/ninmu" --version >/dev/null 2>&1 || die "verification failed"
fi

# ---- install ----
mkdir -p "${INSTALL_DIR}"
cp "${TMPDIR}/ninmu" "${INSTALL_DIR}/ninmu"
ok "installed to ${INSTALL_DIR}/ninmu"

# ---- verify in PATH ----
if command -v ninmu >/dev/null 2>&1; then
    ok "ninmu is ready ($(ninmu --version 2>/dev/null || echo "${TAG}"))"
else
    warn "ninmu installed but not in PATH — add ${INSTALL_DIR} to your \$PATH"
fi
