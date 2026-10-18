#!/usr/bin/env bash
# e2e против двух копий ядра: вендоренного подмодуля core/ (пин-эталон
# контракта) и свежего клона ../shopot-core (ловит дрейф, когда клон уезжает
# вперёд пина). Бинарники собираются, только если их нет.
set -euo pipefail
cd "$(dirname "$0")/.."

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
ZIG_BIN="$(find_zig)"

run_against() {
    local root="$1"
    # Новый layout монорепы stille-messenger-core: zig-проект в core/;
    # старый layout (до переезда) держал его в zig/.
    local proj="$root/core"
    if [ ! -f "$proj/build.zig" ]; then
        proj="$root/zig"
    fi
    local bin="$proj/zig-out/bin/stille-messenger-core"
    if [ ! -x "$bin" ] && [ -x "$proj/zig-out/bin/stille-messenger" ]; then
        bin="$proj/zig-out/bin/stille-messenger"
    fi
    if [ ! -x "$bin" ] && [ -x "$proj/zig-out/bin/shopot-zig" ]; then
        bin="$proj/zig-out/bin/shopot-zig"
    fi
    if [ ! -x "$bin" ]; then
        echo "== сборка ядра: $proj"
        (cd "$proj" && "$ZIG_BIN" build -Doptimize=ReleaseFast)
    fi
    echo "== e2e против $bin"
    # Демоны тестов — отдельные процессы; под параллельной нагрузкой
    # сьюта таймауты знакомства флакили. Сериализуем: e2e и так медленный,
    # честный гейт важнее секунд.
    STILLE_MESSENGER_BIN="$bin" cargo test --test e2e_daemon -- --test-threads=1
}

run_against core
if [ -d "${SHOPOT_CORE:-../shopot-core}" ]; then
    run_against "${SHOPOT_CORE:-../shopot-core}"
fi
