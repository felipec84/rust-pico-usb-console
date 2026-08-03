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
use portable_atomic::{AtomicU32, Ordering};

// Copia publicable del contador, para que `info` pueda mostrarlo sin volver
// a tocar el `static mut` de abajo. Se escribe una sola vez, desde
// check_bootloop(), antes de que exista ninguna tarea.
static LAST_BOOT_COUNT: AtomicU32 = AtomicU32::new(0);

/// Cuántos reinicios consecutivos por timeout del watchdog contaba el
/// firmware en ESTE arranque (0 = venimos de un reset que no fue timeout).
/// Diagnóstico: si esto sube solo tras un `picotool load -x`, quiere decir
/// que el reboot de la ROM se ve como timeout del watchdog y el umbral de
/// MAX_CONSECUTIVE_WATCHDOG_RESETS es una trampa latente.
pub fn boot_count() -> u32 {
    LAST_BOOT_COUNT.load(Ordering::Relaxed)
}

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

// ─── Distinguir "se colgó el firmware" de "picotool reinició la placa" ─────
//
// MEDIDO en hardware (2026-08-03): después de un `picotool load -x`,
// `reset_reason()` reporta **TimedOut**. Los reboots de la ROM (BOOTSEL,
// `picotool reboot`, `reset_to_usb_boot`) se implementan CON el watchdog, así
// que el registro de razón de reset no los distingue de un cuelgue real.
//
// Contar esos reboots como "timeout" convierte el guardia de bootloop en una
// trampa: tres `./flash.sh` seguidos llegaban al umbral y el firmware entraba
// en pánico a propósito en el tercer arranque, sin que hubiera ningún bug.
//
// La discriminación es la misma que usa pico-sdk en
// `watchdog_enable_caused_reboot()`: el scratch[4] del watchdog. Toda ruta de
// reboot de la ROM lo sobrescribe (con 0, o con su propio magic de entrada),
// mientras que nosotros lo dejamos con MAGIC en cada arranque. Entonces:
//
//   TimedOut + scratch[4] == MAGIC  -> timeout real, nuestro watchdog mordió
//   TimedOut + scratch[4] != MAGIC  -> reboot pedido por la ROM/picotool
//
// El valor en sí es arbitrario; solo tiene que ser improbable como basura.
const WATCHDOG_OWN_RESET_MAGIC: u32 = 0x7DC0_FFEE;
const SCRATCH_MAGIC_INDEX: usize = 4;

/// Debe llamarse una sola vez, al principio de `main()`, antes de spawnear
/// tareas. Si detecta demasiados reinicios seguidos por timeout del
/// watchdog, entra en pánico a propósito (panic-persist captura el mensaje).
pub fn check_bootloop(watchdog: &mut Watchdog, reset_reason: Option<ResetReason>) {
    // Leer el magic ANTES de re-armarlo: nos dice quién causó ESTE arranque.
    let was_our_watchdog = watchdog.get_scratch(SCRATCH_MAGIC_INDEX) == WATCHDOG_OWN_RESET_MAGIC;
    // Re-armar para el próximo arranque. Si el firmware se cuelga de verdad y
    // el watchdog muerde, el magic seguirá acá y el reset SÍ se contará.
    watchdog.set_scratch(SCRATCH_MAGIC_INDEX, WATCHDOG_OWN_RESET_MAGIC);

    unsafe {
        if reset_reason == Some(ResetReason::TimedOut) && was_our_watchdog {
            WATCHDOG_REBOOT_COUNT = WATCHDOG_REBOOT_COUNT.wrapping_add(1);
        } else {
            WATCHDOG_REBOOT_COUNT = 0;
        }

        LAST_BOOT_COUNT.store(WATCHDOG_REBOOT_COUNT, Ordering::Relaxed);

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
