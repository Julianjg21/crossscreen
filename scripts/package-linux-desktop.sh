#!/usr/bin/env bash
# Construye el instalador visual Linux de CrossScreen (AppImage y .deb).
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "$(uname -m)" != "x86_64" ]]; then
  echo "El instalador actual requiere Linux x86_64." >&2
  exit 1
fi

if ! command -v npm >/dev/null 2>&1; then
  echo "Falta npm; instala Node.js 22 o superior." >&2
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "Falta Rust; instala rustup y el toolchain estable." >&2
  exit 1
fi

if ! pkg-config --exists webkit2gtk-4.1 gtk+-3.0 libpipewire-0.3; then
  echo "Faltan dependencias GTK/WebKit/PipeWire. En Ubuntu ejecuta:" >&2
  echo "  sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libpipewire-0.3-dev pkg-config" >&2
  exit 1
fi

if [[ "${1:-}" != "--no-build" ]]; then
  (
    cd apps/desktop
    npm ci
    npm run build
  )
  cargo build --release -p crossscreen-agent -p crossscreen-viewer
fi

for binary in crossscreen-agent crossscreen-viewer; do
  if [[ ! -x "target/release/$binary" ]]; then
    echo "Falta target/release/$binary; compila los binarios antes de empaquetar." >&2
    exit 1
  fi
done

(
  cd apps/desktop
  npm exec tauri build -- --bundles appimage,deb
)

mkdir -p dist
bundle_dirs=(
  "target/release/bundle"
  "apps/desktop/src-tauri/target/release/bundle"
)
copied=0
for bundle_dir in "${bundle_dirs[@]}"; do
  if [[ -d "$bundle_dir" ]]; then
    find "$bundle_dir" -maxdepth 2 -type f \
      \( -name '*.AppImage' -o -name '*.deb' \) -exec cp {} dist/ \;
    copied=$((copied + $(find "$bundle_dir" -maxdepth 2 -type f \
      \( -name '*.AppImage' -o -name '*.deb' \) | wc -l)))
  fi
done
if [[ "$copied" -eq 0 ]]; then
  echo "Tauri no generó ningún bundle AppImage/deb." >&2
  exit 1
fi
echo "Instaladores generados en dist/:"
find dist -maxdepth 1 -type f \( -name '*.AppImage' -o -name '*.deb' \) -print
