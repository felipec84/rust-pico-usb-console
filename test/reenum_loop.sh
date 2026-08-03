#!/usr/bin/env bash
# Mide la tasa de fallo de re-enumeracion USB tras ./flash.sh.
#
# Uso: test/reenum_loop.sh [N]
#   N = numero de iteraciones (default 10)
#
# En cada iteracion:
#   1. Corre ./flash.sh (falla o no, no abortamos el loop por eso).
#   2. Espera hasta 20s a que reaparezca el dispositivo de aplicacion
#      (VID:PID 2e8a:000a), sondeando con `uv run test/console_test.py --check`.
#   3. Si reaparece: OK, y ademas corre `--info` para capturar la linea
#      "Ultimo reset:" del banner/respuesta (dato clave que queremos medir).
#   4. Si NO reaparece en 20s: FAIL, aviso visible pidiendo desconectar/
#      reconectar el micro-USB, y espera Enter antes de seguir.
#
# Todo se loguea (append) en test/reenum_loop.log. Al final se imprime un
# resumen: N_ok/N_total, tasa de fallo, y las razones de reset observadas.
#
# Este script NO toca src/*.rs ni flash.sh, y no interpreta el bug: solo
# mide. Ver usb-reenum-investigation.md para el detalle del problema.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LOG_FILE="$SCRIPT_DIR/reenum_loop.log"

N="${1:-10}"
POLL_INTERVAL=1
MAX_WAIT_S=20

N_OK=0
N_FAIL=0
declare -a RESET_REASONS=()

log() {
    printf '%s\n' "$1" | tee -a "$LOG_FILE"
}

ts() {
    date '+%Y-%m-%d %H:%M:%S'
}

cd "$REPO_ROOT"

log ""
log "════════════════════════════════════════════════════"
log "[$(ts)] Iniciando reenum_loop.sh — N=$N iteraciones"
log "════════════════════════════════════════════════════"

for ((i = 1; i <= N; i++)); do
    log ""
    log "[$(ts)] --- Iteracion $i/$N ---"

    # 1) Flashear. No abortar el script si falla: es parte del dato.
    FLASH_OK=1
    if ! ./flash.sh >>"$LOG_FILE" 2>&1; then
        FLASH_OK=0
        log "[$(ts)] iter $i: ./flash.sh devolvio codigo de error (revisar log arriba)."
    fi

    # 2) Esperar reaparicion del dispositivo (VID:PID 2e8a:000a), hasta 20s.
    REAPPEARED=0
    ELAPSED=0
    while (( ELAPSED < MAX_WAIT_S )); do
        if uv run test/console_test.py --check >/dev/null 2>&1; then
            REAPPEARED=1
            break
        fi
        sleep "$POLL_INTERVAL"
        ELAPSED=$((ELAPSED + POLL_INTERVAL))
    done

    if (( REAPPEARED == 1 )); then
        N_OK=$((N_OK + 1))
        log "[$(ts)] iter $i: OK — dispositivo reaparecio en <= ${ELAPSED}s (flash_ok=$FLASH_OK)."

        # 3) Capturar el motivo de reset via --info.
        INFO_OUT=""
        if INFO_OUT="$(uv run test/console_test.py --info 2>&1)"; then
            RESET_LINE="$(printf '%s\n' "$INFO_OUT" | grep -i 'ultimo reset' || true)"
            if [ -z "$RESET_LINE" ]; then
                RESET_LINE="(sin linea 'Ultimo reset:' en la respuesta)"
            fi
        else
            RESET_LINE="(fallo --info: $INFO_OUT)"
        fi
        log "[$(ts)] iter $i: $RESET_LINE"
        RESET_REASONS+=("$RESET_LINE")
    else
        N_FAIL=$((N_FAIL + 1))
        log "[$(ts)] iter $i: FAIL — dispositivo NO reapareció tras ${MAX_WAIT_S}s (flash_ok=$FLASH_OK)."
        RESET_REASONS+=("(FAIL — sin re-enumeracion, sin dato de reset)")

        echo ""
        echo "############################################################"
        echo "#  FAIL en iteracion $i: la Pico no volvio a enumerar.      #"
        echo "#  Hay que DESCONECTAR y RECONECTAR el cable micro-USB      #"
        echo "#  fisicamente para recuperarla.                            #"
        echo "############################################################"
        echo ""

        # Sin terminal interactiva (agente, CI, salida redirigida) no tiene
        # sentido bloquear en `read`: nadie va a apretar Enter, y con `set -e`
        # el EOF abortaria el script perdiendo el resumen. En ese caso cortamos
        # el loop de forma limpia — el resumen igual se imprime abajo con lo
        # medido hasta acá, que es el dato que importa.
        if [ ! -t 0 ]; then
            log "[$(ts)] iter $i: sin TTY, corto el loop acá (faltan $((N - i)) iteraciones)."
            break
        fi

        read -r -p "Presiona Enter cuando hayas reconectado el cable... " _
        log "[$(ts)] iter $i: usuario confirmo reconexion manual, continuando."
    fi
done

log ""
log "════════════════════════════════════════════════════"
log "[$(ts)] Resumen final"
log "════════════════════════════════════════════════════"
log "OK: $N_OK/$N  |  FAIL: $N_FAIL/$N"
if (( N > 0 )); then
    FAIL_RATE=$(awk -v f="$N_FAIL" -v n="$N" 'BEGIN { printf "%.1f", (f/n)*100 }')
    log "Tasa de fallo: ${FAIL_RATE}%"
fi
log "Razones de reset observadas:"
for r in "${RESET_REASONS[@]}"; do
    log "  - $r"
done
log ""
log "Log completo en: $LOG_FILE"
