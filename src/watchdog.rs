// ─── Watchdog + protección contra bootloop ─────────────────────────────────
// Tres responsabilidades separadas:
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
//  3. check_panic_loop: si el equipo arranca TRES veces seguidas desde un
//     pánico, salta a BOOTSEL — un pánico anterior al USB no se ve nunca
//     desde el host, y sin esto la placa queda inalcanzable.

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
    let mut ticks: u32 = 0;
    loop {
        watchdog.feed(Duration::from_millis(1500));
        Timer::after(Duration::from_millis(500)).await;
        ticks = ticks.saturating_add(1);
        if ticks == PANIC_COUNT_CLEAR_TICKS {
            // Ver check_panic_loop: sobrevivir este rato cuenta como arranque
            // sano. Única escritura después del boot, y nadie más la toca.
            unsafe { PANIC_BOOT_COUNT = 0 };
        }
    }
}

// ─── Bucle de pánicos → BOOTSEL ────────────────────────────────────────────
//
// Un pánico ANTES de que el USB se presente al host (un assert! del builder de
// embassy-usb, un unwrap en la inicialización) deja la placa en un bucle sin
// fin: panic-persist reinicia, el mismo código vuelve a entrar en pánico, y el
// host no ve nada — ni siquiera un error de enumeración, porque el pull-up de
// D+ nunca se levanta. El mensaje queda en RAM, pero sin USB nadie puede
// leerlo. Medido el 2026-09-29 en pico-ds18b20-datalogger (el assert! de
// max-interface-count de embassy-usb, ver README): la placa estaba en un
// equipo remoto y solo se recuperó enchufándola con BOOTSEL apretado.
//
// Por eso, tras MAX_CONSECUTIVE_PANIC_BOOTS arranques seguidos que vienen de
// un pánico, se salta a BOOTSEL (solo PICOBOOT): `picotool` puede reflashear
// sin tocar la placa. El mensaje sigue legible desde BOOTSEL, porque
// panic-persist solo borra su magic al leerlo, no el texto, y la ROM no pisa
// PANDUMP:
//
//   picotool save -r 0x2003FC00 0x20040000 panic.bin && strings panic.bin
//
// (dirección = PANDUMP en memory.x). "Seguidos" se corta cuando un arranque
// sobrevive PANIC_COUNT_CLEAR_TICKS: un pánico por hora no es un bucle.
//
// Por qué no chequear en check_bootloop: aquel cuenta timeouts del watchdog y
// termina en panic!(), cuyo mensaje SÍ se ve por USB en el arranque siguiente.
// Este caso es justo el contrario: el pánico impide que haya USB.
//
// `.uninit` por la misma razón que WATCHDOG_REBOOT_COUNT. Tras un power-on
// puede contener basura, pero ahí no hay mensaje de pánico y se pone a 0 sin
// leerla.
#[unsafe(link_section = ".uninit")]
static mut PANIC_BOOT_COUNT: u32 = 0;

const MAX_CONSECUTIVE_PANIC_BOOTS: u32 = 3;
const PANIC_COUNT_CLEAR_TICKS: u32 = 20; // × 500 ms de watchdog_task = 10 s

/// Debe llamarse al principio de `main()`, apenas leído el mensaje de
/// panic-persist y antes de inicializar nada que pueda volver a entrar en
/// pánico. No retorna si detecta el bucle.
pub fn check_panic_loop(came_from_panic: bool) {
    unsafe {
        PANIC_BOOT_COUNT = if came_from_panic { PANIC_BOOT_COUNT.wrapping_add(1) } else { 0 };
        if PANIC_BOOT_COUNT >= MAX_CONSECUTIVE_PANIC_BOOTS {
            PANIC_BOOT_COUNT = 0;
            embassy_rp::rom_data::reset_to_usb_boot(0, 1);
        }
    }
}
