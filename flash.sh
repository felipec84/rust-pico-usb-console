#!/usr/bin/env bash
# Reprograma la Pico sin tocar botones físicos.
# Requiere: cargo, picotool, stty (coreutils)

set -euo pipefail

BINARY="${1:-target/thumbv6m-none-eabi/release/pico_usb_console}"
SERIAL="${PICO_PORT:-/dev/ttyACM0}"
MAX_WAIT=10

echo "════════════════════════════════════"
echo " Pico Flash Script"
echo "════════════════════════════════════"

echo "▶ [1/4] Compilando firmware (release)..."
cargo build --release
echo "   OK: $BINARY"

if [ -e "$SERIAL" ]; then
    echo "▶ [2/4] Enviando señal de reset (baud 1200) a $SERIAL ..."
    stty -F "$SERIAL" 1200 2>/dev/null || true
    sleep 1.5
else
    echo "▶ [2/4] Puerto $SERIAL no encontrado — esperando BOOTSEL manual..."
    echo "   (En el primer flash: mantén BOOTSEL y conecta el USB)"
fi

echo "▶ [3/4] Esperando dispositivo en modo BOOTSEL..."
FOUND=0
for i in $(seq 1 $MAX_WAIT); do
    if picotool info 2>/dev/null | grep -qE "RP2040|RP2350"; then
        FOUND=1
        break
    fi
    echo -n "."
    sleep 1
done
echo ""

if [ $FOUND -eq 0 ]; then
    echo "✗ ERROR: No se encontró la Pico en modo BOOTSEL tras ${MAX_WAIT}s."
    echo "  - Primer flash: conecta con BOOTSEL presionado"
    echo "  - Si es un reflash: verifica que el firmware usa panic-persist"
    echo "    (panic-halt congela el USB y requiere el botón físico)"
    exit 1
fi

echo "▶ [4/4] Cargando firmware con picotool..."
picotool load "$BINARY" -f -x
echo ""
echo "✅ Listo. La Pico está reiniciando."
echo "   Monitor: python3 -m serial.tools.miniterm $SERIAL 115200"