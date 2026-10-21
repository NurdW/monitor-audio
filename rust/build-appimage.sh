#!/usr/bin/env bash
# Build the Rust control panel into an AppImage.
#
# The point of the Rust build is that the file carries its own GUI toolkit, so it
# runs on a machine with no PySide6 and no Qt Python bindings installed. That is
# also why this is a separate script from ../build-appimage.sh: the Python build
# ships scripts and relies on the system, this one ships binaries.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"
root="$(cd .. && pwd)"
out="$root/dist"
appdir="$root/build/rust-appdir"
tool="$root/tools/appimagetool-x86_64.AppImage"

if [ ! -x "$tool" ]; then
    echo "appimagetool is missing at $tool" >&2
    echo "run ../build-appimage.sh once; it downloads the tool into tools/" >&2
    exit 1
fi

echo "building release binaries"
if ! cargo build --release; then
    echo "cargo build failed; nothing was packaged" >&2
    exit 1
fi

rm -rf "$appdir"
mkdir -p "$appdir/usr/bin"
cp target/release/monitor-audio "$appdir/usr/bin/"
cp target/release/monitor-audio-panel "$appdir/usr/bin/"
cp "$root/packaging/monitor-audio.svg" "$appdir/monitor-audio.svg"

# The panel looks for the service next to itself, which is what makes the two
# belong to one file rather than depending on what the machine already has.
cat > "$appdir/AppRun" <<'RUN'
#!/usr/bin/env bash
HERE="$(dirname "$(readlink -f "${0}")")"
exec "$HERE/usr/bin/monitor-audio-panel" "$@"
RUN
chmod +x "$appdir/AppRun"

cat > "$appdir/monitor-audio.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Version=1.0
Name=Monitor Audio
GenericName=Audio routing
Comment=Follow a window's monitor with its audio output
Exec=monitor-audio-panel
Icon=monitor-audio
Categories=AudioVideo;Qt;
Keywords=audio;output;monitor;speaker;route;pipewire;
Terminal=false
DESKTOP

version="$(cat "$root/VERSION")"
archive="$out/monitor-audio-rust-$version-x86_64.AppImage"
mkdir -p "$out"
rm -f "$archive"

echo "packing $archive"
ARCH=x86_64 "$tool" "$appdir" "$archive" || {
    echo "appimagetool failed" >&2
    exit 1
}

if [ -f "$archive" ]; then
    chmod +x "$archive"
    ls -lh "$archive"
    echo "built"
else
    echo "appimagetool reported success but produced no file" >&2
    exit 1
fi
