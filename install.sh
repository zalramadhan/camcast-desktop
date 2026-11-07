#!/usr/bin/env bash
# Build + pasang CamCast sebagai aplikasi desktop (ikon di menu & Desktop).
set -e
cd "$(dirname "$0")"

cargo build --release

BIN="$HOME/.local/bin"
APPS="$HOME/.local/share/applications"
ICONS="$HOME/.local/share/icons/hicolor/scalable/apps"
mkdir -p "$BIN" "$APPS" "$ICONS"

install -m 755 target/release/camcast "$BIN/camcast"
cp assets/camcast.svg "$ICONS/camcast.svg"

cat > "$APPS/camcast.desktop" <<DESK
[Desktop Entry]
Type=Application
Name=CamCast
Comment=Mirror dan rekam kamera HP
Exec=$BIN/camcast
Icon=camcast
Terminal=false
Categories=AudioVideo;Video;
DESK
chmod +x "$APPS/camcast.desktop"

# shortcut di Desktop (kalau foldernya ada)
DESK_DIR="$(xdg-user-dir DESKTOP 2>/dev/null || echo "$HOME/Desktop")"
if [ -d "$DESK_DIR" ]; then
  cp "$APPS/camcast.desktop" "$DESK_DIR/camcast.desktop"
  chmod +x "$DESK_DIR/camcast.desktop"
  gio set "$DESK_DIR/camcast.desktop" metadata::trusted true 2>/dev/null || true
fi

update-desktop-database "$APPS" 2>/dev/null || true
echo
echo "Selesai. Buka 'CamCast' dari menu aplikasi atau ikon di Desktop."
echo "Rekaman tersimpan di: $HOME/Videos/CamCast"
