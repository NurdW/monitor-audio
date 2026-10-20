#!/usr/bin/env bash
# Build the AppImage.
#
#   ./build-appimage.sh
#
# Needs: nothing installed by hand. appimagetool (which carries its own
# mksquashfs) is downloaded into ./tools on first use. The result lands in ./dist.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VERSION="$(tr -d '[:space:]' < "$ROOT/VERSION")"
ARCH="${ARCH:-$(uname -m)}"
APPDIR="$ROOT/build/monitor-audio.AppDir"
DIST="$ROOT/dist"
TOOL="${APPMAGETOOL:-$ROOT/tools/appimagetool-$ARCH.AppImage}"
BUNDLE="$APPDIR/usr/share/monitor-audio"

# mksquashfs is needed to pack the result. appimagetool carries its own copy, so
# a missing system one is only worth mentioning, not a reason to stop.
if ! command -v mksquashfs >/dev/null 2>&1; then
    echo "note: no system mksquashfs; using the one bundled in appimagetool"
fi

if [ ! -x "$TOOL" ]; then
    echo "== downloading appimagetool into $TOOL"
    mkdir -p "$(dirname "$TOOL")"
    curl -fL --retry 3 -o "$TOOL" \
        "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$ARCH.AppImage"
    chmod +x "$TOOL"
fi


echo "== assembling $APPDIR"
rm -rf "$APPDIR"
mkdir -p "$BUNDLE" "$DIST"

cp "$ROOT/packaging/AppRun" "$APPDIR/AppRun"
chmod +x "$APPDIR/AppRun"
cp "$ROOT/packaging/monitor-audio.desktop" "$APPDIR/monitor-audio.desktop"
cp "$ROOT/packaging/monitor-audio.svg" "$APPDIR/monitor-audio.svg"

# The installer in the control panel copies these three things onto the machine,
# so they have to travel inside the AppImage.
cp -r "$ROOT/backend" "$BUNDLE/backend"
cp -r "$ROOT/kwin-script" "$BUNDLE/kwin-script"
cp -r "$ROOT/ui" "$BUNDLE/ui"
mkdir -p "$BUNDLE/packaging"
cp "$ROOT/packaging/monitor-audio.service" "$BUNDLE/packaging/monitor-audio.service"
cp "$ROOT/VERSION" "$BUNDLE/VERSION"
find "$BUNDLE" -name '__pycache__' -type d -prune -exec rm -rf {} +
find "$BUNDLE" -name '*.bak' -delete

# A .desktop file that does not validate is ignored by some menus.
if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$APPDIR/monitor-audio.desktop" \
        || echo "warning: desktop file did not validate cleanly" >&2
fi

if [ "${1:-}" = "--assemble-only" ]; then
    echo "== stopped after assembly; run $APPDIR/AppRun to try it"
    exit 0
fi

OUTPUT="$DIST/monitor-audio-$VERSION-$ARCH.AppImage"
echo "== packing $OUTPUT"
# --appimage-extract-and-run avoids needing a FUSE mount just to build.
ARCH="$ARCH" "$TOOL" --appimage-extract-and-run "$APPDIR" "$OUTPUT"

chmod +x "$OUTPUT"
ls -lh "$OUTPUT"
