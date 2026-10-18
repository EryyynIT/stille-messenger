#!/usr/bin/env bash
# Local AppImage prototype: builds stille-messenger client/core, assembles
# an AppDir, and runs appimagetool (downloaded once into target/appimage/).
# Usage: scripts/build_appimage.sh [version] → target/appimage/stille-messenger-<version>-x86_64.AppImage
set -euo pipefail
cd "$(dirname "$0")/.."

version="${1:-dev}"
work="target/appimage"
appdir="$work/stille-messenger.AppDir"
tool="$work/appimagetool-x86_64.AppImage"
out="$work/stille-messenger-${version}-x86_64.AppImage"

if [ -f "$HOME/.local/lib/pkgconfig/alsa.pc" ]; then
    export PKG_CONFIG_PATH="$HOME/.local/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
fi

find_zig() {
    if [ -n "${ZIG:-}" ]; then echo "$ZIG"; return; fi
    if command -v zig >/dev/null 2>&1; then command -v zig; return; fi
    echo "$HOME/.local/opt/zig-0.17.0/zig"
}

if [ ! -x core/core/zig-out/bin/stille-messenger-core ]; then
    echo "== build core (baseline x86_64)"
    (cd core/core && "$(find_zig)" build -Doptimize=ReleaseSafe -Dcpu=baseline)
fi

if [ ! -x target/release/stille-messenger ]; then
    echo "== build client"
    cargo build --release
fi

echo "== AppDir"
rm -rf "$appdir"
mkdir -p "$appdir/usr/bin"
cp target/release/stille-messenger "$appdir/usr/bin/"
cp core/core/zig-out/bin/stille-messenger-core "$appdir/usr/bin/"
cp packaging/appimage/AppRun "$appdir/"
cp packaging/appimage/io.github.stille_messenger.desktop \
    "$appdir/io.github.stille_messenger.desktop"
cp docs/icon.png "$appdir/io.github.stille_messenger.png"
install -Dm644 packaging/io.github.stille_messenger.metainfo.xml \
    "$appdir/usr/share/metainfo/io.github.stille_messenger.metainfo.xml"
chmod +x "$appdir/AppRun"

if [ ! -x "$tool" ]; then
    echo "== appimagetool (continuous, once in $work)"
    mkdir -p "$work"
    curl -fL --retry 3 -o "$tool" \
        https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
    chmod +x "$tool"
fi

echo "== $out"
ARCH=x86_64 "$tool" --appimage-extract-and-run "$appdir" "$out"
sha256sum "$out"
