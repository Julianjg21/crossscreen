#!/usr/bin/env bash
# Compila y empaqueta el receptor de CrossScreen para Linux x86_64 en
# dist/crossscreen-<versión>-linux-x64.tar.gz (visor + CLI + scripts + guía).
# El binario de OpenH264 NO va dentro: se descarga de Cisco con
# fetch-openh264.sh (licencia de patentes de H.264).
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
if [ -n "${GITHUB_SHA:-}" ]; then
  export CROSSSCREEN_BUILD_ID="${GITHUB_REF_NAME:-ci}-${GITHUB_SHA:0:7}"
else
  export CROSSSCREEN_BUILD_ID="$(git describe --tags --always --dirty 2>/dev/null || echo local)"
fi
echo "Build: $CROSSSCREEN_BUILD_ID"

if [ "${1:-}" != "--no-build" ]; then
  cargo build --release -p crossscreen-viewer -p crossscreen-cli
fi

NAME="crossscreen-${VERSION}-linux-x64"
OUT="dist/$NAME"
mkdir -p "$OUT"
# Sin símbolos de depuración (el perfil release los incluye): ~6x menos tamaño.
install -s -m 0755 target/release/crossscreen-viewer target/release/crossscreen "$OUT/"
install -m 0755 scripts/fetch-openh264.sh "$OUT/"
cp docs/11-receptor-linux.md "$OUT/RECEPTOR-LINUX.md"
cp LICENSE-MIT LICENSE-APACHE "$OUT/"
(cd "$OUT" && sha256sum crossscreen-viewer crossscreen fetch-openh264.sh RECEPTOR-LINUX.md > SHA256SUMS.txt)
tar -C dist -czf "dist/$NAME.tar.gz" "$NAME"
echo "Paquete: dist/$NAME.tar.gz"
sha256sum "dist/$NAME.tar.gz"
