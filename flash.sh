#!/usr/bin/env bash
# Reprograma la Pico sin tocar botones físicos.
# Requiere: cargo, picotool, elf2uf2-rs, stty (coreutils)

set -euo pipefail

FORCE=0
BINARY=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --force|-f)
            FORCE=1
            shift
            ;;
        *)
            if [ -z "$BINARY" ]; then
                BINARY="$1"
            else
                echo "✗ ERROR: Argumento desconocido: $1"
                exit 1
            fi
            shift
            ;;
    esac
done
BINARY="${BINARY:-target/thumbv6m-none-eabi/release/{{crate_name}}}"
MAX_WAIT=10

echo "════════════════════════════════════"
echo " Pico Flash Script"
echo "════════════════════════════════════"

echo "▶ [1/5] Compilando firmware (release)..."
cargo build --release
echo "   OK: $BINARY"

echo "▶ [2/5] Convirtiendo a UF2 con elf2uf2-rs..."
elf2uf2-rs "$BINARY" "${BINARY}.uf2"

SERIAL=""
if [ "$FORCE" -eq 1 ]; then
    echo "▶ [3/5] Modo FORZADO activado. Omitiendo búsqueda de puerto serial y reset."
elif [ -n "${PICO_PORT:-}" ]; then
    SERIAL="$PICO_PORT"
    echo "▶ Usando puerto forzado: $SERIAL"
else
    # Buscar por /dev/serial/by-id (interfaz CDC, sufijo -if01) en vez de
    # asumir /dev/ttyACM0: con más de un dispositivo CDC conectado (p. ej.
    # esta Pico + otra placa de pruebas), ttyACM0 puede terminar apuntando
    # al dispositivo equivocado. No filtramos por nombre de producto porque
    # este es un template genérico — cualquier proyecto derivado le puso su
    # propio USB product string en main.rs, y hardcodear ese nombre acá
    # rompería en cuanto lo cambies.
    BY_ID_PORTS=$(ls /dev/serial/by-id/usb-*-if01 2>/dev/null || true)
    PORT_COUNT=$(echo -n "$BY_ID_PORTS" | grep -c '^' || true)

    if [ -z "$BY_ID_PORTS" ]; then
        SERIAL="/dev/ttyACM0"
        echo "▶ [3/5] No se encontró nada en /dev/serial/by-id/, probando $SERIAL"
    elif [ "$PORT_COUNT" -gt 1 ]; then
        echo "✗ ERROR: Se encontró más de un dispositivo serie CDC conectado:"
        echo "$BY_ID_PORTS"
        echo "  Por seguridad, desconecta los otros dispositivos o especifica PICO_PORT."
        exit 1
    else
        SERIAL=$(readlink -f "$BY_ID_PORTS")
    fi
fi

if [ "$FORCE" -eq 1 ]; then
    : # nada que hacer — se salta directo a esperar BOOTSEL
elif [ -e "$SERIAL" ]; then
    echo "▶ [3/5] Enviando señal de reset (baud 1200) a $SERIAL ..."
    stty -F "$SERIAL" 1200 2>/dev/null || true
    sleep 1.5
else
    echo "▶ [3/5] Puerto $SERIAL no encontrado — esperando BOOTSEL manual..."
    echo "   (En el primer flash: mantén BOOTSEL y conecta el USB)"
fi

echo "▶ [4/5] Esperando dispositivo en modo BOOTSEL..."
FOUND=0
for i in $(seq 1 $MAX_WAIT); do
    if picotool info >/dev/null 2>&1; then
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

# Si hay más de una Pico en BOOTSEL a la vez, picotool puede cargar el
# firmware en la placa equivocada sin avisar cuál eligió.
if picotool info 2>/dev/null | grep -q "Multiple RP-series"; then
    echo "✗ ERROR: Se detectaron múltiples Raspberry Pi Pico en modo BOOTSEL."
    echo "  Por seguridad, desconecta la otra Pico antes de flashear."
    exit 1
fi

echo "▶ [5/5] Cargando firmware con picotool..."
picotool load "${BINARY}.uf2" -f -x
echo ""
echo "✅ Listo. La Pico está reiniciando."
if [ -n "$SERIAL" ]; then
    echo "   Monitor: python3 -m serial.tools.miniterm $SERIAL 115200"
else
    echo "   Monitor: python3 -m serial.tools.miniterm /dev/ttyACM0 115200 (o el puerto asignado)"
fi
