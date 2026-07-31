// ─── Watchdog + protección contra bootloop ─────────────────────────────────
// Dos responsabilidades separadas:
//  1. watchdog_task: alimenta el watchdog periódicamente. Si el executor se
//     cuelga (deadlock, loop infinito en alguna tarea), el chip se reinicia
//     solo en vez de quedar colgado indefinidamente.
//  2. check_bootloop: si el equipo se reinicia por timeout del watchdog TRES
//     veces seguidas, algo está mal de forma persistente (hardware suelto,
//     un bug que se dispara siempre en el mismo punto del boot) y seguir
//     reintentando automáticamente puede ser peor que quedarse detenido —
//     p. ej. si el reinicio hace chattear un relé o un actuador físico en
//     cada ciclo. Tres reinicios y listo: panic limpio (panic-persist deja
//     el mensaje visible por USB-CDC en el siguiente boot manual).

use embassy_rp::watchdog::{ResetReason, Watchdog};
use embassy_time::{Duration, Timer};

// `.uninit`: esta sección NO se inicializa a cero en el arranque — sobrevive
// a un soft-reset (panic-persist, SCB::sys_reset(), timeout del watchdog),
// que es justo lo que hace falta para poder CONTAR reinicios consecutivos.
// Si sobreviviera también a un power-on real dejaría de servir como
// contador ("consecutivos" perdería sentido), pero un power-on sí borra la
// RAM completa en el RP2040, así que el efecto neto es el correcto: cuenta
// reinicios encadenados sin intervención humana, se reinicia solo a 0 en
// cuanto hay un power-cycle real o un reset por otra causa.
//
// Acceso: un único punto de lectura/escritura (check_bootloop), llamado una
// sola vez, antes de spawnear ninguna tarea — no hay concurrencia posible
// todavía, por eso el `unsafe` de abajo es seguro sin mutex.
#[unsafe(link_section = ".uninit")]
static mut WATCHDOG_REBOOT_COUNT: u32 = 0;

const MAX_CONSECUTIVE_WATCHDOG_RESETS: u32 = 3;

/// Debe llamarse una sola vez, al principio de `main()`, antes de spawnear
/// tareas. Si detecta demasiados reinicios seguidos por timeout del
/// watchdog, entra en pánico a propósito (panic-persist captura el mensaje).
pub fn check_bootloop(reset_reason: Option<ResetReason>) {
    unsafe {
        if reset_reason == Some(ResetReason::TimedOut) {
            WATCHDOG_REBOOT_COUNT = WATCHDOG_REBOOT_COUNT.wrapping_add(1);
        } else {
            WATCHDOG_REBOOT_COUNT = 0;
        }

        if WATCHDOG_REBOOT_COUNT >= MAX_CONSECUTIVE_WATCHDOG_RESETS {
            panic!(
                "FATAL: Watchdog bootloop (>= {MAX_CONSECUTIVE_WATCHDOG_RESETS} reinicios consecutivos)"
            );
        }
    }
}

#[embassy_executor::task]
pub async fn watchdog_task(mut watchdog: Watchdog) {
    watchdog.start(Duration::from_millis(1500));
    loop {
        watchdog.feed(Duration::from_millis(1500));
        Timer::after(Duration::from_millis(500)).await;
    }
}
