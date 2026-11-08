#!/usr/bin/env bash
# Descarga el decodificador H.264 OpenH264 **oficial de Cisco** y verifica su
# SHA-256. El visor de CrossScreen en Linux lo carga en tiempo de ejecución:
# usar el binario de Cisco hace que la licencia de patentes de H.264 la cubra
# Cisco (no se puede redistribuir dentro del paquete de CrossScreen).
#
# Uso: ./fetch-openh264.sh [directorio]   (por defecto ~/.local/share/crossscreen)
set -euo pipefail

VERSION="2.6.0"
FILE="libopenh264-${VERSION}-linux64.8.so"
SHA256="2f0cde7c6a6abcf5cae76942894ea42897fa677bce4ed6c91a24dd1b041d5f04"
URL="http://ciscobinary.openh264.org/${FILE}.bz2"
DEST="${1:-${XDG_DATA_HOME:-$HOME/.local/share}/crossscreen}"

if [ "$(uname -m)" != "x86_64" ]; then
  echo "Este script descarga la versión x86_64; arquitectura actual: $(uname -m)" >&2
  exit 1
fi

mkdir -p "$DEST"
tmp="$(mktemp)"
trap 'rm -f "$tmp" "$tmp.out"' EXIT

echo "Descargando ${FILE} de Cisco…"
curl -fsSL "$URL" -o "$tmp"
bunzip2 -c "$tmp" > "$tmp.out"

got="$(sha256sum "$tmp.out" | cut -d' ' -f1)"
if [ "$got" != "$SHA256" ]; then
  echo "ERROR: el SHA-256 no coincide (esperado $SHA256, obtenido $got). No se instala." >&2
  exit 1
fi

install -m 0644 "$tmp.out" "$DEST/$FILE"
echo "Instalado y verificado: $DEST/$FILE"
echo "El visor lo encuentra solo en esa ruta (o indica otra con CROSSSCREEN_OPENH264)."
