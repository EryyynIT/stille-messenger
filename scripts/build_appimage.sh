#!/usr/bin/env bash
# AppImage для релиза: собирает клиента и демона, складывает AppDir и
# гонит appimagetool (скачивается один раз в target/appimage/).
# Использование: scripts/build_appimage.sh [версия] → target/appimage/shopot-<версия>-x86_64.AppImage
set -euo pipefail
cd "$(dirname "$0")/.."

version="${1:-dev}"
work="target/appimage"
appdir="$work/shopot.AppDir"
tool="$work/appimagetool-x86_64.AppImage"
out="$work/shopot-${version}-x86_64.AppImage"

# cpal (S7) линкуется с libasound: dev-файлы либо системные, либо
# локальная сборка в ~/.local (см. check.sh).
if [ -f "$HOME/.local/lib/pkgconfig/alsa.pc" ]; then
    export PKG_CONFIG_PATH="$HOME/.local/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
fi

find_zig() {
    if [ -n "${ZIG:-}" ]; then echo "$ZIG"; return; fi
    if command -v zig >/dev/null 2>&1; then command -v zig; return; fi
    echo "$HOME/.local/opt/zig-0.17.0/zig"
}

if [ ! -x core/zig/zig-out/bin/shopot-zig ]; then
    echo "== сборка демона (baseline x86_64: без native, иначе SIGILL на старых CPU)"
    (cd core/zig && "$(find_zig)" build -Doptimize=ReleaseSafe -Dcpu=baseline)
fi

if [ ! -x target/release/shopot-gui ]; then
    echo "== сборка клиента"
    cargo build --release
fi

echo "== AppDir"
rm -rf "$appdir"
mkdir -p "$appdir/usr/bin"
cp target/release/shopot-gui "$appdir/usr/bin/"
# Демон кладётся под именем shopot: UdsLink спавнит «shopot serve» из PATH,
# когда сокета нет — внутри образа это продолжает работать.
cp core/zig/zig-out/bin/shopot-zig "$appdir/usr/bin/shopot"
cp packaging/appimage/AppRun "$appdir/"
cp packaging/appimage/io.github.EryyynIT.shopot.desktop \
    "$appdir/io.github.EryyynIT.shopot.desktop"
cp docs/icon.png "$appdir/io.github.EryyynIT.shopot.png"
install -Dm644 packaging/io.github.EryyynIT.shopot.metainfo.xml \
    "$appdir/usr/share/metainfo/io.github.EryyynIT.shopot.metainfo.xml"
chmod +x "$appdir/AppRun"

if [ ! -x "$tool" ]; then
    echo "== appimagetool (continuous, один раз в $work)"
    mkdir -p "$work"
    curl -fL --retry 3 -o "$tool" \
        https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
    chmod +x "$tool"
fi

echo "== $out"
ARCH=x86_64 "$tool" --appimage-extract-and-run "$appdir" "$out"
sha256sum "$out"
