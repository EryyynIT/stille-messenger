#!/usr/bin/env bash
# e2e против двух копий ядра: вендоренного подмодуля core/ (пин-эталон
# контракта, сейчас 83ea91e = API 2.2.0) и свежего клона ../shopot-core
# (ловит дрейф, когда клон уезжает вперёд пина). Бинарники собираются,
# только если их нет.
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
    local bin="$root/zig/zig-out/bin/stille-messenger-core"
    if [ ! -x "$bin" ] && [ -x "$root/zig/zig-out/bin/shopot-zig" ]; then
        bin="$root/zig/zig-out/bin/shopot-zig"
    fi
    if [ ! -x "$bin" ]; then
        echo "== сборка ядра: $root"
        (cd "$root/zig" && "$ZIG_BIN" build -Doptimize=ReleaseFast)
    fi
    echo "== e2e против $bin"
    # Демоны тестов — отдельные процессы; под параллельной нагрузкой
    # сьюта таймауты знакомства флакили. Сериализуем: e2e и так медленный,
    # честный гейт важнее секунд.
    STILLE_MESSENGER_BIN="$bin" cargo test --test e2e_daemon -- --test-threads=1
}

run_against core
run_against "${SHOPOT_CORE:-../shopot-core}"
